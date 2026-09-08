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

# name = "{name}"                        # box name prefix
network = "host"                          # host | none | nat

# extra packages installed into this box the first time it is created
packages = []

# read-write mounts (host path, or "host:container" path pair)
rw = []

# read-only mounts: reference code, path dependencies, registries, dotfiles
ro = [
  "~/.gitconfig",
  "~/.config/jj",
]

# environment variables forwarded from the host, if set
pass_env = ["TERM", "COLORTERM", "LANG"]

[env]
# RUST_BACKTRACE = "1"

# resource caps enforced by systemd on the container scope
# memory_max = "8G"
# cpu_quota  = "400%"
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
    // Named explicitly rather than left to the settings file, which applies to
    // booted launches too (see nspawn::settings_text).
    let user = if as_root { "root" } else { &sb.user.name };
    let chdir = sb.project.to_string_lossy().into_owned();
    nspawn::exec_in(&sb, argv, Some(user), Some(&chdir))?;
    unreachable!()
}

pub fn up(dir: &Option<PathBuf>, overrides: &Overrides) -> Result<()> {
    let sb = load(dir, overrides)?;
    nspawn::create(&sb)?;
    sh(argv!["systemctl", "start", sb.service()]).run()?;
    println!(
        "booted {}; enter it with: agentbox enter --dir {}",
        sb.name,
        sb.project.display()
    );
    Ok(())
}

pub fn enter(dir: &Option<PathBuf>, overrides: &Overrides) -> Result<()> {
    let sb = load(dir, overrides)?;
    sh(argv![
        "machinectl",
        "shell",
        format!("{}@{}", sb.user.name, sb.name),
        sb.shell()
    ])
    .quiet()
    .exec()?;
    unreachable!()
}

pub fn down(dir: &Option<PathBuf>, overrides: &Overrides) -> Result<()> {
    let sb = load(dir, overrides)?;
    sh(argv!["machinectl", "poweroff", sb.name.clone()])
        .allow_fail()
        .run()?;
    Ok(())
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
    print!("{}", nspawn::settings_text(&sb));
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

fn poweroff(sb: &Sandbox) {
    let _ = sh(argv!["machinectl", "poweroff", sb.name.clone()])
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
