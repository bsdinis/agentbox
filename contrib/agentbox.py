#!/usr/bin/env python3
# The original Python prototype, superseded by the Rust implementation in
# src/. Kept for reference only: it is not installed, not tested, and will not
# track changes to the Rust version. Read src/ instead.
"""agentbox - per-project systemd-nspawn sandboxes for coding agents.

Design
------
* One shared Arch base rootfs (/var/lib/agentbox/base), pre-shifted on disk into
  an unprivileged UID range, so containers need no expensive chown at start.
* Each project gets an overlayfs on top of that base:
      lower = shared base (read-only, shared by every box)
      upper = /var/lib/agentbox/boxes/<box>/upper   (all container writes)
      merged = /var/lib/machines/<box>              (so machinectl sees it)
  => `pacman -S`, `sudo`, /home writes all persist per project, cost only the diff.
* User namespaces (PrivateUsers=<base>:65536) mean container root == an
  unprivileged host UID: root inside the box cannot touch the host.
* Host directories are bind mounted with `owneridmap`, which maps the host owner
  of the source to the container user, so git/jj see correctly owned files.

Commands
--------
  agentbox build [--refresh]     build or update the shared base image
  agentbox init                  write a .agentbox.toml for the current project
  agentbox shell [-- CMD ...]    create if needed, then enter the project's box
  agentbox run -- CMD ...        same as shell, non-interactive friendly
  agentbox up | enter | down     booted mode (systemd as PID 1 inside)
  agentbox ls | status | config
  agentbox reset                 throw away container writes, keep the project
  agentbox rm                    delete the box
Any command takes --dir PATH to act on another project, and --dry-run.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pwd
import re
import shlex
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

STATE = Path("/var/lib/agentbox")
BASE = STATE / "base"
BOXES = STATE / "boxes"
MACHINES = Path("/var/lib/machines")
NSPAWN_DIR = Path("/etc/systemd/nspawn")

UID_RANGE = 65536
UID_BASE_DEFAULT = 1310720000  # multiple of 65536, inside systemd's container range

# Installed into the shared base image. Override with base_packages in the
# global config, then `agentbox build --refresh`.
DEFAULT_BASE_PACKAGES = [
    "base", "base-devel", "sudo", "openssh", "ca-certificates", "gnupg",
    "git", "jujutsu", "github-cli", "git-lfs",
    "curl", "wget", "rsync", "unzip", "zstd", "jq",
    "vim", "less", "man-db", "tree", "which", "diffutils", "inetutils",
    "procps-ng", "strace", "tmux", "ripgrep", "fd", "fzf",
    "python", "python-pip", "nodejs", "npm",
    "bash-completion", "fish",
]

# Config keys that projects may set. Lists/dicts merge over the global config.
LIST_KEYS = ("rw", "ro", "packages", "pass_env")
DICT_KEYS = ("env",)
SCALAR_KEYS = (
    "name", "network", "shell", "memory_max", "cpu_quota", "tasks_max",
    "uid_base", "ssh_agent", "hostname",
)

DEFAULTS = {
    "network": "host",           # host | none | nat
    "rw": [],                    # extra read-write binds (project dir is implicit)
    "ro": ["~/.gitconfig", "~/.config/jj", "~/.config/git"],
    "packages": [],              # installed into this box on first create
    "env": {},
    "pass_env": ["TERM", "COLORTERM", "LANG"],
    "ssh_agent": False,
    "memory_max": None,
    "cpu_quota": None,
    "tasks_max": None,
    "uid_base": UID_BASE_DEFAULT,
    "shell": None,
    "name": None,
    "hostname": None,
}

PROJECT_CONFIG = ".agentbox.toml"
GLOBAL_CONFIG = "agentbox/config.toml"  # under $XDG_CONFIG_HOME

DRY_RUN = False


# --------------------------------------------------------------------------- #
# plumbing
# --------------------------------------------------------------------------- #

def die(msg: str, code: int = 1) -> "None":
    print(f"agentbox: {msg}", file=sys.stderr)
    raise SystemExit(code)


def info(msg: str) -> None:
    print(f"\033[1;34m::\033[0m {msg}", file=sys.stderr)


def run(argv, check=True, quiet=False, **kw):
    if DRY_RUN:
        print("  " + shlex.join(str(a) for a in argv))
        return subprocess.CompletedProcess(argv, 0, "", "")
    if not quiet:
        info(shlex.join(str(a) for a in argv))
    return subprocess.run([str(a) for a in argv], check=check, **kw)


def out(argv) -> str:
    return subprocess.run(
        [str(a) for a in argv], check=False, capture_output=True, text=True
    ).stdout.strip()


# The invoking user's environment and cwd. sudo resets the environment, so we
# hand it over explicitly across the privilege boundary (see ensure_root).
HOST_ENV: dict[str, str] = {}
HOST_CWD: Path | None = None


def host_env(name: str, default: str | None = None) -> str | None:
    return HOST_ENV.get(name, os.environ.get(name, default))


def host_user() -> pwd.struct_passwd:
    """The invoking human, even after we have re-exec'd under sudo."""
    uid = os.environ.get("AGENTBOX_UID") or os.environ.get("SUDO_UID")
    return pwd.getpwuid(int(uid) if uid else os.getuid())


