//! The subcommands.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::argv;
use crate::base;
use crate::config::{self, Config, Overrides, PROJECT_FILE};
use crate::host::{dry_run, host, oss, sh};
use crate::nspawn;
use crate::sandbox::Sandbox;
use crate::session;

/// Resolve the project directory a command applies to.
pub fn project_dir(dir: &Option<PathBuf>) -> Result<PathBuf> {
    let path = match dir {
        Some(dir) => config::expand(&dir.to_string_lossy()),
        None => host().cwd.clone(),
    };
    let path = path.canonicalize().unwrap_or(path);
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(path)
}

pub fn load(dir: &Option<PathBuf>, overrides: &Overrides) -> Result<Sandbox> {
    let project = project_dir(dir)?;
    let cfg = config::load(&project, overrides)?;
    Ok(Sandbox::new(project, cfg))
}

// --------------------------------------------------------------------------

pub fn build(refresh: bool, force: bool) -> Result<()> {
    base::build(refresh, force)
}

pub fn init(dir: &Option<PathBuf>, force: bool) -> Result<()> {
    let project = project_dir(dir)?;
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

# host | none | nat
# network = "host"

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
    dir: &Option<PathBuf>,
    overrides: &Overrides,
    as_root: bool,
    cmd: &[String],
) -> Result<()> {
    let sb = load(dir, overrides)?;
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

pub fn up(dir: &Option<PathBuf>, overrides: &Overrides) -> Result<()> {
    let sb = load(dir, overrides)?;
    let fresh = nspawn::create(&sb)?;
    session::ensure_up(&sb)?;
    // A fresh box installs its configured packages here, after boot, so the
    // install has a working network even under nat. Prove the box is attachable
    // first (riding out the logind lag with the canary), then install.
    nspawn::wait_attachable(&sb)?;
    nspawn::canary(&sb, &sb.user.name)?;
    nspawn::install_packages(&sb, fresh)?;
    if !dry_run() {
        println!(
            "booted {}; open a shell with: agentbox shell --dir {}",
            sb.name,
            sb.project.display()
        );
    }
    Ok(())
}

pub fn down(dir: &Option<PathBuf>, overrides: &Overrides) -> Result<()> {
    let sb = load(dir, overrides)?;
    session::down(&sb)
}

pub fn list() -> Result<()> {
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
        let project = fs::read_to_string(dir.join("meta.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|meta| {
                meta.get("project")
                    .and_then(|p| p.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| "?".into());
        let overlay = if nspawn::is_mounted(&Path::new(crate::sandbox::MACHINES).join(name)) {
            "mounted"
        } else {
            "-"
        };
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
            overlay.into(),
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

pub fn status(dir: &Option<PathBuf>, overrides: &Overrides) -> Result<()> {
    let sb = load(dir, overrides)?;
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

pub fn show_config(dir: &Option<PathBuf>, overrides: &Overrides) -> Result<()> {
    let sb = load(dir, overrides)?;
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
    out.push_str("\n[env]\n");
    for (key, value) in env {
        out.push_str(&format!("{key} = {value:?}\n"));
    }
    out.push('\n');
    out
}

pub fn reset(dir: &Option<PathBuf>, overrides: &Overrides, yes: bool) -> Result<()> {
    let sb = load(dir, overrides)?;
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
    println!("reset {}", sb.name);
    Ok(())
}

pub fn remove(dir: &Option<PathBuf>, overrides: &Overrides, yes: bool) -> Result<()> {
    let sb = load(dir, overrides)?;
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
