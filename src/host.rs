//! The host side of the privilege boundary: who invoked us, with what
//! environment, and how to get to root without losing either.

use std::collections::BTreeMap;
use std::ffi::{CStr, OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

/// The invoking human, resolved from the passwd database.
#[derive(Clone, Debug)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
}

/// Everything about the caller that survives the hop through sudo.
#[derive(Debug)]
pub struct Host {
    pub user: User,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub dry_run: bool,
}

static HOST: OnceLock<Host> = OnceLock::new();

pub fn host() -> &'static Host {
    HOST.get().expect("host() used before init_host()")
}

pub fn dry_run() -> bool {
    HOST.get().map(|h| h.dry_run).unwrap_or(false)
}

/// A variable from the *caller's* environment, not root's.
pub fn env_var(name: &str) -> Option<String> {
    host()
        .env
        .get(name)
        .cloned()
        .or_else(|| std::env::var(name).ok())
        .filter(|v| !v.is_empty())
}

pub fn init(handover: Option<&Path>, dry_run: bool) -> Result<()> {
    let host = match handover {
        Some(path) => {
            let h = Handover::consume(path)?;
            Host {
                user: lookup_user(h.uid)?,
                env: h.env,
                cwd: h.cwd,
                dry_run,
            }
        }
        None => Host {
            user: lookup_user(caller_uid())?,
            env: std::env::vars().collect(),
            cwd: std::env::current_dir().context("cannot read the working directory")?,
            dry_run,
        },
    };
    let _ = HOST.set(host);
    Ok(())
}

/// `sudo agentbox ...` keeps the real user in SUDO_UID; plain runs use our own.
fn caller_uid() -> u32 {
    std::env::var("SUDO_UID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| unsafe { libc::getuid() })
}

fn lookup_user(uid: u32) -> Result<User> {
    // SAFETY: getpwuid returns a pointer into a static buffer, valid until the
    // next call; we copy everything out before returning.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() {
            bail!("no passwd entry for uid {uid}");
        }
        let cstr = |p: *const libc::c_char| OsStr::from_bytes(CStr::from_ptr(p).to_bytes());
        Ok(User {
            name: cstr((*pw).pw_name).to_string_lossy().into_owned(),
            uid: (*pw).pw_uid,
            gid: (*pw).pw_gid,
            home: PathBuf::from(cstr((*pw).pw_dir)),
            shell: PathBuf::from(cstr((*pw).pw_shell)),
        })
    }
}

// --------------------------------------------------------------------------
// privilege escalation
// --------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct Handover {
    uid: u32,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
}

impl Handover {
    /// Read the caller's environment and destroy the file, whatever happens.
    fn consume(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path)
            .with_context(|| format!("cannot stat handover file {}", path.display()))?;
        let read = fs::read_to_string(path);
        let _ = fs::remove_file(path);
        if let Ok(expected) = std::env::var("SUDO_UID") {
            let expected: u32 = expected.parse().unwrap_or(meta.uid());
            if meta.uid() != expected {
                bail!("{} is not owned by the invoking user", path.display());
            }
        }
        let text = read.with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&text).context("malformed handover file")
    }
}

