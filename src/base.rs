//! Building the shared base image: a normal Arch install, then a one-time
//! ownership shift into the container UID range.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::argv;
use crate::config::{self, UID_RANGE};
use crate::host::{dry_run, host, oss, sh};
use crate::sandbox::STATE;
use crate::info;

pub fn image() -> PathBuf {
    Path::new(STATE).join("base")
}

pub fn require() -> Result<()> {
    if image().join("usr").exists() {
        return Ok(());
    }
    if dry_run() {
        crate::warn(&format!("no base image at {} yet (dry run continues)", image().display()));
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
    let mut args = argv![
        "systemd-nspawn",
        "-q",
        "-D",
        image(),
        "--as-pid2",
        "--register=no",
        "--resolv-conf=copy-host",
        "--timezone=off"
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
    if force && base.exists() {
        info(&format!("removing {}", base.display()));
        sh(argv!["rm", "--one-file-system", "-rf", &base]).run()?;
    }

    // Refreshing an existing image: it is already shifted, so use the same
    // user namespace the boxes use.
    if base.exists() {
        info("refreshing base image");
        let mut cmd = argv!["/usr/bin/pacman", "-Syu", "--noconfirm", "--needed"];
        cmd.extend(packages.iter().map(oss));
        return in_image(cmd, Some(uid_base), false);
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

    info(&format!("installing {} packages inside the image", packages.len()));
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
        "--",
        "/bin/true"
    ])
    .run()?;

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

    let state = Path::new(STATE);
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
        fs::copy("/etc/pacman.d/mirrorlist", base.join("etc/pacman.d/mirrorlist"))
            .context("cannot copy the host mirrorlist")?;
    }

    // Install scriptlets expect the kernel filesystems, as in a chroot.
    let api = ["proc", "sys", "dev", "run"];
    for dir in api {
        sh(argv!["mount", "--rbind", format!("/{dir}"), base.join(dir)]).quiet().run()?;
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
    for dir in api.iter().rev() {
        let _ = sh(argv!["umount", "-R", "-l", base.join(dir)]).quiet().silent().allow_fail().run();
    }
    result.map(|_| ())
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
printf 'Defaults env_keep += "ANTHROPIC_API_KEY GITHUB_TOKEN"\n' > /etc/sudoers.d/10-agentbox-env
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