def host_cwd() -> Path:
    return HOST_CWD or Path(os.getcwd())


# commands that never touch /var/lib/agentbox and so need no privileges
UNPRIVILEGED_COMMANDS = {"init", "config"}


def ensure_root() -> None:
    """systemd-nspawn and mount need real root; re-exec once through sudo.

    The environment is passed through a 0600 file rather than `sudo VAR=...`
    (which default sudoers rejects) or argv (which is world-readable in /proc),
    because it can hold API tokens destined for `pass_env`.
    """
    if os.geteuid() == 0:
        return
    runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    base = Path(runtime) if Path(runtime).is_dir() else Path.home()
    handover = base / f".agentbox-env-{os.getpid()}.json"
    handover.write_text(json.dumps({
        "env": dict(os.environ), "cwd": os.getcwd(), "uid": os.getuid(),
    }))
    handover.chmod(0o600)
    os.execvp("sudo", ["sudo", sys.executable, os.path.abspath(__file__),
                       "--internal-handover", str(handover), *sys.argv[1:]])


def load_handover(path: str) -> None:
    """Adopt the unprivileged caller's environment, then destroy the file."""
    global HOST_ENV, HOST_CWD
    file = Path(path)
    st = file.stat()
    try:
        if st.st_uid != int(os.environ.get("SUDO_UID") or st.st_uid):
            die(f"{path}: not owned by the invoking user")
        data = json.loads(file.read_text())
    finally:
        file.unlink(missing_ok=True)
    HOST_ENV = {str(k): str(v) for k, v in data.get("env", {}).items()}
    HOST_CWD = Path(data["cwd"])
    os.environ.setdefault("AGENTBOX_UID", str(data["uid"]))


def expand(p: str, home: Path) -> Path:
    """Expand ~ and $VAR in a configured path, using the caller's environment."""
    p = p.strip()
    if p == "~" or p.startswith("~/"):
        p = str(home) + p[1:]
    p = re.sub(r"\$\{?(\w+)\}?", lambda m: host_env(m.group(1)) or "", p)
    return Path(p)


# --------------------------------------------------------------------------- #
# configuration
# --------------------------------------------------------------------------- #

def read_toml(path: Path) -> dict:
    if not path.exists():
        return {}
    try:
        return tomllib.loads(path.read_text())
    except tomllib.TOMLDecodeError as e:
        die(f"{path}: {e}")


def global_config_path(home: Path) -> Path:
    xdg = host_env("XDG_CONFIG_HOME")
    root = Path(xdg) if xdg and Path(xdg).is_absolute() else home / ".config"
    return root / GLOBAL_CONFIG


def sanitize(name: str) -> str:
    name = re.sub(r"[^a-zA-Z0-9-]+", "-", name).strip("-").lower()
    return name or "box"


