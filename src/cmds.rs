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

# read-only mounts: reference code, path dependencies, registries, dotfiles
ro = []

# extra environment variables forwarded from the host, if set
# pass_env = ["ANTHROPIC_API_KEY"]

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
    nspawn::create(&sb)?;
    let argv: Vec<OsString> = if cmd.is_empty() {
        argv![sb.shell(), "-l"]
    } else {
        cmd.iter().map(oss).collect()
    };
    // After create, because a box being made now installs its `packages` on the
    // way up and one of them may be what provides the payload.
    if let Some(program) = argv.first() {
        nspawn::check_payload(&sb, program)?;
    }
    let user = if as_root { "root" } else { &sb.user.name };
    let chdir = sb.project.to_string_lossy().into_owned();

    // Boot-and-register, then prove the box is attachable before running the
    // payload exactly once. release() runs whatever the outcome, so a failure
    // between here and the attach does not leak a session or a box booted only
    // to carry it.
    let handle = session::begin(&sb)?;
    let outcome = nspawn::wait_attachable(&sb)
        .and_then(|()| nspawn::canary(&sb, user))
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
    nspawn::create(&sb)?;
    session::ensure_up(&sb)?;
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
        ssh_agent,
        background,
        address_families,
        memory_max,
        cpu_quota,
        tasks_max,
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
    out.push_str(&format!("ssh_agent = {ssh_agent}\n"));
    match background {
        Some(v) => out.push_str(&format!("background = {v:?}\n")),
        None => out.push_str("# background unset (terminal keeps its own colour)\n"),
    }
    match address_families {
        Some(v) => out.push_str(&format!("address_families = {v:?}\n")),
        None => out.push_str("# address_families unset (no filtering)\n"),
    }
    out.push_str(&format!("uid_base = {uid_base}\n"));
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
