# Setup

## Host requirements

| Requirement | Why | Check |
| --- | --- | --- |
| systemd 256 or newer | needs the `owneridmap` bind option and `PrivateUsersOwnership=` | `systemctl --version` |
| Linux 5.12 or newer | ID-mapped mounts | `uname -r` |
| `systemd-container` package | provides `systemd-nspawn`, `machinectl` | `which systemd-nspawn` |
| `pacman` on the host | the base image is bootstrapped with it | Arch and derivatives only |
| A Rust toolchain, to build | `agentbox` is a small Rust binary; nothing is needed at run time | `cargo --version` |
| A filesystem supporting ID-mapped mounts under your code | ext4, xfs, btrfs, f2fs all work | `findmnt -T ~` |
| `sudo` | `agentbox` re-execs itself as root | — |

The host does **not** need `systemd-networkd`, `arch-install-scripts`, or
`btrfs`. Network mode `nat` is the one exception — see below.

Non-Arch hosts: everything except `agentbox build` is distribution-agnostic.
To run on Debian/Fedora, replace `bootstrap()` in `src/base.rs` with
`debootstrap`/`dnf --installroot` and adjust `DEFAULT_BASE_PACKAGES` in
`src/config.rs`. Nothing else in the tool knows which distribution is inside
the box.

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

If a nat box comes up without a route, `agentbox` warns on launch and points
here. Without this setup, use `network = "host"` (shares the host's stack - note
the exposure in [security.md](security.md)) or `network = "none"` (offline).

## Install

```console
$ git clone <this repo> ~/dev/agentbox
$ ~/dev/agentbox/install.sh              # or: install.sh --system
```

This runs `cargo build --release`, installs the binary to `~/.local/bin`
(`--system` puts it in `/usr/local/bin` instead), and copies
`config.example.toml` to `~/.config/agentbox/config.toml` if you have no config
yet. Edit that file to change the base package list or the defaults every box
inherits.

`cargo install --path ~/dev/agentbox` (or, once published, `cargo install
agentbox`) works too, and needs nothing from `install.sh`: a missing
`~/.config/agentbox/config.toml` is not an error, just no extra defaults on
top of the built-in ones (`config::load` folds in an empty layer when the
file is absent). The one thing `install.sh` does that a plain `cargo install`
cannot is put a copy of `config.example.toml` on disk for you to edit, since
`cargo install` discards the source checkout once the binary is built -
`cargo install` from crates.io builds in a scratch directory, and even
`--path` only ever installs the compiled binary. Run this once, whichever way
you installed:

```console
$ agentbox init --global          # writes ~/.config/agentbox/config.toml
```

It writes the same template `install.sh` would have copied, since it's the
same file, compiled into the binary. `--force` overwrites an existing one.

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

1. **Bootstrap** — `pacman --root /var/lib/agentbox/bases/<id> -Sy base archlinux-keyring`,
   using the host's package cache and keyring, with `/proc`, `/sys`, `/dev` and
   `/run` bind mounted so install scriptlets work. This is what
   `pacstrap` does; doing it inline avoids depending on `arch-install-scripts`.
2. **Keyring** — `pacman-key --init && pacman-key --populate archlinux` *inside*
   the image, so the box can install and verify packages on its own later.
3. **Packages** — the full `base_packages` list, installed from inside the image.
4. **Configure** — locale, `/etc/pacman.conf` colour and parallel downloads, a
   sandbox user mirroring your host username/UID/GID, `%wheel NOPASSWD` in
   `/etc/sudoers.d/00-agentbox`, and an empty `/etc/machine-id`.
5. **Shift** — one recursive `chown` of the whole image into the container UID
   range (`systemd-nspawn --private-users-ownership=chown`). This is the reason
   starting a box afterwards is instant: the on-disk ownership already matches
   the user namespace, so `PrivateUsersOwnership=off` is correct and nothing
   needs to be chowned or copied at launch.

Useful variants:

```console
$ agentbox build --refresh    # pacman -Syu a copy of the current image
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
  start from `DEFAULT_BASE_PACKAGES` in `src/config.rs` and add to it. A short
  list costs nothing on `--refresh`, which only ever installs and upgrades, but
  it is the entire image the next time you `--force`.
* **A refresh is a `pacman -Syu`**, so it upgrades everything already in the
  image, not only what you added.

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
$ ./install.sh --system     # so the binary lives somewhere only root can write
$ printf '%s ALL=(root) NOPASSWD: /usr/local/bin/agentbox\n' "$USER" \
  | sudo install -m 440 /dev/stdin /etc/sudoers.d/50-agentbox
```

Be clear-eyed about what that does: the rule lets any process running as you
run `agentbox` as root with arbitrary arguments, and `agentbox` mounts host
directories into containers on request. It is a root-equivalent grant.

Install with `--system` first if you take this route. A NOPASSWD rule naming a
binary in a directory you can write to (`~/.local/bin`, or a checkout's
`target/release`) is strictly worse than no rule at all: anything running as
you can replace that file and become root. With `--system` the binary is
root-owned, so the rule grants only what `agentbox` itself can do.

## Uninstall

```console
$ agentbox ls                             # see what exists
$ agentbox rm <box>                       # per box
$ sudo rm -rf /var/lib/agentbox           # base image and all overlays
$ sudo rm -f /etc/systemd/nspawn/*.nspawn # generated settings (check first)
$ rm ~/.local/bin/agentbox
```
