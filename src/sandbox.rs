//! A sandbox: the naming, paths, mounts and environment of one project's box.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::config::{expand, Config, UID_RANGE};
use crate::host::{env_var, host, User};

/// Default root for the base image and per-box overlays.
pub const STATE: &str = "/var/lib/agentbox";
pub const MACHINES: &str = "/var/lib/machines";
pub const NSPAWN_DIR: &str = "/etc/systemd/nspawn";
pub const UNIT_DIR: &str = "/etc/systemd/system";

/// Where the base image and the boxes live.
///
/// `AGENTBOX_STATE` relocates it so a test can build a throwaway image without
/// touching the real one. It is read from *this* process's environment rather
/// than the caller's handover, so it is stripped by `sudo`'s env_reset on the
/// way to root and cannot be used to aim a privileged agentbox somewhere new.
/// Tests reach it with `sudo env AGENTBOX_STATE=... agentbox build`.
pub fn state_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os("AGENTBOX_STATE")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from(STATE))
    })
}

/// One host directory made visible inside the box.
#[derive(Debug, Clone)]
pub struct Bind {
    pub read_only: bool,
    pub src: PathBuf,
    pub dst: PathBuf,
}

impl Bind {
    pub fn kind(&self) -> &'static str {
        if self.read_only {
            "ro"
        } else {
            "rw"
        }
    }
}

#[derive(Debug)]
pub struct Sandbox {
    pub name: String,
    pub project: PathBuf,
    pub cfg: Config,
    pub user: User,
}

impl Sandbox {
    pub fn new(project: PathBuf, cfg: Config) -> Self {
        let stem = sanitize(cfg.name.clone().unwrap_or_else(|| {
            project
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }));
        // The path hash keeps two projects with the same directory name apart.
        let digest = Sha256::digest(project.as_os_str().as_encoded_bytes());
        let mut name = format!("{stem}-{:x}", digest);
        name.truncate(stem.len() + 7);
        Sandbox {
            name,
            project,
            cfg,
            user: host().user.clone(),
        }
    }

    pub fn dir(&self) -> PathBuf {
        state_dir().join("boxes").join(&self.name)
    }

    /// Everything the container has ever written.
    pub fn upper(&self) -> PathBuf {
        self.dir().join("upper")
    }

    /// overlayfs scratch area.
    pub fn work(&self) -> PathBuf {
        self.dir().join("work")
    }

    pub fn meta(&self) -> PathBuf {
        self.dir().join("meta.json")
    }

    /// The assembled rootfs, under /var/lib/machines so machinectl finds it.
    pub fn root(&self) -> PathBuf {
        Path::new(MACHINES).join(&self.name)
    }

    pub fn settings(&self) -> PathBuf {
        Path::new(NSPAWN_DIR).join(format!("{}.nspawn", self.name))
    }

    pub fn service(&self) -> String {
        format!("systemd-nspawn@{}.service", self.name)
    }

    pub fn hostname(&self) -> String {
        self.cfg
            .hostname
            .clone()
            .unwrap_or_else(|| self.name.clone())
    }

    /// Host UID corresponding to a UID inside the container.
    pub fn shift(&self, uid: u32) -> u32 {
        self.cfg.uid_base + uid
    }

    /// A path inside the container, as seen from the host.
    ///
    /// The join is not normalized, so a `path` carrying `..` components would
    /// escape `root()`. Callers must only pass fixed internal paths or a bind
    /// destination already validated by `nspawn::check_supported`; this assert
    /// catches a regression that routed an un-normalized path here.
    pub fn inside(&self, path: &Path) -> PathBuf {
        debug_assert!(
            !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "inside() called with a `..` path ({}); it would escape the box rootfs",
            path.display()
        );
        self.root().join(path.strip_prefix("/").unwrap_or(path))
    }