class Box:
    def __init__(self, project: Path, cfg: dict, user: pwd.struct_passwd):
        self.project = project
        self.cfg = cfg
        self.user = user
        self.home = Path(user.pw_dir)
        stem = sanitize(cfg["name"] or project.name)
        digest = hashlib.sha256(str(project).encode()).hexdigest()[:6]
        self.name = f"{stem}-{digest}"[:60]
        self.uid_base = int(cfg["uid_base"])

    # paths ----------------------------------------------------------------
    @property
    def dir(self) -> Path:
        return BOXES / self.name

    @property
    def upper(self) -> Path:
        return self.dir / "upper"

    @property
    def work(self) -> Path:
        return self.dir / "work"

    @property
    def root(self) -> Path:
        return MACHINES / self.name

    @property
    def settings(self) -> Path:
        return NSPAWN_DIR / f"{self.name}.nspawn"

    # uid mapping ----------------------------------------------------------
    def shift(self, uid: int) -> int:
        return self.uid_base + uid

    # environment ----------------------------------------------------------
    def env(self) -> dict:
        env = {}
        for name in self.cfg["pass_env"]:
            val = host_env(name)
            if val:
                env[name] = val
        env.update({k: str(v) for k, v in self.cfg["env"].items()})
        if self.cfg["ssh_agent"]:
            env["SSH_AUTH_SOCK"] = "/run/ssh-agent.sock"
        env.setdefault("AGENTBOX", self.name)
        return env

    # binds ----------------------------------------------------------------
    def binds(self) -> list[tuple[str, Path, Path]]:
        """(kind, host source, container destination); kind in {rw, ro}."""
        specs: list[tuple[str, Path, Path]] = [("rw", self.project, self.project)]
        seen = {str(self.project)}
        for kind in ("rw", "ro"):
            for raw in self.cfg[kind]:
                src_s, _, dst_s = str(raw).partition(":")
                src = expand(src_s, self.home).resolve()
                dst = expand(dst_s, self.home) if dst_s else Path(str(src))
                if not src.exists():
                    info(f"skipping {kind} map {src} (does not exist)")
                    continue
                if str(dst) in seen:
                    continue
                seen.add(str(dst))
                specs.append((kind, src, dst))
        if self.cfg["ssh_agent"]:
            sock = host_env("SSH_AUTH_SOCK")
            if sock and Path(sock).exists():
                specs.append(("rw", Path(sock), Path("/run/ssh-agent.sock")))
            else:
                info("ssh_agent requested but SSH_AUTH_SOCK is unset; skipping")
        return specs


def load_box(project: Path, overrides: dict) -> Box:
    user = host_user()
    home = Path(user.pw_dir)
    gcfg = read_toml(global_config_path(home))
    pcfg = read_toml(project / PROJECT_CONFIG)

    cfg = dict(DEFAULTS)
    # global file may nest defaults under [defaults] or set them at top level
    for layer in (gcfg.get("defaults", {}), gcfg, pcfg):
        for k, v in layer.items():
            if k in LIST_KEYS:
                cfg[k] = list(dict.fromkeys([*cfg[k], *v]))
            elif k in DICT_KEYS:
                cfg[k] = {**cfg[k], **v}
            elif k in SCALAR_KEYS:
                cfg[k] = v
            elif k in ("base_packages", "defaults", "base"):
                continue
            else:
                info(f"ignoring unknown config key {k!r}")
    for k, v in overrides.items():
        if v is None:
            continue
        if k in LIST_KEYS:
            cfg[k] = list(dict.fromkeys([*cfg[k], *v]))
        else:
            cfg[k] = v
    if cfg["network"] not in ("host", "none", "nat"):
        die(f"network must be host, none or nat (got {cfg['network']!r})")
    return Box(project, cfg, user)


# --------------------------------------------------------------------------- #
# base image
# --------------------------------------------------------------------------- #

PACMAN_BUILD_CONF = """\
[options]
HoldPkg = pacman glibc
Architecture = auto
CheckSpace
ParallelDownloads = 8
SigLevel = Required DatabaseOptional
LocalFileSigLevel = Optional

[core]
Include = /etc/pacman.d/mirrorlist

[extra]
Include = /etc/pacman.d/mirrorlist
"""


def base_packages(home: Path) -> list[str]:
    gcfg = read_toml(global_config_path(home))
    pkgs = gcfg.get("base_packages") or gcfg.get("base", {}).get("packages")
    return list(pkgs) if pkgs else list(DEFAULT_BASE_PACKAGES)


