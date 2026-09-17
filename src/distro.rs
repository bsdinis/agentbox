//! Which distribution goes inside the image, and everything that differs
//! between them.
//!
//! agentbox itself is distribution-agnostic once a box exists - the overlay,
//! the bind plan, the uid shift and the session machinery never ask what is
//! inside. Three things do: bootstrapping a fresh generation (`pacman --root`
//! against a host `pacman`, or `debootstrap` against a host apt archive),
//! installing packages into one (`pacman -S` or `apt-get install`), and the
//! one-time configuration that makes the image a usable dev box (locale, the
//! sandbox user, the sudo group - `wheel` on Arch, `sudo` on Debian).
//!
//! The guest follows the host by default, because the host is what can
//! bootstrap it: only an Arch host has a `pacman` able to install into an empty
//! root, and only a Debian-family host is guaranteed a `debootstrap` with a
//! script for the suite. `[base] distro` overrides that where the tooling is
//! present anyway.
//!
//! What a generation was built from is stamped beside it (`bases/<id>.distro`),
//! so `--refresh` upgrades a generation with its own package manager rather
//! than whatever this host would build today, and a box's own rootfs answers
//! the same question for `install_packages` (`/etc/os-release`), which is the
//! one copy that is true even for a box built before any of this existed.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::argv;
use crate::config::{self, DEFAULT_BASE_PACKAGES, DEFAULT_BASE_PACKAGES_DEBIAN};
use crate::host::oss;

/// The package manager an image is driven by. This, not the exact
/// distribution, is what every later command needs to know, and it is what a
/// generation is stamped with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Arch,
    Debian,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Arch => "arch",
            Family::Debian => "debian",
        }
    }

    pub fn parse(s: &str) -> Option<Family> {
        match s.trim() {
            "arch" | "archlinux" => Some(Family::Arch),
            "debian" | "ubuntu" => Some(Family::Debian),
            _ => None,
        }
    }

    /// The built-in `base_packages` list for this family. Deliberately two
    /// hand-written lists rather than a translation table: package names are
    /// not a mapping (Arch's `base-devel` is Debian's `build-essential`, and
    /// `python` is `python3`), and a wrong guess here is a fatal build.
    pub fn default_base_packages(self) -> &'static [&'static str] {
        match self {
            Family::Arch => DEFAULT_BASE_PACKAGES,
            Family::Debian => DEFAULT_BASE_PACKAGES_DEBIAN,
        }
    }

    /// The command that installs `packages` into an image, as the caller's
    /// `in_image`/`attach` will run it.
    ///
    /// `booted` says whether a real init is running inside: in a booted box
    /// dpkg's maintainer scripts may start the services they install, which is
    /// what a box wants, but during a build there is no init for them to talk
    /// to, so `policy-rc.d` turns those starts into a no-op rather than an
    /// error that fails the whole install.
    pub fn install_cmd(self, packages: &[String], booted: bool) -> Vec<OsString> {
        match self {
            Family::Arch => {
                let mut cmd = argv!["/usr/bin/pacman", "-Sy", "--noconfirm", "--needed"];
                cmd.extend(packages.iter().map(oss));
                cmd
            }
            Family::Debian => apt_cmd(
                &format!(
                    "apt-get update\n\
                     apt-get install -y --no-install-recommends {}\n",
                    shell_words(packages)
                ),
                booted,
            ),
        }
    }

    /// The command `build --refresh` upgrades a copied generation with. Always
    /// a build, never a booted box, so Debian gets `policy-rc.d`.
    pub fn upgrade_cmd(self, packages: &[String]) -> Vec<OsString> {
        match self {
            Family::Arch => {
                let mut cmd = argv!["/usr/bin/pacman", "-Syu", "--noconfirm", "--needed"];
                cmd.extend(packages.iter().map(oss));
                cmd
            }
            Family::Debian => apt_cmd(
                &format!(
                    "apt-get update\n\
                     apt-get -y dist-upgrade\n\
                     apt-get install -y --no-install-recommends {}\n\
                     apt-get clean\n",
                    shell_words(packages)
                ),
                false,
            ),
        }
    }

    /// What to tell a user to type to install a package by hand in a box.
    pub fn install_hint(self) -> &'static str {
        match self {
            Family::Arch => "pacman -S PKG",
            Family::Debian => "apt-get install PKG",
        }
    }

    /// The host directory worth sharing with a build so it downloads less.
    /// Arch's package cache is a flat directory of signed files that both sides
    /// can read and write; apt's is per-suite state guarded by its own lock, so
    /// the Debian build is left to download its own.
    pub fn host_cache(self) -> Option<&'static str> {
        match self {
            Family::Arch => Some("/var/cache/pacman/pkg"),
            Family::Debian => None,
        }
    }

    /// The other family. Only for diagnostics - there are exactly two, and a
    /// third would turn this into a search rather than a flip.
    fn other(self) -> Family {
        match self {
            Family::Arch => Family::Debian,
            Family::Debian => Family::Arch,
        }
    }

    /// Refuse a `base_packages` list that was written for the other
    /// distribution, before handing it to a package manager that cannot.
    ///
    /// `base_packages` replaces the built-in list rather than adding to it,
    /// and it is spelled the same whichever guest is being built - but the
    /// names in it are not portable (`base-devel` is `build-essential`,
    /// `python-pip` is `python3-pip`, `fd` is `fd-find`). A list carried over
    /// from an Arch host, or uncommented from the wrong one of the two blocks
    /// in `config.example.toml`, therefore reaches `apt-get install` intact.
    ///
    /// Both package managers are atomic about it: one unresolvable name and
    /// *nothing* is installed, so the failure does not even look like a
    /// spelling problem - it looks like every package silently going missing,
    /// including the ones that were spelled correctly. Catching it here means
    /// the message can name the entries and say which list to start from.
    pub fn check_base_packages(self, packages: &[String]) -> Result<()> {
        let mine = self.default_base_packages();
        let theirs = self.other().default_base_packages();
        let foreign: Vec<&str> = packages
            .iter()
            .map(String::as_str)
            .filter(|name| theirs.contains(name) && !mine.contains(name))
            .collect();
        if foreign.is_empty() {
            return Ok(());
        }
        bail!(
            "`base_packages` in {} names {} package{} that only exist on {}: {}.\n\
             This host builds {} images, and {} installs nothing at all when one \
             name does not resolve - so every other package in the list would go \
             missing too, spelled correctly or not.\n\
             `base_packages` replaces the built-in list rather than adding to it, \
             and the names are not the same distribution to distribution. \
             config.example.toml carries one list per family: start from the {} \
             one, or drop the key to get it as the default.",
            config::global_path().display(),
            foreign.len(),
            if foreign.len() == 1 { "" } else { "s" },
            self.other().as_str(),
            foreign.join(", "),
            self.as_str(),
            match self {
                Family::Arch => "pacman",
                Family::Debian => "apt-get",
            },
            self.as_str(),
        )
    }

    /// The group a box's user is put in to get passwordless sudo.
    fn admin_group(self) -> &'static str {
        match self {
            Family::Arch => "wheel",
            Family::Debian => "sudo",
        }
    }
}

