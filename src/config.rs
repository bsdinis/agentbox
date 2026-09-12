//! Layered configuration: built-in defaults, then the global file, then the
//! project file, then command-line overrides. Lists accumulate across layers,
//! tables merge key by key, scalars are replaced.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::host::{env_var, host};

pub const PROJECT_FILE: &str = ".agentbox.toml";
/// The shipped template for `~/.config/agentbox/config.toml`, embedded so
/// `agentbox init --global` can materialize it without the source checkout
/// `cargo install` leaves behind - see `cmds::init_global`.
pub const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");
/// `background = "auto"`: leave systemd-nspawn to tint the terminal as it
/// likes, rather than naming a colour or turning it off.
pub const BACKGROUND_AUTO: &str = "auto";
pub const UID_RANGE: u32 = 65536;
/// Multiple of 65536, inside the range systemd reserves for containers.
pub const UID_BASE_DEFAULT: u32 = 1_310_720_000;

/// Packages baked into the shared base image.
pub const DEFAULT_BASE_PACKAGES: &[&str] = &[
    "base",
    "base-devel",
    "sudo",
    "openssh",
    "ca-certificates",
    "gnupg",
    "git",
    "jujutsu",
    "github-cli",
    "git-lfs",
    "curl",
    "wget",
    "rsync",
    "unzip",
    "zstd",
    "jq",
    "vim",
    "less",
    "man-db",
    "tree",
    "which",
    "diffutils",
    "inetutils",
    "procps-ng",
    "strace",
    "tmux",
    "ripgrep",
    "fd",
    "fzf",
    "iputils",
    "python",
    "python-pip",
    "nodejs",
    "npm",
    "bash-completion",
    "fish",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// Share the host's network namespace. Reaches host localhost services and
    /// abstract sockets (e.g. the X11 display); opt in only for a trusted box.
    Host,
    /// No network at all.
    None,
    /// Private namespace with a veth pair, NAT'd by systemd-networkd. The default.
    Nat,
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Network::Host => "host",
            Network::None => "none",
            Network::Nat => "nat",
        })
    }
}

impl std::str::FromStr for Network {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "host" => Ok(Network::Host),
            "none" => Ok(Network::None),
            "nat" => Ok(Network::Nat),
            other => bail!("network must be host, none or nat (got {other:?})"),
        }
    }
}

/// One configuration layer. Every field is optional so layers can be folded.
#[derive(Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub network: Option<Network>,
    pub rw: Option<Vec<String>>,
    pub ro: Option<Vec<String>>,
    /// One-time copy-if-absent sources: `"src"`/`"src:dst"`, same syntax as
    /// `rw`/`ro`, but never a live mount - see `Sandbox::copies()`.
    pub cpy: Option<Vec<String>>,
    pub packages: Option<Vec<String>>,
    pub pass_env: Option<Vec<String>>,
    pub env: Option<BTreeMap<String, String>>,
    /// Private-key paths a box-scoped ssh-agent should hold. Accumulates across
    /// layers like the other lists. Empty means no agent, and no forwarding.
    pub ssh_keys: Option<Vec<String>>,
    pub shell: Option<String>,
    pub background: Option<String>,
    pub address_families: Option<String>,
    pub memory_max: Option<String>,
    pub cpu_quota: Option<String>,
    pub tasks_max: Option<String>,
    /// Apply the shipped AppArmor profile as a defense-in-depth LSM layer.
    /// `None` - the default - means on-if-available: apply it when it is loaded
    /// on the host, and stay silent otherwise. `Some(true)` also warns when it
    /// is unavailable; `Some(false)` opts out entirely.
    pub apparmor: Option<bool>,
    /// Allow the `perf_event_open` syscall inside the box. Does not touch the
    /// user namespace or grant any capability - see `Config::perf` and
    /// docs/security.md#perf-inside-a-box.
    pub perf: Option<bool>,
    pub uid_base: Option<u32>,
}

