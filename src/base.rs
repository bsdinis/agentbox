//! Building the shared base image: a normal Arch install, then a one-time
//! ownership shift into the container UID range.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::argv;
use crate::config::{self, UID_RANGE};
use crate::host::{dry_run, host, oss, sh};
use crate::info;
use crate::nspawn;
use crate::sandbox::{state_dir, MACHINES};

pub fn image() -> PathBuf {
    state_dir().join("base")
}

/// Where the image's identity is stamped. Beside the image rather than inside
/// it: a box would otherwise read its own lower layer's copy through the
/// overlay, which is the one place it cannot be trusted to be current, and
/// `--force` deletes the image directory wholesale.
fn id_path() -> PathBuf {
    state_dir().join("base.id")
}

/// What the image is right now, changed by every build. An empty string means
/// an image built before this was stamped, which is indistinguishable from any
/// other unstamped image - so an overlay carrying the same empty id is left
/// alone rather than reported stale on every launch.
pub fn id() -> String {
    fs::read_to_string(id_path())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Give the image a new identity, so every overlay mounted on the old one can
/// tell that what is underneath it has changed.
fn stamp() -> Result<()> {
    if dry_run() {
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    fs::write(id_path(), format!("{now}\n"))
        .with_context(|| format!("cannot stamp {}", id_path().display()))
}

pub fn require() -> Result<()> {
    if image().join("usr").exists() {
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

/// Run a command inside the half-built image.
fn in_image(cmd: Vec<OsString>, uid_base: Option<u32>, host_cache: bool) -> Result<()> {
    // --background= for the same reason as the box launches: a build is long
    // and interactive, and there is no config to consult this early.
    let mut args = argv![
        "systemd-nspawn",
        "-q",
        "-D",
        image(),
        "--as-pid2",
        "--register=no",
        "--resolv-conf=copy-host",
        "--timezone=off",
        "--background="
    ];
    if host_cache {
        // Share the host's package cache: a fresh build downloads almost
        // nothing, and whatever it does download stays useful to the host.
        args.push(oss("--bind=/var/cache/pacman/pkg"));
    }
    if let Some(uid_base) = uid_base {
        args.push(oss(format!("--private-users={uid_base}:{UID_RANGE}")));
        args.push(oss("--private-users-ownership=off"));
    }
    args.push(oss("--"));
    args.extend(cmd);
    sh(args).run().map(|_| ())
}

/// Take every box off the base image before it is rewritten.
///
/// A box's overlay has the base as its `lowerdir`, and overlayfs does not
/// tolerate a lower layer changing underneath it: the box goes on serving the
/// view it cached, so a file `--refresh` adds can end up half-visible - listed
/// by `readdir`, `ENOENT` on open - and stay that way for the life of the
/// mount. `--force` is worse still, deleting the layer outright.
///
/// A stopped box is no obstacle: unmounting it costs nothing, since its writes
/// live in `upper` on disk and the next launch remounts on demand. A running
/// box has to be powered off by hand, so name the boxes and stop.
fn release_boxes() -> Result<()> {
    let running: Vec<String> = nspawn::boxes()
        .into_iter()
        .filter(|name| nspawn::service_active(name))
        .collect();
    if !running.is_empty() {
        bail!(
            "these boxes are running on the base image: {}\n\
             power them off first (`agentbox down <box>`, or leave the `shell`/`run` \
             sessions holding them); changing the image under a live box corrupts \
             what that box sees",
            running.join(", ")
        );
    }
    // Everything left is idle, so nothing here can fail for being in use.
    for name in nspawn::mounted_boxes() {
        info(&format!("taking idle box {name} off the base image"));
        nspawn::umount_root(&Path::new(MACHINES).join(&name))?;
    }
    Ok(())
}

pub fn build(refresh: bool, force: bool) -> Result<()> {
    let uid_base = config::global_uid_base()?;
    let packages = config::base_packages()?;
    let base = image();

    if base.exists() && !refresh && !force {
        bail!(
            "base image already exists at {} (use --refresh to update it, \
             --force to rebuild from scratch)",
            base.display()
        );
    }
    // Both destructive paths below rewrite the layer every box is overlaying,
    // so take the boxes off it first.
    if base.exists() && (refresh || force) {
        release_boxes()?;
    }
    if force && base.exists() {
        crate::warn(
            "existing boxes keep the writes they made over the old image; \
             `agentbox reset <box>` if one misbehaves on the new one",
        );
        info(&format!("removing {}", base.display()));
        sh(argv!["rm", "--one-file-system", "-rf", &base]).run()?;
    }

    // Refreshing an existing image: it is already shifted, so use the same
    // user namespace the boxes use.
    if base.exists() {
        info("refreshing base image");
        let mut cmd = argv!["/usr/bin/pacman", "-Syu", "--noconfirm", "--needed"];
        cmd.extend(packages.iter().map(oss));
        in_image(cmd, Some(uid_base), false)?;
        return stamp();
    }

    bootstrap(&base)?;

    info("initialising the pacman keyring inside the image");
    in_image(
        argv![
            "/bin/bash",
            "-euo",
            "pipefail",
            "-c",
            "pacman-key --init && pacman-key --populate archlinux"
        ],
        None,
        false,
    )?;

    info(&format!(
        "installing {} packages inside the image",
        packages.len()
    ));
    let mut cmd = argv!["/usr/bin/pacman", "-Sy", "--noconfirm", "--needed"];
    cmd.extend(packages.iter().map(oss));
    in_image(cmd, None, true)?;

    info("configuring the image");
    in_image(argv!["/bin/bash", "-c", setup_script()], None, false)?;

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

    stamp()?;
    info(&format!("base image ready: {}", base.display()));
    Ok(())
}

/// Stage 1: a minimal system installed by the host's pacman, reusing the host
/// package cache and keyring. This is what pacstrap does; doing it inline
/// avoids depending on arch-install-scripts.
fn bootstrap(base: &Path) -> Result<()> {
    if !Path::new("/usr/bin/pacman").exists() {
        bail!("the base image is bootstrapped with pacman, which this host does not have");
    }
    info(&format!("bootstrapping Arch into {}", base.display()));

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

/// Stage 3: make the image a usable dev box for the invoking user. The sandbox
/// user mirrors the host user so that `~` and project paths are spelled
/// identically inside and outside.
fn setup_script() -> String {
    let user = &host().user;
    let name = shell_quote(&user.name);
    format!(
        r#"set -euo pipefail
sed -i 's/^#\(en_US.UTF-8 UTF-8\)/\1/' /etc/locale.gen
locale-gen
printf 'LANG=en_US.UTF-8\n' > /etc/locale.conf
sed -i 's/^#\(Color\)/\1/;s/^#\(ParallelDownloads.*\)/\1/' /etc/pacman.conf
: > /etc/machine-id
printf 'agentbox\n' > /etc/hostname
groupadd -g {gid} -o {name} 2>/dev/null || true
useradd -m -u {uid} -g {gid} -G wheel -s /bin/bash {name} 2>/dev/null || true
passwd -d {name} >/dev/null 2>&1 || true
install -d -m 750 /etc/sudoers.d
printf '%%wheel ALL=(ALL:ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/00-agentbox
chmod 440 /etc/sudoers.d/00-agentbox
printf 'Defaults env_keep += "CLAUDE_CODE_OAUTH_TOKEN ANTHROPIC_API_KEY GITHUB_TOKEN"\n' > /etc/sudoers.d/10-agentbox-env
chmod 440 /etc/sudoers.d/10-agentbox-env
"#,
        uid = user.uid,
        gid = user.gid,
        name = name,
    )
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::first_shared;

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
}