/// Wrap an apt script in the environment a non-interactive, init-less install
/// needs. `policy-rc.d` returning 101 is the documented way to tell
/// `invoke-rc.d` not to start anything; without it a maintainer script that
/// tries to start a service inside a chroot fails the install.
fn apt_cmd(body: &str, booted: bool) -> Vec<OsString> {
    let script = if booted {
        format!("set -eu\nexport DEBIAN_FRONTEND=noninteractive\n{body}")
    } else {
        format!(
            "set -eu\n\
             export DEBIAN_FRONTEND=noninteractive\n\
             printf '#!/bin/sh\\nexit 101\\n' > /usr/sbin/policy-rc.d\n\
             chmod 755 /usr/sbin/policy-rc.d\n\
             trap 'rm -f /usr/sbin/policy-rc.d' EXIT\n\
             {body}"
        )
    };
    argv!["/bin/sh", "-c", script]
}

fn shell_words(words: &[String]) -> String {
    words
        .iter()
        .map(|w| shell_quote(w))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// --------------------------------------------------------------------------
// os-release
// --------------------------------------------------------------------------

/// Parse an os-release file into its key/value pairs. Values may be bare or
/// quoted with either quote; anything else in the file (comments, blank lines,
/// continuations) is skipped rather than guessed at.
pub fn os_release(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }
        let value = value.trim();
        let value = match value.as_bytes() {
            [q @ (b'"' | b'\''), .., last] if last == q && value.len() >= 2 => {
                &value[1..value.len() - 1]
            }
            _ => value,
        };
        out.insert(key.to_string(), value.to_string());
    }
    out
}

fn read_os_release(dir: &Path) -> Option<BTreeMap<String, String>> {
    for path in ["etc/os-release", "usr/lib/os-release"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(path)) {
            let parsed = os_release(&text);
            if !parsed.is_empty() {
                return Some(parsed);
            }
        }
    }
    None
}

/// Which family an `ID=`/`ID_LIKE=` pair names. `ID` wins; `ID_LIKE` is the
/// fallback that makes derivatives (Manjaro, Linux Mint, Pop!_OS) work without
/// a list of every one of them.
pub fn family_from_ids(id: &str, id_like: &str) -> Option<Family> {
    if let Some(family) = Family::parse(id) {
        return Some(family);
    }
    id_like.split_whitespace().find_map(Family::parse)
}