    /// The project directory is always read-write; everything else is opt-in.
    /// Sources that do not exist are skipped with a warning rather than
    /// failing, so a config can name optional dotfiles.
    ///
    /// The order here - project, then rw, then ro - decides only which entry
    /// wins a *duplicate destination*, since `claimed` matches exact paths and
    /// keeps the first. It does not decide which mount ends up on top: nspawn
    /// sorts its custom mounts by destination before mounting any of them, so a
    /// nested pair is always applied parent first, whatever order it is written
    /// in. See docs/configuration.md, "Nesting: one mount inside another".
    pub fn binds(&self) -> Vec<Bind> {
        let mut binds = vec![Bind {
            read_only: false,
            src: self.project.clone(),
            dst: self.project.clone(),
        }];
        let mut claimed = vec![self.project.clone()];

        for (read_only, specs) in [(false, &self.cfg.rw), (true, &self.cfg.ro)] {
            for spec in specs {
                let (src_spec, dst_spec) = split_spec(spec);
                let src = expand(&src_spec);
                let src = src.canonicalize().unwrap_or(src);
                let dst = dst_spec.map(|d| expand(&d)).unwrap_or_else(|| src.clone());
                if !src.exists() {
                    crate::warn(&format!(
                        "skipping {} map {} (does not exist)",
                        if read_only { "ro" } else { "rw" },
                        src.display()
                    ));
                    continue;
                }
                if claimed.contains(&dst) {
                    continue;
                }
                claimed.push(dst.clone());
                binds.push(Bind {
                    read_only,
                    src,
                    dst,
                });
            }
        }

        if self.cfg.ssh_agent {
            match env_var("SSH_AUTH_SOCK").map(PathBuf::from) {
                Some(sock) if sock.exists() => binds.push(Bind {
                    read_only: false,
                    src: sock,
                    dst: self.ssh_agent_dst(),
                }),
                _ => crate::warn("ssh_agent requested but SSH_AUTH_SOCK is unset; skipping"),
            }
        }
        binds
    }

    /// Where a forwarded agent socket appears inside the box.
    ///
    /// Under the sandbox user's home rather than /run: nspawn mounts its own
    /// tmpfs on /run, which would hide the prepared mount point and leave the
    /// socket owned by container root, unusable by the sandbox user. See
    /// `nspawn::NSPAWN_OWNED`.
    fn ssh_agent_dst(&self) -> PathBuf {
        self.user.home.join(".agentbox/ssh-agent.sock")
    }

    /// Variables set inside the box: forwarded host ones, then explicit ones.
    pub fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for name in &self.cfg.pass_env {
            if let Some(value) = env_var(name) {
                env.insert(name.clone(), value);
            }
        }
        env.extend(self.cfg.env.clone());
        if self.cfg.ssh_agent {
            let sock = self.ssh_agent_dst();
            env.insert("SSH_AUTH_SOCK".into(), sock.display().to_string());
        }
        env.entry("AGENTBOX".into())
            .or_insert_with(|| self.name.clone());
        env
    }

    /// The login shell inside the box: configured, else the host shell if the
    /// image has it, else bash.
    pub fn shell(&self) -> String {
        if let Some(shell) = &self.cfg.shell {
            return shell.clone();
        }
        let host_shell = &host().user.shell;
        if !host_shell.as_os_str().is_empty() && self.inside(host_shell).exists() {
            return host_shell.display().to_string();
        }
        "/bin/bash".into()
    }

    pub fn uid_range(&self) -> (u32, u32) {
        (self.cfg.uid_base, self.cfg.uid_base + UID_RANGE - 1)
    }
}

/// `"src"` or `"src:dst"`, where `\:` is a literal colon rather than the
/// separator.
fn split_spec(spec: &str) -> (String, Option<String>) {
    let bytes = spec.as_bytes();
    let unescape = |s: &str| s.replace("\\:", ":");
    for (i, b) in bytes.iter().enumerate() {
        if *b == b':' && (i == 0 || bytes[i - 1] != b'\\') {
            return (unescape(&spec[..i]), Some(unescape(&spec[i + 1..])));
        }
    }
    (unescape(spec), None)
}

