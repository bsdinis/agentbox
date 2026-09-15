//! The subcommands.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::argv;
use crate::base;
use crate::config::{self, Config, Overrides, PROJECT_FILE};
use crate::host::{dry_run, host, oss, sh};
use crate::nspawn;
use crate::sandbox::Sandbox;
use crate::session;

/// What a `[BOX]` argument turned out to name.
#[derive(Debug, PartialEq)]
enum Target {
    /// A box that exists, and the project directory it was made for.
    Box { name: String, project: PathBuf },
    /// A project directory - given as a path, or defaulted to the cwd.
    Dir(PathBuf),
}

/// Which of the two readings a `[BOX]` argument takes.
///
/// A box name is a hostname label - `Sandbox::new` turns every character that
/// is not alphanumeric into `-` - so a spec containing a `/` can only be a
/// path, and is never looked up as a box. Otherwise an existing box wins over a
/// directory of the same name, since the argument is documented as a box and
/// the name most likely came straight off `agentbox ls`.
///
/// The two lookups are passed in so the rule can be tested without a state
/// directory: `boxes` maps a box name to the project it was made for, `dirs` a
/// spec to the directory it names, if that directory exists.
fn classify(
    spec: &str,
    boxes: impl Fn(&str) -> Option<PathBuf>,
    dirs: impl Fn(&str) -> Option<PathBuf>,
) -> Option<Target> {
    if !spec.contains('/') {
        if let Some(project) = boxes(spec) {
            return Some(Target::Box {
                name: spec.to_string(),
                project,
            });
        }
    }
    dirs(spec).map(Target::Dir)
}

/// The project directory a box was created for, as recorded when it was made.
fn box_project(name: &str) -> Option<PathBuf> {
    let meta = nspawn::state_dir()
        .join("boxes")
        .join(name)
        .join("meta.json");
    let text = fs::read_to_string(meta).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(PathBuf::from(meta.get("project")?.as_str()?))
}

/// Canonicalize a directory a command was pointed at, and refuse a non-directory.
fn project_dir(path: PathBuf) -> Result<PathBuf> {
    let path = path.canonicalize().unwrap_or(path);
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(path)
}

/// Resolve what a command acts on. No argument means the current directory.
///
/// A named box is taken at its word: its recorded project directory is used as
/// it stands, without the `project_dir` check, so a box whose project has since
/// been deleted can still be listed, powered off and removed.
fn resolve(spec: &Option<String>) -> Result<Target> {
    let Some(spec) = spec else {
        return Ok(Target::Dir(project_dir(host().cwd.clone())?));
    };
    let dirs = |spec: &str| {
        let path = config::expand(spec);
        path.is_dir().then(|| path.canonicalize().unwrap_or(path))
    };
    classify(spec, box_project, dirs).ok_or_else(|| {
        anyhow!(
            "no box named {spec:?}, and {} is not a directory - `agentbox ls` \
             lists the boxes",
            config::expand(spec).display()
        )
    })
}

pub fn load(spec: &Option<String>, overrides: &Overrides) -> Result<Sandbox> {
    let (project, named) = match resolve(spec)? {
        Target::Box { name, project } => (project, Some(name)),
        Target::Dir(project) => (project, None),
    };
    let cfg = config::load(&project, overrides)?;
    let sb = Sandbox::new(project, cfg);
    // The name is derived from the project path and `name` in the project file,
    // so a box named on the command line can resolve to a *different* box if
    // that file has been edited since. Say so rather than act on the wrong box.
    if let Some(named) = named.filter(|named| *named != sb.name) {
        bail!(
            "box {named} was created for {}, which now configures itself as box \
             {} - `name` in {PROJECT_FILE} has changed since. Act on {} by name, \
             or put the old name back.",
            sb.project.display(),
            sb.name,
            sb.name
        );
    }
    Ok(sb)
}

// --------------------------------------------------------------------------

pub fn build(refresh: bool, force: bool) -> Result<()> {
    base::build(refresh, force)
}