fn family_of(release: &BTreeMap<String, String>) -> Option<Family> {
    let empty = String::new();
    family_from_ids(
        release.get("ID").unwrap_or(&empty),
        release.get("ID_LIKE").unwrap_or(&empty),
    )
}

/// Which package manager a box's rootfs is driven by, read from the rootfs
/// itself. This is the copy that is authoritative for a box built before
/// generations were stamped, and for one whose generation has since been
/// garbage-collected.
pub fn family_of_root(root: &Path) -> Result<Family> {
    let release = read_os_release(root).with_context(|| {
        format!(
            "cannot read os-release under {}, so agentbox cannot tell which \
             package manager this box uses",
            root.display()
        )
    })?;
    family_of(&release).with_context(|| {
        format!(
            "box rootfs {} is ID={:?}, which agentbox has no package manager for",
            root.display(),
            release.get("ID").cloned().unwrap_or_default()
        )
    })
}

fn host_release() -> BTreeMap<String, String> {
    read_os_release(Path::new("/")).unwrap_or_default()
}

// --------------------------------------------------------------------------
// what to build
// --------------------------------------------------------------------------

/// A resolved recipe for one base image: the family, plus the archive
/// coordinates a Debian-family bootstrap needs. Arch leaves the apt fields
/// empty; it reads the host's own mirrorlist instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guest {
    pub family: Family,
    /// os-release `ID` of the guest: `arch`, `debian` or `ubuntu`.
    pub id: String,
    pub suite: String,
    pub mirror: String,
    pub security_mirror: String,
    pub components: String,
}

impl Guest {
    pub fn describe(&self) -> String {
        match self.family {
            Family::Arch => self.id.clone(),
            Family::Debian => format!("{} {}", self.id, self.suite),
        }
    }

    pub fn admin_group(&self) -> &'static str {
        self.family.admin_group()
    }

    /// Stage 3: make the image a usable dev box for the invoking user. The
    /// sandbox user mirrors the host user so that `~` and project paths are
    /// spelled identically inside and outside.
    pub fn setup_script(&self, user_name: &str, uid: u32, gid: u32) -> String {
        let name = shell_quote(user_name);
        let group = self.admin_group();
        let distro_specific = match self.family {
            // Colour and parallel downloads for the package manager the box
            // will drive itself.
            Family::Arch => concat!(
                "sed -i 's/^#\\(Color\\)/\\1/;s/^#\\(ParallelDownloads.*\\)/\\1/' ",
                "/etc/pacman.conf\n",
                "printf 'LANG=en_US.UTF-8\\n' > /etc/locale.conf\n"
            )
            .to_string(),
            // /etc/default/locale is what Debian's own tooling reads;
            // /etc/locale.conf is what systemd reads. Write both, so a login
            // shell and systemd agree.
            //
            // The downloaded .debs go (nothing reads them again) but the
            // package lists stay: a box is meant to be able to
            // `sudo apt-get install` on its own the moment it boots, which is
            // exactly what the Arch image's synced pacman database gives it.
            //
            // Debian renames fd to fdfind to avoid a clash; the symlink is how
            // its own README tells you to get the name back, and /usr/local/bin
            // comes first in the PATH nspawn hands the payload.
            Family::Debian => concat!(
                "printf 'LANG=en_US.UTF-8\\n' > /etc/locale.conf\n",
                "printf 'LANG=en_US.UTF-8\\n' > /etc/default/locale\n",
                "rm -f /usr/sbin/policy-rc.d\n",
                "apt-get clean\n",
                "if [ -x /usr/bin/fdfind ]; then ln -sf /usr/bin/fdfind /usr/local/bin/fd; fi\n"
            )
            .to_string(),
        };
        format!(
            r#"set -euo pipefail
sed -i 's/^#[[:space:]]*\(en_US.UTF-8 UTF-8\)/\1/' /etc/locale.gen
locale-gen
{distro_specific}: > /etc/machine-id
printf 'agentbox\n' > /etc/hostname
groupadd -g {gid} -o {name} 2>/dev/null || true
useradd -m -u {uid} -g {gid} -G {group} -s /bin/bash {name} 2>/dev/null || true
passwd -d {name} >/dev/null 2>&1 || true
install -d -m 750 /etc/sudoers.d
printf '%%{group} ALL=(ALL:ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/00-agentbox
chmod 440 /etc/sudoers.d/00-agentbox
printf 'Defaults env_keep += "CLAUDE_CODE_OAUTH_TOKEN ANTHROPIC_API_KEY GITHUB_TOKEN"\n' > /etc/sudoers.d/10-agentbox-env
chmod 440 /etc/sudoers.d/10-agentbox-env
"#
        )
    }

    /// The deb822 archive list written into a freshly bootstrapped image, in
    /// place of the single-component one `debootstrap` leaves behind. Empty for
    /// Arch, which needs no equivalent - the image's `/etc/pacman.d/mirrorlist`
    /// comes from the package that provides it.
    pub fn sources_text(&self) -> String {
        let suite = &self.suite;
        // A rolling suite has no -updates or -security pocket; naming one
        // would make every `apt-get update` in every box fail.
        let rolling = matches!(suite.as_str(), "sid" | "unstable" | "rolling");
        let mut out = String::from("# generated by agentbox - rebuild the image to change it\n");
        // `Signed-By` names the keyring explicitly. Without it apt still works,
        // because the bootstrap installs the archive keyring into
        // /etc/apt/trusted.gpg.d, but it prints "Missing Signed-By in the
        // sources.list(5) entry" on every single `apt-get update` in every box.
        let keyring = match self.id.as_str() {
            "ubuntu" => "/usr/share/keyrings/ubuntu-archive-keyring.gpg",
            _ => "/usr/share/keyrings/debian-archive-keyring.gpg",
        };
        let mut stanza = |uris: &str, suites: String| {
            out.push_str(&format!(
                "\nTypes: deb\nURIs: {uris}\nSuites: {suites}\nComponents: {}\n\
                 Signed-By: {keyring}\n",
                self.components
            ));
        };
        if rolling {
            stanza(&self.mirror, suite.clone());
            return out;
        }
        if self.id == "ubuntu" {
            // Ubuntu's archive mirrors carry every pocket, including
            // -security, so one stanza covers all three.
            stanza(
                &self.mirror,
                format!("{suite} {suite}-updates {suite}-security"),
            );
        } else {
            stanza(&self.mirror, format!("{suite} {suite}-updates"));
            stanza(&self.security_mirror, format!("{suite}-security"));
        }
        out
    }
}

