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
    /// An agentbox-generated bind whose source it controls (the box-scoped
    /// ssh-agent socket), exempt from the overbroad-source check that guards
    /// user-configured maps. Never set from configuration.
    pub internal: bool,
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

/// One host path copied into the box once, the first time its destination is
/// absent from the box's own overlay - never a live mount. See
/// `Sandbox::copies()` and `nspawn`'s `perform_copies`.
#[derive(Debug, Clone)]
pub struct Copy {
    pub src: PathBuf,
    pub dst: PathBuf,
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
            internal: false,
        }];
        let mut claimed = vec![self.project.clone()];

        for (read_only, specs) in [(false, &self.cfg.rw), (true, &self.cfg.ro)] {
            for spec in specs {
                let (src_spec, dst_spec) = split_spec(spec);
                let requested = expand(&src_spec);
                let src = requested
                    .canonicalize()
                    .unwrap_or_else(|_| requested.clone());
                let dst = dst_spec
                    .map(|d| expand(&d))
                    .unwrap_or_else(|| default_dst(&requested));
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
                    internal: false,
                });
            }
        }

        // The only sanctioned forwarding path: a relay in front of the box's
        // own dedicated agent, holding just the configured keys. Never the
        // host's `$SSH_AUTH_SOCK`, which would expose every key it holds, and
        // never the agent's own socket directly - see `nspawn::spawn_ssh_agent`
        // for why the relay exists. Both sockets are created by
        // `nspawn::spawn_ssh_agent` before the mount points are prepared; the
        // bind is marked `internal` so `check_supported` does not refuse its
        // source for living under the agentbox state directory.
        if !self.cfg.ssh_keys.is_empty() {
            binds.push(Bind {
                read_only: false,
                src: self.scoped_agent_relay_sock(),
                dst: self.ssh_agent_dst(),
                internal: true,
            });
        }
        binds
    }

    /// One-time copy-if-absent sources: `cpy` entries, in the same
    /// `"src"`/`"src:dst"` syntax as `rw`/`ro` (reusing `split_spec` and
    /// `expand` unchanged). Unlike `binds()`, these are never a live mount -
    /// `nspawn::perform_copies` applies each one with a one-shot `cp -a` into
    /// the box's own overlay, once, the first time its destination is absent.
    ///
    /// A source that does not exist is skipped with a warning rather than
    /// failing, mirroring `binds()` - the current default; a reviewer may
    /// prefer to fail closed here instead.
    ///
    /// A destination already claimed by the project directory or a `rw`/`ro`
    /// bind (or by an earlier `cpy` entry) is skipped with a warning too:
    /// nspawn mounts binds over their destinations only after every mount
    /// point is prepared, so a `cpy` landing under one would become a stray,
    /// invisible inode once the box boots - the same gotcha `prepare_mount_point`'s
    /// doc comment already describes for a bind nested inside another bind.
    ///
    /// The overbroad-source guard (`overbroad_reason`) is applied to these
    /// sources by `nspawn::check_supported`, at the same point it checks
    /// `rw`/`ro` sources, reusing the bind-oriented rationale as a
    /// conservative starting default pending a considered cpy-specific policy.
    pub fn copies(&self) -> Vec<Copy> {
        let mut claimed: Vec<PathBuf> = self.binds().into_iter().map(|b| b.dst).collect();
        let mut copies = vec![];
        for spec in &self.cfg.cpy {
            let (src_spec, dst_spec) = split_spec(spec);
            let requested = expand(&src_spec);
            let src = requested
                .canonicalize()
                .unwrap_or_else(|_| requested.clone());
            let dst = dst_spec
                .map(|d| expand(&d))
                .unwrap_or_else(|| default_dst(&requested));
            if !src.exists() {
                crate::warn(&format!(
                    "skipping cpy map {} (does not exist)",
                    src.display()
                ));
                continue;
            }
            if claimed.contains(&dst) {
                crate::warn(&format!(
                    "skipping cpy map {} -> {} (destination is already claimed by a \
                     bind mount or another cpy entry)",
                    src.display(),
                    dst.display()
                ));
                continue;
            }
            claimed.push(dst.clone());
            copies.push(Copy { src, dst });
        }
        copies
    }

    /// The box-scoped ssh-agent's private state: its socket, its relay's
    /// socket, and both their pid files. Kept under the box's own state
    /// directory (mode 0700, owned by the invoking user) rather than a
    /// world-readable place, since the sockets grant use of the configured
    /// keys.
    pub fn agent_dir(&self) -> PathBuf {
        self.dir().join("ssh-agent")
    }

    pub fn scoped_agent_sock(&self) -> PathBuf {
        self.agent_dir().join("agent.sock")
    }

    pub fn agent_pidfile(&self) -> PathBuf {
        self.agent_dir().join("agent.pid")
    }

    /// The relay's own socket - what actually gets bound into the box. See
    /// `nspawn::spawn_ssh_agent` for why a direct bind of `scoped_agent_sock`
    /// does not work.
    pub fn scoped_agent_relay_sock(&self) -> PathBuf {
        self.agent_dir().join("agent-relay.sock")
    }

    pub fn agent_relay_pidfile(&self) -> PathBuf {
        self.agent_dir().join("relay.pid")
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
        if !self.cfg.ssh_keys.is_empty() {
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

/// Where a one-sided `rw`/`ro`/`cpy` spec lands inside the box: the path as
/// written, with `.`/`..` resolved textually but symlinks left alone.
///
/// The source is canonicalized - nspawn needs a real path to bind, and the
/// `owneridmap` and overbroad-source guards want the real one too - but the
/// *destination* must not inherit that. agentbox's premise is that a path is
/// spelled the same inside and outside, and canonicalizing the destination
/// silently breaks it whenever the source is a symlink:
/// `ro = ["~/.local/bin/claude"]`, where `claude` is a symlink into
/// `~/.local/share/claude/versions/<v>`, mounted the binary at the *target's*
/// path and left `~/.local/bin` missing from the box entirely - so the tool
/// the user asked for was simply not where they asked for it, and nothing
/// warned.
///
/// `..` still has to go, because `unsafe_dst` refuses a destination that is
/// not lexically normalized - that guard is what stops a `dst` steering the
/// privileged mount-point create/chown at an arbitrary host path (V3).
/// Resolving it textually keeps `~/foo/../bar` working without consulting the
/// filesystem. A `..` that would climb past the root is handed on unchanged
/// for `unsafe_dst` to refuse, rather than silently swallowed here.
fn default_dst(requested: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in requested.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    return requested.to_path_buf();
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
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

    fn sandbox_with(ssh_keys: Vec<String>) -> Sandbox {
        // host() is needed for Sandbox::new (the invoking user) and for env().
        let _ = crate::host::init(None, false);
        let cfg = Config {
            ssh_keys,
            ..Config::default()
        };
        Sandbox::new(PathBuf::from("/home/me/project"), cfg)
    }

    #[test]
    fn no_keys_forwards_no_socket_and_sets_no_auth_sock() {
        let sb = sandbox_with(vec![]);
        // The host's own agent is never bound in; with no keys there is no
        // agent bind at all.
        assert!(sb.binds().iter().all(|b| b.dst != sb.ssh_agent_dst()));
        assert!(!sb.env().contains_key("SSH_AUTH_SOCK"));
    }

    #[test]
    fn configured_keys_forward_only_the_scoped_agent_socket() {
        let sb = sandbox_with(vec!["~/.ssh/id_ed25519_projectx".into()]);
        let binds = sb.binds();
        let agent: Vec<&Bind> = binds
            .iter()
            .filter(|b| b.dst == sb.ssh_agent_dst())
            .collect();
        assert_eq!(agent.len(), 1);
        let agent = agent[0];
        // The source is the relay's socket, under the box state dir, not the
        // agent's own socket and not the host's $SSH_AUTH_SOCK.
        assert_eq!(agent.src, sb.scoped_agent_relay_sock());
        assert_ne!(agent.src, sb.scoped_agent_sock());
        assert!(agent.src.starts_with(sb.dir()));
        // Marked internal so check_supported does not refuse a state-dir source.
        assert!(agent.internal);
        // And SSH_AUTH_SOCK inside the box points at the mount destination.
        assert_eq!(
            sb.env().get("SSH_AUTH_SOCK").map(String::as_str),
            Some(sb.ssh_agent_dst().display().to_string().as_str())
        );
    }

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

    /// A scratch directory under the host's own temp dir, unique to this test
    /// process and cleaned up on drop - `cargo test` stays pure/no-container,
    /// but `copies()` still has to be exercised against a real, statable
    /// source, same as `overbroad_reason`'s tempdir-free tests exercise a pure
    /// function instead.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("agentbox-test-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The destination is the path as written, not where the symlink points.
    /// `ro = ["~/.local/bin/claude"]` on a `claude` that symlinks into
    /// `~/.local/share/claude/versions/<v>` used to mount the binary at the
    /// target's path, leaving `~/.local/bin` absent from the box - the tool was
    /// simply not where it had been asked for.
    #[test]
    fn a_symlinked_source_still_mounts_at_the_path_that_was_written() {
        let scratch = Scratch::new("bind-symlink");
        let real = scratch.path().join("versions/2.1.273");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, b"binary").unwrap();
        let bin = scratch.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let link = bin.join("claude");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let cfg = Config {
            ro: vec![link.display().to_string()],
            ..Config::default()
        };
        let sb = sandbox_with_cfg(cfg);
        let bind = sb
            .binds()
            .into_iter()
            .find(|b| b.read_only)
            .expect("the ro map should be planned");
        // The source is resolved - nspawn needs a real path to bind - but the
        // destination is the spelling the user used.
        assert_eq!(bind.src, real.canonicalize().unwrap());
        assert_eq!(bind.dst, link);
    }

    /// The same for `cpy`, which built its destination the same way.
    #[test]
    fn a_symlinked_cpy_source_copies_to_the_path_that_was_written() {
        let scratch = Scratch::new("cpy-symlink");
        let real = scratch.path().join("real.json");
        std::fs::write(&real, b"{}").unwrap();
        let link = scratch.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let cfg = Config {
            cpy: vec![link.display().to_string()],
            ..Config::default()
        };
        let sb = sandbox_with_cfg(cfg);
        let copies = sb.copies();
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].dst, link);
    }

    /// `unsafe_dst` refuses a destination that is not lexically normalized, so
    /// a `..` in a one-sided spec has to be resolved here - textually, without
    /// asking the filesystem what any component really is.
    #[test]
    fn a_default_destination_resolves_dots_without_following_symlinks() {
        assert_eq!(default_dst(Path::new("/a/b/../c")), Path::new("/a/c"));
        assert_eq!(default_dst(Path::new("/a/./b")), Path::new("/a/b"));
        assert_eq!(default_dst(Path::new("/a/b")), Path::new("/a/b"));
        // Climbing past the root is left for `unsafe_dst` to reject, rather
        // than quietly turned into something that passes.
        assert_eq!(default_dst(Path::new("/../etc")), Path::new("/../etc"));
    }

    #[test]
    fn copies_builds_the_planned_list_and_skips_a_missing_source() {
        let scratch = Scratch::new("cpy-basic");
        let src = scratch.path().join("present-src");
        std::fs::create_dir_all(&src).unwrap();
        let dst = scratch.path().join("present-dst");
        let missing_src = scratch.path().join("does-not-exist");

        let cfg = Config {
            cpy: vec![
                format!("{}:{}", src.display(), dst.display()),
                missing_src.display().to_string(),
            ],
            ..Config::default()
        };
        let sb = sandbox_with_cfg(cfg);
        let copies = sb.copies();
        // The missing source is skipped with a warning, not a panic, and only
        // the entry with a real source survives.
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].src, src.canonicalize().unwrap());
        assert_eq!(copies[0].dst, dst);
    }

    #[test]
    fn copies_skip_a_destination_already_claimed_by_a_bind() {
        let scratch = Scratch::new("cpy-claimed");
        let src = scratch.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // The project directory's own destination is always claimed first.
        let sb = sandbox_with_cfg(Config {
            cpy: vec![format!("{}:/home/me/project", src.display())],
            ..Config::default()
        });
        assert!(sb.copies().is_empty());
    }

    /// `nspawn::check_supported` applies the same `overbroad_reason` guard to
    /// `cpy` sources that it already applies to `rw`/`ro` ones (see
    /// `Sandbox::copies()`'s doc comment); `copies()` itself does not filter
    /// on it, mirroring how `binds()` leaves that check to `check_supported`
    /// too. This exercises the shared function against a source a `cpy` entry
    /// could plausibly name, so a reviewer can see the same reason a bind
    /// would be refused for applies here unchanged.
    #[test]
    fn a_cpy_source_naming_the_host_home_is_overbroad_like_a_bind() {
        let home = Path::new("/home/alice");
        let state = Path::new("/var/lib/agentbox");
        assert!(overbroad_reason(home, home, state).is_some());
        assert!(overbroad_reason(state, home, state).is_some());
    }

    fn sandbox_with_cfg(cfg: Config) -> Sandbox {
        let _ = crate::host::init(None, false);
        Sandbox::new(PathBuf::from("/home/me/project"), cfg)
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
