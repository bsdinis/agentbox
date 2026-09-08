//! A sandbox: the naming, paths, mounts and environment of one project's box.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::{expand, Config, UID_RANGE};
use crate::host::{env_var, host, User};

pub const STATE: &str = "/var/lib/agentbox";
pub const MACHINES: &str = "/var/lib/machines";
pub const NSPAWN_DIR: &str = "/etc/systemd/nspawn";

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
        let stem = sanitize(
            cfg.name
                .clone()
                .unwrap_or_else(|| project.file_name().unwrap_or_default().to_string_lossy().into_owned()),
        );
        // The path hash keeps two projects with the same directory name apart.
        let digest = Sha256::digest(project.as_os_str().as_encoded_bytes());
        let mut name = format!("{stem}-{:x}", digest);
        name.truncate(stem.len() + 7);
        Sandbox { name, project, cfg, user: host().user.clone() }
    }

    pub fn dir(&self) -> PathBuf {
        Path::new(STATE).join("boxes").join(&self.name)
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
        self.cfg.hostname.clone().unwrap_or_else(|| self.name.clone())
    }

    /// Host UID corresponding to a UID inside the container.
    pub fn shift(&self, uid: u32) -> u32 {
        self.cfg.uid_base + uid
    }

    /// A path inside the container, as seen from the host.
    pub fn inside(&self, path: &Path) -> PathBuf {
        self.root().join(path.strip_prefix("/").unwrap_or(path))
    }

    /// The project directory is always read-write; everything else is opt-in.
    /// Sources that do not exist are skipped with a warning rather than
    /// failing, so a config can name optional dotfiles.
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
                binds.push(Bind { read_only, src, dst });
            }
        }

        if self.cfg.ssh_agent {
            match env_var("SSH_AUTH_SOCK").map(PathBuf::from) {
                Some(sock) if sock.exists() => binds.push(Bind {
                    read_only: false,
                    src: sock,
                    dst: PathBuf::from("/run/ssh-agent.sock"),
                }),
                _ => crate::warn("ssh_agent requested but SSH_AUTH_SOCK is unset; skipping"),
            }
        }
        binds
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
            env.insert("SSH_AUTH_SOCK".into(), "/run/ssh-agent.sock".into());
        }
        env.entry("AGENTBOX".into()).or_insert_with(|| self.name.clone());
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

fn sanitize(name: String) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
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
        assert_eq!(split_spec(r"/wei\:rd:/dst"), ("/wei:rd".into(), Some("/dst".into())));
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
}