pub fn init(dir: &Option<String>, force: bool) -> Result<()> {
    let project = project_dir(match dir {
        Some(dir) => config::expand(dir),
        None => host().cwd.clone(),
    })?;
    let dest = project.join(PROJECT_FILE);
    if dest.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            dest.display()
        );
    }
    let name = project.file_name().unwrap_or_default().to_string_lossy();
    // Compute the read-only `.git/hooks` block once, so the dry-run print and
    // the real write below stay identical.
    let hooks_ro = git_hooks_ro(
        &project,
        &host().user.home,
        project.join(".git/hooks").is_dir(),
    );
    let ro_block = match &hooks_ro {
        Some(entry) => format!(
            "# read-only mounts: reference code, path dependencies, registries, dotfiles.\n\
             # `.git/hooks` is mapped read-only so an agent in the box cannot plant a\n\
             # hook that would then run on the host, as you, the next time you\n\
             # git commit / checkout / merge / push here; leave it in place. See\n\
             # docs/security.md.\n\
             ro = [{entry:?}]"
        ),
        None => {
            "# read-only mounts: reference code, path dependencies, registries, dotfiles\nro = []"
                .to_string()
        }
    };
    let template = format!(
        r#"# agentbox sandbox for {name}
# The project directory itself is always mounted read-write at its real path.
#
# Every box already gets ~/.gitconfig, ~/.config/git and ~/.config/jj read-only
# and TERM, COLORTERM and LANG forwarded, plus whatever
# ~/.config/agentbox/config.toml adds. Lists here are added to those, so only
# what this project needs on top belongs below. `agentbox config` prints the
# result.

# box name prefix; the default is this directory's name
# name = "{name}"

# nat (default) | host | none  (host exposes localhost and X11 to the box)
# network = "nat"

# extra packages installed into this box the first time it is created
packages = []

# read-write mounts (host path, or "host:container" path pair)
rw = []

{ro_block}

# extra environment variables forwarded from the host, if set
# pass_env = ["ANTHROPIC_API_KEY"]

# Specific SSH private keys this box may use. agentbox starts a dedicated
# ssh-agent holding ONLY these keys and forwards that agent's socket into the
# box; the host's own agent (and every other key it holds) is never exposed.
# Each key is added with confirm-on-use, so the box using it prompts you on the
# host. Empty - the default - means no agent and no forwarding.
# ssh_keys = ["~/.ssh/id_ed25519"]

# Set false only for unattended use (no human present to confirm a key's use):
# the box can then push, pull and open outbound SSH connections as you with no
# per-use prompt. true (the default) is the only safe choice for a box you do
# not fully trust.
# ssh_keys_confirm = true

# terminal background while a session runs. Unset leaves your terminal the
# colour it already is; "auto" lets systemd-run pick its own per-box tint, and
# an ANSI SGR background such as "48;5;52" picks your own.
# background = "auto"

# socket address families the box may use. Unset means no filtering at all,
# which is what a dev box wants: AF_NETLINK alone is needed by ip, ss, udev and
# glibc's resolver.
# address_families = "AF_INET AF_INET6 AF_UNIX AF_NETLINK"

# resource caps enforced by systemd on the container scope
# memory_max = "8G"
# cpu_quota  = "400%"

# AppArmor confinement (defense in depth). Unset applies the shipped profile if
# it is loaded on the host and does nothing otherwise; false opts out, true warns
# when it is expected but missing. See contrib/apparmor/README.md.
# apparmor = true

# Variables set inside the box. Keep this table last: in TOML every key after a
# table header belongs to that table, so a scalar moved below it silently
# becomes an environment variable instead.
[env]
# RUST_BACKTRACE = "1"
"#
    );
    if dry_run() {
        print!("{template}");
        warn_if_unmappable(&project);
        return Ok(());
    }
    fs::write(&dest, template).with_context(|| format!("cannot write {}", dest.display()))?;
    // `init` runs unprivileged, but may also be reached through sudo.
    let _ = std::os::unix::fs::chown(&dest, Some(host().user.uid), Some(host().user.gid));
    println!("wrote {}", dest.display());
    warn_if_unmappable(&project);
    Ok(())
}