/// Whether `src` is too broad to bind into a box, and a short human reason.
///
/// Binds are owneridmap-mapped, so their source is writable inside the box as
/// the host user. A source that is the filesystem root, the host home itself,
/// or any ancestor of the home (`/home`, `/`, ...) exposes far more than the
/// project and is refused. The agentbox state directory - and its subtree and
/// ancestors - is likewise off limits, since a box could otherwise tamper with
/// the base image and sibling boxes. Ordinary subdirectories (a reference repo
/// under the home, a registry cache) are `None` and mount normally.
pub(crate) fn overbroad_reason(src: &Path, home: &Path, state: &Path) -> Option<&'static str> {
    if src == Path::new("/") {
        return Some("the filesystem root");
    }
    // `home.starts_with(src)` is true exactly when src == home or src is an
    // ancestor of home; a subdirectory of home does not match.
    if home.starts_with(src) {
        return Some("the host home directory, or an ancestor of it");
    }
    if src.starts_with(state) || state.starts_with(src) {
        return Some("the agentbox state directory, or an ancestor of it");
    }
    None
}

fn sanitize(name: String) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed: Vec<&str> = cleaned.split('-').filter(|s| !s.is_empty()).collect();
    let joined = trimmed.join("-");
    if joined.is_empty() {
        "box".into()
    } else {
        joined.chars().take(48).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_split_on_the_first_unescaped_colon() {
        assert_eq!(split_spec("/a/b"), ("/a/b".into(), None));
        assert_eq!(split_spec("/a:/b"), ("/a".into(), Some("/b".into())));
        assert_eq!(split_spec("/a:/b:/c"), ("/a".into(), Some("/b:/c".into())));
        assert_eq!(split_spec(r"/wei\:rd"), (r"/wei:rd".into(), None));
        assert_eq!(
            split_spec(r"/wei\:rd:/dst"),
            ("/wei:rd".into(), Some("/dst".into()))
        );
    }

    #[test]
    fn names_are_valid_hostnames() {
        assert_eq!(sanitize("MyProject".into()), "myproject");
        assert_eq!(sanitize("my_project.v2".into()), "my-project-v2");
        assert_eq!(sanitize("--weird--".into()), "weird");
        assert_eq!(sanitize("".into()), "box");
        assert_eq!(sanitize("!!!".into()), "box");
        assert!(sanitize("x".repeat(200)).len() <= 48);
    }

    #[test]
    fn overbroad_sources_are_refused() {
        let home = Path::new("/home/alice");
        let state = Path::new("/var/lib/agentbox");
        // The whole host, via the root - the rw=["/:/x"] vector.
        assert!(overbroad_reason(Path::new("/"), home, state).is_some());
        // The host home itself - the rw=["~:/x"] vector.
        assert!(overbroad_reason(home, home, state).is_some());
        // An ancestor of the home still exposes the home.
        assert!(overbroad_reason(Path::new("/home"), home, state).is_some());
        // The state directory (and its ancestors) protect the base image and
        // sibling boxes.
        assert!(overbroad_reason(state, home, state).is_some());
        assert!(overbroad_reason(Path::new("/var/lib/agentbox/boxes"), home, state).is_some());
        assert!(overbroad_reason(Path::new("/var/lib"), home, state).is_some());
    }

    #[test]
    fn ordinary_sources_still_mount() {
        let home = Path::new("/home/alice");
        let state = Path::new("/var/lib/agentbox");
        // A reference repo under the home, but not the home itself.
        assert!(overbroad_reason(Path::new("/home/alice/src/refrepo"), home, state).is_none());
        // A sibling of the home is unrelated to it.
        assert!(overbroad_reason(Path::new("/home/bob"), home, state).is_none());
        // An ordinary registry/cache mount elsewhere on the host.
        assert!(overbroad_reason(Path::new("/srv/registry"), home, state).is_none());
    }
}