/// The recipe this host builds by default, with `[base]` in the global config
/// overriding any part of it.
pub fn guest() -> Result<Guest> {
    let base = config::base_table()?;
    let release = host_release();
    let host_id = release.get("ID").cloned().unwrap_or_default();

    let id = match base.get("distro") {
        Some(value) => {
            let id = value
                .as_str()
                .context("[base] distro must be a string: \"arch\", \"debian\" or \"ubuntu\"")?
                .trim()
                .to_string();
            if Family::parse(&id).is_none() {
                bail!("[base] distro must be \"arch\", \"debian\" or \"ubuntu\" (got {id:?})");
            }
            id
        }
        // No explicit choice: build what this host can bootstrap. `ID` is used
        // rather than the family so that an Ubuntu host builds Ubuntu (and
        // reads the Ubuntu keyring and pockets), not Debian.
        None => match family_of(&release) {
            Some(Family::Arch) => "arch".to_string(),
            Some(Family::Debian) if host_id == "debian" || host_id == "ubuntu" => host_id.clone(),
            // A Debian derivative that is neither: Mint and Pop!_OS have no
            // archive of their own that debootstrap knows, and their codename
            // is not an Ubuntu suite. Build the Ubuntu they are built from,
            // which the host's own sources name.
            Some(Family::Debian) => "ubuntu".to_string(),
            None => bail!(
                "cannot tell what base image to build on this host (os-release ID={host_id:?}). \
                 Set it explicitly in {}:\n  [base]\n  distro = \"debian\"   # or \"ubuntu\", \"arch\"",
                config::global_path().display()
            ),
        },
    };
    let family = Family::parse(&id).expect("validated above");

    let string = |key: &str| -> Result<Option<String>> {
        match base.get(key) {
            None => Ok(None),
            Some(value) => Ok(Some(
                value
                    .as_str()
                    .with_context(|| format!("[base] {key} must be a string"))?
                    .trim()
                    .to_string(),
            )),
        }
    };

    if family == Family::Arch {
        return Ok(Guest {
            family,
            id,
            suite: String::new(),
            mirror: String::new(),
            security_mirror: String::new(),
            components: String::new(),
        });
    }

    let suite = match string("suite")? {
        Some(suite) => suite,
        // The host's own codename, which is the one suite its debootstrap is
        // certain to have a script for and its mirror is certain to carry.
        None if id == host_id => release
            .get("VERSION_CODENAME")
            .cloned()
            .filter(|s| !s.is_empty())
            .with_context(|| {
                format!(
                    "this host's os-release has no VERSION_CODENAME, so agentbox cannot \
                     tell which {id} release to bootstrap. Name one in {}:\n  \
                     [base]\n  suite = \"trixie\"",
                    config::global_path().display()
                )
            })?,
        None if id == "debian" => "stable".to_string(),
        None => bail!(
            "building an {id} image on a {host_id} host needs the release named \
             explicitly in {}:\n  [base]\n  suite = \"noble\"",
            config::global_path().display()
        ),
    };
    check_token("[base] suite", &suite)?;

    let mirror = match string("mirror")? {
        Some(mirror) => mirror,
        None => host_apt_mirror(&id, &suite).unwrap_or_else(|| default_mirror(&id)),
    };
    check_url("[base] mirror", &mirror)?;

    let security_mirror = match string("security_mirror")? {
        Some(mirror) => mirror,
        // Debian's security archive is a separate tree beside the main one,
        // under whatever host serves it; Ubuntu's is a pocket of the main
        // archive and this is never used.
        None => match mirror.strip_suffix("/debian") {
            Some(prefix) => format!("{prefix}/debian-security"),
            None => "http://security.debian.org/debian-security".to_string(),
        },
    };
    check_url("[base] security_mirror", &security_mirror)?;

    let components = match string("components")? {
        Some(components) => components,
        None if id == "ubuntu" => "main restricted universe multiverse".to_string(),
        None => "main contrib non-free-firmware".to_string(),
    };
    if components.split_whitespace().next().is_none() {
        bail!("[base] components must name at least one component, e.g. \"main\"");
    }
    for component in components.split_whitespace() {
        check_token("[base] components", component)?;
    }

    Ok(Guest {
        family,
        id,
        suite,
        mirror,
        security_mirror,
        components,
    })
}