/// Materialize `~/.config/agentbox/config.toml` from the embedded example.
/// `cargo install` - the only install path, whether from crates.io or
/// `--path` - discards the source checkout once the binary is built, so this
/// is the only way to get `config.example.toml` onto disk without one lying
/// around. Config layering already treats a missing global file as "no extra
/// defaults" (see `config::load`), so this is a one-time convenience for
/// editing it, not a requirement for agentbox to run.
pub fn init_global(force: bool) -> Result<()> {
    let dest = config::global_path();
    if dest.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            dest.display()
        );
    }
    if dry_run() {
        print!("{}", config::EXAMPLE_CONFIG);
        return Ok(());
    }
    let confdir = dest
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", dest.display()))?;
    let created = !confdir.exists();
    fs::create_dir_all(confdir).with_context(|| format!("cannot create {}", confdir.display()))?;
    fs::write(&dest, config::EXAMPLE_CONFIG)
        .with_context(|| format!("cannot write {}", dest.display()))?;
    // `init` runs unprivileged, but may also be reached through sudo.
    let _ = std::os::unix::fs::chown(&dest, Some(host().user.uid), Some(host().user.gid));
    if created {
        let _ = std::os::unix::fs::chown(confdir, Some(host().user.uid), Some(host().user.gid));
    }
    println!("wrote {}", dest.display());
    Ok(())
}

/// `init` is usually the first command run in a project, which makes it the
/// earliest chance to say that this one can never be put in a box.
fn warn_if_unmappable(project: &Path) {
    if let Some(base) = nspawn::shadowed_by_nspawn(project) {
        crate::warn(&format!(
            "{} is under {base}, which systemd-nspawn covers with a mount of its \
             own, so this project cannot be mapped into a box. Move it elsewhere \
             - see docs/troubleshooting.md.",
            project.display()
        ));
    }
}

/// The read-only `.git/hooks` entry `init` bakes into a git project's config,
/// or `None` when the project is not a (normal) git repository.
///
/// `hooks_exists` is whether `<project>/.git/hooks` is a directory; the caller
/// passes `project.join(".git/hooks").is_dir()`. The path is tildified to
/// `~/...` when the project sits under `home`, matching the convention in the
/// example configs, and rendered absolute otherwise. The project dir is mounted
/// at its real path and an `ro` entry's destination defaults to its source, so
/// this lands `.git/hooks` read-only exactly over the copy in the working tree.
///
/// A git worktree or submodule keeps `.git` as a *file*, not a directory, so it
/// has no local `.git/hooks` and is simply skipped here; resolving the real
/// gitdir for those is out of scope.
fn git_hooks_ro(project: &Path, home: &Path, hooks_exists: bool) -> Option<String> {
    if !hooks_exists {
        return None;
    }
    let hooks = project.join(".git/hooks");
    Some(match hooks.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => hooks.display().to_string(),
    })
}

/// Boot the box if it is not already running, then open a session in it: an
/// interactive shell, or the given command. The box outlives the session (so
/// another `shell`/`run` can attach to the same running box), and powers off
/// with the last session unless `up` marked it kept - see `session`.
pub fn shell(
    spec: &Option<String>,
    overrides: &Overrides,
    as_root: bool,
    cmd: &[String],
) -> Result<()> {
    // `agentbox run cmd` used to run `cmd`, and now reads it as the box to act
    // on. Nothing else in the CLI would explain the "no box named cmd" that
    // comes back, so add the missing half here.
    let sb = load(spec, overrides).map_err(|err| match (spec, cmd.is_empty()) {
        (Some(_), true) => anyhow!("{err:#}; a command to run goes after `--`"),
        _ => err,
    })?;
    let fresh = nspawn::create(&sb)?;
    let argv: Vec<OsString> = if cmd.is_empty() {
        argv![sb.shell(), "-l"]
    } else {
        cmd.iter().map(oss).collect()
    };
    // Cloned out of `argv` so the payload check can borrow the program name while
    // `argv` itself is moved into `attach` at the end of the chain below.
    let program = argv.first().cloned();
    let user = if as_root { "root" } else { &sb.user.name };
    let chdir = sb.project.to_string_lossy().into_owned();

    // Boot-and-register, then prove the box is attachable, install a fresh box's
    // packages, and check the payload is present - all before running it exactly
    // once. The package install waits until here because a pre-boot install has
    // no network under nat; and the payload check waits too, because those very
    // packages may be what provides it. release() runs whatever the outcome, so
    // a failure anywhere here leaks neither a session nor a box booted only to
    // carry it.
    let handle = session::begin(&sb)?;
    let outcome = nspawn::wait_attachable(&sb)
        .and_then(|()| nspawn::canary(&sb, user))
        .and_then(|()| nspawn::await_nat_network(&sb))
        .and_then(|()| nspawn::install_packages(&sb, fresh))
        .and_then(|()| match &program {
            Some(program) => nspawn::check_payload(&sb, program),
            None => Ok(()),
        })
        .and_then(|()| nspawn::attach(&sb, argv, user, &chdir));
    handle.release(&sb);
    // Exit with the payload's own code, as replacing the process used to.
    match outcome {
        Ok(code) => std::process::exit(code),
        Err(err) => Err(err),
    }
}

