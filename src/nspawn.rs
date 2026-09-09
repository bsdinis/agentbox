//! Assembling a box and handing it to systemd-nspawn: the overlay mount, the
//! generated settings file, and the launch itself.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::argv;
use crate::config::{Network, BACKGROUND_AUTO, PROJECT_FILE, UID_RANGE};
use crate::host::{dry_run, env_var, host, sh};
use crate::sandbox::{overbroad_reason, Bind, Sandbox, NSPAWN_DIR, UNIT_DIR};
use crate::{base, info};

// --------------------------------------------------------------------------
// the overlay
// --------------------------------------------------------------------------

pub fn is_mounted(path: &Path) -> bool {
    let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let target = path.to_string_lossy();
    mountinfo.lines().any(|line| {
        // fields: id parent dev:node root mountpoint ...
        line.split_whitespace()
            .nth(4)
            .is_some_and(|mp| mp == target)
    })
}

/// lower = the shared base image, upper = this box's writes. The overlay
/// features that rewrite how upper refers to lower are switched off: they buy
/// little here and interact badly with an image whose UIDs were shifted.
pub fn mount(sb: &Sandbox) -> Result<()> {
    if is_mounted(&sb.root()) {
        return Ok(());
    }
    base::require()?;
    for dir in [sb.upper(), sb.work(), sb.root()] {
        sh(argv!["mkdir", "-p", dir]).quiet().run()?;
    }
    let options = format!(
        "lowerdir={base},upperdir={upper},workdir={work},\
         index=off,metacopy=off,redirect_dir=off,xino=off",
        base = base::image().display(),
        upper = sb.upper().display(),
        work = sb.work().display(),
    );
    sh(argv![
        "mount",
        "-t",
        "overlay",
        format!("agentbox-{}", sb.name),
        "-o",
        options,
        sb.root()
    ])
    .run()?;
    Ok(())
}

pub fn umount(sb: &Sandbox) -> Result<()> {
    if is_mounted(&sb.root()) {
        sh(argv!["umount", sb.root()]).run()?;
    }
    Ok(())
}

// --------------------------------------------------------------------------
// the settings file
// --------------------------------------------------------------------------

/// A `.nspawn` file is line-oriented and systemd offers no way to escape a
/// newline inside a value, so a control character in a config-derived value
/// would let it break out of its line and inject its own directives - a second
/// `Bind=/:/x`, `PrivateUsers=no`, a whole `[Files]` section. There is nothing
/// safe to write, so refuse it rather than try to encode it. `char::is_control`
/// covers `\n`, `\r`, every other C0 control and the C1 range.
fn reject_control_chars(what: &str, value: &str) -> Result<()> {
    if let Some(bad) = value.chars().find(|c| c.is_control()) {
        bail!(
            "{what} contains a control character (U+{:04X}); \
             refusing to write it into the .nspawn settings file",
            bad as u32
        );
    }
    Ok(())
}

/// Colons separate the fields of a `Bind=`, so they have to be escaped. A
/// control character has no escape here (see `reject_control_chars`), so a path
/// carrying one is refused rather than written.
fn escape(path: &Path) -> Result<String> {
    let path = path.to_string_lossy();
    reject_control_chars("a bind path", &path)?;
    Ok(path.replace('\\', r"\\").replace(':', r"\:"))
}

/// systemd-nspawn currently lets a container use every socket address family,
/// but warns on every launch that a future version will narrow the default to
/// AF_INET, AF_INET6 and AF_UNIX, and asks to be told which it should be.
///
/// Say "all of them", explicitly. Narrowing would be the wrong default here: a
/// box is a working dev machine, and AF_NETLINK alone is what `ip`, `ss`, udev,
/// glibc's resolver and - in nat mode - the container's own networkd all need.
/// The isolation agentbox actually relies on is the user namespace and the
/// mount plan, not a socket filter. Stating it also pins today's behaviour
/// across the version where the default changes.
///
/// The setting and the warning both arrived in systemd 261, so on anything
/// older it is omitted: there is no warning to silence, and an unknown key in a
/// .nspawn file only earns a different complaint.
const RESTRICT_ADDRESS_FAMILIES_SINCE: u32 = 261;

/// Written to both places it can go, for different reasons: the settings file
/// is all a booted box reads, and the command line is what was measured to
/// silence the notice on the launches a person actually watches. Both are
/// omitted below 261, where the flag is an unrecognised option and the key an
/// unknown setting.
fn address_families(sb: &Sandbox) -> Option<String> {
    (systemd_version()? >= RESTRICT_ADDRESS_FAMILIES_SINCE)
        .then(|| sb.cfg.address_families.clone().unwrap_or_default())
}