def in_base(argv, uid_base: int | None = None, host_cache: bool = False, **kw):
    """Run a command inside the base image via nspawn."""
    cmd = ["systemd-nspawn", "-q", "-D", BASE, "--as-pid2", "--register=no",
           "--resolv-conf=copy-host", "--timezone=off"]
    if host_cache:
        # share the host's package cache: a fresh build downloads almost
        # nothing, and what it does download stays useful to the host
        cmd += ["--bind=/var/cache/pacman/pkg"]
    if uid_base is not None:
        cmd += [f"--private-users={uid_base}:{UID_RANGE}",
                "--private-users-ownership=off"]
    cmd += ["--", *argv]
    return run(cmd, **kw)


def cmd_build(args) -> None:
    user = host_user()
    home = Path(user.pw_dir)
    gcfg = read_toml(global_config_path(home))
    uid_base = int(gcfg.get("defaults", {}).get(
        "uid_base", gcfg.get("uid_base", UID_BASE_DEFAULT)))
    pkgs = base_packages(home)

    if BASE.exists() and not args.refresh and not args.force:
        die(f"base image already exists at {BASE} "
            f"(use --refresh to update packages, --force to rebuild)")
    if args.force and BASE.exists():
        info(f"removing {BASE}")
        run(["rm", "--one-file-system", "-rf", BASE])

    if BASE.exists():  # refresh in place, ownership already shifted
        info("refreshing base image")
        in_base(["/usr/bin/pacman", "-Syu", "--noconfirm", "--needed", *pkgs],
                uid_base=uid_base)
        return

    STATE.mkdir(parents=True, exist_ok=True)
    BOXES.mkdir(parents=True, exist_ok=True)
    conf = STATE / "pacman-build.conf"
    if not DRY_RUN:
        conf.write_text(PACMAN_BUILD_CONF)

    info(f"bootstrapping Arch into {BASE}")
    for d in ("var/lib/pacman", "var/cache/pacman/pkg", "var/log", "etc/pacman.d",
              "dev", "proc", "sys", "run", "tmp", "root"):
        (BASE / d).mkdir(parents=True, exist_ok=True) if not DRY_RUN else None
    if not DRY_RUN:
        os.chmod(BASE / "tmp", 0o1777)
        shutil.copy("/etc/pacman.d/mirrorlist", BASE / "etc/pacman.d/mirrorlist")

    # Stage 1: minimal system from the host's pacman, reusing the host package
    # cache and keyring. Bind /proc /sys /dev /run so install scriptlets work.
    api = ["proc", "sys", "dev", "run"]
    try:
        for d in api:
            run(["mount", "--rbind", f"/{d}", BASE / d], quiet=True,
                stdout=subprocess.DEVNULL)
        run(["pacman", "--config", conf, "--root", BASE,
             "--cachedir", "/var/cache/pacman/pkg",
             "--gpgdir", "/etc/pacman.d/gnupg",
             "--noconfirm", "--needed", "-Sy", "base", "archlinux-keyring"])
    finally:
        for d in reversed(api):
            run(["umount", "-R", "-l", BASE / d], check=False, quiet=True,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    # Stage 2: finish the install from inside the container.
    info("initialising pacman keyring inside the image")
    in_base(["/bin/bash", "-euo", "pipefail", "-c",
             "pacman-key --init && pacman-key --populate archlinux"])
    info(f"installing {len(pkgs)} packages inside the image")
    in_base(["/usr/bin/pacman", "-Sy", "--noconfirm", "--needed", *pkgs],
            host_cache=True)

    # Stage 3: make it a usable dev box for `user`.
    info("configuring image")
    setup = f"""
set -euo pipefail
sed -i 's/^#\\(en_US.UTF-8 UTF-8\\)/\\1/' /etc/locale.gen
locale-gen
printf 'LANG=en_US.UTF-8\\n' > /etc/locale.conf
sed -i 's/^#\\(Color\\)/\\1/;s/^#\\(ParallelDownloads.*\\)/\\1/' /etc/pacman.conf
: > /etc/machine-id
printf 'agentbox\\n' > /etc/hostname
# the sandbox user mirrors the host user, so ~ and project paths match exactly
groupadd -g {user.pw_gid} -o {shlex.quote(user.pw_name)} 2>/dev/null || true
useradd -m -u {user.pw_uid} -g {user.pw_gid} -G wheel \
        -s /bin/bash {shlex.quote(user.pw_name)} 2>/dev/null || true
passwd -d {shlex.quote(user.pw_name)} >/dev/null 2>&1 || true
install -d -m 750 /etc/sudoers.d
printf '%%wheel ALL=(ALL:ALL) NOPASSWD: ALL\\n' > /etc/sudoers.d/00-agentbox
chmod 440 /etc/sudoers.d/00-agentbox
# let `sudo` keep the tokens agents need instead of scrubbing them
printf 'Defaults env_keep += "ANTHROPIC_API_KEY GITHUB_TOKEN"\\n' \
        > /etc/sudoers.d/10-agentbox-env
chmod 440 /etc/sudoers.d/10-agentbox-env
"""
    in_base(["/bin/bash", "-c", setup])

    # Stage 4: shift every UID/GID into the container range so boxes start
    # instantly with PrivateUsersOwnership=off.
    info(f"shifting image ownership to UID base {uid_base}")
    run(["systemd-nspawn", "-q", "-D", BASE, "--as-pid2", "--register=no",
         f"--private-users={uid_base}:{UID_RANGE}",
         "--private-users-ownership=chown", "--", "/bin/true"])
    info(f"base image ready: {BASE}")


def require_base() -> None:
    if (BASE / "usr").exists():
        return
    if DRY_RUN:
        info(f"no base image at {BASE} yet (dry run continues)")
        return
    die("no base image yet - run `agentbox build` first")


# --------------------------------------------------------------------------- #
# box lifecycle
# --------------------------------------------------------------------------- #

def is_mounted(path: Path) -> bool:
    return subprocess.run(["mountpoint", "-q", str(path)], check=False).returncode == 0


def mount_box(box: Box) -> None:
    if is_mounted(box.root):
        return
    require_base()
    for d in (box.upper, box.work, box.root):
        run(["mkdir", "-p", d], quiet=True)
    run(["mount", "-t", "overlay", f"agentbox-{box.name}", "-o",
         f"lowerdir={BASE},upperdir={box.upper},workdir={box.work},"
         "index=off,metacopy=off,redirect_dir=off,xino=off",
         box.root])


def umount_box(box: Box) -> None:
    if is_mounted(box.root):
        run(["umount", box.root])


def write_settings(box: Box) -> None:
    """Single source of truth, honoured by both direct nspawn and machinectl."""
    lines = ["# generated by agentbox - do not edit", "[Exec]",
             f"PrivateUsers={box.uid_base}:{UID_RANGE}",
             f"User={box.user.pw_name}",
             f"WorkingDirectory={box.project}",
             f"Hostname={box.cfg['hostname'] or box.name}",
             "Timezone=copy",
             "LinkJournal=no",
             "ResolvConf=" + ("off" if box.cfg["network"] == "none" else "copy-host")]
    for k, v in box.env().items():
        lines.append(f"Environment={k}={v}")
    lines += ["", "[Files]", "PrivateUsersOwnership=off"]
    for kind, src, dst in box.binds():
        key = "Bind" if kind == "rw" else "BindReadOnly"
        lines.append(f"{key}={escape_bind(src)}:{escape_bind(dst)}:owneridmap")
    lines += ["", "[Network]"]
    if box.cfg["network"] == "host":
        lines += ["Private=no", "VirtualEthernet=no"]
    elif box.cfg["network"] == "none":
        lines += ["Private=yes", "VirtualEthernet=no"]
    else:  # nat
        lines += ["Private=yes", "VirtualEthernet=yes"]
    text = "\n".join(lines) + "\n"
    if DRY_RUN:
        print(f"--- {box.settings} ---")
        print(text, end="")
        return
    NSPAWN_DIR.mkdir(parents=True, exist_ok=True)
    box.settings.write_text(text)


def escape_bind(p: Path) -> str:
    return str(p).replace("\\", "\\\\").replace(":", "\\:")




def create_box(box: Box) -> None:
    require_base()
    fresh = not box.dir.exists()
    mount_box(box)
    if not DRY_RUN:
        box.dir.mkdir(parents=True, exist_ok=True)
        (box.dir / "meta.json").write_text(json.dumps({
            "name": box.name, "project": str(box.project),
            "user": box.user.pw_name, "uid_base": box.uid_base,
            "network": box.cfg["network"],
        }, indent=2) + "\n")
        # per-box identity, generated once and then left alone
        machine_id = box.root / "etc/machine-id"
        if not machine_id.exists() or not machine_id.read_text().strip():
            machine_id.write_text(os.urandom(16).hex() + "\n")
        hostname = (box.cfg["hostname"] or box.name) + "\n"
        hostname_file = box.root / "etc/hostname"
        if not hostname_file.exists() or hostname_file.read_text() != hostname:
            hostname_file.write_text(hostname)
        # mount points must exist and be owned by the sandbox user, because
        # `owneridmap` maps the host owner of the source onto the owner of the
        # destination inside the container.
        for kind, src, dst in box.binds():
            target = box.root / str(dst).lstrip("/")
            parent = target.parent
            parent.mkdir(parents=True, exist_ok=True)
            if src.is_dir():
                target.mkdir(exist_ok=True)
            elif not target.exists():
                target.touch()
            st = src.stat()
            for p in (target,):
                os.chown(p, box.shift(st.st_uid if st.st_uid < UID_RANGE else 0),
                         box.shift(st.st_gid if st.st_gid < UID_RANGE else 0))
    mask_host_network_units(box)
    write_settings(box)
    shell = box.cfg["shell"] or default_shell(box)
    if fresh:
        set_login_shell(box, shell)
        if box.cfg["packages"]:
            info(f"installing project packages: {' '.join(box.cfg['packages'])}")
            nspawn(box, ["/usr/bin/pacman", "-Sy", "--noconfirm", "--needed",
                         *box.cfg["packages"]], user="root", chdir="/")


def mask_host_network_units(box: Box) -> None:
    """In host-network mode the box shares the host's network namespace, so a
    booted box must never run networkd/resolved - it would reconfigure the
    host's own interfaces. Mask them inside the box (not in the shared base,
    since nat mode needs networkd to bring up host0)."""
    if DRY_RUN:
        return
    unit_dir = box.root / "etc/systemd/system"
    unit_dir.mkdir(parents=True, exist_ok=True)
    for unit in ("systemd-networkd.service", "systemd-networkd.socket",
                 "systemd-resolved.service"):
        link = unit_dir / unit
        masked = link.is_symlink() and os.readlink(link) == "/dev/null"
        if box.cfg["network"] == "host" and not masked:
            link.unlink(missing_ok=True)
            link.symlink_to("/dev/null")
        elif box.cfg["network"] != "host" and masked:
            link.unlink()


def default_shell(box: Box) -> str:
    host_shell = box.user.pw_shell or "/bin/bash"
    candidate = box.root / str(host_shell).lstrip("/")
    return host_shell if candidate.exists() else "/bin/bash"


def set_login_shell(box: Box, shell: str) -> None:
    if DRY_RUN:
        return
    nspawn(box, ["/usr/bin/chsh", "-s", shell, box.user.pw_name],
           user="root", chdir="/", quiet=True, stdout=subprocess.DEVNULL)


def nspawn(box: Box, cmd: list[str], user: str | None = None,
           chdir: str | None = None, quiet: bool = False, check=True, **kw):
    argv = ["systemd-nspawn", "-q", "-M", box.name, "-D", box.root,
            "--settings=yes", "--as-pid2", "--register=no"]
    if user:
        argv += ["-u", user]
    if chdir:
        argv += [f"--chdir={chdir}"]
    for prop, val in (("MemoryMax", box.cfg["memory_max"]),
                      ("CPUQuota", box.cfg["cpu_quota"]),
                      ("TasksMax", box.cfg["tasks_max"])):
        if val:
            argv += [f"--property={prop}={val}"]
    argv += ["--", *cmd]
    return run(argv, quiet=quiet, check=check, **kw)


# --------------------------------------------------------------------------- #
# commands
# --------------------------------------------------------------------------- #

def project_dir(args) -> Path:
    p = (Path(args.dir).expanduser() if args.dir else host_cwd()).resolve()
    if not p.is_dir():
        die(f"{p} is not a directory")
    return p


def overrides_from(args) -> dict:
    return {
        "rw": getattr(args, "rw_map", None),
        "ro": getattr(args, "map", None),
        "network": getattr(args, "network", None),
        "packages": getattr(args, "packages", None),
        "ssh_agent": True if getattr(args, "ssh_agent", False) else None,
    }


def cmd_init(args) -> None:
    project = project_dir(args)
    dest = project / PROJECT_CONFIG
    if dest.exists() and not args.force:
        die(f"{dest} already exists (use --force to overwrite)")
    user = host_user()
    template = f"""\
# agentbox sandbox for {project.name}
# The project directory itself is always mounted read-write at its real path.

# name = "{sanitize(project.name)}"      # box name prefix
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
"""
    if DRY_RUN:
        print(template, end="")
        return
    dest.write_text(template)
    os.chown(dest, user.pw_uid, user.pw_gid)
    print(f"wrote {dest}")


def cmd_shell(args) -> None:
    box = load_box(project_dir(args), overrides_from(args))
    create_box(box)
    cmd = args.cmd or [box.cfg["shell"] or default_shell(box), "-l"]
    if args.root:
        r = nspawn(box, cmd, user="root", chdir=str(box.project), check=False)
    else:
        r = nspawn(box, cmd, chdir=str(box.project), check=False)
    raise SystemExit(r.returncode)


def cmd_up(args) -> None:
    box = load_box(project_dir(args), overrides_from(args))
    create_box(box)
    run(["systemctl", "start", f"systemd-nspawn@{box.name}.service"])
    print(f"booted {box.name}; enter with: agentbox enter --dir {box.project}")


def cmd_enter(args) -> None:
    box = load_box(project_dir(args), overrides_from(args))
    argv = ["machinectl", "shell", f"{box.user.pw_name}@{box.name}",
            box.cfg["shell"] or "/bin/bash"]
    r = run(argv, check=False)
    raise SystemExit(r.returncode)


def cmd_down(args) -> None:
    box = load_box(project_dir(args), overrides_from(args))
    run(["machinectl", "poweroff", box.name], check=False)


def cmd_ls(args) -> None:
    if not BOXES.exists():
        return
    rows = []
    for d in sorted(BOXES.iterdir()):
        meta = d / "meta.json"
        m = json.loads(meta.read_text()) if meta.exists() else {}
        size = out(["du", "-sh", "--apparent-size", str(d / "upper")]).split("\t")[0]
        state = "mounted" if is_mounted(MACHINES / d.name) else "-"
        booted = out(["systemctl", "is-active",
                      f"systemd-nspawn@{d.name}.service"]) or "-"
        rows.append((d.name, m.get("project", "?"), state, booted, size or "?"))
    if not rows:
        return
    w = [max(len(r[i]) for r in rows) for i in range(5)]
    hdr = ("BOX", "PROJECT", "OVERLAY", "BOOTED", "WRITES")
    w = [max(w[i], len(hdr[i])) for i in range(5)]
    for r in (hdr, *rows):
        print("  ".join(str(c).ljust(w[i]) for i, c in enumerate(r)))


def cmd_status(args) -> None:
    box = load_box(project_dir(args), overrides_from(args))
    print(f"box        {box.name}")
    print(f"project    {box.project}")
    print(f"rootfs     {box.root} ({'mounted' if is_mounted(box.root) else 'not mounted'})")
    print(f"writes     {box.upper}")
    print(f"settings   {box.settings}")
    print(f"uid range  {box.uid_base}..{box.uid_base + UID_RANGE - 1} "
          f"(container root == host uid {box.uid_base})")
    print(f"network    {box.cfg['network']}")
    print("mounts")
    for kind, src, dst in box.binds():
        print(f"  {kind:<2} {src} -> {dst}")


def cmd_config(args) -> None:
    box = load_box(project_dir(args), overrides_from(args))
    print(json.dumps({"box": box.name, "project": str(box.project),
                      **box.cfg}, indent=2, default=str))
    write_settings_preview(box)


def write_settings_preview(box: Box) -> None:
    global DRY_RUN
    keep, DRY_RUN = DRY_RUN, True
    write_settings(box)
    DRY_RUN = keep


def cmd_reset(args) -> None:
    box = load_box(project_dir(args), overrides_from(args))
    if not box.dir.exists():
        die(f"no box for {box.project}")
    if not args.yes and not confirm(f"discard all container writes for {box.name}?"):
        return
    run(["machinectl", "poweroff", box.name], check=False,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    umount_box(box)
    run(["rm", "--one-file-system", "-rf", box.dir])
    create_box(box)
    print(f"reset {box.name}")


def cmd_rm(args) -> None:
    box = load_box(project_dir(args), overrides_from(args))
    if not box.dir.exists():
        die(f"no box for {box.project}")
    if not args.yes and not confirm(f"delete box {box.name}?"):
        return
    run(["machinectl", "poweroff", box.name], check=False,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    umount_box(box)
    run(["rm", "--one-file-system", "-rf", box.dir])
    run(["rmdir", box.root], check=False)
    if box.settings.exists():
        box.settings.unlink()
    print(f"removed {box.name}")


def confirm(prompt: str) -> bool:
    try:
        return input(f"{prompt} [y/N] ").strip().lower() in ("y", "yes")
    except EOFError:
        return False


# --------------------------------------------------------------------------- #
# cli
# --------------------------------------------------------------------------- #

def main() -> None:
    global DRY_RUN
    ap = argparse.ArgumentParser(prog="agentbox", description=__doc__.split("\n")[0])
    ap.add_argument("--dry-run", action="store_true",
                    help="print what would be done and the generated settings")
    ap.add_argument("--internal-handover", help=argparse.SUPPRESS)
    sub = ap.add_subparsers(dest="command", required=True)

    def common(p, maps=True):
        p.add_argument("--dir", help="project directory (default: cwd)")
        if maps:
            p.add_argument("--map", action="append", metavar="PATH[:DEST]",
                           help="extra read-only mount (repeatable)")
            p.add_argument("--rw-map", action="append", metavar="PATH[:DEST]",
                           help="extra read-write mount (repeatable)")
            p.add_argument("--network", choices=["host", "none", "nat"])
            p.add_argument("--ssh-agent", action="store_true",
                           help="forward $SSH_AUTH_SOCK into the box")
        return p

    p = sub.add_parser("build", help="build or update the shared base image")
    p.add_argument("--refresh", action="store_true", help="pacman -Syu the base")
    p.add_argument("--force", action="store_true", help="delete and rebuild")
    p.set_defaults(func=cmd_build)

    p = common(sub.add_parser("init", help=f"write {PROJECT_CONFIG}"), maps=False)
    p.add_argument("--force", action="store_true")
    p.set_defaults(func=cmd_init)

    for verb, helptext in (("shell", "enter the project's sandbox"),
                           ("run", "run a command in the project's sandbox")):
        p = common(sub.add_parser(verb, help=helptext))
        p.add_argument("--root", action="store_true", help="enter as container root")
        p.add_argument("--packages", action="append", metavar="PKG")
        p.add_argument("cmd", nargs="*", help="command (prefix with --)")
        p.set_defaults(func=cmd_shell)

    p = common(sub.add_parser("up", help="boot the sandbox with systemd inside"))
    p.set_defaults(func=cmd_up)
    p = common(sub.add_parser("enter", help="shell into a booted sandbox"))
    p.set_defaults(func=cmd_enter)
    p = common(sub.add_parser("down", help="power off a booted sandbox"))
    p.set_defaults(func=cmd_down)

    p = sub.add_parser("ls", help="list boxes")
    p.set_defaults(func=cmd_ls)
    p = common(sub.add_parser("status", help="show the box for a project"))
    p.set_defaults(func=cmd_status)
    p = common(sub.add_parser("config", help="show effective config"))
    p.set_defaults(func=cmd_config)

    p = common(sub.add_parser("reset", help="discard container writes"))
    p.add_argument("-y", "--yes", action="store_true")
    p.set_defaults(func=cmd_reset)
    p = common(sub.add_parser("rm", help="delete the box"))
    p.add_argument("-y", "--yes", action="store_true")
    p.set_defaults(func=cmd_rm)

    args = ap.parse_args()
    DRY_RUN = args.dry_run
    if args.internal_handover:
        load_handover(args.internal_handover)
    elif not (DRY_RUN or args.command in UNPRIVILEGED_COMMANDS):
        ensure_root()
    args.func(args)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        raise SystemExit(130)