pub fn up(spec: &Option<String>, overrides: &Overrides) -> Result<()> {
    let sb = load(spec, overrides)?;
    let fresh = nspawn::create(&sb)?;
    session::ensure_up(&sb)?;
    // A fresh box installs its configured packages here, after boot, so the
    // install has a working network even under nat. Prove the box is attachable
    // first (riding out the logind lag with the canary), then install.
    nspawn::wait_attachable(&sb)?;
    nspawn::canary(&sb, &sb.user.name)?;
    nspawn::await_nat_network(&sb)?;
    nspawn::install_packages(&sb, fresh)?;
    if !dry_run() {
        println!("booted {0}; open a shell with: agentbox shell {0}", sb.name);
    }
    Ok(())
}

pub fn down(spec: &Option<String>, overrides: &Overrides) -> Result<()> {
    let sb = load(spec, overrides)?;
    session::down(&sb)?;
    // Powering off doesn't unmount the overlay, but something else pinning a
    // generation may have changed since this box last touched the state
    // directory, so it costs nothing to sweep here too.
    base::gc_generations();
    Ok(())
}

/// Drop a box's overlay and mount it again, so it sees the base image as it is
/// now rather than as it was when the box was last started.
///
/// Lossless and quick: everything the box has written lives in `upper` on disk,
/// not in the mount, and only the kernel's cached view of the image underneath
/// is discarded. This is the whole cure for a box left straddling a rebuild -
/// `reset` also cures it, but by deleting the box's writes, which is a far
/// larger hammer than the problem needs.
pub fn remount(spec: &Option<String>, overrides: &Overrides) -> Result<()> {
    let sb = load(spec, overrides)?;
    if !sb.dir().exists() {
        bail!("no box for {}", sb.project.display());
    }
    // An overlay cannot be swapped underneath a container that is running on
    // it, and powering someone's box off is not this command's call to make.
    if nspawn::service_active(&sb.name) {
        bail!(
            "{name} is running; `agentbox down {name}` first, then remount it",
            name = sb.name
        );
    }
    nspawn::umount(&sb)?;
    nspawn::mount(&sb)?;
    // The old generation may have just lost its last reference.
    base::gc_generations();
    if !dry_run() {
        println!("remounted {} on the current base image", sb.name);
    }
    Ok(())
}

pub fn list() -> Result<()> {
    // Best effort, and cheap when there's nothing to do: keeps the OVERLAY
    // column from listing a generation that's about to be reclaimed anyway.
    base::gc_generations();
    let boxes = nspawn::state_dir().join("boxes");
    let Ok(entries) = fs::read_dir(&boxes) else {
        return Ok(());
    };
    let mut names: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    let mut rows = vec![[
        "BOX".to_string(),
        "PROJECT".into(),
        "OVERLAY".into(),
        "BOOTED".into(),
        "WRITES".into(),
    ]];
    for name in &names {
        let dir = boxes.join(name);
        let project = box_project(name)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "?".into());
        let overlay = nspawn::overlay_label(name);
        let booted = sh(argv![
            "systemctl",
            "is-active",
            format!("systemd-nspawn@{name}.service")
        ])
        .output();
        let writes = sh(argv!["du", "-sh", "--apparent-size", dir.join("upper")]).output();
        rows.push([
            name.clone(),
            project,
            overlay,
            if booted.is_empty() {
                "-".into()
            } else {
                booted
            },
            writes.split('\t').next().unwrap_or("?").to_string(),
        ]);
    }
    if rows.len() == 1 {
        return Ok(());
    }
    let widths: Vec<usize> = (0..5)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{cell:<width$}", width = widths[i]))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
    Ok(())
}

