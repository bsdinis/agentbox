# Troubleshooting

First move, always:

```console
$ agentbox --dry-run shell     # the exact nspawn line and generated settings
$ agentbox status              # box name, mount plan, UID range
$ agentbox ls                  # overlay mounted? booted? how much written?
```

`--dry-run` needs no root, so it never prompts.

## `Unknown setting PrivateUsersOwnership` or `Failed to parse bind mount option owneridmap`

Your systemd is too old. `PrivateUsersOwnership=` and the `owneridmap` bind
option both landed in systemd 256; the tool depends on them and there is no
graceful fallback.

```console
$ systemctl --version | head -1
```

On systemd 249-255 the equivalent of the first is `PrivateUsersChown=`, but
`owneridmap` has no substitute, so mapped directories would show up as
`nobody`. Upgrade rather than patching around it.

## `cargo build` fails

```console
$ cargo --version     # 1.74 or newer
$ cargo clean && cargo build --release
```

The dependencies are `clap`, `serde`, `toml`, `serde_json`, `sha2`, `libc` and
`anyhow`, all from crates.io. If you are offline and have them vendored,
`cargo build --release --offline` works.

## `Failed to create ID-mapped mount` / mounts show up as `nobody`

The `owneridmap` bind option needs ID-mapped mount support in the *source*
filesystem. ext4, xfs, btrfs and f2fs have it; NFS, CIFS, FUSE mounts
(sshfs, gocryptfs, many `~/Documents` sync tools) and overlayfs do not.

```console
$ findmnt -T ~/dev/myproject    # check what the source really lives on
```

Workarounds: copy the tree onto a local filesystem, or map it with `ro` and
accept `nobody` ownership (reads still work if the files are world-readable).

## `Permission denied` writing to a mapped directory

Three things to check, in order:

1. `agentbox status` — is it in the `rw` list, or did it land in `ro`?
2. Is the host directory owned by *you*? `owneridmap` maps the source's owner
   onto the sandbox user; a subdirectory owned by another host user, or by
   `root`, appears as `nobody` and is not writable.
3. Did you create the box before adding the mount? Mount points are created at
   box creation. `agentbox rm && agentbox shell` (or `reset`) re-creates them.

## `cannot map ... systemd-nspawn mounts its own /tmp there`

A project, or a mapped directory, cannot live under `/tmp`, `/run`, `/dev`,
`/proc` or `/sys`. systemd-nspawn mounts its own filesystem over each of those
inside the container, and it does so *before* applying the binds - so the
mount point agentbox prepared underneath is hidden, nspawn creates its own
owned by container root, and `owneridmap` maps you onto that instead of onto
the sandbox user. A mode 700 project then cannot even be entered, which is why
this is refused up front rather than failing later as a `chdir` error.

Move the project somewhere else. `$HOME`, `/srv`, `/var/tmp` and anything else
outside that list are all fine.

## `no base image yet - run agentbox build first`

Expected on a fresh install. If you *have* built it, check the image is where
the tool looks:

```console
$ sudo ls /var/lib/agentbox/base/usr
```

## Base build fails during bootstrap

Usually the host's pacman keyring or mirrors:

```console
$ sudo pacman -Sy archlinux-keyring     # refresh host keyring first
$ sudo pacman-key --refresh-keys        # if signatures are rejected
$ head -5 /etc/pacman.d/mirrorlist      # a dead mirror stalls the bootstrap
```

The build reuses the host's package cache (`/var/cache/pacman/pkg`) and
keyring (`/etc/pacman.d/gnupg`), so a broken host pacman breaks the build. A
partial image is safe to discard: `agentbox build --force`.

## `pacman` inside the box rejects signatures

The box's own keyring failed to initialise. Fix it in the box:

```console
[box]$ sudo pacman-key --init && sudo pacman-key --populate archlinux
[box]$ sudo pacman -Sy archlinux-keyring
```

If a fresh box has the same problem, the base image is at fault:
`agentbox build --force`.

## Overlay will not mount

```console
$ sudo dmesg | tail -20         # overlayfs is chatty about why
```

Common causes: a stale `work` directory after an unclean shutdown (fix with
`agentbox reset`), or `/var/lib/agentbox` on a filesystem without `trusted.*`
xattr support. overlayfs also refuses to use an upperdir that is itself on
overlayfs, so do not relocate the state directory into one.

## No network inside the box

```console
[box]$ cat /etc/resolv.conf
[box]$ ping -c1 1.1.1.1
```

With `network = "host"` the box shares your namespace and `/etc/resolv.conf` is
copied from the host at start, so DNS breaks only if the host's is broken *and*
the box was started before you fixed it — restart the box.

With `network = "nat"` you need `systemd-networkd` enabled on the host, and the
box must be in booted mode (`agentbox up`) so it can configure `host0`. Check
the host side with `networkctl status ve-<box>`.

With `network = "none"` there is no network by design.

## `agentbox up` fails or the box will not boot

```console
$ systemctl status systemd-nspawn@<box>.service
$ journalctl -u systemd-nspawn@<box>.service -n 50
```

Booted mode needs a real init in the image (`base` provides systemd) and
the rootfs mounted at `/var/lib/machines/<box>`. Run `agentbox shell` once
first: it mounts the overlay and writes the settings file that booted mode
depends on.

## `machinectl shell` says the machine is unknown

`agentbox shell` runs with `--register=no`, so direct-mode boxes deliberately
do not appear in `machinectl list`. Use `agentbox up` if you want a registered,
booted machine.

## Overlay is still mounted after a reboot

It is not — mounts do not survive a reboot, and `agentbox shell` remounts
on demand. `agentbox ls` will show `OVERLAY` as `-` until then. Nothing is
lost; the writes are in `upper` on disk.

## A box is eating disk

```console
$ agentbox ls                                    # WRITES column
$ sudo du -sh /var/lib/agentbox/boxes/*/upper    # per box
$ agentbox reset --dir <project>                 # back to the base image
```

Package caches inside the box are the usual culprit: `[box]$ sudo pacman -Scc`.

## sudo asks for a password on every launch

Expected: `agentbox` re-execs itself as root. Either rely on sudo's timestamp,
or install the `NOPASSWD` rule described in [setup.md](setup.md#passwordless-launches),
after reading the caveat there.

## An agent broke the box beyond repair

```console
$ agentbox reset      # discard all container writes, keep the code
```

Your project is a bind mount, so this cannot lose committed or uncommitted
work in the repository. If the agent broke the *code*, that is what `jj op log`
and `git reflog` are for — on the host.