#[derive(Debug)]
pub struct Config {
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub network: Network,
    pub rw: Vec<String>,
    pub ro: Vec<String>,
    /// One-time copy-if-absent sources, before `expand()`. Empty - the
    /// default - means nothing is copied.
    pub cpy: Vec<String>,
    pub packages: Vec<String>,
    pub pass_env: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Private keys the box's dedicated ssh-agent holds, before `expand()`.
    /// Empty - the default - means no agent is started and nothing is
    /// forwarded; the host's own `$SSH_AUTH_SOCK` is never bound in.
    pub ssh_keys: Vec<String>,
    pub shell: Option<String>,
    /// Terminal background while the box runs. `None` - the default - means no
    /// tint at all, leaving the terminal the colour it already was.
    pub background: Option<String>,
    /// Socket address families the box may use. `None` - the default - means
    /// no filtering, stated explicitly so the coming systemd default does not
    /// silently apply one.
    pub address_families: Option<String>,
    pub memory_max: Option<String>,
    pub cpu_quota: Option<String>,
    pub tasks_max: Option<String>,
    /// Whether to apply the shipped AppArmor profile. `None` - the default -
    /// means on-if-available (see `Layer::apparmor`).
    pub apparmor: Option<bool>,
    /// Add `perf_event_open` to the box's syscall allow list. Off by default.
    /// This alone rarely unlocks much: the box's user namespace means the
    /// kernel's own `perfmon_capable()` check can never succeed inside it, no
    /// matter what capability the box holds, so almost everything beyond
    /// self-only software counters additionally needs the *host's*
    /// `kernel.perf_event_paranoid` lowered - a machine-wide decision agentbox
    /// cannot make on a box's behalf. The shipped AppArmor profile does not
    /// mediate this syscall at all (no `perf_event_open` LSM hook exists in
    /// AppArmor), so it adds no defense-in-depth here. See
    /// docs/security.md#perf-inside-a-box.
    pub perf: bool,
    pub uid_base: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: None,
            hostname: None,
            network: Network::Nat,
            rw: vec![],
            ro: ["~/.gitconfig", "~/.config/jj", "~/.config/git"]
                .map(String::from)
                .to_vec(),
            cpy: vec![],
            packages: vec![],
            pass_env: ["TERM", "COLORTERM", "LANG"].map(String::from).to_vec(),
            env: BTreeMap::new(),
            ssh_keys: vec![],
            shell: None,
            background: None,
            address_families: None,
            memory_max: None,
            cpu_quota: None,
            tasks_max: None,
            apparmor: None,
            perf: false,
            uid_base: UID_BASE_DEFAULT,
        }
    }
}

impl Config {
    fn apply(&mut self, layer: Layer) {
        fn extend(dst: &mut Vec<String>, src: Option<Vec<String>>) {
            for item in src.unwrap_or_default() {
                if !dst.contains(&item) {
                    dst.push(item);
                }
            }
        }
        extend(&mut self.rw, layer.rw);
        extend(&mut self.ro, layer.ro);
        extend(&mut self.cpy, layer.cpy);
        extend(&mut self.packages, layer.packages);
        extend(&mut self.pass_env, layer.pass_env);
        extend(&mut self.ssh_keys, layer.ssh_keys);
        self.env.extend(layer.env.unwrap_or_default());
        self.name = layer.name.or(self.name.take());
        self.hostname = layer.hostname.or(self.hostname.take());
        self.shell = layer.shell.or(self.shell.take());
        self.background = layer.background.or(self.background.take());
        self.address_families = layer.address_families.or(self.address_families.take());
        self.memory_max = layer.memory_max.or(self.memory_max.take());
        self.cpu_quota = layer.cpu_quota.or(self.cpu_quota.take());
        self.tasks_max = layer.tasks_max.or(self.tasks_max.take());
        self.apparmor = layer.apparmor.or(self.apparmor.take());
        self.perf = layer.perf.unwrap_or(self.perf);
        self.network = layer.network.unwrap_or(self.network);
        self.uid_base = layer.uid_base.unwrap_or(self.uid_base);
    }
}

/// Command-line overrides, which win over every file.
#[derive(Default, Debug)]
pub struct Overrides {
    pub rw: Vec<String>,
    pub ro: Vec<String>,
    pub packages: Vec<String>,
    pub network: Option<Network>,
    pub ssh_keys: Vec<String>,
    pub perf: Option<bool>,
}

pub fn global_path() -> PathBuf {
    let base = env_var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| host().user.home.join(".config"));
    base.join("agentbox/config.toml")
}