pub fn status(spec: &Option<String>, overrides: &Overrides) -> Result<()> {
    let sb = load(spec, overrides)?;
    let (lo, hi) = sb.uid_range();
    let mounted = if nspawn::is_mounted(&sb.root()) {
        "mounted"
    } else {
        "not mounted"
    };
    println!("box        {}", sb.name);
    println!("project    {}", sb.project.display());
    println!("rootfs     {} ({mounted})", sb.root().display());
    println!("writes     {}", sb.upper().display());
    println!("settings   {}", sb.settings().display());
    println!("uid range  {lo}..{hi} (container root == host uid {lo})");
    println!("network    {}", sb.cfg.network);
    println!("shell      {}", sb.shell());
    println!("mounts");
    for bind in sb.binds() {
        let note = match nspawn::shadowed_by_nspawn(&bind.dst) {
            Some(base) => format!("   << unsupported: nspawn owns {base}"),
            None => String::new(),
        };
        println!(
            "  {:<2} {} -> {}{note}",
            bind.kind(),
            bind.src.display(),
            bind.dst.display()
        );
    }
    // Printing a plan that cannot be launched and saying nothing is how this
    // used to surface: as a chdir error, several commands later.
    if let Err(err) = nspawn::check_supported(&sb) {
        crate::warn(&format!("{err:#}"));
    }
    Ok(())
}

pub fn show_config(spec: &Option<String>, overrides: &Overrides) -> Result<()> {
    let sb = load(spec, overrides)?;
    print!("{}", effective_toml(&sb));
    println!("--- {} ---", sb.settings().display());
    print!("{}", nspawn::settings_text(&sb)?);
    Ok(())
}

/// Round-trip the effective configuration back into TOML, so it can be pasted
/// into a project file.
fn effective_toml(sb: &Sandbox) -> String {
    let Config {
        network,
        rw,
        ro,
        packages,
        pass_env,
        env,
        ssh_keys,
        ssh_keys_confirm,
        background,
        address_families,
        memory_max,
        cpu_quota,
        tasks_max,
        apparmor,
        uid_base,
        ..
    } = &sb.cfg;
    let list = |items: &Vec<String>| {
        let quoted: Vec<String> = items.iter().map(|s| format!("{s:?}")).collect();
        format!("[{}]", quoted.join(", "))
    };
    let mut out = format!("# effective configuration for box {}\n", sb.name);
    out.push_str(&format!("name = {:?}\n", sb.name));
    out.push_str(&format!("network = {:?}\n", network.to_string()));
    out.push_str(&format!("shell = {:?}\n", sb.shell()));
    match background {
        Some(v) => out.push_str(&format!("background = {v:?}\n")),
        None => out.push_str("# background unset (terminal keeps its own colour)\n"),
    }
    match address_families {
        Some(v) => out.push_str(&format!("address_families = {v:?}\n")),
        None => out.push_str("# address_families unset (no filtering)\n"),
    }
    out.push_str(&format!("uid_base = {uid_base}\n"));
    match apparmor {
        Some(v) => out.push_str(&format!("apparmor = {v}\n")),
        None => out.push_str("# apparmor unset (applied if the profile is loaded)\n"),
    }
    for (key, value) in [
        ("memory_max", memory_max),
        ("cpu_quota", cpu_quota),
        ("tasks_max", tasks_max),
    ] {
        match value {
            Some(v) => out.push_str(&format!("{key} = {v:?}\n")),
            None => out.push_str(&format!("# {key} unset\n")),
        }
    }
    out.push_str(&format!("rw = {}\n", list(rw)));
    out.push_str(&format!("ro = {}\n", list(ro)));
    out.push_str(&format!("packages = {}\n", list(packages)));
    out.push_str(&format!("pass_env = {}\n", list(pass_env)));
    out.push_str(&format!("ssh_keys = {}\n", list(ssh_keys)));
    out.push_str(&format!("ssh_keys_confirm = {ssh_keys_confirm}\n"));
    out.push_str("\n[env]\n");
    for (key, value) in env {
        out.push_str(&format!("{key} = {value:?}\n"));
    }
    out.push('\n');
    out
}