fn default_mirror(id: &str) -> String {
    match id {
        "ubuntu" => "http://archive.ubuntu.com/ubuntu".to_string(),
        _ => "http://deb.debian.org/debian".to_string(),
    }
}

/// A suite or component name, as it is written into the image's archive list
/// and passed to `debootstrap`. Kept to the characters those names actually
/// use: this value comes from a config file and ends up in a generated file
/// and on a command line, and `-`-leading would be read as an option.
fn check_token(what: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{what} must not be empty");
    }
    if value.starts_with('-') {
        bail!("{what} must not start with a dash (got {value:?})");
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
    {
        bail!("{what} may only contain letters, digits and - _ . / (got {value:?})");
    }
    Ok(())
}

/// A mirror URL. Same reasoning as `check_token`, plus a scheme check: this is
/// where every package in the image comes from, so a typo that silently turns
/// into a relative path is worth refusing.
fn check_url(what: &str, value: &str) -> Result<()> {
    let ok_scheme = ["http://", "https://", "ftp://", "file://"]
        .iter()
        .any(|scheme| value.starts_with(scheme));
    if !ok_scheme {
        bail!("{what} must be an http://, https://, ftp:// or file:// URL (got {value:?})");
    }
    if let Some(bad) = value
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || "\"'\\`$<>|;&".contains(*c))
    {
        bail!("{what} contains {bad:?}, which is not allowed in a mirror URL");
    }
    Ok(())
}

// --------------------------------------------------------------------------
// finding the host's own mirror
// --------------------------------------------------------------------------

/// The distribution's own archive files, and only those. A third-party repo can
/// carry the same suite and a `main` component - a launchpad PPA does exactly
/// that - so matching on content alone would happily bootstrap an image from
/// someone's PPA. Restricting the search to the file the distribution ships
/// keeps a wrong answer out of reach; `[base] mirror` covers everything else.
fn apt_source_files(id: &str) -> Vec<PathBuf> {
    vec![
        PathBuf::from("/etc/apt/sources.list"),
        PathBuf::from(format!("/etc/apt/sources.list.d/{id}.sources")),
        PathBuf::from(format!("/etc/apt/sources.list.d/{id}.list")),
        PathBuf::from("/etc/apt/sources.list.d/system.sources"),
    ]
}

/// The mirror this host installs from, so a build reuses whatever mirror (or
/// local cache) the user already picked instead of crossing the internet to the
/// default archive.
fn host_apt_mirror(id: &str, suite: &str) -> Option<String> {
    for path in apt_source_files(id) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let found = if path.extension().is_some_and(|e| e == "sources") {
            deb822_mirror(&text, suite)
        } else {
            one_line_mirror(&text, suite)
        };
        if let Some(mirror) = found {
            if check_url("mirror", &mirror).is_ok() {
                return Some(mirror);
            }
        }
    }
    None
}

