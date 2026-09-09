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

1. **Bootstrap** — `pacman --root /var/lib/agentbox/base -Sy base archlinux-keyring`,
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
$ agentbox build --refresh    # pacman -Syu the base, keeps existing boxes' overlays
$ agentbox build --force      # delete and rebuild from scratch
```

`--refresh` updates the shared lower layer under running boxes. Existing boxes
keep any file they have already modified (that copy lives in their overlay),
so refresh, then `agentbox reset` a box if you want it to pick everything up.

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