pub fn reset(spec: &Option<String>, overrides: &Overrides, yes: bool) -> Result<()> {
    let sb = load(spec, overrides)?;
    if !sb.dir().exists() {
        bail!("no box for {}", sb.project.display());
    }
    if !yes && !confirm(&format!("discard all container writes for {}?", sb.name))? {
        return Ok(());
    }
    poweroff(&sb);
    // Before rm -rf: the agent's pid file lives under sb.dir(), and removing it
    // would strip our only handle on the running ssh-agent process.
    nspawn::teardown_ssh_agent(&sb)?;
    nspawn::umount(&sb)?;
    sh(argv!["rm", "--one-file-system", "-rf", sb.dir()]).run()?;
    nspawn::create(&sb)?;
    // The overlay just dropped and remounted may have freed a generation.
    base::gc_generations();
    println!("reset {}", sb.name);
    Ok(())
}

pub fn remove(spec: &Option<String>, overrides: &Overrides, yes: bool) -> Result<()> {
    let sb = load(spec, overrides)?;
    if !sb.dir().exists() {
        bail!("no box for {}", sb.project.display());
    }
    if !yes && !confirm(&format!("delete box {}?", sb.name))? {
        return Ok(());
    }
    poweroff(&sb);
    // Before rm -rf, for the same reason as reset: keep the handle on the agent.
    nspawn::teardown_ssh_agent(&sb)?;
    nspawn::umount(&sb)?;
    sh(argv!["rm", "--one-file-system", "-rf", sb.dir()]).run()?;
    let _ = fs::remove_dir(sb.root());
    if sb.settings().exists() {
        fs::remove_file(sb.settings())?;
    }
    nspawn::clear_unit_caps(&sb)?;
    // The box's overlay is gone for good, which may have freed a generation.
    base::gc_generations();
    println!("removed {}", sb.name);
    Ok(())
}

/// Stop the box and wait for it, so the overlay is idle before `umount`.
/// `systemctl stop` blocks until the unit is gone; `machinectl poweroff` would
/// return while the container was still shutting down and leave the mount busy.
fn poweroff(sb: &Sandbox) {
    let _ = sh(argv!["systemctl", "stop", sb.service()])
        .quiet()
        .silent()
        .allow_fail()
        .run();
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state directory holding one box, `proj-abc1234`, made for `/w/proj`,
    /// and a working directory holding a `proj-abc1234` of its own.
    fn boxes(name: &str) -> Option<PathBuf> {
        (name == "proj-abc1234").then(|| PathBuf::from("/w/proj"))
    }
    fn dirs(spec: &str) -> Option<PathBuf> {
        ["proj-abc1234", "./proj-abc1234", "/w/proj"]
            .contains(&spec)
            .then(|| PathBuf::from("/cwd").join(spec))
    }

    #[test]
    fn a_box_wins_over_a_directory_of_the_same_name() {
        assert_eq!(
            classify("proj-abc1234", boxes, dirs),
            Some(Target::Box {
                name: "proj-abc1234".into(),
                project: "/w/proj".into(),
            })
        );
    }

    #[test]
    fn a_spec_with_a_separator_is_only_ever_a_path() {
        assert_eq!(
            classify("./proj-abc1234", boxes, dirs),
            Some(Target::Dir("/cwd/./proj-abc1234".into()))
        );
        assert_eq!(
            classify("/w/proj", boxes, dirs),
            Some(Target::Dir("/w/proj".into()))
        );
    }

    #[test]
    fn a_name_that_is_neither_a_box_nor_a_directory_does_not_resolve() {
        assert_eq!(classify("proj", boxes, dirs), None);
    }

    #[test]
    fn git_project_under_home_yields_a_tildified_hooks_entry() {
        let home = Path::new("/home/alice");
        let project = Path::new("/home/alice/dev/proj");
        assert_eq!(
            git_hooks_ro(project, home, true),
            Some("~/dev/proj/.git/hooks".to_string())
        );
    }

    #[test]
    fn git_project_outside_home_yields_an_absolute_hooks_entry() {
        let home = Path::new("/home/alice");
        let project = Path::new("/srv/work/proj");
        assert_eq!(
            git_hooks_ro(project, home, true),
            Some("/srv/work/proj/.git/hooks".to_string())
        );
    }

    #[test]
    fn a_non_git_project_yields_no_hooks_entry() {
        let home = Path::new("/home/alice");
        let project = Path::new("/home/alice/dev/proj");
        assert_eq!(git_hooks_ro(project, home, false), None);
    }
}
