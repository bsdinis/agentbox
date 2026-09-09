//! Who a running box belongs to, and when it powers off.
//!
//! A box launched with `shell`/`run` boots and stays up so more than one
//! session can attach to it. That raises a lifecycle question the transient
//! `--as-pid2` launch never had: when the last session leaves, does the box
//! stay running or shut down?
//!
//! The answer is "it depends who started it", recorded the moment the box goes
//! from stopped to running:
//!
//! * `up` marks the box `Up` - an explicit "keep this running"; it survives
//!   every session leaving and only `down` stops it.
//! * the first `shell`/`run` marks it `Session` - it exists to carry sessions,
//!   so when the last one exits it powers off.
//!
//! Live sessions are tracked as a *set* of files, one per attachment, not a
//! counter: the last session to exit is whichever leaves last, not the one that
//! arrived last, and a set makes that fall out for free. A crashed session
//! leaves a file behind; each release sweeps files whose owning process is
//! gone, the same way `host::sweep_stale_handovers` reaps abandoned handovers.
//!
//! All of this is read and written under a per-box `flock`, so a cold start and
//! a concurrent attach cannot race over who boots the box or who is last out.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::argv;
use crate::host::{dry_run, sh};
use crate::sandbox::Sandbox;

/// How a running box was first started, which decides whether it outlives its
/// sessions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owner {
    /// Booted by `up`: kept running until `down`.
    Up,
    /// Booted by the first `shell`/`run`: powers off with the last session.
    Session,
}

impl Owner {
    fn as_str(self) -> &'static str {
        match self {
            Owner::Up => "up",
            Owner::Session => "session",
        }
    }

    fn parse(text: &str) -> Option<Owner> {
        match text.trim() {
            "up" => Some(Owner::Up),
            "session" => Some(Owner::Session),
            _ => None,
        }
    }
}

fn runtime_dir(sb: &Sandbox) -> PathBuf {
    sb.dir().join("runtime")
}

fn owner_file(sb: &Sandbox) -> PathBuf {
    runtime_dir(sb).join("owner")
}

fn sessions_dir(sb: &Sandbox) -> PathBuf {
    runtime_dir(sb).join("sessions")
}

/// An exclusive advisory lock over one box's runtime state, held for as long as
/// the returned guard lives (the open file; closing it releases the lock).
struct Lock(#[allow(dead_code)] fs::File);

impl Lock {
    fn acquire(sb: &Sandbox) -> Result<Lock> {
        fs::create_dir_all(sb.dir())
            .with_context(|| format!("cannot create {}", sb.dir().display()))?;
        let path = sb.dir().join("lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        // SAFETY: the fd is valid for the call; LOCK_EX blocks until held.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot lock {}", path.display()));
        }
        Ok(Lock(file))
    }
}

/// Whether the box's container service is up right now.
fn running(sb: &Sandbox) -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", &sb.service()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
        .unwrap_or(false)
}

/// Boot the box's container service and wait for the start job to finish. The
/// stock `systemd-nspawn@.service` is `Type=notify`, so this returns once the
/// container signalled readiness; `nspawn::wait_attachable` covers the rest.
fn boot(sb: &Sandbox) -> Result<()> {
    sh(argv!["systemctl", "start", sb.service()])
        .run()
        .map(|_| ())
}

fn set_owner(sb: &Sandbox, owner: Owner) -> Result<()> {
    let dir = runtime_dir(sb);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    fs::write(owner_file(sb), owner.as_str())
        .with_context(|| format!("cannot write {}", owner_file(sb).display()))
}

fn owner(sb: &Sandbox) -> Option<Owner> {
    fs::read_to_string(owner_file(sb))
        .ok()
        .and_then(|text| Owner::parse(&text))
}

/// Boot the box if it is stopped and mark it `Up`.
///
/// `up` only ever raises persistence: a box already running as `Session` is
/// promoted to `Up` (an explicit "keep it"), never the reverse. It attaches no
/// session of its own.
pub fn ensure_up(sb: &Sandbox) -> Result<()> {
    if dry_run() {
        println!("  would boot {} if stopped and keep it up", sb.name);
        return Ok(());
    }
    let _lock = Lock::acquire(sb)?;
    if !running(sb) {
        boot(sb)?;
    }
    set_owner(sb, Owner::Up)
}

/// A registered attachment. `release` drops it and, if it was the last one on a
/// `Session`-owned box, powers the box off.
pub struct Handle {
    token: Option<String>,
}