fn read_table(path: &Path) -> Result<toml::Table> {
    if !path.exists() {
        return Ok(toml::Table::new());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    text.parse()
        .with_context(|| format!("cannot parse {}", path.display()))
}

/// The global file may spell the per-box keys at the top level or inside a
/// `[defaults]` table; `[defaults]` wins where both are present.
fn global_layer() -> Result<Layer> {
    let path = global_path();
    let mut table = read_table(&path)?;
    table.remove("base_packages");
    if let Some(toml::Value::Table(base)) = table.remove("base") {
        let _ = base; // [base] packages = [...] is read by base_packages()
    }
    if let Some(toml::Value::Table(defaults)) = table.remove("defaults") {
        table.extend(defaults);
    }
    reject_removed_keys(&table, &path.display().to_string())?;
    toml::Value::Table(table)
        .try_into()
        .with_context(|| format!("cannot understand {}", path.display()))
}

fn project_layer(project: &Path) -> Result<Layer> {
    let path = project.join(PROJECT_FILE);
    let table = read_table(&path)?;
    reject_removed_keys(&table, &path.display().to_string())?;
    toml::Value::Table(table)
        .try_into()
        .with_context(|| format!("cannot understand {}", path.display()))
}

/// `ssh_agent` was removed in favour of `ssh_keys`. Forwarding the host's whole
/// agent exposed every key it held, so it is no longer a supported mode. Catch
/// the old key with a migration message rather than letting `deny_unknown_fields`
/// emit a bare "unknown field" and rather than silently ignoring it, which would
/// leave a user believing an agent is being forwarded when none is.
fn reject_removed_keys(table: &toml::Table, source: &str) -> Result<()> {
    if table.contains_key("ssh_agent") {
        bail!(
            "{source} sets `ssh_agent`, which has been removed: forwarding the \
             host's whole SSH agent exposed every key it held. List the specific \
             private keys a box may use instead, e.g.\n  \
             ssh_keys = [\"~/.ssh/id_ed25519\"]\n\
             agentbox starts a dedicated agent holding only those keys."
        );
    }
    Ok(())
}

pub fn load(project: &Path, overrides: &Overrides) -> Result<Config> {
    let mut cfg = Config::default();
    cfg.apply(global_layer()?);
    cfg.apply(project_layer(project)?);
    cfg.apply(Layer {
        rw: Some(overrides.rw.clone()),
        ro: Some(overrides.ro.clone()),
        packages: Some(overrides.packages.clone()),
        network: overrides.network,
        ssh_keys: Some(overrides.ssh_keys.clone()),
        perf: overrides.perf,
        ..Layer::default()
    });
    check_background(cfg.background.as_deref())?;
    check_address_families(cfg.address_families.as_deref())?;
    Ok(cfg)
}

/// systemd-nspawn rejects a malformed `--background=` itself, but only once a
/// launch is already under way, with the overlay mounted and the box created.
/// The grammar is narrow enough to check here, where a bad value costs nothing.
fn check_background(value: Option<&str>) -> Result<()> {
    let Some(value) = value.filter(|v| *v != BACKGROUND_AUTO) else {
        return Ok(());
    };
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit() || b == b';') {
        return Ok(());
    }
    bail!(
        "background must be {BACKGROUND_AUTO:?} or an ANSI SGR background colour \
         - \"40\" to \"47\", \"48;5;N\" or \"48;2;R;G;B\" (got {value:?}). \
         Remove the key to leave the terminal its own colour."
    )
}

/// Address family names, as systemd spells them: `AF_INET`, `~AF_PACKET` to
/// prohibit one, or the special value `none`. Empty means no filtering.
fn check_address_families(value: Option<&str>) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    let ok = |token: &str| {
        token == "none"
            || token
                .strip_prefix('~')
                .unwrap_or(token)
                .strip_prefix("AF_")
                .is_some_and(|rest| {
                    !rest.is_empty()
                        && rest
                            .bytes()
                            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                })
    };
    if let Some(bad) = value.split_whitespace().find(|t| !ok(t)) {
        bail!(
            "address_families takes systemd address family names such as \
             \"AF_INET AF_INET6 AF_UNIX AF_NETLINK\", \"~AF_PACKET\" to prohibit one, \
             or \"none\" (got {bad:?}). Remove the key to leave the box unfiltered."
        );
    }
    Ok(())
}

/// Packages for the shared base image: `base_packages` at the top level, or
/// `[base] packages`, else the built-in list.
pub fn base_packages() -> Result<Vec<String>> {
    let table = read_table(&global_path())?;
    let from_key = table.get("base_packages").cloned().or_else(|| {
        table
            .get("base")
            .and_then(|b| b.as_table())
            .and_then(|b| b.get("packages"))
            .cloned()
    });
    match from_key {
        Some(value) => Ok(value
            .try_into()
            .context("base_packages must be a list of strings")?),
        None => Ok(DEFAULT_BASE_PACKAGES
            .iter()
            .map(|s| s.to_string())
            .collect()),
    }
}