/// A directory we can actually create a file in, not merely one that exists.
fn writable_dir(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let Ok(cstr) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: cstr outlives the call and is a valid NUL-terminated string.
    unsafe { libc::access(cstr.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

/// The handover file is normally eaten by the child process, but a `sudo` that
/// never authenticates leaves it behind - carrying the caller's environment,
/// tokens included. In a runtime directory that is merely untidy; in $HOME it
/// persists across reboots, so drop any left by a process that is gone.
fn sweep_stale_handovers(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(".agentbox-handover-"))
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if !Path::new("/proc").join(pid).exists() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Re-exec ourselves under sudo, carrying the caller's environment in a 0600
/// file. `sudo VAR=x` is rejected by default sudoers and argv is world-readable
/// through /proc, but the environment may hold API tokens bound for `pass_env`.
pub fn ensure_root() -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return Ok(());
    }
    let uid = unsafe { libc::getuid() };
    // Existing is not the same as usable: a runtime directory can be present
    // but unwritable (a session that never got one, a damaged /run/user), so
    // fall through to the next candidate rather than failing outright.
    let dir = [
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        Some(PathBuf::from(format!("/run/user/{uid}"))),
        std::env::var_os("HOME").map(PathBuf::from),
    ]
    .into_iter()
    .flatten()
    .find(|p| writable_dir(p))
    .context(
        "no writable directory for the handover file \
         (tried $XDG_RUNTIME_DIR, /run/user/$UID, $HOME)",
    )?;
    sweep_stale_handovers(&dir);
    let path = dir.join(format!(".agentbox-handover-{}.json", std::process::id()));

    let handover = Handover {
        uid,
        cwd: std::env::current_dir().context("cannot read the working directory")?,
        env: std::env::vars().collect(),
    };
    fs::write(&path, serde_json::to_vec(&handover)?)
        .with_context(|| format!("cannot write {}", path.display()))?;
    let mut perms = fs::metadata(&path)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o600);
    fs::set_permissions(&path, perms)?;

    let exe = fs::read_link("/proc/self/exe").context("cannot resolve /proc/self/exe")?;
    let err = Command::new("sudo")
        .arg(exe)
        .arg("--internal-handover")
        .arg(&path)
        .args(std::env::args_os().skip(1))
        .exec();
    let _ = fs::remove_file(&path);
    Err(err).context("cannot re-exec under sudo")
}

// --------------------------------------------------------------------------
// running things
// --------------------------------------------------------------------------

pub fn oss(v: impl AsRef<OsStr>) -> OsString {
    v.as_ref().to_os_string()
}

/// Build an argv. Accepts anything `AsRef<OsStr>`: &str, String, &Path, PathBuf.
#[macro_export]
macro_rules! argv {
    ($($x:expr),* $(,)?) => { vec![$($crate::host::oss($x)),*] };
}

/// A command about to run on the host, with dry-run and logging built in.
pub struct Sh {
    argv: Vec<OsString>,
    quiet: bool,
    silent: bool,
    allow_fail: bool,
}

pub fn sh(argv: Vec<OsString>) -> Sh {
    Sh {
        argv,
        quiet: false,
        silent: false,
        allow_fail: false,
    }
}

impl Sh {
    /// Do not echo the command line.
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }

    /// Discard the command's own stdout and stderr.
    pub fn silent(mut self) -> Self {
        self.silent = true;
        self
    }

    /// A non-zero exit is a value, not an error.
    pub fn allow_fail(mut self) -> Self {
        self.allow_fail = true;
        self
    }

    fn rendered(&self) -> String {
        self.argv
            .iter()
            .map(|a| {
                let s = a.to_string_lossy();
                if s.contains(|c: char| c.is_whitespace() || c == '\'') {
                    format!("'{}'", s.replace('\'', r"'\''"))
                } else {
                    s.into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn build(&self) -> Command {
        let mut cmd = Command::new(&self.argv[0]);
        cmd.args(&self.argv[1..]);
        if self.silent {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        cmd
    }

    /// Run to completion. Returns whether it succeeded.
    pub fn run(self) -> Result<bool> {
        if dry_run() {
            println!("  {}", self.rendered());
            return Ok(true);
        }
        if !self.quiet {
            crate::info(&self.rendered());
        }
        let status = self
            .build()
            .status()
            .with_context(|| format!("cannot run {:?}", self.argv[0]))?;
        if !status.success() && !self.allow_fail {
            bail!("command failed ({}): {}", status, self.rendered());
        }
        Ok(status.success())
    }

    /// Run and capture trimmed stdout. Failure yields an empty string.
    pub fn output(self) -> String {
        if dry_run() {
            return String::new();
        }
        self.build()
            .stderr(Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    /// Replace this process, so the child owns the terminal and signals.
    pub fn exec(self) -> Result<std::convert::Infallible> {
        if dry_run() {
            println!("  {}", self.rendered());
            std::process::exit(0);
        }
        let err = self.build().exec();
        Err(err).with_context(|| format!("cannot exec {:?}", self.argv[0]))
    }
}
