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
* [docs/security.md](docs/security.md) — the trust model: read-write mounts, and running Claude in a box
* [docs/troubleshooting.md](docs/troubleshooting.md) — when something breaks
* [examples/](examples/) — ready-made `.agentbox.toml` files

## Install

```console
$ git clone <this repo> ~/dev/agentbox
$ cd ~/dev/agentbox
$ ./install-deps.sh            # host packages: systemd-container, rust, nat networking
$ cargo install --path .       # builds and installs to ~/.cargo/bin/agentbox
$ agentbox build               # one-time: build the shared base image (~5 min)
```

A single Rust binary with no run-time dependencies beyond `systemd-nspawn`
itself. See [docs/setup.md](docs/setup.md#install) for installing to a
root-owned directory instead, which is what you want if you add the
passwordless-sudo rule from
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
agentbox init [DIR] [--force]        write .agentbox.toml for this project
agentbox shell [BOX] [-- CMD ...]    boot or attach to the box, open a shell
agentbox run [BOX] -- CMD ...        boot or attach to the box, run one command
agentbox up | down [BOX]             boot a box and keep it up | power it off
agentbox remount [BOX]               remount on the current base image, keeping writes
agentbox ls                          list boxes, overlay state, bytes written
agentbox status [BOX]                show the box and mount plan for this project
agentbox config [BOX]                show the effective config and .nspawn file
agentbox reset [BOX] [-y]            discard everything the container wrote
agentbox rm [BOX] [-y]               delete the box
```

Every command except `build`, `ls` and `init` takes an optional box to act on:
a name as `agentbox ls` prints it, or the project directory it was made for.
With no box named, they act on the box for the current directory. `--map`,
`--rw-map`, `--network` and `--ssh-key` are one-off overrides, and `--dry-run`
prints the exact commands and generated settings without changing anything.

## Layout

Source:

```
src/main.rs      CLI surface and dispatch
src/config.rs    layered TOML config, path expansion
src/sandbox.rs   one box: naming, paths, mounts, environment
src/nspawn.rs    overlay mount, generated .nspawn settings, launching
src/base.rs      building and refreshing the shared base image
src/host.rs      the privilege boundary: sudo re-exec, caller identity
contrib/         the original Python prototype (reference) and an optional
                 AppArmor profile (contrib/apparmor/)
```

State on disk:

```
/var/lib/agentbox/bases/<id>/           one base generation (the lower layer); bases/current names the newest
/var/lib/agentbox/boxes/<box>/overlay.id which generation this box's overlay is mounted on
/var/lib/agentbox/boxes/<box>/upper     everything this box has written
/var/lib/agentbox/boxes/<box>/meta.json project path, UID range, network mode
/var/lib/agentbox/boxes/<box>/runtime/  who booted the box, and its live sessions
/var/lib/machines/<box>                 the assembled rootfs (so machinectl sees it)
/etc/systemd/nspawn/<box>.nspawn        generated settings: binds, UID map, network
/etc/systemd/system/systemd-nspawn@<box>.service.d/  resource caps, and the AppArmor profile when loaded, on the box's unit
```

## Upgrading

Two recent changes are **not** backward compatible:

* **The default network is now `nat`, not `host`.** By default a box no longer shares the
  host's network namespace: it still reaches the internet (via NAT), but it can no longer reach
  services on the host's `localhost`. If a box needs to talk to something running on the host — a
  local database, say — set `network = "host"` in its `.agentbox.toml`. `nat` requires
  `systemd-networkd` enabled on the host; see [docs/usage.md](docs/usage.md#networking). The reason
  for the change: host networking also exposes the host's loopback services and abstract sockets
  (such as the X11 display) to the box — see [docs/security.md](docs/security.md).
* **`ssh_agent` is replaced by `ssh_keys`.** Forwarding the host's entire agent is gone; list the
  specific keys a box may use instead (`ssh_keys = ["~/.ssh/id_ed25519_x"]`) and agentbox forwards
  a box-scoped agent holding only those (added with `ssh-add -c`, confirm-on-use). A leftover
  `ssh_agent` key is rejected at load with a message pointing at `ssh_keys`.

## Caveats

nspawn with a user namespace is a solid boundary against accidents and ordinary
mistakes, and a reasonable one against hostile code, but it is a shared-kernel
container, not a VM. If you are running something you actively expect to attack
you, use a VM. See [docs/design.md](docs/design.md#security-model) for the
specifics of what is and is not isolated.

For a mandatory-access-control layer behind the user namespace, seccomp and
mount plan, an optional AppArmor profile ships in
[contrib/apparmor/](contrib/apparmor/); agentbox applies it automatically once it
is loaded, and runs fine without it. It is per-distro (AppArmor, not SELinux).

One limitation worth knowing before you point it at a project: directories
under `/tmp`, `/run`, `/dev`, `/proc` and `/sys` cannot be mapped into a box,
because systemd-nspawn mounts its own filesystem over each of them. A project
has to live somewhere else.
