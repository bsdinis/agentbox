//! Building the shared base image: a normal install of whatever distribution
//! this host can bootstrap, then a one-time ownership shift into the container
//! UID range.
//!
//! Which distribution that is, and everything that differs between them, lives
//! in `distro`; this module is the five stages and the generation bookkeeping
//! they happen inside.
//!
//! The image lives as a sequence of *generations* under `bases/<id>/`, not one
//! mutable directory. A box's overlay has a generation directory open as its
//! lowerdir for the life of the mount, and overlayfs never revalidates that -
//! a file a rebuild adds can be listed by `readdir` and still `ENOENT` on open,
//! for as long as the mount lives. Writing each build to a fresh directory
//! instead of mutating `current` in place means a live box's lowerdir is
//! simply never touched by anything `build` does, so `build` need not refuse
//! or unmount anything running. A generation only goes away once nothing
//! references it any more - see `gc_generations`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::argv;
use crate::config::{self, UID_RANGE};
use crate::distro::{self, Family, Guest};
use crate::host::{dry_run, host, oss, sh};
use crate::info;
use crate::nspawn;
use crate::sandbox::state_dir;

pub fn bases_dir() -> PathBuf {
    state_dir().join("bases")
}

fn current_path() -> PathBuf {
    bases_dir().join("current")
}

pub fn generation_dir(id: &str) -> PathBuf {
    bases_dir().join(id)
}

/// The generation a fresh mount should target, or an empty string if nothing
/// has ever been built. Never rewritten in place - `set_current` replaces the
/// whole file - so a reader never sees a half-written id.
pub fn current_id() -> String {
    fs::read_to_string(current_path())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The generation new mounts should use right now.
pub fn image() -> PathBuf {
    generation_dir(&current_id())
}

fn set_current(id: &str) -> Result<()> {
    if dry_run() {
        return Ok(());
    }
    fs::create_dir_all(bases_dir())?;
    fs::write(current_path(), format!("{id}\n"))
        .with_context(|| format!("cannot write {}", current_path().display()))
}

/// A generation id that sorts and reads like the timestamp it is, prefixed so
/// it is never mistaken for a bare number in a path or a message.
fn new_generation_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("g{now}")
}

pub fn generation_exists(id: &str) -> bool {
    !id.is_empty() && generation_dir(id).join("usr").exists()
}

/// Where a generation's uid_base is stamped: beside the generation directory
/// rather than inside it, for the same reason the old `base.id` lived beside
/// the image - a box would otherwise read its own lower layer's cached copy,
/// which is the one place it cannot be trusted to be current.
fn generation_uid_base_path(id: &str) -> PathBuf {
    bases_dir().join(format!("{id}.uid_base"))
}

/// Where a generation's distribution is stamped, beside it for the same reason
/// as `uid_base`.
fn generation_distro_path(id: &str) -> PathBuf {
    bases_dir().join(format!("{id}.distro"))
}

/// Which package manager a generation is driven by: the stamp if there is one,
/// else the image's own os-release. A generation built before this was
/// recorded still answers correctly through the second path, so `--refresh`
/// never has to guess.
pub fn generation_family(id: &str) -> Option<Family> {
    let stamped = fs::read_to_string(generation_distro_path(id))
        .ok()
        .and_then(|text| Family::parse(text.trim()));
    stamped.or_else(|| distro::family_of_root(&generation_dir(id)).ok())
}

/// Record what a generation's on-disk ownership was shifted for, so a box
/// configured with a different `uid_base` can be refused at mount time
/// instead of silently seeing ownership that does not match its own
/// `PrivateUsers=` range.
fn stamp_generation(id: &str, uid_base: u32, family: Family) -> Result<()> {
    if dry_run() {
        return Ok(());
    }
    fs::create_dir_all(bases_dir())?;
    for (path, value) in [
        (generation_uid_base_path(id), uid_base.to_string()),
        (generation_distro_path(id), family.as_str().to_string()),
    ] {
        fs::write(&path, format!("{value}\n"))
            .with_context(|| format!("cannot stamp {}", path.display()))?;
    }
    Ok(())
}

