# Configuration reference

Two files, both optional, plus CLI overrides. Precedence, lowest to highest:

1. built-in defaults
2. `~/.config/agentbox/config.toml` (respects `XDG_CONFIG_HOME`)
3. `./.agentbox.toml` in the project directory
4. command-line flags

Lists (`rw`, `ro`, `packages`, `pass_env`) **accumulate** across layers, keeping
first-seen order and dropping duplicates. Tables (`env`) merge key by key.
Scalars are replaced. So a project can add mounts but not remove the ones your
global config insists on.

## Keys

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | project directory name | Box name prefix. The box is `<name>-<6 hex of the project path>`, so two projects called `api` never collide. |
| `hostname` | string | box name | Hostname inside the box; shows in your prompt. |
| `network` | `"host"` \| `"none"` \| `"nat"` | `"host"` | See [usage.md](usage.md#networking). |
| `rw` | list of strings | `[]` | Extra read-write mounts. `"PATH"` or `"HOST:CONTAINER"`. |
| `ro` | list of strings | `["~/.gitconfig", "~/.config/jj", "~/.config/git"]` | Read-only mounts, same syntax. |
| `packages` | list of strings | `[]` | pacman packages installed into the box the first time it is created. |
| `env` | table | `{}` | Variables set inside the box, verbatim. |
| `pass_env` | list of strings | `["TERM", "COLORTERM", "LANG"]` | Host variables forwarded **if set**. Use for `ANTHROPIC_API_KEY`, `GITHUB_TOKEN`. |
| `ssh_agent` | bool | `false` | Bind `$SSH_AUTH_SOCK` to `~/.agentbox/ssh-agent.sock` and export it. |
| `shell` | path | your host shell if present in the image, else `/bin/bash` | Login shell inside the box. |
| `memory_max` | string | unset | `MemoryMax=` on the container scope, e.g. `"16G"`. |
| `cpu_quota` | string | unset | `CPUQuota=`, e.g. `"400%"`. |
| `tasks_max` | string | unset | `TasksMax=`. |
| `uid_base` | int | `1310720000` | Host UID that container UID 0 maps to. Multiple of 65536. Only meaningful before `agentbox build`. |

The three caps are unit properties rather than container settings, so they are
applied in the two places a box can be launched from: `run` and `shell` start
the container inside a transient scope of their own carrying the caps, and
`up` gets a drop-in on its `systemd-nspawn@` instance. `agentbox rm` removes
the drop-in.

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

## Project file example

```toml
# ~/dev/api/.agentbox.toml
name = "api"
network = "host"

packages = ["postgresql", "redis", "cargo-nextest"]

rw = ["~/dev/shared-proto"]
ro = [
  "~/.gitconfig",
  "~/.config/jj",
  "~/.cargo/registry",
  "~/dev/legacy-api",
]

pass_env = ["TERM", "COLORTERM", "LANG", "ANTHROPIC_API_KEY"]

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
| `--ssh-agent` | same | Forward the SSH agent socket. |
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
| `/var/lib/machines/<box>` | Mount point of the assembled rootfs. |
| `/etc/systemd/nspawn/<box>.nspawn` | Generated settings. Regenerated on every launch — edit the TOML, not this. |
| `/etc/systemd/system/systemd-nspawn@<box>.service.d/` | Generated drop-in carrying the resource caps for booted mode. Removed by `agentbox rm`. |
