//! Assembling a box and handing it to systemd-nspawn: the overlay mount, the
//! generated settings file, and the launch itself.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::argv;
use crate::config::{Network, BACKGROUND_AUTO, PROJECT_FILE, UID_RANGE};
use crate::host::{dry_run, env_var, host, sh};
use crate::sandbox::{overbroad_reason, Bind, Sandbox, MACHINES, NSPAWN_DIR, UNIT_DIR};
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

/// lower = a base generation, upper = this box's writes. The overlay features
/// that rewrite how upper refers to lower are switched off: they buy little
/// here and interact badly with an image whose UIDs were shifted.
///
/// The lowerdir is whichever generation `base::current_id()` names *right
/// now* - fixed for the life of the mount once it's made, since overlayfs
/// never revalidates it. That's what makes `agentbox build` safe to run
/// against a live box: a rebuild writes a new generation directory and never
/// touches the one this mount already opened.
pub fn mount(sb: &Sandbox) -> Result<()> {
    if is_mounted(&sb.root()) {
        if !overlay_stale(&sb.name) {
            return Ok(());
        }
        // The box is idle here - a launch mounts before it boots - so the cure
        // is free: drop the stale mount and build a new one over the current
        // generation. The box's writes are in `upper` on disk, not in the mount.
        if service_active(&sb.name) {
            crate::warn(&format!(
                "{name} is running on a base generation that no longer exists on \
                 disk, so files it needs may be missing entirely; \
                 `agentbox down {name}` then `agentbox remount {name}` clears it",
                name = sb.name
            ));
            return Ok(());
        }
        info(&format!(
            "{}'s base generation no longer exists on disk; remounting",
            sb.name
        ));
        umount_root(&sb.root())?;
    }
    base::require()?;
    let generation = base::current_id();
    if let Some(expected) =
        uid_base_mismatch(sb.cfg.uid_base, base::generation_uid_base(&generation))
    {
        bail!(
            "box {} is configured with uid_base {}, but base generation {generation} was \
             shifted for uid_base {expected}; set `uid_base = {expected}` in {PROJECT_FILE}, \
             or `agentbox build --force` after changing the global uid_base back",
            sb.name,
            sb.cfg.uid_base,
        );
    }
    for dir in [sb.upper(), sb.work(), sb.root()] {
        sh(argv!["mkdir", "-p", dir]).quiet().run()?;
    }
    let options = format!(
        "lowerdir={base},upperdir={upper},workdir={work},\
         index=off,metacopy=off,redirect_dir=off,xino=off",
        base = base::generation_dir(&generation).display(),
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
    // Which generation this mount is a view of, so a later command can tell
    // whether it's still on disk at all (`overlay_stale`) and whether it's the
    // one a fresh mount would pick (`overlay_label`, for `agentbox ls`). The
    // mkdir above made the box directory, and dry runs never mounted anything
    // to record.
    if !dry_run() {
        let path = overlay_id_path(&sb.name);
        fs::write(&path, format!("{generation}\n"))
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(())
}

pub fn umount(sb: &Sandbox) -> Result<()> {
    umount_root(&sb.root())
}

/// Unmount one box's overlay by path. Shared by `umount`'s public entry point
/// and `mount`'s own straddled-a-rebuild repair, both of which already hold
/// (or have just built) the root path rather than a bare box name.
fn umount_root(root: &Path) -> Result<()> {
    if is_mounted(root) {
        sh(argv!["umount", root]).run()?;
    }
    Ok(())
}

/// Whether a box's configured `uid_base` is safe to mount against a
/// generation shifted for `generation_uid_base` - `None` there means the
/// generation predates this being recorded, so there is nothing to check and
/// it is let through. Returns the generation's uid_base when it conflicts,
/// for the error message. Pure, so the "unknown means allow, known-and-
/// different means refuse" rule is covered without a real generation on disk.
fn uid_base_mismatch(configured: u32, generation_uid_base: Option<u32>) -> Option<u32> {
    generation_uid_base.filter(|&shifted| shifted != configured)
}

/// Every box on this host, sorted by name.
pub fn boxes() -> Vec<String> {
    let Ok(entries) = fs::read_dir(state_dir().join("boxes")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every box whose overlay is mounted right now. Each one has the base image as
/// its lower layer, so each one is a reason not to touch it.
pub fn mounted_boxes() -> Vec<String> {
    boxes()
        .into_iter()
        .filter(|name| is_mounted(&Path::new(MACHINES).join(name)))
        .collect()
}

/// Which base generation a box's overlay was mounted on, written when it is
/// mounted.
fn overlay_id_path(name: &str) -> PathBuf {
    state_dir().join("boxes").join(name).join("overlay.id")
}

/// The generation id a box's overlay claims, if the record exists and is
/// non-empty. Used by `base::gc_generations` to know what a *mounted* box
/// pins, and by `overlay_stale`/`overlay_label` to tell "on an old generation"
/// (fine) from "on no generation at all" (broken).
pub fn overlay_generation(name: &str) -> Option<String> {
    fs::read_to_string(overlay_id_path(name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Pure core of `overlay_stale`: given whether a box is mounted and whether
/// its recorded generation still exists on disk (`None` = no usable record at
/// all), is the mount stale? Generations are never mutated in place and
/// `gc_generations` never deletes one a mounted box still names, so "on an
/// older-but-present generation" is the ordinary case here, not stale -
/// unlike the single mutable base this replaced, where any mismatch meant the
/// image had moved on underneath the mount.
fn is_stale(mounted: bool, recorded_generation_exists: Option<bool>) -> bool {
    mounted && recorded_generation_exists != Some(true)
}

/// Whether a box's overlay claims a generation that no longer exists on disk.
///
/// The only way this can be true is a generation gone missing out from under
/// a still-mounted box: a GC race, or a mount left by a version of agentbox
/// that predates generations and never wrote a matching record. Either way
/// there is nothing left to serve reads from.
pub fn overlay_stale(name: &str) -> bool {
    let mounted = is_mounted(&Path::new(MACHINES).join(name));
    let recorded_generation_exists =
        overlay_generation(name).map(|id| base::generation_exists(&id));
    is_stale(mounted, recorded_generation_exists)
}

/// Pure core of `overlay_label`: the `OVERLAY` column `agentbox ls` prints,
/// given whether a box is mounted, whether it's stale, its recorded
/// generation (if any) and the current one. A box can be mounted on an older
/// generation than a fresh launch would pick without that being an error, so
/// that case gets its own label rather than either "mounted" or "stale".
fn overlay_label_from(mounted: bool, stale: bool, recorded: Option<&str>, current: &str) -> String {
    if !mounted {
        return "-".into();
    }
    if stale {
        return "stale".into();
    }
    let recorded = recorded.unwrap_or_default();
    if recorded == current {
        "mounted".into()
    } else {
        format!("mounted ({recorded}, current {current})")
    }
}

/// The `OVERLAY` column for one box in `agentbox ls`.
pub fn overlay_label(name: &str) -> String {
    let mounted = is_mounted(&Path::new(MACHINES).join(name));
    let stale = overlay_stale(name);
    let recorded = overlay_generation(name);
    overlay_label_from(mounted, stale, recorded.as_deref(), &base::current_id())
}

/// Whether a box's container service is up, by name. `session::running`
/// answers the same question for a box there is a `Sandbox` for.
pub fn service_active(name: &str) -> bool {
    sh(argv![
        "systemctl",
        "is-active",
        format!("systemd-nspawn@{name}.service")
    ])
    .output()
        == "active"
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
    // `perf_event_open` is not in nspawn's default syscall allow list, so
    // `perf = true` is what adds it. This is the only thing the flag does: it
    // does not touch PrivateUsers= or grant any capability, because it
    // couldn't help either way - perf's own permission check
    // (`perfmon_capable()`) is `capable(CAP_PERFMON)`, which the kernel defines
    // against `init_user_ns` specifically, so a process in this box's private
    // user namespace can never satisfy it no matter what capability it holds
    // internally. What that check gates (hardware/kernel/tracepoint events) is
    // additionally controlled by the *host's* `kernel.perf_event_paranoid`
    // sysctl - unaffected by this box, and not something agentbox changes for
    // you. See docs/security.md#perf-inside-a-box.
    if sb.cfg.perf {
        out.push_str("SystemCallFilter=perf_event_open\n");
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

// --------------------------------------------------------------------------
// the AppArmor layer (defense in depth)
// --------------------------------------------------------------------------

/// The name of the AppArmor profile agentbox ships, in `contrib/apparmor/`. It
/// has to be loaded on the host (`apparmor_parser -r`) before it can be applied;
/// see contrib/apparmor/README.md. The file's `profile <name>` header must match
/// this exactly.
const APPARMOR_PROFILE: &str = "agentbox-nspawn";

/// The set of AppArmor profiles currently loaded, as the kernel lists them.
/// Read once: the loaded set does not change under us mid-run, and the file is
/// only readable as root (mode 0640), so a non-root `--dry-run` reads nothing
/// and simply reports no confinement.
fn apparmor_profiles() -> &'static str {
    static PROFILES: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PROFILES.get_or_init(|| {
        fs::read_to_string("/sys/kernel/security/apparmor/profiles").unwrap_or_default()
    })
}

/// Each line is `<profile name> (<mode>)`, so the name is the first field.
fn apparmor_profile_loaded(name: &str) -> bool {
    apparmor_profiles()
        .lines()
        .any(|line| line.split_whitespace().next() == Some(name))
}

fn warn_once(message: &str) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| crate::warn(message));
}

/// The AppArmor profile to apply to this box, or `None` when confinement is off
/// or unavailable.
///
/// This is a *second wall*, independent of the user namespace, capability set,
/// seccomp filter and mount plan - not a launch gate. So it fails soft: a host
/// without AppArmor, or without this profile loaded, is not an error and the box
/// launches exactly as before. Availability is judged by whether the profile is
/// actually loaded, which is the honest reading of "on if available": an unloaded
/// profile is nothing to apply, and gating on it also means agentbox never wires
/// a profile onto a launch on a host that has not opted in by loading it - so
/// existing users on other distros see no behaviour change at all.
///
/// `apparmor = false` opts out; `apparmor = true` opts in loudly, warning when
/// the profile is not there to apply; the default (`None`) opts in quietly.
fn apparmor_profile(sb: &Sandbox) -> Option<&'static str> {
    if sb.cfg.apparmor == Some(false) {
        return None;
    }
    if apparmor_profile_loaded(APPARMOR_PROFILE) {
        return Some(APPARMOR_PROFILE);
    }
    if sb.cfg.apparmor == Some(true) {
        warn_once(&format!(
            "apparmor = true, but the AppArmor profile {APPARMOR_PROFILE:?} is not \
             loaded (or AppArmor is disabled) on this host, so the box runs without \
             it. See contrib/apparmor/README.md to install and load the profile."
        ));
    }
    None
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

/// Write, or remove, one drop-in file under the box's unit directory, reloading
/// systemd only when the content actually changed - a daemon-reload is not free.
/// An empty `want` means the drop-in should not exist, so it is removed (and the
/// directory too, if that leaves it empty and no sibling drop-in remains).
fn write_dropin(dir: &Path, filename: &str, want: &str) -> Result<()> {
    let path = dir.join(filename);
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
        let _ = fs::remove_dir(dir); // only succeeds once the dir is empty
    } else {
        fs::create_dir_all(dir)?;
        fs::write(&path, want).with_context(|| format!("cannot write {}", path.display()))?;
    }
    daemon_reload()
}

/// Caps for a booted box.
///
/// A cap is a property of a unit, not of a container, so the generated .nspawn
/// file has nowhere to put it: `agentbox up` starts an instance of
/// systemd-nspawn@.service, and the caps belong in a drop-in on that.
pub fn write_unit_caps(sb: &Sandbox) -> Result<()> {
    let caps = caps(sb);
    let mut want = String::new();
    if !caps.is_empty() {
        want.push_str("# generated by agentbox - edit .agentbox.toml instead\n[Service]\n");
        for (key, value) in &caps {
            want.push_str(&format!("{key}={value}\n"));
        }
    }
    write_dropin(&unit_dropin_dir(sb), "50-agentbox-caps.conf", &want)
}

/// AppArmor confinement for a booted box.
///
/// Like a cap, an LSM profile is a property of the unit, so `agentbox up` gets it
/// through a drop-in on its systemd-nspawn@.service instance rather than through
/// the .nspawn file. The `-` prefix on `AppArmorProfile=` makes systemd treat a
/// profile that cannot be applied (unloaded, or AppArmor off) as non-fatal, so
/// the box still boots - defense in depth must never become a launch gate. The
/// drop-in is only written at all when the profile is currently loaded, and is
/// removed when it is not, so a box self-heals in both directions on its next
/// `up`/`shell`/`run`, which regenerate it.
pub fn write_unit_apparmor(sb: &Sandbox) -> Result<()> {
    let mut want = String::new();
    if let Some(profile) = apparmor_profile(sb) {
        want.push_str("# generated by agentbox - edit .agentbox.toml instead\n[Service]\n");
        want.push_str(&format!("AppArmorProfile=-{profile}\n"));
    }
    write_dropin(&unit_dropin_dir(sb), "60-agentbox-apparmor.conf", &want)
}

/// Drop a box's unit drop-ins (caps and AppArmor), on the way to deleting it.
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
    let path = sb.settings();
    fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    // The file carries `[env]`/`pass_env` values verbatim - an API key, say -
    // and only root and systemd ever read it, so keep it out of other users'
    // reach. Set the mode explicitly rather than trusting umask, which also
    // tightens any world-readable file an older agentbox left behind.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot secure {}", path.display()))
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
        // Before the mount points, since one of the binds is this agent's socket
        // and `prepare_mount_point` stats every bind source.
        spawn_ssh_agent(sb)?;
        for bind in sb.binds() {
            prepare_mount_point(sb, &bind)?;
        }
        mask_host_network_units(sb)?;
    }
    perform_copies(sb)?;
    write_settings(sb)?;
    write_unit_caps(sb)?;
    write_unit_apparmor(sb)?;
    if fresh && !dry_run() {
        // Only the network-free bootstrap runs here. Setting the login shell is
        // a local write into the overlay; the configured `packages`, which need
        // a working network, are installed after the box has booted (see
        // `install_packages`) so they also work under nat.
        set_login_shell(sb)?;
    }
    Ok(fresh)
}

/// Install the box's configured packages, once, into the freshly booted box.
///
/// This is deliberately *not* part of the pre-boot bootstrap: that runs the box
/// as a transient `--as-pid2` payload with no network under nat, where
/// networkd only brings `host0` up once the box actually boots. So the install
/// is deferred to here, run through the same `systemd-run -M` attach path a
/// session uses, once `wait_attachable`/`canary` have shown the box is up.
///
/// Gated on `fresh`, so it is a one-time step on creation like the login-shell
/// bootstrap it moved out of. A non-zero `pacman` exit is fatal: a box missing
/// its configured packages is broken, and continuing would surface later as a
/// confusing "not found" for whatever those packages were meant to provide.
pub fn install_packages(sb: &Sandbox, fresh: bool) -> Result<()> {
    if !fresh || dry_run() || sb.cfg.packages.is_empty() {
        return Ok(());
    }
    info(&format!(
        "installing packages: {}",
        sb.cfg.packages.join(" ")
    ));
    let mut cmd = argv!["/usr/bin/pacman", "-Sy", "--noconfirm", "--needed"];
    cmd.extend(sb.cfg.packages.iter().map(crate::host::oss));
    let code = attach(sb, cmd, "root", "/")?;
    if code != 0 {
        bail!(
            "installing packages into box {} failed (pacman exited {code}); \
             the box is booted but its configured packages are not all present. \
             Fix `packages` in {PROJECT_FILE}, or install them by hand with \
             `agentbox run --root -- pacman -S PKG`.",
            sb.name,
        );
    }
    Ok(())
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
        // An `internal` bind (the box-scoped ssh-agent socket) has a source
        // agentbox itself chose and controls - a single socket file under the
        // box's state dir - so it is exempt from this check, which would
        // otherwise refuse it for living under the state directory.
        if bind.src != sb.project && !bind.internal {
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
    // `cpy` destinations land inside the box rootfs before it boots too (a
    // one-time `cp -a` rather than a mount), so they need the same
    // destination checks as a bind, and the same overbroad-source guard on
    // where they may read from - reused here as a conservative starting
    // default (see `Sandbox::copies()`'s doc comment); a considered
    // cpy-specific policy is left to a follow-up review.
    for copy in sb.copies() {
        if let Some(reason) = unsafe_dst(&copy.dst) {
            bail!("refusing an unsafe cpy destination: {reason}");
        }
        if let Some(base) = shadowed_by_nspawn(&copy.dst) {
            bail!(
                "cpy destination {} is under {base}, which systemd-nspawn covers with \
                 a mount of its own, so a copy placed there would be hidden once the \
                 box boots; give it a destination of its own.",
                copy.dst.display()
            );
        }
        if let Some(reason) = overbroad_reason(&copy.src, &sb.user.home, state_dir()) {
            bail!(
                "refusing cpy map of {}: its source is {}; a box must not be \
                 granted access to the host beyond its project",
                copy.src.display(),
                reason
            );
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

/// A host UID/GID as it should be written on a box's mount point: IDs outside
/// the container's own range have no meaning inside it, so they collapse onto
/// container root.
fn map_id(id: u32) -> u32 {
    if id < UID_RANGE {
        id
    } else {
        0
    }
}

/// Create the ancestors of a mount point, each owned by someone the container
/// can actually resolve.
///
/// `create_dir_all` leaves every directory it creates owned by *host* root, and
/// host UID 0 is not in the box's shifted range - inside the box it is an
/// unmapped owner (`nobody`) on a 0755 directory, so the sandbox user cannot
/// write there. That is invisible until something tries: every dotfile bind
/// hangs a fresh ancestor off the sandbox user's home - `~/.claude` for a
/// credentials file, `~/.local`+`~/.local/share` for `~/.local/share/nvim/lazy`,
/// `~/.config` for `~/.config/jj` - and the mount itself works, while the user
/// can no longer create anything *beside* it. fish cannot write
/// `~/.local/share/fish` ("Permission denied", no history), and claude cannot
/// write the state it keeps next to the credentials file it was handed, so it
/// asks to log in again.
///
/// agentbox spells paths identically inside and outside the box (see
/// `base::setup_script`), so the host's own directory of the same name is the
/// right model: mirror its owner when it exists, and fall back to container
/// root when it does not.
fn create_parents_mapped(sb: &Sandbox, parent: &Path) -> Result<()> {
    let root = sb.root();
    let rel = parent.strip_prefix(&root).with_context(|| {
        format!(
            "refusing to prepare {} outside the box rootfs {}",
            parent.display(),
            root.display()
        )
    })?;
    let mut target = root.clone();
    let mut on_host = PathBuf::from("/");
    for component in rel.components() {
        target.push(component);
        on_host.push(component);
        match fs::symlink_metadata(&target) {
            // Already there and owned by someone in the box's range: the image
            // made it, or an earlier launch did it right. Leave it alone.
            Ok(meta) if self_owned(sb, meta.uid()) => continue,
            // Already there and owned from outside the range - which nothing
            // legitimate is, since the image is pre-shifted wholesale. This is
            // a directory an older agentbox created with `create_dir_all` and
            // left as host root, so repair it in place rather than making the
            // fix apply only to boxes created from here on.
            Ok(_) => {}
            Err(_) => fs::create_dir(&target)
                .with_context(|| format!("cannot create {}", target.display()))?,
        }
        let (uid, gid) = match fs::metadata(&on_host) {
            Ok(meta) => (map_id(meta.uid()), map_id(meta.gid())),
            Err(_) => (0, 0),
        };
        std::os::unix::fs::chown(&target, Some(sb.shift(uid)), Some(sb.shift(gid)))
            .with_context(|| format!("cannot chown {}", target.display()))?;
    }
    Ok(())
}

/// Is `uid` one the box can resolve - that is, inside the range its image was
/// shifted into? Everything else, host root included, shows up inside the box
/// as an unmapped owner.
fn self_owned(sb: &Sandbox, uid: u32) -> bool {
    uid >= sb.cfg.uid_base && uid - sb.cfg.uid_base < UID_RANGE
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
        create_parents_mapped(sb, parent)?;
    }
    if meta.is_dir() {
        if !target.exists() {
            fs::create_dir(&target)?;
        }
    } else if !target.exists() {
        fs::File::create(&target)?;
    }
    std::os::unix::fs::chown(
        &target,
        Some(sb.shift(map_id(meta.uid()))),
        Some(sb.shift(map_id(meta.gid()))),
    )
    .with_context(|| format!("cannot chown {}", target.display()))
}

/// Apply every `cpy` entry: a one-time, copy-if-absent snapshot into the
/// box's own overlay, never a live mount.
///
/// Unlike a bind, a `cpy` source is never mounted, so it cannot straddle a
/// rewrite of its lower layer the way a live overlay mount can (see
/// "Overlayfs never revalidates its lower layer" in CLAUDE.md) - once copied,
/// the destination is just an ordinary file in the box's own upper layer,
/// with no lower layer to go stale. That also means it needs no
/// `owneridmap`/ID-mapped-mount support: the copy is chowned once, right
/// here, to the box's own shifted sandbox user, reusing the same
/// `shift`/`map_id` math `prepare_mount_point` already uses for a bind's
/// mount point.
///
/// A destination that already exists inside the box - because a previous
/// session copied it, or the box itself has since written there - is left
/// alone entirely: that is what makes this copy-if-absent rather than a
/// resync, and what makes `agentbox reset` (which empties the box's upper
/// layer) the way to force a fresh copy on the box's next boot.
///
/// The actual copy shells out to `cp -a` rather than walking the tree in
/// Rust, so symlinks, modes and xattrs are coreutils' problem, not ours - the
/// same convention `mount`/`umount`/`mkdir -p` already follow in this file.
/// `sh(..).run()` already fails closed on a non-zero exit (bails, aborting
/// the launch), matching how a missing `ssh_keys` entry or a failed package
/// install are already fatal here: a half-copied box is never let through to
/// boot.
///
/// `--dry-run` never mounts the overlay, so there is no way to know whether a
/// destination is already present inside it; the report below lists the
/// planned mappings rather than predicting which would actually copy.
fn perform_copies(sb: &Sandbox) -> Result<()> {
    let copies = sb.copies();
    if dry_run() {
        if copies.is_empty() {
            return Ok(());
        }
        println!("--- cpy (copied into the box the first time each destination is absent) ---");
        for copy in &copies {
            println!("  {} -> {}", copy.src.display(), copy.dst.display());
        }
        return Ok(());
    }
    for copy in &copies {
        let target = sb.inside(&copy.dst);
        if target.symlink_metadata().is_ok() {
            continue; // already present: copy-if-absent means leave it alone.
        }
        if let Some(parent) = target.parent() {
            create_parents_mapped(sb, parent)?;
        }
        info(&format!(
            "copying {} into box {} at {}",
            copy.src.display(),
            sb.name,
            copy.dst.display()
        ));
        if let Err(e) = sh(argv!["cp", "-a", "--", copy.src.clone(), target.clone()]).run() {
            cleanup_partial_copy(&target);
            return Err(e);
        }
        let uid = sb.shift(map_id(sb.user.uid));
        let gid = sb.shift(map_id(sb.user.gid));
        if let Err(e) = sh(argv![
            "chown",
            "-R",
            "--",
            format!("{uid}:{gid}"),
            target.clone()
        ])
        .run()
        {
            cleanup_partial_copy(&target);
            return Err(e);
        }
    }
    Ok(())
}

/// Undo whatever a failed `cp -a`/`chown -R` left on disk. Copy-if-absent's
/// only signal is "does the destination exist", so a half-copied or
/// wrong-owner leftover would otherwise look identical to a finished copy on
/// every future launch and be skipped forever - stuck until `agentbox reset`
/// throws away the box's whole upper layer. Removing it here means the next
/// launch attempt retries this one entry from scratch instead.
fn cleanup_partial_copy(target: &Path) {
    if sh(argv!["rm", "-rf", "--", target]).run().is_err() {
        crate::warn(&format!(
            "failed to clean up partial cpy target {} left by a failed copy",
            target.display()
        ));
    }
}

/// Line up the box's networking units with its network mode.
///
/// * `host` shares the host's network namespace, so a booted box must never run
///   networkd or resolved - they would reconfigure the host's own interfaces.
///   Mask them.
/// * `nat` gives the box its own veth, so it needs its *own* networkd to bring
///   `host0` up and resolved to answer DNS. Arch enables neither by default, so
///   enable them here (enabling the service pulls in its socket via `Also=`).
/// * `none` has no interfaces; leave them off.
///
/// Done per box against the box's own rootfs (`systemctl --root` writes into the
/// overlay's upper layer), so the shared base image is untouched and a mode
/// change is re-applied on the next launch. `unmask` first clears whatever a
/// previous mode left, so the decision below is authoritative.
pub fn mask_host_network_units(sb: &Sandbox) -> Result<()> {
    let root = sb.root();
    let all = [
        "systemd-networkd.service",
        "systemd-networkd.socket",
        "systemd-resolved.service",
    ];
    let mut unmask = argv!["systemctl", "--root", root.clone(), "unmask"];
    unmask.extend(all.iter().map(crate::host::oss));
    sh(unmask).quiet().silent().run()?;
    match sb.cfg.network {
        Network::Host => {
            let mut mask = argv!["systemctl", "--root", root, "mask"];
            mask.extend(all.iter().map(crate::host::oss));
            sh(mask).quiet().silent().run()?;
        }
        Network::Nat => {
            sh(argv![
                "systemctl",
                "--root",
                root,
                "enable",
                "systemd-networkd.service",
                "systemd-resolved.service"
            ])
            .quiet()
            .silent()
            .run()?;
        }
        Network::None => {}
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
// the box-scoped ssh-agent
// --------------------------------------------------------------------------

/// Caller variables the scoped agent needs so that `ssh-add -c`'s confirm-on-use
/// prompt can reach a display when a key is later used. Forwarded from the
/// invoking user's environment; whatever is unset is simply not passed.
const AGENT_ASKPASS_ENV: [&str; 6] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "SSH_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Expand and validate the configured private-key paths. Fails closed: a
/// missing key is an error, never a silent fallback to forwarding more.
fn ssh_key_paths(sb: &Sandbox) -> Result<Vec<PathBuf>> {
    let mut keys = Vec::with_capacity(sb.cfg.ssh_keys.len());
    for spec in &sb.cfg.ssh_keys {
        let path = crate::config::expand(spec);
        if !path.exists() {
            bail!(
                "ssh key {} (from ssh_keys = [.. {:?} ..]) does not exist; \
                 refusing to launch. agentbox never falls back to forwarding the \
                 host's whole SSH agent.",
                path.display(),
                spec
            );
        }
        keys.push(path);
    }
    Ok(keys)
}

/// A command that will run as the invoking user with a clean, minimal
/// environment, so the scoped agent and `ssh-add` never inherit root's.
fn as_user(sb: &Sandbox, program: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    // Drop to the user: gid before uid, the order std applies them in. The
    // child keeps root's supplementary groups (CommandExt::groups is unstable),
    // which is harmless here - it runs as the user, reading only the user's own
    // keys, which they already own.
    cmd.gid(sb.user.gid).uid(sb.user.uid);
    cmd.env_clear();
    cmd.env("HOME", &sb.user.home);
    cmd.env("USER", &sb.user.name);
    cmd.env("LOGNAME", &sb.user.name);
    cmd.env("PATH", NSPAWN_PATH);
    for name in AGENT_ASKPASS_ENV {
        if let Some(value) = env_var(name) {
            cmd.env(name, value);
        }
    }
    cmd
}

/// Is a process with this pid still around? Cheap and signal-free, matching how
/// stale handovers are detected in `host.rs`.
fn process_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn read_agent_pid(sb: &Sandbox) -> Option<u32> {
    fs::read_to_string(sb.agent_pidfile())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn read_relay_pid(sb: &Sandbox) -> Option<u32> {
    fs::read_to_string(sb.agent_relay_pidfile())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// True when this box already has a live scoped agent *and* a live relay in
/// front of it, so `spawn_ssh_agent` can reuse them rather than restart both.
fn agent_alive(sb: &Sandbox) -> bool {
    sb.scoped_agent_sock().exists()
        && read_agent_pid(sb).is_some_and(process_alive)
        && sb.scoped_agent_relay_sock().exists()
        && read_relay_pid(sb).is_some_and(process_alive)
}

/// `SSH_AGENT_PID=12345; export SSH_AGENT_PID;` -> 12345.
fn parse_agent_pid(output: &str) -> Option<u32> {
    output.split("SSH_AGENT_PID=").nth(1).and_then(|rest| {
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        rest[..end].parse().ok()
    })
}

/// Start a dedicated ssh-agent for this box holding only the configured keys,
/// put a relay in front of it, and forward *the relay's* socket into the box.
/// The host's own `$SSH_AUTH_SOCK` is never used. Idempotent: a live agent and
/// relay are left running.
///
/// Runs as the invoking user, so the socket is owned by them and `owneridmap`
/// maps it onto the sandbox user inside the box (exactly as the previous
/// host-agent forwarding relied on), and so the keys never sit in root's memory.
/// Each key is added with `ssh-add -c`, so every use of it prompts the user on
/// the host to confirm. Fails closed at the first sign of trouble.
///
/// The relay exists because binding the agent's own socket straight into the
/// box does not work. `owneridmap` only translates file *ownership metadata* -
/// what `stat`/`ls` see - not process credentials. The box runs in its own
/// user namespace (`PrivateUsers=`), so a process inside it has a real,
/// host-global UID of `uid_base + <its in-box uid>`, never the invoking user's
/// actual UID. `ssh-agent` checks the connecting peer's real UID via
/// `getsockopt(SO_PEERCRED)` and closes the connection if it does not match
/// its own - so a direct bind lets `connect()` succeed and then the agent
/// hangs up immediately, surfacing to the box as `ssh-add -l` dying with
/// SIGPIPE ("communication with agent failed"). `socat` relays the connection
/// instead: it runs as the same user as the agent, so *its* leg to the agent
/// passes the peer-UID check, and its own listening socket - the one actually
/// bound into the box - performs no such check on the box's connections at
/// all.
pub fn spawn_ssh_agent(sb: &Sandbox) -> Result<()> {
    if sb.cfg.ssh_keys.is_empty() {
        return Ok(());
    }
    let keys = ssh_key_paths(sb)?; // validate before touching anything
    if dry_run() {
        info(&format!(
            "would start a box-scoped ssh-agent at {} holding {} key(s), confirm-on-use",
            sb.scoped_agent_sock().display(),
            keys.len()
        ));
        return Ok(());
    }
    if agent_alive(sb) {
        return Ok(());
    }
    // Clear any stale socket/pid from a crashed run before starting fresh.
    teardown_ssh_agent(sb)?;

    let dir = sb.agent_dir();
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    std::os::unix::fs::chown(&dir, Some(sb.user.uid), Some(sb.user.gid))
        .with_context(|| format!("cannot chown {}", dir.display()))?;
    let mut perms = fs::metadata(&dir)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o700);
    fs::set_permissions(&dir, perms)?;

    let sock = sb.scoped_agent_sock();
    info(&format!(
        "starting box-scoped ssh-agent for {} ({} key(s), confirm-on-use)",
        sb.name,
        keys.len()
    ));
    let out = as_user(sb, "ssh-agent")
        .arg("-s")
        .arg("-a")
        .arg(&sock)
        .output()
        .context("cannot run ssh-agent")?;
    if !out.status.success() {
        let _ = fs::remove_file(&sock);
        bail!(
            "ssh-agent failed to start: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let pid = parse_agent_pid(&String::from_utf8_lossy(&out.stdout))
        .context("could not read the pid of the box-scoped ssh-agent from its output")?;
    fs::write(sb.agent_pidfile(), format!("{pid}\n"))
        .with_context(|| format!("cannot write {}", sb.agent_pidfile().display()))?;

    for key in &keys {
        let ok = as_user(sb, "ssh-add")
            .env("SSH_AUTH_SOCK", &sock)
            .arg("-c")
            .arg(key)
            .status()
            .with_context(|| format!("cannot run ssh-add for {}", key.display()))?
            .success();
        if !ok {
            // Fail closed: never leave a half-loaded agent forwarded.
            teardown_ssh_agent(sb)?;
            bail!(
                "ssh-add failed for {}; tore down the box-scoped ssh-agent \
                 rather than forward an incomplete or wrong set of keys",
                key.display()
            );
        }
    }

    let relay_sock = sb.scoped_agent_relay_sock();
    let _ = fs::remove_file(&relay_sock);
    let log_path = sb.agent_dir().join("relay.log");
    let log = fs::File::create(&log_path)
        .with_context(|| format!("cannot create {}", log_path.display()))?;
    // SAFETY: pre_exec runs in the forked child before exec, single-threaded
    // at that point, and only calls the async-signal-safe `setsid(2)`; it
    // touches no Rust state shared with the parent.
    let mut relay = unsafe {
        as_user(sb, "socat")
            .arg(format!(
                "UNIX-LISTEN:{},fork,unlink-early,mode=600",
                relay_sock.display()
            ))
            .arg(format!("UNIX-CONNECT:{}", sock.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log)
            .pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()
    }
    .context(
        "cannot start the box-scoped ssh-agent relay; install `socat` \
         (see docs/setup.md)",
    )?;
    fs::write(sb.agent_relay_pidfile(), format!("{}\n", relay.id()))
        .with_context(|| format!("cannot write {}", sb.agent_relay_pidfile().display()))?;

    // socat has no synchronous "ready" signal like ssh-agent's own stdout
    // line, so poll briefly for its listening socket rather than trust a
    // bare `spawn()` success, which only proves the fork/exec happened.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        if relay_sock.exists() {
            break;
        }
        if let Ok(Some(status)) = relay.try_wait() {
            let detail = fs::read_to_string(&log_path).unwrap_or_default();
            teardown_ssh_agent(sb)?;
            bail!("the box-scoped ssh-agent relay exited immediately ({status}): {detail}");
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

/// Stop this box's scoped agent and its relay, and remove their sockets and
/// pid files. Idempotent and best-effort: safe to call when neither was ever
/// started, and it never leaves either process running.
pub fn teardown_ssh_agent(sb: &Sandbox) -> Result<()> {
    if dry_run() {
        return Ok(());
    }
    for pid in [read_agent_pid(sb), read_relay_pid(sb)]
        .into_iter()
        .flatten()
    {
        if process_alive(pid) {
            // SAFETY: kill only signals the process; the pid was written by us
            // for this box's agent or relay. As root we may signal the user's
            // process.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
    let _ = fs::remove_file(sb.scoped_agent_sock());
    let _ = fs::remove_file(sb.agent_pidfile());
    let _ = fs::remove_file(sb.scoped_agent_relay_sock());
    let _ = fs::remove_file(sb.agent_relay_pidfile());
    let _ = fs::remove_file(sb.agent_dir().join("relay.log"));
    let _ = fs::remove_dir(sb.agent_dir());
    Ok(())
}

// --------------------------------------------------------------------------
// bootstrap launch
// --------------------------------------------------------------------------
//
// `run_in` runs a command in the box *before* it is ever booted: setting the
// login shell while `create` is still assembling the box. It launches nspawn
// directly, as PID 2 under a stub init, which is right for a one-shot write
// into the overlay. Only network-free bootstrap belongs here - the configured
// packages, which need networkd up, are installed after boot instead (see
// `install_packages`). Every launch a user asks for goes the other way - it
// boots the box and attaches (see `attach`), so resource caps live only on that
// booted unit's drop-in (`write_unit_caps`); this bootstrap path is short-lived
// and uncapped.

/// True only when stdin, stdout and stderr are all terminals.
///
/// nspawn decides how to wire up the payload from whether it was "invoked on a
/// terminal", and either answer is wrong for a redirected launch. Invoked with
/// a terminal it allocates a pseudo-TTY, so piped stdout arrives as CRLF with
/// stderr merged into it, and EOF cannot propagate through a shell pipeline;
/// invoked without one it defaults to `read-only`, which drops our stdin. The
/// bootstrap installs run redirected, and an attach (which is where a pipeline
/// actually matters) makes the same choice through `--pty`/`--pipe`.
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
    let mut args: Vec<std::ffi::OsString> = Vec::new();
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
    // nspawn tints the terminal background for as long as it runs, which fights
    // with whatever theme the host terminal already has. The configured tint
    // belongs to the session a user watches (see `attach_argv`), not to this
    // one-time bootstrap install, so suppress it here unconditionally - an empty
    // value is nspawn's spelling for "do not tint".
    args.push(crate::host::oss("--background="));
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

// --------------------------------------------------------------------------
// attaching to a booted box
// --------------------------------------------------------------------------
//
// A box that runs an agent is booted (systemd as PID 1, registered with
// machined) rather than launched as the transient `--as-pid2` payload above,
// so more than one session can attach to the same running box. Each attach is
// a transient unit started inside the container's own systemd with
// `systemd-run -M`, which - unlike `machinectl shell` - takes a working
// directory, runs as a chosen user, and with `--wait` propagates the payload's
// exit code back out. `--pty` for an interactive session, `--pipe` for a
// redirected one, mirroring the terminal test the direct launch used.

/// The `systemd-run` line that opens one session in the booted box.
///
/// The box's environment reaches the payload only if we pass it: nspawn's
/// `Environment=` settings apply to the container's PID 1, and systemd does not
/// hand its own environment to a transient unit, so `sb.env()` is forwarded
/// with `--setenv`. Resource caps need no repeating here - they sit on the
/// container's scope (see `write_unit_caps`) and cover every session in it.
fn attach_argv(
    sb: &Sandbox,
    cmd: Vec<std::ffi::OsString>,
    user: &str,
    chdir: &str,
) -> Vec<std::ffi::OsString> {
    let mut args = argv![
        "systemd-run",
        "--quiet",
        "--collect",
        "--wait",
        "-M",
        sb.name.clone()
    ];
    let interactive = interactive_stdio();
    args.push(crate::host::oss(if interactive {
        "--pty"
    } else {
        "--pipe"
    }));
    args.extend(argv!["--uid", user]);
    args.push(crate::host::oss(format!("--working-directory={chdir}")));
    for (key, value) in sb.env() {
        args.push(crate::host::oss(format!("--setenv={key}={value}")));
    }
    // The terminal tint follows the box for the life of the session, so it goes
    // on the interactive attach - the one a person watches. `systemd-run` takes
    // the same `--background` as nspawn: an SGR colour tints, an empty value
    // suppresses, and leaving it off lets systemd-run pick its own per-machine
    // default (what `background = "auto"` asks for). A redirected `--pipe`
    // session has no terminal to colour, so it is left out there.
    if interactive {
        match sb.cfg.background.as_deref() {
            Some(BACKGROUND_AUTO) => {}
            Some(color) => args.push(crate::host::oss(format!("--background={color}"))),
            None => args.push(crate::host::oss("--background=")),
        }
    }
    args.push(crate::host::oss("--"));
    args.extend(cmd);
    args
}

/// Open a session and wait for it, returning the payload's exit code so the
/// caller can exit with it. The child inherits our stdio, so its output - and
/// any message systemd-run itself prints - reaches the terminal directly.
pub fn attach(sb: &Sandbox, cmd: Vec<std::ffi::OsString>, user: &str, chdir: &str) -> Result<i32> {
    let argv = attach_argv(sb, cmd, user, chdir);
    if dry_run() {
        println!("  {}", crate::host::render(&argv));
        return Ok(0);
    }
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    // In `--pipe` mode systemd-run forwards our stdin to the container's
    // transient unit, but a *terminal* stdin cannot be set up there - systemd
    // fails the unit with EXIT_STDIN (exit 208) - and there is no interactive
    // input to forward when the output is redirected anyway. Feed the unit
    // /dev/null in that case; a genuine piped or redirected stdin (not a tty,
    // e.g. `data | agentbox run box -- cat`) is forwarded as-is.
    if !interactive_stdio() && unsafe { libc::isatty(0) } == 1 {
        command.stdin(std::process::Stdio::null());
    }
    let status = command
        .status()
        .with_context(|| format!("cannot run {:?}", argv[0]))?;
    // A signalled payload has no code; report the conventional 128+signum so a
    // ^C in the box still leaves agentbox with a non-zero, non-arbitrary exit.
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

/// How long to wait for a freshly booted box to become attachable.
const ATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const ATTACH_POLL: std::time::Duration = std::time::Duration::from_millis(150);

/// Wait until the box is far enough into boot to accept a session.
///
/// `systemctl start` returns when the container's PID 1 signalled readiness,
/// which is *before* the container's D-Bus and logind - the pieces an attach
/// needs - are guaranteed up. Poll the container's own systemd over the same
/// channel an attach uses; a reply at all proves the bus is reachable, and the
/// value tells us the boot stage. `degraded` counts as up: some unit failed
/// (our masked networkd, say) but the box is fully attachable.
pub fn wait_attachable(sb: &Sandbox) -> Result<()> {
    if dry_run() {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + ATTACH_TIMEOUT;
    loop {
        let state = std::process::Command::new("systemctl")
            .args(["-M", &sb.name, "is-system-running"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        match state.as_str() {
            "running" | "degraded" => return Ok(()),
            "stopping" | "maintenance" => {
                bail!("box {} is {state}, not attachable", sb.name)
            }
            // "", "initializing", "starting": the bus or boot is not ready yet.
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "box {0} booted but did not become attachable within {1}s \
                 (last state: {2:?}); see `journalctl -M {0}`",
                sb.name,
                ATTACH_TIMEOUT.as_secs(),
                state,
            );
        }
        std::thread::sleep(ATTACH_POLL);
    }
}

/// Prove the attach channel works before running the real payload.
///
/// `wait_attachable` shows the bus answers; this shows a unit can actually be
/// started and a process spawned in the container - the exact path the payload
/// takes. `/bin/true` has no side effects, so a failure is unambiguous (nothing
/// of the caller's ran) and safe to retry through the residual logind lag.
/// Reported as an agentbox error rather than mistaken for the payload failing.
pub fn canary(sb: &Sandbox, user: &str) -> Result<()> {
    if dry_run() {
        return Ok(());
    }
    let argv = argv![
        "systemd-run",
        "--quiet",
        "--collect",
        "--wait",
        "-M",
        sb.name.clone(),
        "--pipe",
        "--uid",
        user,
        "--",
        "/bin/true"
    ];
    let mut last = String::new();
    for attempt in 0..CANARY_TRIES {
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("cannot run {:?}", argv[0]))?;
        if out.status.success() {
            return Ok(());
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if attempt + 1 < CANARY_TRIES {
            std::thread::sleep(ATTACH_POLL);
        }
    }
    bail!(
        "couldn't open a session in {0}: {1}; see `journalctl -M {0}`",
        sb.name,
        if last.is_empty() {
            "systemd-run failed".into()
        } else {
            last
        },
    )
}

/// Canary attempts before giving up - a couple of short retries to ride out the
/// gap between the bus answering and logind being ready.
/// Wait for a nat box's uplink, warning if it never comes.
///
/// A nat box gets its address over the veth from the host's DHCP server a couple
/// of seconds *after* boot, so this waits for the default route - the network has
/// to be usable before we install packages or run the payload, and returns as
/// soon as it appears. If it never does, the host side of the veth was not
/// configured - typically NetworkManager still owns `ve-*`/`vz-*`, so no DHCP
/// answers and `host0` stays link-local. Point at the fix (docs/setup.md) rather
/// than failing later with a baffling package-install error. A single in-box
/// poll loop, so it costs one transient unit whatever the outcome.
pub fn await_nat_network(sb: &Sandbox) -> Result<()> {
    if dry_run() || sb.cfg.network != Network::Nat {
        return Ok(());
    }
    let up = std::process::Command::new("systemd-run")
        .args([
            "--quiet",
            "--pipe",
            "--wait",
            "-M",
            &sb.name,
            "--",
            "sh",
            "-c",
            "for _ in $(seq 15); do \
                 ip -4 route show default 2>/dev/null | grep -q default && exit 0; \
                 sleep 1; \
             done; exit 1",
        ])
        .stdin(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !up {
        crate::warn(&format!(
            "box {0} has network = nat but never got a default route: the host side \
             of its veth was not configured, so the box has no network. On a \
             NetworkManager host, tell NM to leave the container veths alone and \
             enable systemd-networkd - see docs/setup.md, \"nat networking\". Or \
             set network = \"host\".",
            sb.name
        ));
    }
    Ok(())
}
const CANARY_TRIES: u32 = 3;

pub use crate::sandbox::state_dir;

#[cfg(test)]
mod tests {
    use super::*;

    /// An existence oracle over a fixed set of paths, standing in for the
    /// container rootfs so the lookup can be tested without one.
    fn has(paths: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |candidate| paths.iter().any(|p| Path::new(p) == candidate)
    }

    fn sandbox_with_perf(perf: bool) -> Sandbox {
        // host() is needed for Sandbox::new (the invoking user) and for env().
        let _ = crate::host::init(None, false);
        let cfg = crate::config::Config {
            perf,
            ..crate::config::Config::default()
        };
        Sandbox::new(PathBuf::from("/home/me/project"), cfg)
    }

    #[test]
    fn perf_true_adds_the_syscall_filter_line() {
        let sb = sandbox_with_perf(true);
        let text = settings_text(&sb).unwrap();
        assert!(
            text.contains("SystemCallFilter=perf_event_open\n"),
            "{text}"
        );
    }

    #[test]
    fn perf_false_omits_the_syscall_filter_line() {
        let sb = sandbox_with_perf(false);
        let text = settings_text(&sb).unwrap();
        assert!(!text.contains("SystemCallFilter"), "{text}");
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
    fn the_scoped_agent_pid_is_read_from_ssh_agents_banner() {
        assert_eq!(
            parse_agent_pid("SSH_AGENT_PID=12345; export SSH_AGENT_PID;\n"),
            Some(12345)
        );
        assert_eq!(
            parse_agent_pid(
                "SSH_AUTH_SOCK=/x/agent.sock; export SSH_AUTH_SOCK;\n\
                 SSH_AGENT_PID=42; export SSH_AGENT_PID;\n\
                 echo Agent pid 42;\n"
            ),
            Some(42)
        );
        // Nothing to parse -> None, so the caller fails closed.
        assert_eq!(parse_agent_pid(""), None);
        assert_eq!(parse_agent_pid("no pid here"), None);
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
        assert!(
            reject_control_chars("an environment variable value", "value\nCapability=all").is_err()
        );
        assert!(reject_control_chars("an environment variable name", "KEY\n[Files]").is_err());
        // An ordinary value is accepted.
        assert!(
            reject_control_chars("an environment variable value", "some/value:with-colon").is_ok()
        );
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
        assert!(!within_root(root, &root.join("../../../home/me/i_win")));
        assert!(!within_root(root, Path::new("/home/me/i_win")));
    }

    #[test]
    fn an_unmounted_box_is_never_stale() {
        assert!(!is_stale(false, None));
        assert!(!is_stale(false, Some(false)));
    }

    #[test]
    fn a_mounted_box_with_no_usable_record_is_stale() {
        assert!(is_stale(true, None));
    }

    #[test]
    fn a_mounted_box_whose_generation_vanished_is_stale() {
        assert!(is_stale(true, Some(false)));
    }

    /// The whole point of generations: an old-but-still-present one is not an
    /// error, unlike the single mutable base this replaced.
    #[test]
    fn a_mounted_box_on_an_existing_older_generation_is_not_stale() {
        assert!(!is_stale(true, Some(true)));
    }

    #[test]
    fn an_unrecorded_generation_is_let_through() {
        assert_eq!(uid_base_mismatch(1_310_720_000, None), None);
    }

    #[test]
    fn a_matching_uid_base_is_let_through() {
        assert_eq!(uid_base_mismatch(1_310_720_000, Some(1_310_720_000)), None);
    }

    #[test]
    fn a_mismatched_uid_base_is_refused_with_the_expected_value() {
        assert_eq!(
            uid_base_mismatch(1_310_720_000, Some(1_310_785_536)),
            Some(1_310_785_536)
        );
    }

    #[test]
    fn an_unmounted_box_shows_a_dash_regardless_of_its_stale_record() {
        assert_eq!(overlay_label_from(false, true, Some("g1"), "g2"), "-");
    }

    #[test]
    fn a_stale_mount_is_labelled_stale_even_with_a_recorded_id() {
        assert_eq!(overlay_label_from(true, true, Some("g1"), "g2"), "stale");
    }

    #[test]
    fn a_mount_on_the_current_generation_is_just_mounted() {
        assert_eq!(overlay_label_from(true, false, Some("g2"), "g2"), "mounted");
    }

    #[test]
    fn a_mount_on_an_older_generation_names_both() {
        assert_eq!(
            overlay_label_from(true, false, Some("g1"), "g2"),
            "mounted (g1, current g2)"
        );
    }
}