/// The first URI of the first deb822 stanza that serves `suite`'s `main`.
fn deb822_mirror(text: &str, suite: &str) -> Option<String> {
    for stanza in text.split("\n\n") {
        let field = |name: &str| -> Option<String> {
            stanza
                .lines()
                .map(str::trim)
                .find(|line| {
                    line.len() > name.len()
                        && line[..name.len()].eq_ignore_ascii_case(name)
                        && line[name.len()..].starts_with(':')
                })
                .map(|line| line[name.len() + 1..].trim().to_string())
        };
        let types = field("Types").unwrap_or_else(|| "deb".to_string());
        // `Enabled: no` is how deb822 switches a stanza off without deleting
        // it, and a release upgrade leaves the old one lying there that way.
        // Reading it anyway picks a mirror the host has deliberately stopped
        // using - which is how an image ended up pulling from a kernel.org
        // mirror that `apt` on the host itself ignores.
        let enabled = field("Enabled").unwrap_or_else(|| "yes".to_string());
        if matches!(
            enabled.trim().to_ascii_lowercase().as_str(),
            "no" | "false" | "off"
        ) {
            continue;
        }
        let (Some(uris), Some(suites), Some(components)) =
            (field("URIs"), field("Suites"), field("Components"))
        else {
            continue;
        };
        if !types.split_whitespace().any(|t| t == "deb")
            || !suites.split_whitespace().any(|s| s == suite)
            || !components.split_whitespace().any(|c| c == "main")
        {
            continue;
        }
        if let Some(uri) = uris.split_whitespace().next() {
            return Some(uri.trim_end_matches('/').to_string());
        }
    }
    None
}

