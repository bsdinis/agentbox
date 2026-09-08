# agentbox

Per-project sandboxes for coding agents, built on `systemd-nspawn`.

You point it at a project directory and get a throwaway Arch Linux box that the
agent can wreck freely: it can `pacman -S` whatever it likes, run `sudo`, break
its own `/etc`, and it still cannot touch anything on the host except the
directories you mapped in.

```console
$ cd ~/dev/myproject
$ agentbox init                 # writes .agentbox.toml
$ agentbox shell                # you are now inside the sandbox
[bsdinis@myproject-4f1c ~/dev/myproject]$ sudo pacman -S cargo-nextest
[bsdinis@myproject-4f1c ~/dev/myproject]$ jj st
[bsdinis@myproject-4f1c ~/dev/myproject]$ claude --dangerously-skip-permissions
```

## What it gives you

| Requirement | How |
| --- | --- |
| Install packages inside the box | Full Arch rootfs with a working `pacman` + keyring; writes land in a per-project overlay |
| `sudo` that cannot reach the host | Container `root` is an unprivileged host UID via a user namespace; `wheel` is `NOPASSWD` inside |
| `git` and `jj` work normally | Both preinstalled; host `~/.gitconfig` and `~/.config/jj` mapped read-only; bind mounts are ID-mapped so file ownership matches and no `safe.directory` warnings appear |
| Read-write code, read-only references | Project dir is always read-write; `rw = [...]` and `ro = [...]` add more, mounted at their real host paths |
| One-command setup per project | `agentbox init` + `agentbox shell` |

## Documentation

* [docs/setup.md](docs/setup.md) — host requirements, install, building the base image
* [docs/usage.md](docs/usage.md) — the user guide: daily workflow, packages, VCS, agents
* [docs/configuration.md](docs/configuration.md) — every config key and CLI flag
* [docs/design.md](docs/design.md) — how it works and what the isolation actually buys you
* [docs/troubleshooting.md](docs/troubleshooting.md) — when something breaks
* [examples/](examples/) — ready-made `.agentbox.toml` files

## Install

```console
$ git clone <this repo> ~/dev/agentbox
$ ~/dev/agentbox/install.sh    # cargo build --release, then into ~/.local/bin
$ agentbox build               # one-time: build the shared base image (~5 min)
```

A single Rust binary with no run-time dependencies beyond `systemd-nspawn`
itself. `install.sh --system` installs to `/usr/local/bin`, which is what you
want if you later add the passwordless-sudo rule from
[docs/setup.md](docs/setup.md#passwordless-launches).

## Verify it works

```console
$ tests/verify.sh
```

Creates a throwaway project under `$XDG_CACHE_HOME`, then asserts the
promises above: it
installs a package inside the box, checks that container `root` maps to an
unprivileged host UID and cannot reach the host `/etc`, makes a git commit and
a jj operation on the mapped repo, confirms `ro` mounts reject writes even from
container root, and checks resource limits, booted mode and network modes. It
also asserts that the privilege handover leaves no environment file behind and
that `AGENTBOX_STATE` cannot redirect a privileged run, and finishes by
asserting that the host's own mounts are all still there. Ends with
a pass/fail count. `KEEP=1 tests/verify.sh` leaves the box behind to poke at.

```console
$ WITH_BUILD=1 tests/verify.sh
```

adds a full `agentbox build` into a throwaway state directory, so the build
path is covered too. It takes a few minutes and downloads packages, which is
why it is opt-in; the real base image is never touched.

## Commands

```
agentbox build [--refresh|--force]   build or update the shared base image
agentbox init [--force]              write .agentbox.toml for this project
agentbox shell [-- CMD ...]          create the box if needed, then enter it
agentbox run -- CMD ...               run one command inside the box
agentbox up | enter | down           booted mode: systemd as PID 1 inside
agentbox ls                          list boxes, overlay state, bytes written
agentbox status                      show the box and mount plan for this project
agentbox config                      show the effective config and .nspawn file
agentbox reset [-y]                  discard everything the container wrote
agentbox rm [-y]                     delete the box
```

Every command except `build`, `ls` and `init` accepts `--dir PATH` to act on a
project other than the current directory, plus `--map`, `--rw-map`,
`--network`, `--ssh-agent` as one-off overrides. `--dry-run` prints the exact
`systemd-nspawn` invocation and generated settings without changing anything.

## Layout

Source:

```
src/main.rs      CLI surface and dispatch
src/config.rs    layered TOML config, path expansion
src/sandbox.rs   one box: naming, paths, mounts, environment
src/nspawn.rs    overlay mount, generated .nspawn settings, launching
src/base.rs      building and refreshing the shared base image
src/host.rs      the privilege boundary: sudo re-exec, caller identity
contrib/         the original Python prototype, kept only for reference
```

State on disk:

```
/var/lib/agentbox/base                 shared read-only Arch rootfs (the lower layer)
/var/lib/agentbox/boxes/<box>/upper     everything this box has written
/var/lib/agentbox/boxes/<box>/meta.json project path, UID range, network mode
/var/lib/machines/<box>                 the assembled rootfs (so machinectl sees it)
/etc/systemd/nspawn/<box>.nspawn        generated settings: binds, UID map, network
/etc/systemd/system/systemd-nspawn@<box>.service.d/  resource caps for booted mode
```

## Caveats

nspawn with a user namespace is a solid boundary against accidents and ordinary
mistakes, and a reasonable one against hostile code, but it is a shared-kernel
container, not a VM. If you are running something you actively expect to attack
you, use a VM. See [docs/design.md](docs/design.md#security-model) for the
specifics of what is and is not isolated.

One limitation worth knowing before you point it at a project: directories
under `/tmp`, `/run`, `/dev`, `/proc` and `/sys` cannot be mapped into a box,
because systemd-nspawn mounts its own filesystem over each of them. A project
has to live somewhere else.