/// What a generation was shifted for, or `None` if it predates this being
/// recorded - nothing to check a box's configured uid_base against, so
/// `nspawn::mount` lets it through rather than refusing over a record that
/// was never written.
pub fn generation_uid_base(id: &str) -> Option<u32> {
    fs::read_to_string(generation_uid_base_path(id))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Every generation id under `bases/`.
fn generations() -> Vec<String> {
    let Ok(entries) = fs::read_dir(bases_dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Which of `all` the generations on disk are safe to delete: everything
/// except `current` and whatever a *mounted* box's own `overlay.id` names. An
/// idle (unmounted) box pins nothing here even if its own record names an
/// older generation, because its next mount always targets `current` anyway
/// (see `nspawn::overlay_stale`'s remount-on-launch) - so there is nothing for
/// its stale record to protect. Pure and separate from `gc_generations` so the
/// set logic is testable without a state directory or a real mount.
fn unreferenced(all: &[String], mounted_ids: &[String], current: &str) -> Vec<String> {
    let mut pinned: HashSet<&str> = mounted_ids.iter().map(String::as_str).collect();
    if !current.is_empty() {
        pinned.insert(current);
    }
    all.iter()
        .filter(|id| !pinned.contains(id.as_str()))
        .cloned()
        .collect()
}

/// Delete every generation nothing references any more.
///
/// Best-effort and silent about it: this runs incidentally on the way through
/// several commands (`build`, `down`, `reset`, `rm`, `remount`, `ls`), and a
/// sweep that fails to delete something - a race with another process still
/// reading it, say - should leave it for the next sweep rather than fail the
/// caller's own command.
pub fn gc_generations() {
    if dry_run() {
        return;
    }
    let mounted_ids: Vec<String> = nspawn::mounted_boxes()
        .iter()
        .filter_map(|name| nspawn::overlay_generation(name))
        .collect();
    for id in unreferenced(&generations(), &mounted_ids, &current_id()) {
        info(&format!(
            "garbage-collecting unreferenced base generation {id}"
        ));
        let _ = sh(argv!["rm", "--one-file-system", "-rf", generation_dir(&id)])
            .quiet()
            .allow_fail()
            .run();
        let _ = fs::remove_file(generation_uid_base_path(&id));
        let _ = fs::remove_file(generation_distro_path(&id));
    }
}

/// Minimal pacman config for the bootstrap. The host's mirrorlist is included
/// by path, since this one runs on the host.
const PACMAN_CONF: &str = "\
[options]
HoldPkg = pacman glibc
Architecture = auto
CheckSpace
ParallelDownloads = 8
SigLevel = Required DatabaseOptional
LocalFileSigLevel = Optional

[core]
Include = /etc/pacman.d/mirrorlist

[extra]
Include = /etc/pacman.d/mirrorlist
";

/// Run a command inside a half-built (or being-refreshed) generation at `base`.
fn in_image(
    base: &Path,
    cmd: Vec<OsString>,
    uid_base: Option<u32>,
    host_cache: Option<&str>,
) -> Result<()> {
    // --background= for the same reason as the box launches: a build is long
    // and interactive, and there is no config to consult this early.
    //
    // `replace-host` rather than `copy-host` for the reason spelled out in
    // `nspawn::settings_text`: `copy-host` does nothing when the image's
    // `/etc/resolv.conf` is a symlink, and a Debian image grows exactly that
    // symlink the moment `systemd-resolved` is installed - which is stage 2,
    // so every stage after it would run without a resolver. A fresh build
    // survives that (only stage 2 needs the network, and debootstrap leaves a
    // regular file behind for it), but `--refresh` upgrades an image that
    // already has the symlink, so its `apt-get update` would resolve nothing.
    let mut args = argv![
        "systemd-nspawn",
        "-q",
        "-D",
        base,
        "--as-pid2",
        "--register=no",
        "--resolv-conf=replace-host",
        "--timezone=off",
        "--background="
    ];
    if let Some(cache) = host_cache {
        // Share the host's package cache: a fresh build downloads almost
        // nothing, and whatever it does download stays useful to the host.
        args.push(oss(format!("--bind={cache}")));
    }
    if let Some(uid_base) = uid_base {
        args.push(oss(format!("--private-users={uid_base}:{UID_RANGE}")));
        args.push(oss("--private-users-ownership=off"));
    }
    args.push(oss("--"));
    args.extend(cmd);
    sh(args).run().map(|_| ())
}

pub fn require() -> Result<()> {
    if generation_exists(&current_id()) {
        return Ok(());
    }
    if dry_run() {
        crate::warn(&format!(
            "no base image at {} yet (dry run continues)",
            image().display()
        ));
        return Ok(());
    }
    bail!("no base image yet - run `agentbox build` first");
}

pub fn build(refresh: bool, force: bool) -> Result<()> {
    // Opportunistic: whatever a previous command's teardown left unreferenced
    // is worth reclaiming before spending disk on a new generation.
    gc_generations();

    let uid_base = config::global_uid_base()?;
    let guest = distro::guest()?;
    let current = current_id();
    let exists = generation_exists(&current);

    if exists && !refresh && !force {
        bail!(
            "base image already exists (generation {current}) (use --refresh to update it, \
             --force to rebuild from scratch)",
        );
    }

    // Refreshing an existing image: copy it forward into a new generation and
    // upgrade the copy, never touching whatever any live box has open. `force`
    // takes precedence when both are given, exactly as it did when refresh
    // meant "pacman -Syu in place" - see the branch below.
    if exists && refresh && !force {
        // A refresh follows the generation it copies, not what this host would
        // build today: the copy is upgraded with its own package manager, and
        // its own family's built-in package list is the one whose names exist
        // in it. Only `--force` changes the distribution.
        let family = generation_family(&current).unwrap_or(guest.family);
        let packages = base_packages_for(family)?;
        return refresh_generation(&current, uid_base, &packages, family);
    }

    let packages = base_packages_for(guest.family)?;

    if force && exists {
        crate::warn(
            "existing boxes keep the writes they made over the old image; \
             `agentbox reset <box>` if one misbehaves on the new one",
        );
    }

    // The first build, or --force: bootstrap a wholly new generation from
    // scratch. This never touches whatever `current` still names, so a build
    // racing a live box is a non-event - the box's overlay keeps serving the
    // directory it already opened, untouched, until nothing references it any
    // more (see `gc_generations`).
    let id = new_generation_id();
    let base = generation_dir(&id);
    bootstrap(&guest, &base)?;

    if guest.family == Family::Arch {
        // Arch only: the image needs its own trusted keyring before it can
        // install or verify anything on its own later. debootstrap installs
        // the equivalent keyring package as part of the bootstrap itself.
        info("initialising the pacman keyring inside the image");
        in_image(
            &base,
            argv![
                "/bin/bash",
                "-euo",
                "pipefail",
                "-c",
                "pacman-key --init && pacman-key --populate archlinux"
            ],
            None,
            None,
        )?;
    }

    info(&format!(
        "installing {} packages inside the image",
        packages.len()
    ));
    in_image(
        &base,
        guest.family.install_cmd(&packages, false),
        None,
        guest.family.host_cache(),
    )?;

    info("configuring the image");
    let user = &host().user;
    in_image(
        &base,
        argv![
            "/bin/bash",
            "-c",
            guest.setup_script(&user.name, user.uid, user.gid)
        ],
        None,
        None,
    )?;

    // The reason boxes start instantly: with the on-disk ownership already
    // matching the container's user namespace, nothing has to be chowned or
    // ID-mapped at launch (see docs/design.md).
    info(&format!("shifting image ownership to UID base {uid_base}"));
    sh(argv![
        "systemd-nspawn",
        "-q",
        "-D",
        &base,
        "--as-pid2",
        "--register=no",
        format!("--private-users={uid_base}:{UID_RANGE}"),
        "--private-users-ownership=chown",
        "--background=",
        "--",
        "/bin/true"
    ])
    .run()?;

    stamp_generation(&id, uid_base, guest.family)?;
    set_current(&id)?;
    info(&format!(
        "base image ready: {} ({}, generation {id})",
        base.display(),
        guest.describe()
    ));
    Ok(())
}

/// `--refresh`: copy the current generation's tree into a fresh one and
/// `pacman -Syu` the copy, rather than mutating `current` in place. A live
/// box's overlay has `current`'s directory open as its lowerdir, and
/// overlayfs never revalidates that once mounted, so writing into it under a
/// live mount would serve that mount a half-updated tree for the rest of its
/// life. `--reflink=auto` costs nothing where it works; this host's ext4 does
/// not support it, so this pays the image's full size in disk and time - but
/// the copy, never the original, is what gets upgraded, so nothing already
/// mounted is ever touched.
fn refresh_generation(
    current: &str,
    uid_base: u32,
    packages: &[String],
    family: Family,
) -> Result<()> {
    let old = generation_dir(current);
    let id = new_generation_id();
    let new = generation_dir(&id);
    info(&format!(
        "copying base generation {current} to {id} for refresh"
    ));
    sh(argv!["cp", "-a", "--reflink=auto", &old, &new]).run()?;

    info("refreshing the copy");
    in_image(&new, family.upgrade_cmd(packages), Some(uid_base), None)?;

    stamp_generation(&id, uid_base, family)?;
    set_current(&id)?;
    info(&format!(
        "base image ready: {} (generation {id})",
        new.display()
    ));
    Ok(())
}

/// The image's package list, refused if it was written for the other
/// distribution. Both build paths go through here, because both hand the list
/// straight to a package manager that installs nothing when one name in it
/// does not resolve - see `Family::check_base_packages`.
fn base_packages_for(family: Family) -> Result<Vec<String>> {
    let packages = config::base_packages(family.default_base_packages())?;
    family.check_base_packages(&packages)?;
    Ok(packages)
}

/// Stage 1: a minimal system, installed into an empty directory by whichever
/// of the host's tools can do it.
///
/// This is the one stage that runs on the host rather than inside the image,
/// because there is nothing inside the image yet to run it. Each family has a
/// tool for exactly this - `pacman --root`, `debootstrap` - and only the host's
/// own family's tool can be assumed present, which is why the guest follows
/// the host unless told otherwise.
fn bootstrap(guest: &Guest, base: &Path) -> Result<()> {
    match guest.family {
        Family::Arch => bootstrap_arch(guest, base),
        Family::Debian => bootstrap_debian(guest, base),
    }
}

/// `debootstrap` into an empty directory, then replace the single-component
/// archive list it leaves behind.
///
/// Unlike the pacman path below, nothing here mounts anything: debootstrap
/// sets up and tears down the chroot's /proc itself, with a fresh mount rather
/// than a bind of the host's, so none of the mount-propagation care that
/// `bootstrap_arch` needs applies - there is no host subtree inside the image
/// to propagate an unmount back out through.
fn bootstrap_debian(guest: &Guest, base: &Path) -> Result<()> {
    let tool = ["/usr/sbin/debootstrap", "/usr/bin/debootstrap"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .context(
            "the base image is bootstrapped with debootstrap, which this host does not \
             have; install it (`apt-get install debootstrap`)",
        )?;
    let script = Path::new("/usr/share/debootstrap/scripts").join(&guest.suite);
    if !script.exists() {
        bail!(
            "this host's debootstrap has no script for suite {:?} ({} does not exist), \
             so it cannot bootstrap that release. Name a suite it does have, in {}:\n  \
             [base]\n  suite = \"...\"",
            guest.suite,
            script.display(),
            config::global_path().display()
        );
    }
    info(&format!(
        "bootstrapping {} into {} from {}",
        guest.describe(),
        base.display(),
        guest.mirror
    ));
    if !dry_run() {
        fs::create_dir_all(state_dir().join("boxes"))?;
        fs::create_dir_all(base)?;
    }
    // minbase is the smallest variant that still has apt, which is all the
    // later stages need - every other package in the image is named by
    // `base_packages` and installed from inside it, exactly as on Arch.
    // ca-certificates comes in here rather than there so that an https mirror
    // is usable by the time apt is first run inside the image.
    //
    // Run it from the state directory, not the cwd agentbox was invoked from:
    // debootstrap fetches with `wget`, which drops a `wget-log` beside itself
    // whenever it cannot write its output to the terminal, and every stage
    // here runs as root after the sudo re-exec. Inheriting the caller's cwd
    // therefore litters root-owned `wget-log`, `wget-log.1`, ... into whatever
    // directory the user happened to type `agentbox build` in - a git repo,
    // usually, where they show up as untracked files nobody can delete
    // without sudo.
    sh(argv![
        tool,
        "--variant=minbase",
        format!(
            "--components={}",
            guest
                .components
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(",")
        ),
        "--include=ca-certificates",
        &guest.suite,
        base,
        &guest.mirror
    ])
    .cwd(state_dir())
    .run()?;
    write_apt_sources(guest, base)
}

/// Point the image at the full archive. debootstrap writes a one-line list
/// naming only the components it was given and no -updates or -security
/// pocket, so a box would never see a security update; this replaces it
/// outright rather than adding to it, because two lists naming the same
/// archive make every `apt-get update` in every box warn about duplicates.
fn write_apt_sources(guest: &Guest, base: &Path) -> Result<()> {
    if dry_run() {
        return Ok(());
    }
    let dir = base.join("etc/apt/sources.list.d");
    fs::create_dir_all(&dir)?;
    let path = dir.join("agentbox.sources");
    fs::write(&path, guest.sources_text())
        .with_context(|| format!("cannot write {}", path.display()))?;
    let legacy = base.join("etc/apt/sources.list");
    if legacy.exists() {
        fs::remove_file(&legacy).with_context(|| format!("cannot remove {}", legacy.display()))?;
    }
    Ok(())
}

/// Stage 1 on Arch: a minimal system installed by the host's pacman, reusing
/// the host package cache and keyring. This is what pacstrap does; doing it
/// inline avoids depending on arch-install-scripts.
fn bootstrap_arch(guest: &Guest, base: &Path) -> Result<()> {
    if !Path::new("/usr/bin/pacman").exists() {
        bail!("the base image is bootstrapped with pacman, which this host does not have");
    }
    info(&format!(
        "bootstrapping {} into {}",
        guest.describe(),
        base.display()
    ));

    let state = state_dir();
    let conf = state.join("pacman-build.conf");
    if !dry_run() {
        fs::create_dir_all(state.join("boxes"))?;
        fs::write(&conf, PACMAN_CONF)
            .with_context(|| format!("cannot write {}", conf.display()))?;
        for dir in [
            "var/lib/pacman",
            "var/cache/pacman/pkg",
            "var/log",
            "etc/pacman.d",
            "dev",
            "proc",
            "sys",
            "run",
            "tmp",
            "root",
        ] {
            fs::create_dir_all(base.join(dir))?;
        }
        fs::set_permissions(
            base.join("tmp"),
            std::os::unix::fs::PermissionsExt::from_mode(0o1777),
        )?;
        fs::copy(
            "/etc/pacman.d/mirrorlist",
            base.join("etc/pacman.d/mirrorlist"),
        )
        .context("cannot copy the host mirrorlist")?;
    }

    // Install scriptlets expect the kernel filesystems, as in a chroot.
    //
    // /proc and /sys are rbound from the host: they are what the scriptlets
    // actually need to read, and neither carries the kind of state a chown can
    // damage. /run and /dev get a *fresh tmpfs* instead of a bind, because
    // binding the host's would hand the host to the scriptlets: pacman's
    // systemd install runs `systemd-tmpfiles --create` inside the chroot, and
    // the rules in /usr/lib/tmpfiles.d are full of `/run` and `/dev` paths with
    // an owner named by *user name*. Resolved against the image's /etc/passwd,
    // those names give the image's IDs, and the chown then lands on the host's
    // real files. That is not hypothetical: it chowned this host's
    // /run/systemd/netif{,/links,/leases} to the image's `systemd-network`
    // (977) while the host's own networkd runs as 979, so networkd lost write
    // access to its own runtime state, could not configure the container veth,
    // and every `network = "nat"` box came up with no address, no route and no
    // NAT. It also took /run/uuidd, /run/tpm2-tss/eventlog and /dev/kvm's
    // group. A throwaway tmpfs gives the scriptlets somewhere to write that
    // nothing outside the build can see.
    //
    // Each rbind is immediately made rslave. Without that the copied submounts
    // join the *peer group* of the host's own mounts - / is shared under
    // systemd - and the `umount -R` below propagates back out, tearing
    // /dev/pts, /dev/shm and /run/user/$UID off the running host. rslave lets
    // host mount events propagate inwards while nothing propagates outwards.
    let api = [
        ("proc", Api::HostBind),
        ("sys", Api::HostBind),
        ("dev", Api::PrivateTmpfs),
        ("run", Api::PrivateTmpfs),
    ];
    let mut mounted: Vec<PathBuf> = Vec::new();
    for (dir, kind) in api {
        let target = base.join(dir);
        let bind = match kind {
            Api::HostBind => sh(argv!["mount", "--rbind", format!("/{dir}"), &target])
                .quiet()
                .run(),
            Api::PrivateTmpfs => sh(argv![
                "mount",
                "-t",
                "tmpfs",
                "-o",
                "mode=0755,nosuid",
                format!("agentbox-{dir}"),
                &target
            ])
            .quiet()
            .run(),
        };
        if let Err(err) = bind {
            unmount_api(&mounted);
            return Err(err);
        }
        if kind == Api::PrivateTmpfs {
            if let Err(err) = populate_dev(dir, &target) {
                unmount_api(&mounted);
                return Err(err);
            }
        }
        let detached = sh(argv!["mount", "--make-rslave", &target])
            .quiet()
            .run()
            .and_then(|_| assert_detached(&target));
        if let Err(err) = detached {
            // `target` is deliberately left behind. It is the one subtree we
            // could not prove is detached, and `umount -R` on a still-shared
            // bind is precisely what tears /dev/pts and /run/user/$UID off the
            // running host - the failure this whole dance exists to prevent.
            // A leaked bind under the image costs a reboot; propagating the
            // teardown costs the session.
            unmount_api(&mounted);
            return Err(err).with_context(|| {
                format!(
                    "cannot detach {} from host mount propagation; it was left \
                     mounted deliberately, since unmounting a shared bind would \
                     tear filesystems off the running host",
                    target.display()
                )
            });
        }
        // Only now is it safe to tear down: everything in `mounted` is known
        // rslave, so `umount -R` on it cannot reach the host.
        mounted.push(target);
    }
    let result = sh(argv![
        "pacman",
        "--config",
        &conf,
        "--root",
        base,
        "--cachedir",
        "/var/cache/pacman/pkg",
        "--gpgdir",
        "/etc/pacman.d/gnupg",
        "--noconfirm",
        "--needed",
        "-Sy",
        "base",
        "archlinux-keyring"
    ])
    .run();
    unmount_api(&mounted);
    result.map(|_| ())
}

/// How one of the chroot's API filesystems is provided.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Api {
    /// Recursively bound from the host: the scriptlets need to *read* the real
    /// thing, and it holds nothing a chown can spoil.
    HostBind,
    /// A throwaway tmpfs. For /run and /dev, whose host copies are full of
    /// paths that `systemd-tmpfiles --create` chowns by user *name* - resolved
    /// against the image's /etc/passwd, so a bind would rewrite the host's own
    /// files to the image's IDs.
    PrivateTmpfs,
}

/// Give a freshly mounted tmpfs the bits a chroot's scriptlets expect.
///
/// /run only needs to be writable, which it already is. /dev needs the handful
/// of nodes anything might open - /dev/null above all - since a tmpfs starts
/// empty, where the host's /dev came fully populated. These are new nodes on a
/// private tmpfs, so tmpfiles is welcome to chown them.
fn populate_dev(dir: &str, target: &Path) -> Result<()> {
    if dir != "dev" || dry_run() {
        return Ok(());
    }
    // (name, mode, major, minor)
    for (name, mode, major, minor) in [
        ("null", "0666", 1, 3),
        ("zero", "0666", 1, 5),
        ("full", "0666", 1, 7),
        ("random", "0666", 1, 8),
        ("urandom", "0666", 1, 9),
        ("tty", "0666", 5, 0),
        ("console", "0600", 5, 1),
    ] {
        sh(argv![
            "mknod",
            "-m",
            mode,
            target.join(name),
            "c",
            major.to_string(),
            minor.to_string()
        ])
        .quiet()
        .run()
        .with_context(|| format!("cannot create /dev/{name} in the image"))?;
    }
    for (link, dest) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        std::os::unix::fs::symlink(dest, target.join(link))
            .with_context(|| format!("cannot link /dev/{link} in the image"))?;
    }
    for sub in ["pts", "shm"] {
        fs::create_dir_all(target.join(sub))?;
    }
    sh(argv![
        "mount",
        "-t",
        "devpts",
        "-o",
        "mode=0620,gid=5,nosuid,noexec",
        "agentbox-devpts",
        target.join("pts")
    ])
    .quiet()
    .run()
    .context("cannot mount devpts in the image")?;
    sh(argv![
        "mount",
        "-t",
        "tmpfs",
        "-o",
        "mode=1777,nosuid,nodev",
        "agentbox-shm",
        target.join("shm")
    ])
    .quiet()
    .run()
    .context("cannot mount /dev/shm in the image")?;
    std::os::unix::fs::symlink("/dev/pts/ptmx", target.join("ptmx"))
        .context("cannot link /dev/ptmx in the image")?;
    Ok(())
}

/// One `KEY="value"` field out of a `findmnt -P` line. That format is used in
/// preference to columns because findmnt escapes quotes and spaces inside the
/// values, so a mount point containing a space cannot shift the parse.
fn findmnt_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let start = line.find(&format!("{key}=\""))? + key.len() + 2;
    let rest = &line[start..];
    Some(&rest[..rest.find('"')?])
}

/// The first still-shared mount in `findmnt -R -n -P -o TARGET,PROPAGATION`
/// output, if any. A shared mount is one whose teardown propagates to its peers
/// - which, for a bind of a host subtree, means the host's own mounts.
fn first_shared(findmnt_output: &str) -> Option<&str> {
    findmnt_output.lines().find(|line| {
        findmnt_field(line, "PROPAGATION")
            .is_some_and(|flags| flags.split(',').any(|f| f == "shared"))
    })
}

/// `mount --make-rslave` reporting success is not proof it took effect, and the
/// cost of being wrong is the host losing /dev/pts and /run/user/$UID when the
/// teardown runs. Read the propagation flags back and refuse to go on if any
/// mount in the subtree is still shared.
fn assert_detached(target: &Path) -> Result<()> {
    if dry_run() {
        return Ok(());
    }
    let out = sh(argv![
        "findmnt",
        "-R",
        "-n",
        "-P",
        "-o",
        "TARGET,PROPAGATION",
        target
    ])
    .quiet()
    .output();
    if out.is_empty() {
        bail!(
            "cannot read mount propagation for {}; refusing to continue, tearing down \
             a shared mount would unmount the host's own filesystems",
            target.display()
        );
    }
    if let Some(line) = first_shared(&out) {
        bail!(
            "{} is still shared after --make-rslave ({}); refusing to continue, \
             unmounting it would tear filesystems off the running host",
            target.display(),
            findmnt_field(line, "TARGET").unwrap_or(line)
        );
    }
    Ok(())
}

/// Tear down the bootstrap API mounts, innermost first. Every path passed in
/// must already have been proved detached by `assert_detached`: `umount -R` on
/// a still-shared subtree is what unmounts the host's own filesystems.
fn unmount_api(mounted: &[PathBuf]) {
    for target in mounted.iter().rev() {
        let _ = sh(argv!["umount", "-R", "-l", target])
            .quiet()
            .silent()
            .allow_fail()
            .run();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `findmnt -R -n -P -o TARGET,PROPAGATION` output, one mount per line.
    fn findmnt(mounts: &[(&str, &str)]) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for (target, prop) in mounts {
            let _ = writeln!(out, "TARGET=\"{target}\" PROPAGATION=\"{prop}\"");
        }
        out
    }

    fn target_of(line: &str) -> &str {
        super::findmnt_field(line, "TARGET").unwrap()
    }

    #[test]
    fn detects_a_shared_mount_anywhere_in_the_subtree() {
        let out = findmnt(&[
            ("/base/dev", "private"),
            ("/base/dev/pts", "shared"),
            ("/base/dev/shm", "private"),
        ]);
        assert_eq!(first_shared(&out).map(target_of), Some("/base/dev/pts"));
    }

    #[test]
    fn a_fully_detached_subtree_is_accepted() {
        let out = findmnt(&[("/base/run", "private"), ("/base/run/user/1000", "slave")]);
        assert_eq!(first_shared(&out), None);
    }

    #[test]
    fn shared_and_slave_together_still_propagates_outwards() {
        let out = findmnt(&[("/base/run", "shared,slave")]);
        assert_eq!(first_shared(&out).map(target_of), Some("/base/run"));
    }

    #[test]
    fn a_target_merely_containing_the_word_is_not_a_match() {
        let out = findmnt(&[
            ("/base/shared-things", "private"),
            ("/base/unshared", "private"),
        ]);
        assert_eq!(first_shared(&out), None);
    }

    /// The reason for parsing `KEY="value"` rather than whitespace columns.
    #[test]
    fn a_mount_point_containing_a_space_does_not_shift_the_parse() {
        let out = findmnt(&[("/base/my code", "private"), ("/base/other dir", "shared")]);
        assert_eq!(first_shared(&out).map(target_of), Some("/base/other dir"));
    }

    #[test]
    fn a_line_without_a_propagation_field_is_not_treated_as_shared() {
        assert_eq!(first_shared("TARGET=\"/base/dev\"\n"), None);
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn current_and_every_mounted_generation_are_kept() {
        let all = ids(&["g1", "g2", "g3"]);
        // g1 is current, g2 is a mounted box's own record, g3 is neither.
        assert_eq!(unreferenced(&all, &ids(&["g2"]), "g1"), ids(&["g3"]));
    }

    #[test]
    fn nothing_is_deleted_while_everything_is_referenced() {
        let all = ids(&["g1", "g2"]);
        assert!(unreferenced(&all, &ids(&["g2"]), "g1").is_empty());
    }

    #[test]
    fn an_idle_boxs_stale_record_pins_nothing() {
        // g1 exists on disk but no box is *mounted* on it (mounted_ids empty),
        // even though it might be some idle box's leftover overlay.id: an idle
        // box's next mount always targets current, so its old record protects
        // nothing.
        let all = ids(&["g1", "g2"]);
        assert_eq!(unreferenced(&all, &[], "g2"), ids(&["g1"]));
    }

    #[test]
    fn no_current_generation_yet_pins_nothing_by_itself() {
        // Before the first build, current_id() is empty; that must never match
        // a real (non-empty) generation id and accidentally spare it.
        let all = ids(&["g1"]);
        assert_eq!(unreferenced(&all, &[], ""), ids(&["g1"]));
    }
}