/// The same, from the one-line `deb [options] URI suite components...` format.
fn one_line_mirror(text: &str, suite: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some(rest) = line.strip_prefix("deb ") else {
            continue;
        };
        // Options are a bracketed group that may contain spaces.
        let rest = match rest.trim().strip_prefix('[') {
            Some(after) => match after.split_once(']') {
                Some((_, after)) => after,
                None => continue,
            },
            None => rest,
        };
        let mut fields = rest.split_whitespace();
        let (Some(uri), Some(found)) = (fields.next(), fields.next()) else {
            continue;
        };
        if found == suite && fields.any(|c| c == "main") {
            return Some(uri.trim_end_matches('/').to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn debian(id: &str, suite: &str) -> Guest {
        Guest {
            family: Family::Debian,
            id: id.to_string(),
            suite: suite.to_string(),
            mirror: "http://mirror/ubuntu".to_string(),
            security_mirror: "http://security/debian-security".to_string(),
            components: "main".to_string(),
        }
    }

    #[test]
    fn os_release_values_may_be_quoted_or_bare() {
        let parsed = os_release(
            "# a comment\nID=ubuntu\nID_LIKE=\"debian\"\nVERSION_CODENAME='noble'\nbroken\n",
        );
        assert_eq!(parsed.get("ID").unwrap(), "ubuntu");
        assert_eq!(parsed.get("ID_LIKE").unwrap(), "debian");
        assert_eq!(parsed.get("VERSION_CODENAME").unwrap(), "noble");
        assert_eq!(parsed.get("broken"), None);
    }

    #[test]
    fn a_value_containing_an_equals_sign_keeps_it() {
        let parsed = os_release("HOME_URL=https://example.invalid/?a=b\n");
        assert_eq!(
            parsed.get("HOME_URL").unwrap(),
            "https://example.invalid/?a=b"
        );
    }

    #[test]
    fn derivatives_are_recognised_through_id_like() {
        assert_eq!(family_from_ids("ubuntu", ""), Some(Family::Debian));
        assert_eq!(family_from_ids("debian", ""), Some(Family::Debian));
        assert_eq!(family_from_ids("arch", ""), Some(Family::Arch));
        assert_eq!(
            family_from_ids("linuxmint", "ubuntu debian"),
            Some(Family::Debian)
        );
        assert_eq!(family_from_ids("manjaro", "arch"), Some(Family::Arch));
        assert_eq!(family_from_ids("fedora", ""), None);
    }

    /// `ID` is the answer even when `ID_LIKE` names another family, since a
    /// distribution knows its own package manager better than its lineage does.
    #[test]
    fn id_wins_over_id_like() {
        assert_eq!(family_from_ids("debian", "arch"), Some(Family::Debian));
    }

    #[test]
    fn ubuntu_serves_every_pocket_from_one_mirror() {
        let text = debian("ubuntu", "noble").sources_text();
        assert!(text.contains("Suites: noble noble-updates noble-security\n"));
        assert_eq!(text.matches("URIs:").count(), 1);
    }

    /// Debian's security archive is a separate tree, so it needs a stanza of
    /// its own or `apt-get update` simply never sees a security update.
    #[test]
    fn debian_gets_a_second_stanza_for_security() {
        let text = debian("debian", "trixie").sources_text();
        assert!(text.contains("URIs: http://mirror/ubuntu\nSuites: trixie trixie-updates\n"));
        assert!(text.contains("URIs: http://security/debian-security\nSuites: trixie-security\n"));
    }

    /// sid has no -updates or -security pocket; naming one would make every
    /// `apt-get update` inside every box fail.
    #[test]
    fn a_rolling_suite_gets_one_pocket_only() {
        let text = debian("debian", "sid").sources_text();
        assert!(text.contains("Suites: sid\n"));
        assert!(!text.contains("sid-"));
    }

    #[test]
    fn the_admin_group_follows_the_family() {
        assert_eq!(debian("ubuntu", "noble").admin_group(), "sudo");
        assert_eq!(
            Guest {
                family: Family::Arch,
                id: "arch".into(),
                suite: String::new(),
                mirror: String::new(),
                security_mirror: String::new(),
                components: String::new(),
            }
            .admin_group(),
            "wheel"
        );
    }

    /// The sudoers line and the useradd line must name the same group, or the
    /// box's user has no sudo at all.
    #[test]
    fn the_setup_script_grants_sudo_to_the_group_it_adds_the_user_to() {
        let script = debian("ubuntu", "noble").setup_script("me", 1000, 1000);
        assert!(script.contains("-G sudo -s /bin/bash 'me'"));
        assert!(script.contains("%sudo ALL=(ALL:ALL) NOPASSWD: ALL"));
    }

    #[test]
    fn a_user_name_with_a_quote_cannot_escape_the_setup_script() {
        let script = debian("ubuntu", "noble").setup_script("o'brien", 1000, 1000);
        assert!(script.contains(r"'o'\''brien'"));
    }

    #[test]
    fn the_debian_build_removes_the_policy_block_it_installed() {
        let script = debian("ubuntu", "noble").setup_script("me", 1000, 1000);
        assert!(script.contains("rm -f /usr/sbin/policy-rc.d"));
    }

    #[test]
    fn deb822_finds_the_mirror_for_the_wanted_suite() {
        let text = "Types: deb\n\
                    URIs: http://mirrors.example/ubuntu\n\
                    Suites: noble noble-updates noble-backports\n\
                    Components: main restricted universe multiverse\n\
                    \n\
                    Types: deb\n\
                    URIs: http://security.example/ubuntu\n\
                    Suites: noble-security\n\
                    Components: main\n";
        assert_eq!(
            deb822_mirror(text, "noble").as_deref(),
            Some("http://mirrors.example/ubuntu")
        );
        assert_eq!(deb822_mirror(text, "jammy"), None);
    }

    /// A release upgrade leaves the previous release's stanza in place with
    /// `Enabled: no`. Reading it anyway builds the image from a mirror the
    /// host itself has stopped using.
    #[test]
    fn a_disabled_stanza_is_not_a_mirror() {
        let text = "Types: deb\n\
                    URIs: http://old.example/ubuntu\n\
                    Suites: noble noble-updates\n\
                    Components: main restricted\n\
                    Enabled: no\n\
                    \n\
                    Types: deb\n\
                    URIs: http://live.example/ubuntu\n\
                    Suites: noble noble-updates\n\
                    Components: main restricted\n";
        assert_eq!(
            deb822_mirror(text, "noble").as_deref(),
            Some("http://live.example/ubuntu")
        );
    }

    /// Only the spellings deb822 actually uses for off; anything else, and a
    /// stanza with no `Enabled` field at all, stays enabled.
    #[test]
    fn enabled_defaults_to_yes() {
        let stanza = |extra: &str| {
            format!(
                "Types: deb\nURIs: http://m.example/ubuntu\nSuites: noble\n\
                 Components: main\n{extra}"
            )
        };
        assert!(deb822_mirror(&stanza(""), "noble").is_some());
        assert!(deb822_mirror(&stanza("Enabled: yes\n"), "noble").is_some());
        assert!(deb822_mirror(&stanza("Enabled: no\n"), "noble").is_none());
        assert!(deb822_mirror(&stanza("Enabled: NO\n"), "noble").is_none());
    }

    /// Every generated stanza names its keyring, or apt complains on each
    /// `apt-get update` inside every box.
    #[test]
    fn generated_sources_name_the_keyring() {
        let text = debian("ubuntu", "noble").sources_text();
        assert_eq!(
            text.matches("Signed-By:").count(),
            text.matches("URIs:").count()
        );
        assert!(text.contains("ubuntu-archive-keyring.gpg"));
        let text = debian("debian", "trixie").sources_text();
        assert!(text.contains("debian-archive-keyring.gpg"));
    }

    /// A stanza that does not carry `main` is not the distribution archive,
    /// whatever else it claims.
    #[test]
    fn a_stanza_without_main_is_not_a_mirror() {
        let text = "Types: deb\nURIs: http://ppa.example/x\nSuites: noble\nComponents: extra\n";
        assert_eq!(deb822_mirror(text, "noble"), None);
    }

    #[test]
    fn one_line_sources_are_parsed_with_and_without_options() {
        let text = "# comment\n\
                    deb http://plain.example/debian trixie main contrib\n";
        assert_eq!(
            one_line_mirror(text, "trixie").as_deref(),
            Some("http://plain.example/debian")
        );
        let text = "deb [arch=amd64 signed-by=/k.gpg] http://opt.example/debian trixie main\n";
        assert_eq!(
            one_line_mirror(text, "trixie").as_deref(),
            Some("http://opt.example/debian")
        );
    }

    #[test]
    fn a_trailing_slash_is_dropped_so_paths_do_not_double_up() {
        let text = "deb http://x.example/debian/ trixie main\n";
        assert_eq!(
            one_line_mirror(text, "trixie").as_deref(),
            Some("http://x.example/debian")
        );
    }

    #[test]
    fn deb_src_lines_are_not_mirrors() {
        let text = "deb-src http://x.example/debian trixie main\n";
        assert_eq!(one_line_mirror(text, "trixie"), None);
    }

    #[test]
    fn a_mirror_must_be_an_absolute_url_without_shell_metacharacters() {
        assert!(check_url("m", "http://ok.example/ubuntu").is_ok());
        assert!(check_url("m", "mirrors.example/ubuntu").is_err());
        assert!(check_url("m", "http://x.example/a b").is_err());
        assert!(check_url("m", "http://x.example/$(id)").is_err());
        assert!(check_url("m", "http://x.example/a\nb").is_err());
    }

    #[test]
    fn a_suite_may_not_be_an_option_or_carry_metacharacters() {
        assert!(check_token("s", "noble").is_ok());
        assert!(check_token("s", "--debug").is_err());
        assert!(check_token("s", "a b").is_err());
        assert!(check_token("s", "").is_err());
    }

    /// The two package lists are what a family's default image is; a family
    /// that returned the other's list would fail every build with "target not
    /// found".
    #[test]
    fn each_family_gets_its_own_package_list() {
        assert!(Family::Arch.default_base_packages().contains(&"base-devel"));
        assert!(Family::Debian
            .default_base_packages()
            .contains(&"build-essential"));
    }

    /// The realistic mistake: a config carried from an Arch host, or
    /// uncommented from the wrong block of config.example.toml, reaching a
    /// Debian build. apt installs nothing when one name fails, so the whole
    /// list goes missing - worth refusing before that rather than after.
    #[test]
    fn a_base_packages_list_for_the_other_family_is_refused() {
        let arch: Vec<String> = DEFAULT_BASE_PACKAGES
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = Family::Debian
            .check_base_packages(&arch)
            .expect_err("an Arch list must not reach apt-get")
            .to_string();
        assert!(err.contains("base-devel"), "{err}");
        assert!(err.contains("only exist on arch"), "{err}");
        // ...and the mirror image, so neither direction is special-cased.
        let debian: Vec<String> = DEFAULT_BASE_PACKAGES_DEBIAN
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(Family::Arch.check_base_packages(&debian).is_err());
    }

    #[test]
    fn each_familys_own_list_passes_its_own_check() {
        for family in [Family::Arch, Family::Debian] {
            let own: Vec<String> = family
                .default_base_packages()
                .iter()
                .map(|s| s.to_string())
                .collect();
            assert!(
                family.check_base_packages(&own).is_ok(),
                "{} rejected its own built-in list",
                family.as_str()
            );
        }
    }

    /// Only names that are *exclusive* to the other family count. Plenty are
    /// spelled identically in both archives, and flagging those would refuse
    /// every hand-written list that happens to mention git.
    #[test]
    fn names_both_families_share_are_not_foreign() {
        let shared = ["sudo", "git", "curl", "jq", "tmux", "fish"]
            .map(String::from)
            .to_vec();
        for family in [Family::Arch, Family::Debian] {
            assert!(family.check_base_packages(&shared).is_ok());
        }
    }

    /// A list of names neither built-in mentions is the user's business - the
    /// check is for the one mistake it can prove, not a whitelist.
    #[test]
    fn unrecognised_names_are_left_alone() {
        let theirs = ["cowsay", "some-internal-tool"].map(String::from).to_vec();
        assert!(Family::Debian.check_base_packages(&theirs).is_ok());
    }

    /// The argv as a single plain string. `{:?}` on an `OsString` escapes the
    /// quotes that the shell-quoting under test puts there.
    fn rendered(argv: &[OsString]) -> String {
        argv.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn a_build_install_blocks_service_starts_but_a_booted_one_does_not() {
        let pkgs = vec!["cowsay".to_string()];
        let build = rendered(&Family::Debian.install_cmd(&pkgs, false));
        let booted = rendered(&Family::Debian.install_cmd(&pkgs, true));
        assert!(build.contains("policy-rc.d"));
        assert!(!booted.contains("policy-rc.d"));
        assert!(booted.contains("DEBIAN_FRONTEND=noninteractive"));
    }

    #[test]
    fn a_package_name_cannot_break_out_of_the_apt_command() {
        let pkgs = vec!["cowsay; rm -rf /".to_string()];
        let cmd = rendered(&Family::Debian.install_cmd(&pkgs, true));
        assert!(cmd.contains(r"'cowsay; rm -rf /'"));
    }
}