/// The major version of the systemd on this host, from `systemd-nspawn
/// --version` - probed directly rather than through `sh`, which reports
/// nothing under `--dry-run`, and which would log a line per call.
fn systemd_version() -> Option<u32> {
    static VERSION: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *VERSION.get_or_init(|| {
        let out = std::process::Command::new("systemd-nspawn")
            .arg("--version")
            .output()
            .ok()?;
        parse_systemd_version(&String::from_utf8_lossy(&out.stdout))
    })
}

/// `systemd 261 (261.2-1-arch)` -> 261.
fn parse_systemd_version(output: &str) -> Option<u32> {
    output
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// One source of truth for both `agentbox shell` (which runs nspawn directly)
/// and `agentbox up` (which goes through systemd-nspawn@.service).
pub fn settings_text(sb: &Sandbox) -> Result<String> {
    let mut out = String::from("# generated by agentbox - edit .agentbox.toml instead\n");
    out.push_str("[Exec]\n");
    out.push_str(&format!("PrivateUsers={}:{}\n", sb.cfg.uid_base, UID_RANGE));
    // Deliberately no User= or WorkingDirectory=. This file applies to *every*
    // launch of the box, and `User=` names the user to invoke the container's
    // main process as - which for a booted box is systemd itself. Running PID 1
    // as the sandbox user leaves it unable to create /init.scope, so it dies
    // with "Failed to allocate manager object: Permission denied" a second
    // after `agentbox up` reports success. The direct launches pass both on the
    // command line instead, where they only affect that one payload.
    let hostname = sb.hostname();
    reject_control_chars("the hostname", &hostname)?;
    out.push_str(&format!("Hostname={hostname}\n"));
    out.push_str("Timezone=copy\n");
    out.push_str("LinkJournal=no\n");
    out.push_str(match sb.cfg.network {
        Network::None => "ResolvConf=off\n",
        _ => "ResolvConf=copy-host\n",
    });
    if let Some(families) = address_families(sb) {
        out.push_str(&format!("RestrictAddressFamilies={families}\n"));
    }
    for (key, value) in sb.env() {
        reject_control_chars("an environment variable name", &key)?;
        reject_control_chars("an environment variable value", &value)?;
        out.push_str(&format!("Environment={key}={value}\n"));
    }

    out.push_str("\n[Files]\n");
    // The image is pre-shifted on disk, so nothing needs chowning or mapping
    // at start; see docs/design.md.
    out.push_str("PrivateUsersOwnership=off\n");
    for bind in sb.binds() {
        let key = if bind.read_only {
            "BindReadOnly"
        } else {
            "Bind"
        };
        out.push_str(&format!(
            "{key}={}:{}:owneridmap\n",
            escape(&bind.src)?,
            escape(&bind.dst)?
        ));
    }

    out.push_str("\n[Network]\n");
    out.push_str(match sb.cfg.network {
        Network::Host => "Private=no\nVirtualEthernet=no\n",
        Network::None => "Private=yes\nVirtualEthernet=no\n",
        Network::Nat => "Private=yes\nVirtualEthernet=yes\n",
    });
    Ok(out)
}

/// The configured resource caps, in the `systemctl set-property` spelling that
/// both `--property=` and a unit drop-in take.
fn caps(sb: &Sandbox) -> Vec<(&'static str, String)> {
    [
        ("MemoryMax", &sb.cfg.memory_max),
        ("CPUQuota", &sb.cfg.cpu_quota),
        ("TasksMax", &sb.cfg.tasks_max),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.clone().map(|value| (key, value)))
    .collect()
}

/// Drop-in directory for this box's instance of `systemd-nspawn@.service`.
pub fn unit_dropin_dir(sb: &Sandbox) -> PathBuf {
    Path::new(UNIT_DIR).join(format!("{}.d", sb.service()))
}

/// Caps for a booted box.
///
/// A cap is a property of a unit, not of a container, so the generated .nspawn
/// file has nowhere to put it: `agentbox up` starts an instance of
/// systemd-nspawn@.service, and the caps belong in a drop-in on that. Rewritten
/// only when the content actually changes, since applying it costs a
/// daemon-reload.
pub fn write_unit_caps(sb: &Sandbox) -> Result<()> {
    let dir = unit_dropin_dir(sb);
    let path = dir.join("50-agentbox-caps.conf");
    let caps = caps(sb);
    let mut want = String::new();
    if !caps.is_empty() {
        want.push_str("# generated by agentbox - edit .agentbox.toml instead\n[Service]\n");
        for (key, value) in &caps {
            want.push_str(&format!("{key}={value}\n"));
        }
    }
    if dry_run() {
        if !want.is_empty() {
            println!("--- {} ---", path.display());
            print!("{want}");
        }
        return Ok(());
    }
    if fs::read_to_string(&path).unwrap_or_default() == want {
        return Ok(());
    }
    if want.is_empty() {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    } else {
        fs::create_dir_all(&dir)?;
        fs::write(&path, &want).with_context(|| format!("cannot write {}", path.display()))?;
    }
    daemon_reload()
}

/// Drop a box's caps drop-in, on the way to deleting the box.
pub fn clear_unit_caps(sb: &Sandbox) -> Result<()> {
    let dir = unit_dropin_dir(sb);
    if !dir.exists() {
        return Ok(());
    }
    fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
    daemon_reload()
}

fn daemon_reload() -> Result<()> {
    sh(argv!["systemctl", "daemon-reload"])
        .quiet()
        .run()
        .map(|_| ())
}

pub fn write_settings(sb: &Sandbox) -> Result<()> {
    let text = settings_text(sb)?;
    if dry_run() {
        println!("--- {} ---", sb.settings().display());
        print!("{text}");
        return Ok(());
    }
    fs::create_dir_all(NSPAWN_DIR)?;
    fs::write(sb.settings(), text)
        .with_context(|| format!("cannot write {}", sb.settings().display()))
}

// --------------------------------------------------------------------------
// creating a box
// --------------------------------------------------------------------------

/// Idempotent: safe to call on every launch. Returns true if the box was new.
pub fn create(sb: &Sandbox) -> Result<bool> {
    check_supported(sb)?; // before the overlay, so a refusal leaves no state
    let fresh = !sb.dir().exists();
    mount(sb)?; // checks that the base image exists
    if !dry_run() {
        fs::create_dir_all(sb.dir())?;
        write_meta(sb)?;
        seed_identity(sb)?;
        for bind in sb.binds() {
            prepare_mount_point(sb, &bind)?;
        }
        mask_host_network_units(sb)?;
    }
    write_settings(sb)?;
    write_unit_caps(sb)?;
    if fresh && !dry_run() {
        set_login_shell(sb)?;
        if !sb.cfg.packages.is_empty() {
            info(&format!(
                "installing packages: {}",
                sb.cfg.packages.join(" ")
            ));
            let mut cmd = argv!["/usr/bin/pacman", "-Sy", "--noconfirm", "--needed"];
            cmd.extend(sb.cfg.packages.iter().map(crate::host::oss));
            run_in(sb, cmd, Some("root"), Some("/"))?;
        }
    }
    Ok(fresh)
}

fn write_meta(sb: &Sandbox) -> Result<()> {
    let meta = serde_json::json!({
        "name": sb.name,
        "project": sb.project,
        "user": sb.user.name,
        "uid_base": sb.cfg.uid_base,
        "network": sb.cfg.network.to_string(),
    });
    fs::write(sb.meta(), format!("{meta:#}\n"))
        .with_context(|| format!("cannot write {}", sb.meta().display()))
}

/// A machine ID generated once, and a hostname that follows the config.
fn seed_identity(sb: &Sandbox) -> Result<()> {
    let machine_id = sb.inside(Path::new("/etc/machine-id"));
    let unset = fs::read_to_string(&machine_id)
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    if unset {
        let mut bytes = [0u8; 16];
        getrandom(&mut bytes)?;
        let mut hex = String::with_capacity(33);
        for byte in bytes {
            use std::fmt::Write;
            write!(hex, "{byte:02x}")?;
        }
        hex.push('\n');
        fs::write(&machine_id, hex)?;
    }
    let hostname = sb.inside(Path::new("/etc/hostname"));
    let want = format!("{}\n", sb.hostname());
    if fs::read_to_string(&hostname).unwrap_or_default() != want {
        fs::write(&hostname, want)?;
    }
    Ok(())
}

fn getrandom(buf: &mut [u8]) -> Result<()> {
    let mut file = fs::File::open("/dev/urandom").context("cannot open /dev/urandom")?;
    std::io::Read::read_exact(&mut file, buf).context("cannot read /dev/urandom")
}

/// Container paths systemd-nspawn covers with a mount of its own.
///
/// Custom binds are set up *after* those, so a mount point prepared below one
/// of them is invisible by the time it matters: nspawn creates its own target
/// inside its tmpfs, owned by container root, and `owneridmap` then maps the
/// host owner onto container root rather than onto the sandbox user. A mode
/// 700 project directory becomes unenterable, and the failure surfaces as
/// nspawn refusing to chdir into it.
///
/// Supporting these destinations means picking a cost: `idmap` instead of
/// `owneridmap` would map container root to host root on the mount, which
/// hands the box a way to leave setuid-root binaries in the project; binding
/// our own directory over nspawn's tmpfs would make the box's /tmp persist in
/// the overlay instead of being a tmpfs. Until one is chosen, say so plainly.
const NSPAWN_OWNED: [&str; 5] = ["/tmp", "/run", "/dev", "/proc", "/sys"];

pub fn shadowed_by_nspawn(dst: &Path) -> Option<&'static str> {
    NSPAWN_OWNED.into_iter().find(|base| dst.starts_with(base))
}