/// Boot the box if needed and register one session against it.
///
/// A cold start records `Session` ownership and registers this session under
/// the same lock, so a box is never observed running-and-session-owned with no
/// sessions - the state a concurrent `release` would read as "power off".
pub fn begin(sb: &Sandbox) -> Result<Handle> {
    if dry_run() {
        println!("  would boot {} if stopped and open a session", sb.name);
        return Ok(Handle { token: None });
    }
    let _lock = Lock::acquire(sb)?;
    if !running(sb) {
        boot(sb)?;
        set_owner(sb, Owner::Session)?;
    }
    let token = register(sb)?;
    Ok(Handle { token: Some(token) })
}

/// Record this session as a file named by, and containing, our PID, so a later
/// sweep can tell a live session from one whose process is gone.
fn register(sb: &Sandbox) -> Result<String> {
    let dir = sessions_dir(sb);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let token = format!("{}-{nanos}", std::process::id());
    fs::write(dir.join(&token), std::process::id().to_string())
        .with_context(|| format!("cannot register session in {}", dir.display()))?;
    Ok(token)
}

impl Handle {
    /// Drop this session; power a `Session`-owned box off once it was the last.
    ///
    /// Best-effort by design: it runs on the way out of every launch, including
    /// a failed one, so a hiccup here must not mask the real result. A box left
    /// booted by such a hiccup is harmless and `down`/`ls` still reach it.
    pub fn release(self, sb: &Sandbox) {
        let Some(token) = self.token else {
            return; // dry-run handle
        };
        let Ok(_lock) = Lock::acquire(sb) else {
            return;
        };
        let dir = sessions_dir(sb);
        let _ = fs::remove_file(dir.join(&token));
        sweep(&dir);
        if !sessions_empty(&dir) {
            return;
        }
        if owner(sb) == Some(Owner::Session) {
            // `systemctl stop`, not `machinectl poweroff`: it shuts the
            // container down gracefully *and* waits for the unit to be gone
            // before returning. Because we still hold the lock, a launch racing
            // to reuse this box blocks until the box is fully stopped and then
            // boots it afresh, instead of catching it mid-shutdown.
            let _ = sh(argv!["systemctl", "stop", sb.service()])
                .quiet()
                .silent()
                .allow_fail()
                .run();
            let _ = fs::remove_dir_all(runtime_dir(sb));
        }
    }
}

/// Forget sessions whose owning process is gone (a crashed or killed launch),
/// so a stale file cannot keep a `Session`-owned box alive forever.
fn sweep(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let alive = fs::read_to_string(entry.path())
            .ok()
            .and_then(|pid| pid.trim().parse::<u32>().ok())
            .map(|pid| Path::new("/proc").join(pid.to_string()).exists())
            .unwrap_or(false);
        if !alive {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn sessions_empty(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|mut it| it.next().is_none())
        .unwrap_or(true)
}

/// Power a box off whoever started it, and drop its runtime state - what
/// `agentbox down` does. Under the lock and synchronous (`systemctl stop`
/// waits for the unit to be gone), so it serialises with `begin`/`release`: a
/// launch racing `down` waits for the box to be fully stopped, then boots a
/// fresh one, rather than attaching to a box on its way out.
pub fn down(sb: &Sandbox) -> Result<()> {
    if dry_run() {
        println!("  would power off {}", sb.name);
        return Ok(());
    }
    let _lock = Lock::acquire(sb)?;
    let _ = sh(argv!["systemctl", "stop", sb.service()])
        .allow_fail()
        .run();
    let _ = fs::remove_dir_all(runtime_dir(sb));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two strings are the on-disk record the poweroff policy reads back, so
    /// they must round-trip exactly - a typo would silently keep a session box
    /// running, or power an `up` box off under its user.
    #[test]
    fn owner_round_trips_through_its_on_disk_form() {
        for owner in [Owner::Up, Owner::Session] {
            assert_eq!(Owner::parse(owner.as_str()), Some(owner));
        }
        // Trailing newline (how it may be read back) is tolerated.
        assert_eq!(Owner::parse("session\n"), Some(Owner::Session));
        assert_eq!(Owner::parse("up\n"), Some(Owner::Up));
        // Anything else is "unknown", which the policy treats as "do not kill".
        assert_eq!(Owner::parse(""), None);
        assert_eq!(Owner::parse("keep"), None);
    }
}
