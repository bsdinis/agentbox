//! agentbox - per-project systemd-nspawn sandboxes for coding agents.
//!
//! * One shared Arch base image, pre-shifted on disk into an unprivileged UID
//!   range, so a box needs no chown or ID-mapping work at start.
//! * Each project gets an overlayfs on top of it: lower is the shared base,
//!   upper holds every write the box makes, merged at /var/lib/machines/<box>.
//!   `pacman -S`, `sudo` and /home writes persist per project and cost only
//!   the diff.
//! * A user namespace maps container UID 0 to an unprivileged host UID, so
//!   root inside the box cannot touch the host.
//! * Host directories are bound with `owneridmap`, which maps the host owner
//!   of the source onto the sandbox user, so git and jj see sane ownership.
//!
//! See docs/design.md for why each of those is the way it is.

mod base;
mod cmds;
mod config;
mod host;
mod nspawn;
mod sandbox;
mod session;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use config::{Network, Overrides};

pub fn info(message: &str) {
    eprintln!("\x1b[1;34m::\x1b[0m {message}");
}

pub fn warn(message: &str) {
    eprintln!("\x1b[1;33m::\x1b[0m {message}");
}

#[derive(Parser)]
#[command(
    name = "agentbox",
    about = "Per-project systemd-nspawn sandboxes for coding agents",
    version,
    max_term_width = 96
)]
struct Cli {
    /// Print what would be done, and the generated settings, changing nothing
    #[arg(long, global = true)]
    dry_run: bool,

    /// Internal: the caller's environment, handed across the sudo boundary
    #[arg(long, hide = true)]
    internal_handover: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

/// Which box a command applies to, and one-off mount overrides.
#[derive(Args, Default, Clone)]
struct Target {
    /// Box to act on, named as `agentbox ls` prints it, or the project
    /// directory it was made for (default: the current directory's box)
    #[arg(value_name = "BOX")]
    r#box: Option<String>,

    /// Extra read-only mount, PATH or HOST:CONTAINER (repeatable)
    #[arg(long = "map", value_name = "PATH[:DEST]")]
    map: Vec<String>,

    /// Extra read-write mount, PATH or HOST:CONTAINER (repeatable)
    #[arg(long = "rw-map", value_name = "PATH[:DEST]")]
    rw_map: Vec<String>,

    /// Network mode for this launch
    #[arg(long)]
    network: Option<Network>,

    /// Private key the box's dedicated ssh-agent may use (confirm-on-use).
    /// Repeatable. The host's own agent is never forwarded.
    #[arg(long = "ssh-key", value_name = "PATH")]
    ssh_key: Vec<String>,
}

impl Target {
    fn overrides(&self) -> Overrides {
        Overrides {
            rw: self.rw_map.clone(),
            ro: self.map.clone(),
            packages: vec![],
            network: self.network,
            ssh_keys: self.ssh_key.clone(),
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Build or update the shared base image
    Build {
        /// Update the packages in an existing image
        #[arg(long)]
        refresh: bool,
        /// Delete the image and rebuild it from scratch
        #[arg(long)]
        force: bool,
    },

    /// Write a .agentbox.toml for this project
    Init {
        #[arg(long)]
        force: bool,
        /// Write ~/.config/agentbox/config.toml instead, from the shipped
        /// example - the one file `cargo install` cannot copy in for you,
        /// since it does not leave the source checkout behind
        #[arg(long, conflicts_with = "dir")]
        global: bool,
        /// Project directory (default: the current directory)
        #[arg(value_name = "DIR")]
        dir: Option<String>,
    },

    /// Create or attach to the box, then open a shell in it
    Shell(RunArgs),

    /// Create or attach to the box, then run one command in it
    Run(RunArgs),

    /// Boot the box and keep it running until `down`
    Up {
        #[command(flatten)]
        target: Target,
    },

    /// Power off a booted box
    Down {
        #[command(flatten)]
        target: Target,
    },

    /// Remount a box on the base image as it stands now, keeping its writes
    Remount {
        #[command(flatten)]
        target: Target,
    },

    /// List boxes, their overlay state and how much each has written
    Ls,

    /// Show the box and mount plan for a project
    Status {
        #[command(flatten)]
        target: Target,
    },

    /// Show the effective configuration and generated settings
    Config {
        #[command(flatten)]
        target: Target,
    },

    /// Discard everything the container has written
    Reset {
        #[arg(short, long)]
        yes: bool,
        #[command(flatten)]
        target: Target,
    },

    /// Delete the box
    Rm {
        #[arg(short, long)]
        yes: bool,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Args)]
struct RunArgs {
    /// Run as container root instead of the sandbox user
    #[arg(long)]
    root: bool,

    /// Extra packages installed if the box is created now (repeatable)
    #[arg(long = "packages", value_name = "PKG")]
    packages: Vec<String>,

    #[command(flatten)]
    target: Target,

    /// Command to run, after `--`. Defaults to a login shell.
    // `last` keeps the payload behind the `--`, so the free argument in
    // `agentbox run mybox -- cmd` is read as the box and not as the command.
    #[arg(last = true, allow_hyphen_values = true)]
    cmd: Vec<String>,
}

impl RunArgs {
    fn overrides(&self) -> Overrides {
        Overrides {
            packages: self.packages.clone(),
            ..self.target.overrides()
        }
    }
}

/// Commands that never touch /var/lib/agentbox, and so need no privileges.
fn needs_root(command: &Command) -> bool {
    !matches!(command, Command::Init { .. } | Command::Config { .. })
}

fn main() {
    if let Err(err) = run() {
        eprintln!("agentbox: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    host::init(cli.internal_handover.as_deref(), cli.dry_run)?;
    if cli.internal_handover.is_none() && !cli.dry_run && needs_root(&cli.command) {
        host::ensure_root()?;
    }

    match &cli.command {
        Command::Build { refresh, force } => cmds::build(*refresh, *force),
        Command::Init { force, global, dir } => {
            if *global {
                cmds::init_global(*force)
            } else {
                cmds::init(dir, *force)
            }
        }
        Command::Shell(args) | Command::Run(args) => {
            cmds::shell(&args.target.r#box, &args.overrides(), args.root, &args.cmd)
        }
        Command::Up { target } => cmds::up(&target.r#box, &target.overrides()),
        Command::Down { target } => cmds::down(&target.r#box, &target.overrides()),
        Command::Remount { target } => cmds::remount(&target.r#box, &target.overrides()),
        Command::Ls => cmds::list(),
        Command::Status { target } => cmds::status(&target.r#box, &target.overrides()),
        Command::Config { target } => cmds::show_config(&target.r#box, &target.overrides()),
        Command::Reset { yes, target } => cmds::reset(&target.r#box, &target.overrides(), *yes),
        Command::Rm { yes, target } => cmds::remove(&target.r#box, &target.overrides(), *yes),
    }
}