/// Why a bind destination is unsafe to place inside the box, or `None` if it
/// is safe. `Sandbox::inside` builds the on-host target as
/// `root().join(dst.strip_prefix("/"))` with no normalization, so a `dst`
/// that is not absolute, or that carries `..`/`.` components, would let the
/// join escape the box rootfs - and since the mount point is *created and
/// chowned as root on the host* before nspawn ever starts, an escaping `dst`
/// (e.g. `/../../../home/me/i_win`) turns into an arbitrary host path owned by
/// root. Require an absolute, lexically normalized path so that join can only
/// ever land under `root()`.
fn unsafe_dst(dst: &Path) -> Option<String> {
    use std::path::Component;
    if !dst.is_absolute() {
        return Some(format!("{} is not absolute", dst.display()));
    }
    for comp in dst.components() {
        match comp {
            Component::RootDir | Component::Normal(_) => {}
            Component::ParentDir => {
                return Some(format!("{} contains a `..` component", dst.display()));
            }
            Component::CurDir => {
                return Some(format!("{} contains a `.` component", dst.display()));
            }
            Component::Prefix(_) => {
                return Some(format!("{} contains a path prefix", dst.display()));
            }
        }
    }
    None
}

/// Lexical containment, checked without touching the filesystem: does
/// `target`, once its `.`/`..` components are resolved, stay within `root`?
/// A defense-in-depth guard for the privileged create/chown in
/// [`prepare_mount_point`], independent of the up-front [`unsafe_dst`] gate.
/// A `..` that would climb above the path's own root makes it return `false`.
fn within_root(root: &Path, target: &Path) -> bool {
    use std::path::Component;
    fn normalize(p: &Path) -> Option<PathBuf> {
        let mut out = PathBuf::new();
        for comp in p.components() {
            match comp {
                Component::ParentDir => {
                    if !out.pop() {
                        return None;
                    }
                }
                Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        Some(out)
    }
    match (normalize(root), normalize(target)) {
        (Some(r), Some(t)) => t.starts_with(&r),
        _ => false,
    }
}

/// Every bind nspawn would shadow, paired with the path it owns. Empty means
/// the mount plan is launchable.
pub fn unsupported_binds(sb: &Sandbox) -> Vec<(Bind, &'static str)> {
    sb.binds()
        .into_iter()
        .filter_map(|bind| shadowed_by_nspawn(&bind.dst).map(|base| (bind, base)))
        .collect()
}

/// Refuse a plan that cannot work, before anything is mounted or written.
///
/// Checked up front rather than when the mount point is prepared, so that a
/// doomed launch leaves no overlay, box directory or settings file behind, and
/// so that `status` and `--dry-run` can report the same thing without creating
/// anything at all.
pub fn check_supported(sb: &Sandbox) -> Result<()> {
    // Reject a destination that could escape the box rootfs before anything is
    // created or chowned. `inside()` does not normalize, so an un-normalized
    // `dst` from a `.agentbox.toml` would otherwise steer the privileged
    // create/chown at an arbitrary host path.
    for bind in sb.binds() {
        if let Some(reason) = unsafe_dst(&bind.dst) {
            bail!("refusing an unsafe bind destination: {reason}");
        }
        // A bind is mounted with owneridmap, so its source is writable inside
        // the box as the real host user. The project dir is the working dir the
        // user chose; a configured rw/ro map whose source is the filesystem
        // root, the host home, or an ancestor of it would hand the box the whole
        // host and defeat the sandbox, so refuse it before anything is mounted.
        if bind.src != sb.project {
            if let Some(reason) = overbroad_reason(&bind.src, &sb.user.home, state_dir()) {
                bail!(
                    "refusing {} map of {}: its source is {}; a box must not be \
                     granted access to the host beyond its project",
                    bind.kind(),
                    bind.src.display(),
                    reason
                );
            }
        }
    }
    let unsupported = unsupported_binds(sb);
    if unsupported.is_empty() {
        return Ok(());
    }
    let mut msg = String::from("this project cannot be mapped into a box:\n");
    for (bind, base) in &unsupported {
        let what = if bind.dst == sb.project {
            "the project directory"
        } else {
            "mapped directory"
        };
        msg.push_str(&format!(
            "  {what} {} is under {base}\n",
            bind.dst.display()
        ));
    }
    msg.push_str(
        "systemd-nspawn covers /tmp, /run, /dev, /proc and /sys with mounts of\n\
         its own, applied before the binds, so a mount point below one of them\n\
         is hidden and the sandbox user never ends up owning it.\n",
    );
    if unsupported.iter().any(|(bind, _)| bind.dst == sb.project) {
        msg.push_str("Move the project somewhere else.\n");
    }
    if unsupported.iter().any(|(bind, _)| bind.dst != sb.project) {
        msg.push_str(
            "Give the mapping a destination of its own, as in\n\
             `ro = [\"/tmp/sysroot:/sysroot\"]`.\n",
        );
    }
    msg.push_str("docs/troubleshooting.md has the long version.");
    bail!("{msg}")
}

/// `owneridmap` maps the host owner of the source onto *the owner of the
/// destination inside the container*, so the mount point has to exist first,
/// owned by the right container user.
///
/// Only for destinations at the top level of the box's filesystem. A
/// destination nested inside another bind is prepared here too, but that inode
/// is then hidden by the parent mount and never used: the mount point nspawn
/// actually lands on is the one inside the parent's *source*, which nspawn
/// creates itself if it is missing. The stray inode is left in the overlay.
fn prepare_mount_point(sb: &Sandbox, bind: &Bind) -> Result<()> {
    debug_assert!(
        shadowed_by_nspawn(&bind.dst).is_none(),
        "check_supported was skipped"
    );
    let target = sb.inside(&bind.dst);
    // Belt and suspenders: even though check_supported already rejected an
    // unsafe dst, never create or chown a path that lands outside the rootfs.
    let root = sb.root();
    if !within_root(&root, &target) {
        bail!(
            "refusing to prepare mount point {} outside the box rootfs {}",
            target.display(),
            root.display()
        );
    }
    let meta =
        fs::metadata(&bind.src).with_context(|| format!("cannot stat {}", bind.src.display()))?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if meta.is_dir() {
        if !target.exists() {
            fs::create_dir(&target)?;
        }
    } else if !target.exists() {
        fs::File::create(&target)?;
    }
    // Host UIDs outside the container's own range have no meaning inside it;
    // map those mounts onto container root instead.
    let inside = |id: u32| if id < UID_RANGE { id } else { 0 };
    std::os::unix::fs::chown(
        &target,
        Some(sb.shift(inside(meta.uid()))),
        Some(sb.shift(inside(meta.gid()))),
    )
    .with_context(|| format!("cannot chown {}", target.display()))
}

/// In host-network mode the box shares the host's network namespace, so a
/// booted box must never run networkd or resolved: it would reconfigure the
/// host's own interfaces. Mask them per box rather than in the shared base,
/// because nat mode needs networkd to bring up host0.
pub fn mask_host_network_units(sb: &Sandbox) -> Result<()> {
    let dir = sb.inside(Path::new("/etc/systemd/system"));
    fs::create_dir_all(&dir)?;
    for unit in [
        "systemd-networkd.service",
        "systemd-networkd.socket",
        "systemd-resolved.service",
    ] {
        let link = dir.join(unit);
        let masked = fs::read_link(&link)
            .map(|t| t == Path::new("/dev/null"))
            .unwrap_or(false);
        match sb.cfg.network {
            Network::Host if !masked => {
                let _ = fs::remove_file(&link);
                std::os::unix::fs::symlink("/dev/null", &link)?;
            }
            Network::Host => {}
            _ if masked => fs::remove_file(&link)?,
            _ => {}
        }
    }
    Ok(())
}

fn set_login_shell(sb: &Sandbox) -> Result<()> {
    let shell = sb.shell();
    run_in(
        sb,
        argv!["/usr/bin/chsh", "-s", shell, sb.user.name.clone()],
        Some("root"),
        Some("/"),
    )
    .map(|_| ())
}

// --------------------------------------------------------------------------
// the payload
// --------------------------------------------------------------------------

/// The PATH systemd-nspawn gives the payload.
///
/// nspawn builds the payload's environment itself rather than passing the
/// caller's through, so mirroring its lookup means searching this list and not
/// the host's $PATH. Copied from systemd-nspawn; a box that sets PATH through
/// `[env]` or `pass_env` overrides it.
const NSPAWN_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn container_path(sb: &Sandbox) -> String {
    sb.env()
        .remove("PATH")
        .unwrap_or_else(|| NSPAWN_PATH.to_string())
}

/// Where `program` resolves to, mirroring execvp: a name containing a slash is
/// a path, taken relative to `cwd`, and anything else is searched for along
/// `path`, whose empty elements mean `cwd`. `exists` answers for one candidate.
fn resolve(
    path: &str,
    cwd: &Path,
    program: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if program.as_os_str().as_bytes().contains(&b'/') {
        // join() drops cwd when the program is already absolute.
        let full = cwd.join(program);
        return exists(&full).then_some(full);
    }
    path.split(':')
        .map(|dir| match dir {
            "" => cwd.join(program),
            dir => Path::new(dir).join(program),
        })
        .find(|full| exists(full))
}

/// Refuse a payload the box does not have.
///
/// Left to nspawn this arrives as `execv(claude) failed: No such file or
/// directory`, after the box has been created, its login shell set and its
/// packages installed, and with nothing to say which of those was meant to
/// provide the program. Checked after `create` rather than before it for that
/// last reason: a fresh box installs its configured `packages` on the way up,
/// and those are exactly where the payload may be coming from.
///
/// Generous about what counts as found - the entry merely has to exist, with
/// its symlink unresolved and its execute bits unchecked. Following a
/// container symlink from the host would chase an absolute target into the
/// *host's* filesystem and answer about the wrong file, and the case worth
/// catching is a program that is not in the box at all.
pub fn check_payload(sb: &Sandbox, program: &OsStr) -> Result<()> {
    // Unmounted means we cannot answer, so say nothing and let nspawn try.
    if dry_run() || !is_mounted(&sb.root()) {
        return Ok(());
    }
    let path = container_path(sb);
    let program = Path::new(program);
    if resolve(&path, &sb.project, program, |full| {
        sb.inside(full).symlink_metadata().is_ok()
    })
    .is_some()
    {
        return Ok(());
    }

    let mut msg = format!(
        "{}: not found in box {}\n\
         A box is a separate system, so a program installed on the host is not in\n\
         it unless you put it there. Searched {path}.\n",
        program.display(),
        sb.name,
    );
    // The same lookup against the host answers "where is the copy I meant?",
    // which is the mapping the user almost always wants.
    let on_host =
        env_var("PATH").and_then(|path| resolve(&path, &host().cwd, program, |full| full.exists()));
    match on_host {
        Some(found) => msg.push_str(&format!(
            "The host has one at {0}, which {PROJECT_FILE} can map read-only:\n  \
             ro = [\"{0}\"]\n\
             Or install it in the box, now:\n",
            found.display(),
        )),
        None => msg.push_str("Install it in the box, now:\n"),
    }
    msg.push_str(&format!(
        "  agentbox run --root -- pacman -S PKG\n\
         or on every creation, in {PROJECT_FILE}:\n  \
         packages = [\"PKG\"]\n\
         docs/usage.md has the long version."
    ));
    bail!("{msg}")
}

// --------------------------------------------------------------------------
// launching
// --------------------------------------------------------------------------

/// True only when stdin, stdout and stderr are all terminals.
///
/// nspawn decides how to wire up the payload from whether it was "invoked on a
/// terminal", and either answer is wrong for a redirected launch. Invoked with
/// a terminal it allocates a pseudo-TTY, so piped stdout arrives as CRLF with
/// stderr merged into it, and EOF cannot propagate through a shell pipeline;
/// invoked without one it defaults to `read-only`, which drops our stdin. Both
/// make `agentbox run` unusable as a pipeline component, so anything short of
/// fully interactive asks for the raw descriptors instead.
fn interactive_stdio() -> bool {
    // SAFETY: isatty only inspects the descriptor.
    (0..=2).all(|fd| unsafe { libc::isatty(fd) } == 1)
}

fn launch_argv(
    sb: &Sandbox,
    cmd: Vec<std::ffi::OsString>,
    user: Option<&str>,
    chdir: Option<&str>,
) -> Vec<std::ffi::OsString> {
    // Settings files under /etc/systemd/nspawn are trusted, so `--settings=yes`
    // applies all of them while still letting these flags win.
    // Resource caps have to sit on a unit, and nspawn allocates none of its own
    // here: with --register=no the container simply inherits the caller's
    // cgroup, so `--property=` had nowhere to land and every cap read back as
    // the session default. Launch inside a transient scope of our own instead.
    // Delegate=yes mirrors the stock systemd-nspawn@.service - nspawn creates
    // its payload and supervisor subgroups below the scope.
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    let caps = caps(sb);
    if !caps.is_empty() {
        args.extend(argv![
            "systemd-run",
            "--scope",
            "--quiet",
            "--property=Delegate=yes"
        ]);
        for (key, value) in &caps {
            args.push(crate::host::oss(format!("--property={key}={value}")));
        }
        // Not optional: systemd-run has its own -q and -M, and getopt would
        // otherwise be free to read nspawn's flags as its own.
        args.push(crate::host::oss("--"));
    }
    args.extend(argv![
        "systemd-nspawn",
        "-q",
        "-M",
        sb.name.clone(),
        "-D",
        sb.root(),
        "--settings=yes",
        "--as-pid2",
        "--register=no"
    ]);
    // nspawn tints the terminal background blue for as long as the container
    // runs, which fights with whatever theme the host terminal already has. It
    // has no .nspawn settings key, only this flag, and an empty value is its
    // spelling for "do not tint" - the default here. A booted box never touches
    // a terminal, so `up` needs none of this.
    match sb.cfg.background.as_deref() {
        Some(BACKGROUND_AUTO) => {} // leave nspawn to its own devices
        Some(color) => args.push(crate::host::oss(format!("--background={color}"))),
        None => args.push(crate::host::oss("--background=")),
    }
    if let Some(families) = address_families(sb) {
        args.push(crate::host::oss(format!(
            "--restrict-address-families={families}"
        )));
    }
    // The man page's caution about handing file descriptors to the payload is
    // the reason this is conditional: pipe mode is chosen precisely when they
    // are not terminals. A terminal stdin can still be passed through when
    // only the output is redirected, which is what TIOCSTI would abuse - that
    // ioctl is off by default since Linux 6.2 (dev.tty.legacy_tiocsti).
    if !interactive_stdio() {
        args.push(crate::host::oss("--console=pipe"));
    }
    if let Some(user) = user {
        args.extend(argv!["-u", user]);
    }
    if let Some(chdir) = chdir {
        args.push(crate::host::oss(format!("--chdir={chdir}")));
    }
    args.push(crate::host::oss("--"));
    args.extend(cmd);
    args
}

/// Run a command in the box and wait for it.
pub fn run_in(
    sb: &Sandbox,
    cmd: Vec<std::ffi::OsString>,
    user: Option<&str>,
    chdir: Option<&str>,
) -> Result<bool> {
    sh(launch_argv(sb, cmd, user, chdir)).quiet().run()
}

/// Replace this process with the box's payload, so it owns the terminal.
pub fn exec_in(
    sb: &Sandbox,
    cmd: Vec<std::ffi::OsString>,
    user: Option<&str>,
    chdir: Option<&str>,
) -> Result<std::convert::Infallible> {
    sh(launch_argv(sb, cmd, user, chdir)).quiet().exec()
}

pub use crate::sandbox::state_dir;

#[cfg(test)]
mod tests {
    use super::*;

    /// An existence oracle over a fixed set of paths, standing in for the
    /// container rootfs so the lookup can be tested without one.
    fn has(paths: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |candidate| paths.iter().any(|p| Path::new(p) == candidate)
    }

    const CWD: &str = "/home/me/project";

    #[test]
    fn a_bare_name_is_searched_along_the_path() {
        assert_eq!(
            resolve(
                "/usr/local/bin:/usr/bin",
                Path::new(CWD),
                Path::new("claude"),
                has(&["/usr/bin/claude"])
            ),
            Some(PathBuf::from("/usr/bin/claude"))
        );
    }

    #[test]
    fn the_earliest_path_element_wins() {
        assert_eq!(
            resolve(
                "/usr/local/bin:/usr/bin",
                Path::new(CWD),
                Path::new("claude"),
                has(&["/usr/bin/claude", "/usr/local/bin/claude"])
            ),
            Some(PathBuf::from("/usr/local/bin/claude"))
        );
    }

    /// The case that sent a user hunting: nothing of that name in the box.
    #[test]
    fn a_program_that_is_nowhere_on_the_path_is_not_found() {
        assert_eq!(
            resolve(
                "/usr/local/bin:/usr/bin",
                Path::new(CWD),
                Path::new("claude"),
                has(&["/usr/bin/git"])
            ),
            None
        );
    }

    /// execvp does not search for a name containing a slash, so neither do we:
    /// `./build.sh` must not be found as `/usr/bin/./build.sh`.
    #[test]
    fn a_name_containing_a_slash_is_a_path_not_a_search() {
        let program = Path::new("./build.sh");
        assert_eq!(
            resolve(
                "/usr/bin",
                Path::new(CWD),
                program,
                has(&["/home/me/project/./build.sh"])
            ),
            Some(PathBuf::from("/home/me/project/./build.sh"))
        );
        assert_eq!(
            resolve(
                "/usr/bin",
                Path::new(CWD),
                program,
                has(&["/usr/bin/build.sh"])
            ),
            None
        );
    }

    #[test]
    fn an_absolute_program_ignores_both_the_path_and_the_working_directory() {
        assert_eq!(
            resolve(
                "/usr/bin",
                Path::new(CWD),
                Path::new("/opt/claude-code/bin/claude"),
                has(&["/opt/claude-code/bin/claude"])
            ),
            Some(PathBuf::from("/opt/claude-code/bin/claude"))
        );
    }

    #[test]
    fn an_empty_path_element_means_the_working_directory() {
        assert_eq!(
            resolve(
                "/usr/bin::/bin",
                Path::new(CWD),
                Path::new("task"),
                has(&["/home/me/project/task"])
            ),
            Some(PathBuf::from("/home/me/project/task"))
        );
    }

    #[test]
    fn the_systemd_major_version_is_read_from_its_banner() {
        assert_eq!(
            parse_systemd_version("systemd 261 (261.2-1-arch)\n+PAM +AUDIT\n"),
            Some(261)
        );
        assert_eq!(parse_systemd_version("systemd 256 (256)\n"), Some(256));
        // Never guess: an unreadable banner means the setting is left out.
        assert_eq!(parse_systemd_version(""), None);
        assert_eq!(parse_systemd_version("systemd\n"), None);
        assert_eq!(parse_systemd_version("systemd v261\n"), None);
    }

    #[test]
    fn bind_paths_escape_the_field_separator() {
        assert_eq!(escape(Path::new("/plain/path")).unwrap(), "/plain/path");
        assert_eq!(escape(Path::new("/od:d")).unwrap(), r"/od\:d");
        assert_eq!(escape(Path::new(r"/back\slash")).unwrap(), r"/back\\slash");
    }

    #[test]
    fn destinations_nspawn_would_shadow_are_recognised() {
        assert_eq!(shadowed_by_nspawn(Path::new("/tmp/project")), Some("/tmp"));
        assert_eq!(shadowed_by_nspawn(Path::new("/run/x")), Some("/run"));
        assert_eq!(shadowed_by_nspawn(Path::new("/tmp")), Some("/tmp"));
    }

    /// Component-wise, so a directory merely starting with those letters is
    /// not mistaken for one of them.
    #[test]
    fn ordinary_destinations_are_left_alone() {
        for dst in ["/home/me/code", "/srv/work", "/tmpfoo/x", "/var/tmp/x"] {
            assert_eq!(shadowed_by_nspawn(Path::new(dst)), None, "{dst}");
        }
    }

    /// A newline in a bind path could open a second `Bind=` or a fresh
    /// `[Files]` section, so a control character in a path is refused outright.
    #[test]
    fn a_bind_path_with_a_control_char_is_refused() {
        assert!(escape(Path::new("/ok/path\ninjected")).is_err());
        assert!(escape(Path::new("/ok/path\rinjected")).is_err());
        // An ordinary path still escapes cleanly.
        assert_eq!(escape(Path::new("/ok/path")).unwrap(), "/ok/path");
    }

    /// The hostname is written as `Hostname=<value>`; a newline in it would
    /// let it append `PrivateUsers=no` or any other directive to `[Exec]`.
    #[test]
    fn a_hostname_with_a_newline_is_refused() {
        assert!(reject_control_chars("the hostname", "box\nPrivateUsers=no").is_err());
        // A plausible ordinary hostname is accepted.
        assert!(reject_control_chars("the hostname", "my-project-box").is_ok());
    }

    /// Each env var is written as `Environment=<key>=<value>`; a newline in
    /// either half would inject its own line into the file.
    #[test]
    fn an_env_value_with_a_newline_is_refused() {
        assert!(reject_control_chars(
            "an environment variable value",
            "value\nCapability=all"
        )
        .is_err());
        assert!(reject_control_chars("an environment variable name", "KEY\n[Files]").is_err());
        // An ordinary value is accepted.
        assert!(reject_control_chars("an environment variable value", "some/value:with-colon").is_ok());
    }

    /// The A4 vector: an un-normalized `dst` with `..` would let `inside()`
    /// climb out of the rootfs, so `check_supported` refuses it up front (it
    /// bails on any `dst` for which `unsafe_dst` returns a reason).
    #[test]
    fn a_traversing_destination_is_refused() {
        assert!(unsafe_dst(Path::new("/../../../home/me/i_win")).is_some());
        assert!(unsafe_dst(Path::new("/srv/../etc/shadow")).is_some());
        // A relative destination cannot be joined safely either.
        assert!(unsafe_dst(Path::new("home/me/i_win")).is_some());
    }

    /// An ordinary absolute, normalized destination is accepted.
    #[test]
    fn a_normal_nested_destination_is_accepted() {
        assert_eq!(unsafe_dst(Path::new("/home/me/project")), None);
        assert_eq!(unsafe_dst(Path::new("/srv/work")), None);
        assert_eq!(unsafe_dst(Path::new("/")), None);
    }

    /// The defense-in-depth containment guard used before the privileged
    /// create/chown: an escaping target is rejected, a legit one accepted.
    #[test]
    fn containment_guard_keeps_targets_under_root() {
        let root = Path::new("/var/lib/machines/box");
        // What inside() would build for a legit dst stays under root.
        assert!(within_root(root, &root.join("home/me/project")));
        assert!(within_root(root, root));
        // What inside() would build for a `..` dst escapes and is rejected.
        assert!(!within_root(
            root,
            &root.join("../../../home/me/i_win")
        ));
        assert!(!within_root(root, Path::new("/home/me/i_win")));
    }
}
