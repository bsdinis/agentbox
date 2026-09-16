# Setup

## Host requirements

| Requirement | Why | Check |
| --- | --- | --- |
| systemd 256 or newer | needs the `owneridmap` bind option and `PrivateUsersOwnership=` | `systemctl --version` |
| Linux 5.12 or newer | ID-mapped mounts | `uname -r` |
| `systemd-container` package | provides `systemd-nspawn`, `machinectl` | `which systemd-nspawn` |
| `pacman` or `debootstrap` on the host | the base image is bootstrapped with one of them | Arch has `pacman`; on Debian/Ubuntu, `apt-get install debootstrap` |
| A Rust toolchain, to build | `agentbox` is a small Rust binary; nothing is needed at run time | `cargo --version` |
| A filesystem supporting ID-mapped mounts under your code | ext4, xfs, btrfs, f2fs all work | `findmnt -T ~` |
| `sudo` | `agentbox` re-execs itself as root | — |
| `socat` | relays the box-scoped ssh-agent's socket into the box; only exercised by projects that set `ssh_keys` | `which socat` |

The host does **not** need `systemd-networkd`, `arch-install-scripts`, or
`btrfs`. Network mode `nat` is the one exception — see below.

Note that systemd 256 is a hard floor, and a common thing to be short of:
Ubuntu 24.04 LTS ships systemd 255, which has no `owneridmap`, so boxes cannot
start there. Ubuntu 24.10 and later and Debian trixie are new enough.
`install-deps.sh` checks and says so.

## Which distribution is inside the box

The guest follows the host, because the host is what bootstraps it:

| Host | Guest | Bootstrapped with |
| --- | --- | --- |
| Arch (or a derivative: `ID_LIKE=arch`) | Arch | the host's own `pacman --root`, reusing its package cache and keyring |
| Debian | the same Debian release | `debootstrap --variant=minbase` |
| Ubuntu (or a derivative: `ID_LIKE=debian`) | the same Ubuntu release | `debootstrap --variant=minbase` |