/// `uid_base` has to be readable before any project is known, for `build`.
pub fn global_uid_base() -> Result<u32> {
    let table = read_table(&global_path())?;
    let value = table
        .get("defaults")
        .and_then(|d| d.as_table())
        .and_then(|d| d.get("uid_base"))
        .or_else(|| table.get("uid_base"));
    match value {
        Some(v) => {
            let n = v.as_integer().context("uid_base must be an integer")?;
            let n = u32::try_from(n).context("uid_base out of range")?;
            if n % UID_RANGE != 0 {
                bail!("uid_base must be a multiple of {UID_RANGE}");
            }
            Ok(n)
        }
        None => Ok(UID_BASE_DEFAULT),
    }
}

/// Expand `~` and `$VAR` in a configured path, against the caller's environment.
pub fn expand(spec: &str) -> PathBuf {
    let spec = spec.trim();
    let with_home = if spec == "~" {
        host().user.home.display().to_string()
    } else if let Some(rest) = spec.strip_prefix("~/") {
        host().user.home.join(rest).display().to_string()
    } else {
        spec.to_string()
    };

    // One left-to-right pass, so a substituted value is never rescanned.
    let bytes = with_home.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(with_home.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            out.push(bytes[i]); // raw bytes, so multibyte paths survive
            i += 1;
            continue;
        }
        let braced = bytes.get(i + 1) == Some(&b'{');
        let start = i + 1 + usize::from(braced);
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        let closed = !braced || bytes.get(end) == Some(&b'}');
        if end == start || !closed {
            out.push(b'$'); // a lone $ or ${ is a literal
            i += 1;
            continue;
        }
        out.extend_from_slice(
            env_var(&with_home[start..end])
                .unwrap_or_default()
                .as_bytes(),
        );
        i = end + usize::from(braced);
    }
    PathBuf::from(OsString::from_vec(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The commented `base_packages` in config.example.toml is the built-in
    /// list verbatim, so that uncommenting it is a no-op and adding one line to
    /// it cannot quietly shrink the image - this list replaces the built-in one
    /// rather than adding to it, unlike every other list in the file. Nothing
    /// keeps the two in step but this test.
    #[test]
    fn the_example_config_shows_the_real_default_package_list() {
        let listed: Vec<&str> = EXAMPLE_CONFIG
            .lines()
            .skip_while(|line| !line.starts_with("# base_packages = ["))
            .take_while(|line| !line.starts_with("# ]"))
            .flat_map(|line| line.split('"').skip(1).step_by(2))
            .collect();
        assert_eq!(listed, DEFAULT_BASE_PACKAGES);
    }

    fn layer(toml_text: &str) -> Layer {
        toml_text
            .parse::<toml::Table>()
            .map(toml::Value::Table)
            .unwrap()
            .try_into()
            .unwrap()
    }

    #[test]
    fn lists_accumulate_and_scalars_replace() {
        let mut cfg = Config::default();
        cfg.apply(layer(
            "ro = ['~/global']\nnetwork = 'nat'\nmemory_max = '4G'",
        ));
        cfg.apply(layer("ro = ['~/project']\nnetwork = 'none'"));
        // defaults first, then each layer, no duplicates
        assert_eq!(
            cfg.ro,
            [
                "~/.gitconfig",
                "~/.config/jj",
                "~/.config/git",
                "~/global",
                "~/project"
            ]
        );
        assert_eq!(cfg.network, Network::None);
        assert_eq!(cfg.memory_max.as_deref(), Some("4G"));
    }

    #[test]
    fn cpy_accumulates_across_layers_like_rw_and_ro() {
        let mut cfg = Config::default();
        assert!(cfg.cpy.is_empty());
        cfg.apply(layer("cpy = ['~/seed-a']"));
        cfg.apply(layer("cpy = ['~/seed-a', '~/seed-b']"));
        // Accumulates in layer order, and a repeat is dropped - same as rw/ro.
        assert_eq!(cfg.cpy, ["~/seed-a", "~/seed-b"]);
    }

    #[test]
    fn apparmor_defaults_to_unset_and_a_layer_can_set_it() {
        // Default: unset, i.e. on-if-available with no nag.
        let cfg = Config::default();
        assert_eq!(cfg.apparmor, None);
        // A later layer wins, in both directions.
        let mut cfg = Config::default();
        cfg.apply(layer("apparmor = true"));
        assert_eq!(cfg.apparmor, Some(true));
        cfg.apply(layer("apparmor = false"));
        assert_eq!(cfg.apparmor, Some(false));
        // A layer that says nothing leaves the earlier value in place.
        cfg.apply(layer("network = 'none'"));
        assert_eq!(cfg.apparmor, Some(false));
    }

    #[test]
    fn perf_defaults_to_off_and_a_layer_can_turn_it_on() {
        let mut cfg = Config::default();
        assert!(!cfg.perf);
        cfg.apply(layer("perf = true"));
        assert!(cfg.perf);
        // A layer that says nothing leaves the earlier value in place.
        cfg.apply(layer("network = 'none'"));
        assert!(cfg.perf);
        cfg.apply(layer("perf = false"));
        assert!(!cfg.perf);
    }

    #[test]
    fn the_default_network_is_nat() {
        assert_eq!(Config::default().network, Network::Nat);
        // An empty layer leaves the default in place.
        let mut cfg = Config::default();
        cfg.apply(layer(""));
        assert_eq!(cfg.network, Network::Nat);
        // An explicit mode still wins.
        cfg.apply(layer("network = 'host'"));
        assert_eq!(cfg.network, Network::Host);
    }

    #[test]
    fn ssh_keys_accumulate_and_dedup_across_layers() {
        let mut cfg = Config::default();
        // The default is no keys, hence no agent and no forwarding.
        assert!(cfg.ssh_keys.is_empty());
        cfg.apply(layer("ssh_keys = ['~/.ssh/id_ed25519_a']"));
        cfg.apply(layer(
            "ssh_keys = ['~/.ssh/id_ed25519_a', '~/.ssh/id_ed25519_b']",
        ));
        // Raw specs are preserved (expansion is deferred to spawn time), the
        // list accumulates across layers, and a repeat is dropped.
        assert_eq!(cfg.ssh_keys, ["~/.ssh/id_ed25519_a", "~/.ssh/id_ed25519_b"]);
    }

    #[test]
    fn the_removed_ssh_agent_key_is_rejected_with_a_migration_message() {
        let table: toml::Table = "ssh_agent = true".parse().unwrap();
        let err = reject_removed_keys(&table, "test.toml").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ssh_agent"), "{msg}");
        assert!(msg.contains("ssh_keys"), "{msg}");
        // A table without it passes.
        let ok: toml::Table = "ssh_keys = ['~/.ssh/id_ed25519']".parse().unwrap();
        assert!(reject_removed_keys(&ok, "test.toml").is_ok());
    }

    #[test]
    fn duplicate_list_entries_are_dropped() {
        let mut cfg = Config::default();
        cfg.apply(layer("ro = ['~/.gitconfig', '~/x']"));
        cfg.apply(layer("ro = ['~/x']"));
        assert_eq!(cfg.ro.iter().filter(|r| *r == "~/x").count(), 1);
    }

    #[test]
    fn env_tables_merge_key_by_key() {
        let mut cfg = Config::default();
        cfg.apply(layer("[env]\nA = '1'\nB = '2'"));
        cfg.apply(layer("[env]\nB = 'override'\nC = '3'"));
        assert_eq!(cfg.env.get("A").unwrap(), "1");
        assert_eq!(cfg.env.get("B").unwrap(), "override");
        assert_eq!(cfg.env.get("C").unwrap(), "3");
    }

    #[test]
    fn a_background_colour_is_an_sgr_sequence_or_auto() {
        for good in [
            None,
            Some("auto"),
            Some("40"),
            Some("48;5;52"),
            Some("48;2;0;0;80"),
        ] {
            assert!(check_background(good).is_ok(), "{good:?}");
        }
        // The shapes a person actually types by mistake.
        for bad in [Some(""), Some("blue"), Some("#000080"), Some("48,5,52")] {
            assert!(check_background(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn network_round_trips() {
        for text in ["host", "none", "nat"] {
            assert_eq!(text.parse::<Network>().unwrap().to_string(), text);
        }
        assert!("bridge".parse::<Network>().is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let table: toml::Table = "netwrok = 'host'".parse().unwrap();
        let parsed: Result<Layer, _> = toml::Value::Table(table).try_into();
        let err = parsed.unwrap_err();
        assert!(err.to_string().contains("netwrok"), "{err}");
    }

    #[test]
    fn expands_tilde_and_variables() {
        let _ = crate::host::init(None, false);
        let home = &crate::host::host().user.home;
        assert_eq!(expand("~"), *home);
        assert_eq!(expand("~/x/y"), home.join("x/y"));
        assert_eq!(expand("/etc/passwd"), Path::new("/etc/passwd"));
        // a lone $ and an unterminated ${ stay literal rather than eating the path
        assert_eq!(expand("/tmp/a$"), Path::new("/tmp/a$"));
        assert_eq!(expand("/tmp/${x"), Path::new("/tmp/${x"));
        // an unset variable expands to nothing, like a shell
        assert_eq!(
            expand("/tmp/$AGENTBOX_DEFINITELY_UNSET/z"),
            Path::new("/tmp//z")
        );
    }
}
