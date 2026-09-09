# Configuration reference

Two files, both optional, plus CLI overrides. Precedence, lowest to highest:

1. built-in defaults
2. `~/.config/agentbox/config.toml` (respects `XDG_CONFIG_HOME`)
3. `./.agentbox.toml` in the project directory
4. command-line flags

Lists (`rw`, `ro`, `packages`, `pass_env`) **accumulate** across layers, keeping
first-seen order and dropping duplicates. Tables (`env`) merge key by key.
Scalars are replaced. So a project can add mounts but not remove the ones the
built-in defaults or your global config insist on - which also means there is
no reason to restate them. `agentbox init` writes a file with the additions
left blank, and `agentbox config` prints what a project actually resolves to.

## Keys

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | project directory name | Box name prefix. The box is `<name>-<6 hex of the project path>`, so two projects called `api` never collide. |
| `hostname` | string | box name | Hostname inside the box; shows in your prompt. |
| `network` | `"host"` \| `"none"` \| `"nat"` | `"nat"` | See [usage.md](usage.md#networking). |
| `rw` | list of strings | `[]` | Extra read-write mounts. `"PATH"` or `"HOST:CONTAINER"`. A box can write these, and the host may later execute what it wrote — see [security.md](security.md). |
| `ro` | list of strings | `["~/.gitconfig", "~/.config/jj", "~/.config/git"]` | Read-only mounts, same syntax. |
| `packages` | list of strings | `[]` | pacman packages installed into the box the first time it is created. |
| `env` | table | `{}` | Variables set inside the box, verbatim. |
| `pass_env` | list of strings | `["TERM", "COLORTERM", "LANG"]` | Host variables forwarded **if set**. Use for `ANTHROPIC_API_KEY`, `GITHUB_TOKEN`. |
| `ssh_keys` | list of strings | `[]` | Private-key paths the box may use. agentbox starts a **dedicated** ssh-agent holding only these keys (added with `ssh-add -c`, so each use prompts you on the host to confirm) and binds *that* agent's socket to `~/.agentbox/ssh-agent.sock`, exporting `SSH_AUTH_SOCK`. The host's own `$SSH_AUTH_SOCK` is never forwarded. Empty (the default) means no agent and no forwarding. Fails closed: a missing key aborts the launch. |
| `shell` | path | your host shell if present in the image, else `/bin/bash` | Login shell inside the box. |
| `background` | string | unset | Terminal background while a session runs. Unset means no tint, so the terminal keeps its own colour. `"auto"` lets `systemd-run` pick its own per-box tint; an ANSI SGR background (`"40"`..`"47"`, `"48;5;N"`, `"48;2;R;G;B"`) picks a specific one. |
| `address_families` | string | unset | Socket address families the box may use, as `RestrictAddressFamilies=`. Unset applies no filter and says so explicitly. Space-separated names (`"AF_INET AF_INET6 AF_UNIX AF_NETLINK"`), `~` to prohibit one, or `"none"`. Needs systemd 261; omitted below that. |
| `memory_max` | string | unset | `MemoryMax=` on the container scope, e.g. `"16G"`. |
| `cpu_quota` | string | unset | `CPUQuota=`, e.g. `"400%"`. |
| `tasks_max` | string | unset | `TasksMax=`. |
| `apparmor` | bool | unset | Apply the shipped AppArmor profile as a defense-in-depth LSM layer. Unset means on-if-available: applied when the profile is loaded on the host, silently skipped otherwise. `true` also warns when it is expected but unavailable; `false` opts out. See below and [contrib/apparmor/README.md](../contrib/apparmor/README.md). |
| `uid_base` | int | `1310720000` | Host UID that container UID 0 maps to. Multiple of 65536. Only meaningful before `agentbox build`. |

The three caps are unit properties rather than container settings. Every launch
boots the box's `systemd-nspawn@<box>.service`, so they live in one place: a
drop-in on that unit, applied identically to `run`, `shell` and `up` and
covering every session attached to the box. `agentbox rm` removes the drop-in.

`address_families` goes to both places it can: `RestrictAddressFamilies=` in the
settings file, which is all a booted box reads, and `--restrict-address-families=`
on the bootstrap launch, which is what silences systemd-nspawn's notice about the
coming default. Both need systemd 261 and are left out below it, where the notice
does not exist either.

`background` tints the terminal of a session for as long as it runs. It has no
`.nspawn` settings key, so it is passed on the `systemd-run` command line of an
interactive `shell`/`run` attach — the session a person watches. A redirected
(`--pipe`) run and the bootstrap install have no terminal to colour and get the
"no tint" default; `up` attaches no session at all.

`apparmor` is, like the caps, a unit property with nowhere to go in the `.nspawn`
file, so it is applied in the same two places: `run` and `shell` add
`--property=AppArmorProfile=-agentbox-nspawn` to the transient scope the box runs
in, and `up` gets a `60-agentbox-apparmor.conf` drop-in on its
`systemd-nspawn@` instance. Both use the leading `-`, which makes application
non-fatal, and agentbox only wires the profile in when it is actually loaded on
the host, so a box never refuses to launch because the LSM layer is missing —
this is defense in depth, not a gate. The profile confines the `systemd-nspawn`
process and, by inheritance, the box's own init and everything it runs; what it
denies and how to install it (per-distro — AppArmor, not SELinux) is in
[contrib/apparmor/README.md](../contrib/apparmor/README.md). `--dry-run` only
shows the profile when run from a context that can read the kernel's loaded-
profile list (i.e. as root), since that list is root-readable.

Global-config-only:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `base_packages` | list of strings | see `DEFAULT_BASE_PACKAGES` in `src/config.rs` | Packages in the shared base image. Apply with `agentbox build --refresh`. |

In the global file you may put the per-box keys either at the top level or
under a `[defaults]` table; both work, and `[defaults]` is clearer.

## Mount path semantics

* `~` on the source side is your host home; on the destination side it is the
  sandbox user's home. The usernames match, so it is the same string.
* `$VAR` is expanded from your host environment.
* Source paths are resolved (symlinks followed) before mounting.
* A bare path maps to the identical path inside the box. Prefer this: identical
  absolute paths are what make relative path dependencies work.
* Destinations are created inside the box if missing, owned by the sandbox
  user, because `owneridmap` derives the mapping from the destination's owner.
* Destinations under `/tmp`, `/run`, `/dev`, `/proc` or `/sys` are refused:
  systemd-nspawn mounts its own filesystem over each of those, after which the
  prepared destination is hidden and its ownership no longer ours to set. A
  project directory under one of them cannot be boxed at all.
* The project directory is always mounted read-write and cannot be dropped.
  If you list it again explicitly the first entry wins.
* Nonexistent sources are skipped with a warning.

### Nesting: one mount inside another

Mapping a directory and something below it is supported, and **the inner one
wins** — in both directions. `agentbox status` lists mounts in the order
agentbox generates them (project, then `rw`, then `ro`), which is *not* the
order they are applied: systemd-nspawn sorts its custom mounts by destination
path before mounting any of them, so a parent is always mounted, made
read-only and ID-mapped before its children are laid on top. Order within
`.agentbox.toml` therefore does not matter.

```toml
# a read-only reference tree with one writable scratch area inside it
ro = ["~/dev/reference"]
rw = ["~/dev/reference/scratch"]

# a writable state directory with one file held read-only inside it
rw = ["~/.claude"]
ro = ["~/.claude/.credentials.json"]
```

Three consequences worth knowing:

* **A missing inner mount point is created, and where it is created differs.**
  nspawn creates a destination that does not exist. Inside a read-write parent
  that write goes through the bind to the **host** directory, so a typo leaves a
  real empty directory or file in your home. Inside a read-only parent it cannot
  be created at all and the launch fails — so an inner mount under a `ro` parent
  only works if the path already exists in the parent's source.
* **A read-only *file* inside a read-write parent is protected oddly.** It
  cannot be written in place, but it also cannot be renamed over or deleted:
  those fail with `EBUSY` rather than `EROFS`, because the destination is a
  mount point. A program that saves by writing a temporary file and renaming it
  will fail in a way that does not look like a permissions problem.
* **Nesting is not deduplication.** Entries are deduplicated only on an exact
  destination match, first one wins, and `rw` is processed before `ro`. So the
  *same* path in both lists resolves to read-write, and the project directory —
  claimed before either list — can never be made read-only as a whole. Making a
  subdirectory of the project read-only does work, since that is nesting:
  `ro = ["~/dev/api/vendor"]`.

## Project file example

```toml
# ~/dev/api/.agentbox.toml
name = "api"
network = "host"

packages = ["postgresql", "redis", "cargo-nextest"]

# Added to the defaults above, not instead of them: ~/.gitconfig,
# ~/.config/git and ~/.config/jj are mapped and TERM, COLORTERM and LANG
# forwarded whether or not this file mentions them.
rw = ["~/dev/shared-proto"]
ro = [
  "~/.cargo/registry",
  "~/dev/legacy-api",
]

pass_env = ["ANTHROPIC_API_KEY"]

memory_max = "16G"
cpu_quota  = "600%"

[env]
RUST_BACKTRACE = "1"
DATABASE_URL = "postgres://localhost/api_dev"
```

Check what a config actually resolves to, including the generated nspawn
settings, without touching anything:

```console
$ agentbox config
$ agentbox --dry-run shell
```

`agentbox config` and `agentbox init` are the only commands that do not need
root, so they never prompt for a password.

## CLI flags

| Flag | Applies to | Effect |
| --- | --- | --- |
| `--dir PATH` | all but `build`, `ls` | Act on another project directory. |
| `--map PATH[:DEST]` | `shell`, `run`, `up`, `status`, `config` | Extra read-only mount. Repeatable. |
| `--rw-map PATH[:DEST]` | same | Extra read-write mount. Repeatable. |
| `--network MODE` | same | Override network mode for this launch. |
| `--ssh-key PATH` | same | Add a private key to the box's dedicated ssh-agent (confirm-on-use). Repeatable. The host's own agent is never forwarded. |
| `--packages PKG` | `shell`, `run` | Extra packages on box creation. Repeatable. |
| `--root` | `shell`, `run` | Run as container root instead of the sandbox user. |
| `--dry-run` | all | Print the commands and the `.nspawn` file, change nothing. |
| `-y`/`--yes` | `reset`, `rm` | Skip the confirmation prompt. |

One-off overrides are not persisted. If you find yourself repeating a flag, put
it in `.agentbox.toml`.

## Where state lives

| Path | Contents |
| --- | --- |
| `/var/lib/agentbox/base` | Shared base image, the overlay lower layer. |
| `/var/lib/agentbox/boxes/<box>/upper` | Every byte this box has written. |
| `/var/lib/agentbox/boxes/<box>/work` | overlayfs scratch area. Do not touch. |
| `/var/lib/agentbox/boxes/<box>/meta.json` | Project path, UID base, network mode. |
| `/var/lib/agentbox/boxes/<box>/ssh-agent/` | Present only with `ssh_keys` set: the box-scoped ssh-agent's socket and pid, mode 0700, owned by you. Torn down by `down`, `reset` and `rm`. |
| `/var/lib/machines/<box>` | Mount point of the assembled rootfs. |
| `/etc/systemd/nspawn/<box>.nspawn` | Generated settings. Regenerated on every launch — edit the TOML, not this. |
| `/etc/systemd/system/systemd-nspawn@<box>.service.d/` | Generated drop-ins for booted mode: `50-agentbox-caps.conf` (resource caps) and, when the AppArmor profile is loaded, `60-agentbox-apparmor.conf`. Removed by `agentbox rm`. |