The release, the mirror and the components are read off the host — the mirror
from the host's own `/etc/apt/sources.list`(`.d/<id>.sources`), so a build
reuses whatever mirror you already chose — and every one of them can be
overridden under `[base]` in the global config
([configuration.md](configuration.md#the-base-image)). Nothing but
`agentbox build` cares: once an image exists, the overlay, the bind plan, the
UID shift and the session machinery are the same whatever is inside it, and a
box's own `/etc/os-release` is what later commands consult when they need to
install a package into it.

Two differences are worth knowing before you use a Debian-family box:

* **`jj` is not in it.** Jujutsu is packaged in Debian unstable only, and not
  in Ubuntu at all, so it is absent from the built-in package list — a name
  that does not resolve would fail the whole build. Add it per project with
  `packages`, or install it inside the box.
* **`fd` is `fdfind`** on Debian, which renames it to avoid a clash. The image
  symlinks it back to `fd` in `/usr/local/bin`.

To add a third family, implement it in `src/distro.rs`: a bootstrap, a package
list, an install/upgrade command and a setup script. That module is the only
place in the tool that knows what a distribution is.

## nat networking

`nat` (the default) gives each box its own network namespace, so it never sees
the host's `localhost` services or abstract sockets such as the X11 display. It
still reaches the internet, through the host, which needs two things:

1. **`systemd-networkd` enabled on the host.** nspawn hands the box's veth to
   networkd, which runs a DHCP server for it and sets up the NAT. It is not
   enabled by default on Arch:

   ```console
   $ sudo systemctl enable --now systemd-networkd
   ```

2. **If you use NetworkManager, tell it to leave the container veths alone.**
   Otherwise NM claims `ve-*`/`vz-*` before networkd does, no DHCP answers, and
   the box gets only a link-local address with no route:

   ```console
   $ printf '[keyfile]\nunmanaged-devices=interface-name:ve-*;interface-name:vz-*\n' \
       | sudo tee /etc/NetworkManager/conf.d/agentbox-nspawn.conf
   $ sudo systemctl reload NetworkManager
   ```

3. **If Docker is installed, its `FORWARD` policy has to stop dropping the
   box's traffic.** Docker sets `iptables -P FORWARD DROP` and then whitelists
   only its own `docker0`, so a nat box gets an address and a default route
   from networkd and still cannot reach anything — masquerading is fine, the
   packets never get past `FORWARD`. `sudo iptables -S FORWARD | head -1` shows
   the policy; the narrow fix is to accept the container bridge explicitly:

   ```console
   $ sudo iptables -I DOCKER-USER -i ve-+ -j ACCEPT
   $ sudo iptables -I DOCKER-USER -o ve-+ -j ACCEPT
   ```

   `DOCKER-USER` is the chain Docker leaves for exactly this and never
   rewrites, but the rules are not persistent on their own — add them to
   whatever restores your firewall at boot.

If a nat box comes up without a route, `agentbox` warns on launch and points
here. Without this setup, use `network = "host"` (shares the host's stack - note
the exposure in [security.md](security.md)) or `network = "none"` (offline).

## Install

```console
$ git clone <this repo> ~/dev/agentbox
$ cd ~/dev/agentbox
$ ./install-deps.sh                # host packages: systemd-container, rust, nat networking
$ cargo install --path .           # builds and installs to ~/.cargo/bin/agentbox
```

`install-deps.sh` only installs host dependencies (`systemd-container` for
`systemd-nspawn`/`machinectl`, a Rust toolchain if you don't have one, and the
`systemd-networkd`/NetworkManager setup `nat` networking needs) via `pacman`
- it never touches agentbox itself. Once `cargo`, `systemd-container` and (for
`agentbox build`) `pacman` are present, `cargo install --path .` (or, once
published, `cargo install agentbox`) is the whole install: it builds and
copies the binary to `~/.cargo/bin/agentbox` (`$CARGO_INSTALL_ROOT/bin` if
you've set that), which most shells already have on `PATH`.

A missing `~/.config/agentbox/config.toml` is not an error, just no extra
defaults on top of the built-in ones (`config::load` folds in an empty layer
when the file is absent). Run this once, regardless of how you installed, to
get an editable copy - `cargo install` discards the source checkout once the
binary is built, so this is the only way to put `config.example.toml` on disk
without a checkout lying around:

```console
$ agentbox init --global          # writes ~/.config/agentbox/config.toml
```

It writes the same file that's checked in as `config.example.toml`, compiled
into the binary. `--force` overwrites an existing one. Edit the result to
change the base package list or the defaults every box inherits.

The optional AppArmor profile is the one piece of the source checkout that
genuinely has no substitute: `contrib/apparmor/agentbox-nspawn` has to be
installed as root from an actual file on disk (see
[contrib/apparmor/README.md](../contrib/apparmor/README.md)), so it still
needs a checkout (or the file downloaded on its own) regardless of how you
installed the binary. agentbox runs fine without it either way - the profile
is defense in depth, applied only when it is loaded, never a launch gate.

Nothing but the binary is needed at run time, so you can build once and copy
`target/release/agentbox` to another machine with the same systemd generation.

Development:

```console
$ cargo test          # unit tests for config layering, path expansion, naming
$ cargo clippy
$ tests/verify.sh     # the end-to-end check, described below
```

## Build the base image

```console
$ agentbox build
```

One-time, a few minutes, and every project shares the result. The stages are:

1. **Bootstrap** — the one stage that runs on the host, because there is
   nothing inside the image yet to run anything.
   On Arch: `pacman --root /var/lib/agentbox/bases/<id> -Sy base
   archlinux-keyring`, using the host's package cache and keyring, with
   `/proc`, `/sys`, `/dev` and `/run` mounted so install scriptlets work. This
   is what `pacstrap` does; doing it inline avoids depending on
   `arch-install-scripts`.
   On Debian/Ubuntu: `debootstrap --variant=minbase --components=…
   --include=ca-certificates <suite> <dir> <mirror>`, which does its own chroot
   setup and teardown, followed by writing the real archive list
   (`/etc/apt/sources.list.d/agentbox.sources`, with the `-updates` and
   `-security` pockets debootstrap leaves out) in place of the stub one.
2. **Keyring** — Arch only: `pacman-key --init && pacman-key --populate
   archlinux` *inside* the image, so the box can install and verify packages on
   its own later. debootstrap installs the archive keyring as part of stage 1.
3. **Packages** — the full `base_packages` list, installed from inside the
   image (`pacman -Sy`, or `apt-get install --no-install-recommends` with
   `policy-rc.d` blocking service starts — there is no init inside a
   half-built image for a maintainer script to talk to).
4. **Configure** — locale, a sandbox user mirroring your host
   username/UID/GID, `NOPASSWD` for the distribution's admin group (`%wheel` on
   Arch, `%sudo` on Debian) in `/etc/sudoers.d/00-agentbox`, an empty
   `/etc/machine-id`, and whatever else that family wants (pacman colour and
   parallel downloads; `/etc/default/locale` and the `fd` symlink on Debian).
5. **Shift** — one recursive `chown` of the whole image into the container UID
   range (`systemd-nspawn --private-users-ownership=chown`). This is the reason
   starting a box afterwards is instant: the on-disk ownership already matches
   the user namespace, so `PrivateUsersOwnership=off` is correct and nothing
   needs to be chowned or copied at launch.

Useful variants:

```console
$ agentbox build --refresh    # upgrade a copy of the current image in place
$ agentbox build --force      # bootstrap a brand new one from scratch
```

Neither one touches the image any running box already has mounted. The base
lives as a sequence of *generations* under `/var/lib/agentbox/bases/<id>/`
rather than one directory rewritten in place: `--refresh` copies the current
generation forward and upgrades the copy, `--force` bootstraps a new
generation unconditionally, and either way a `current` pointer only swaps
once the new generation is completely built. A box's overlay keeps whatever
generation it already had open as its lowerdir - overlayfs never revalidates
that once mounted - so a build no longer has to refuse while a box is running,
or unmount one that's merely idle, to stay safe: there is nothing left for it
to touch that a live mount depends on.

That used to be the failure mode this guarded against: overlayfs does not
tolerate its lower layer changing underneath a mount, so rewriting the shared
image in place while a box was still on it could leave a file half-visible —
listed by `ls` but `ENOENT` on open, which is
[its own troubleshooting entry](troubleshooting.md#a-program-the-base-image-has-is-missing-inside-a-box).
Writing every build to its own directory instead removes the failure mode
rather than just guarding it.

A generation only disappears once nothing references it any more — nothing is
`current`, and no *mounted* box's overlay still names it — which agentbox
sweeps for opportunistically (on `build`, `down`, `reset`, `rm`, `remount` and
`ls`) rather than as a step you run yourself. A box on an older-but-present
generation is not an error: `agentbox ls` shows it informationally
(`mounted (g..., current g...)`). It stays there on purpose - `agentbox down`
stops the box but never touches its overlay, so a plain relaunch (`shell`,
`run`, `up`) reattaches to the exact mount it already had - until you move it
yourself with `agentbox remount <box>` (needs the box stopped first) or
`agentbox reset <box>` (which also clears its writes). `stale` in `agentbox ls`
is reserved for a generation that's actually missing from disk, which *is*
repaired automatically by the next launch, or on demand by
`agentbox remount <box>`, since a box's writes live in `upper` on disk rather
than in the mount either way.

## Adding a package to every box

`base_packages` is the image's package list, so changing it means rebuilding
the image:

```console
$ agentbox ls                                  # who is on the image right now
$ $EDITOR ~/.config/agentbox/config.toml       # base_packages = [...]
$ agentbox build --refresh                     # safe to run with boxes above still up
$ agentbox down <box>                          # for each one you want on the new image
$ agentbox remount <box>                       # picks up the generation build just made
$ agentbox shell <box>
```

A box left running keeps serving the generation it already had mounted - that
is the whole point, not an oversight - so `remount` (or `reset`, which also
clears its writes) is what actually moves it onto the new one; a plain
relaunch of a box that was never stopped and remounted stays exactly where it
was.

Two things to know before editing that list:

* **It replaces the built-in one rather than adding to it** — unlike every
  other list in the configuration. Whatever you write is the whole image, so
  start from `DEFAULT_BASE_PACKAGES` (or `DEFAULT_BASE_PACKAGES_DEBIAN`, for a
  Debian-family image) in `src/config.rs` and add to it — both are reproduced
  in `config.example.toml`. A short
  list costs nothing on `--refresh`, which only ever installs and upgrades, but
  it is the entire image the next time you `--force`.
* **A refresh is a full upgrade** (`pacman -Syu`, or `apt-get dist-upgrade`),
  so it upgrades everything already in the image, not only what you added. It
  follows the generation it copies rather than what this host would build
  today, so it never changes the distribution inside the image — only
  `--force` does that.

An existing box keeps any file it had already modified, since that copy lives
in its own overlay. `agentbox reset <box>` if you want one to pick the new
image up completely. For a package only one project needs, `packages` in its
`.agentbox.toml` is cheaper than a rebuild — see
[usage.md](usage.md#installing-packages).

## Verify the install

```console
$ ~/dev/agentbox/tests/verify.sh
```

A throwaway project under `$XDG_CACHE_HOME` is created, exercised and removed
(`AGENTBOX_VERIFY_ROOT` moves it, though not to `/tmp`, which nspawn cannot
map into a box — see [usage.md](usage.md#mapping-directories)). Every
assertion maps to a promise in the README, so a failure names the thing that is
broken (`ro mount rejects container root too`, `container root maps to an
unprivileged host uid`, and so on). `KEEP=1` keeps the box for inspection.

## The sandbox user

The box gets a user with **your** name, UID and GID, and `/home/<you>` as its
home. That is deliberate: `~` and every project path is spelled identically
inside and outside, so stack traces, `path = "../lib"` dependencies, compiler
output and editor jumps all line up. The home directory itself is *not* your
host home — it lives in the box's overlay, so the agent can scribble in it
freely. Only the paths you map in are shared.

## Passwordless launches

`agentbox` re-execs itself under `sudo`, so you get a password prompt per
launch (subject to sudo's timestamp). If that annoys you:

```console
$ cargo install --path .                            # if not already installed
$ sudo install -Dm755 "$(command -v agentbox)" /usr/local/bin/agentbox
$ printf '%s ALL=(root) NOPASSWD: /usr/local/bin/agentbox\n' "$USER" \
  | sudo install -m 440 /dev/stdin /etc/sudoers.d/50-agentbox
```

Be clear-eyed about what that does: the rule lets any process running as you
run `agentbox` as root with arbitrary arguments, and `agentbox` mounts host
directories into containers on request. It is a root-equivalent grant.

Copy the binary into a root-owned directory first if you take this route. A
NOPASSWD rule naming a binary in a directory you can write to (`~/.cargo/bin`,
or a checkout's `target/release`) is strictly worse than no rule at all:
anything running as you can replace that file and become root. Once it's
under `/usr/local/bin` (root-owned, unless you've made it writable), the rule
grants only what `agentbox` itself can do.

## Uninstall

```console
$ agentbox ls                             # see what exists
$ agentbox rm <box>                       # per box
$ sudo rm -rf /var/lib/agentbox           # base image and all overlays
$ sudo rm -f /etc/systemd/nspawn/*.nspawn # generated settings (check first)
$ cargo uninstall agentbox               # or: rm the binary wherever you installed it
```
