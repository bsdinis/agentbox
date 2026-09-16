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

## `<program>: not found in box <name>`

The box does not have that program. A box is a separate install of the
distribution the host builds: it starts from the shared base image and whatever `packages` you asked for, so nothing
you installed on the host is in it unless you put it there. `claude`, `codex`
and friends are the usual case — see
[usage.md](usage.md#running-agents-inside).

```console
[box]$ sudo pacman -S PKG                          # once, in this box (Arch)
[box]$ sudo apt-get install PKG                    # ... or Debian/Ubuntu
[box]$ sudo npm install -g @anthropic-ai/claude-code
```

agentbox names the right one for you: the "not found in box" message reads the
box's own `/etc/os-release` before suggesting a command.

```toml
packages = ["PKG"]          # .agentbox.toml, installed when the box is created
ro = ["/usr/bin/claude", "/opt/claude-code"]    # or map the host's copy
```

Mapping the host's copy is the cheapest route for something already installed
there, but map everything it needs: a wrapper in `/usr/bin` that execs a
payload elsewhere is useless without the payload. The message names the host
path it found, if there is one.

Older versions let systemd-nspawn discover this instead, which surfaced as
`execv(claude) failed: No such file or directory` after the box was already
created, its login shell set and its packages installed.

If the program *is* installed in the box and `ls` still lists it, this is a
different problem — see the next entry.

## A program the base image has is missing inside a box

The symptom is distinctive: the file is listed, but nothing can touch it.

```console
[box]$ ls -l /usr/bin/nvim
-????????? ? ? ? ?            ? /usr/bin/nvim
[box]$ nvim
bash: /usr/bin/nvim: No such file or directory
[box]$ pacman -Qkk neovim          # dpkg -V neovim on a Debian/Ubuntu box
neovim: 2311 total files, 1 altered file
```

The package manager is telling the truth: the package is installed and every other file in
it is fine. What is broken is this box's *view* of the base image. The image
underneath a box's overlay was rewritten while that overlay was mounted on it,
and overlayfs does not tolerate a lower layer changing underneath it. `readdir`
picks the new name up from the lower layer, but a lookup that resolved
*before* the rewrite (any `which nvim` that came back empty) is still cached
as a miss. So the one path something had already asked about stays missing,
while everything the refresh added under paths nobody had touched works
normally — which is why it looks like a single corrupt file rather than a
stale mount.

`agentbox build --refresh|--force` cannot arrange this any more: the base
lives as a sequence of *generations* (`/var/lib/agentbox/bases/<id>/`), and a
build always writes a brand new one rather than rewriting the one a running
box has open — `--refresh` copies the current generation forward and upgrades
the copy, `--force` bootstraps a fresh one from scratch, and either way the
directory any live overlay already has as its lowerdir is never touched. A
build no longer refuses while a box is alive, and no longer needs to unmount
anything, because there is nothing left it could break by running.

So this now shows up only two ways: a generation genuinely vanishing out from
under a mounted box (agentbox never deletes one anything still has mounted,
so this means a bug), or a mount left by a version of agentbox that predates
generations, with no matching record at all. Either reads as `stale` in
`agentbox ls`:

```console
$ agentbox ls
BOX             PROJECT           OVERLAY  BOOTED    WRITES
myproj-a1b2c3d  /home/you/myproj  stale    inactive  184M
```

A box mounted on an older generation than the newest one is *not* this
problem, and is not `stale` — it is the ordinary state of a box that predates
the last `agentbox build`, shown informationally instead:

```console
$ agentbox ls
BOX             PROJECT           OVERLAY                       BOOTED    WRITES
myproj-a1b2c3d  /home/you/myproj  mounted (g...041, current g...199)  active    184M
```

Nothing is wrong there; the box just hasn't been moved onto the newer
generation, and nothing does that on its own while its overlay stays mounted -
`agentbox down` stops the box but never unmounts it, so a plain relaunch
reattaches to the exact same mount it already had. `agentbox remount <box>`
(needs the box stopped first) moves it on purpose; `agentbox reset <box>` does
too, but by also clearing its writes.

Starting the box fixes actual staleness (the `stale` case above): a launch
remounts a stale overlay before the box boots, since there is nothing valid
left for it to keep. `agentbox remount <box>` does the same without starting
anything, and is what to reach for on an old-but-fine generation, since a plain
relaunch will not move it.

```console
$ agentbox down <box>       # only if it is running - a live overlay cannot be swapped
$ agentbox remount <box>
remounted myproj-a1b2c3d on the current base image
```

Neither costs the box anything: its writes are in `upper` on disk, not in the
mount, so all a remount discards is the kernel's cached view of the layer
below. `agentbox reset <box>` cures it too, but by deleting everything the box
has ever written — a far larger hammer than this needs.

To repair one program *without* dropping the session you are sitting in — a
remount cannot happen underneath a running box, and you may not want to leave —
reinstalling it writes the file into the box's own upper layer, where the stale
lower lookup cannot mask it:

```console
[box]$ sudo pacman -S --overwrite '/usr/bin/nvim' neovim     # Arch
[box]$ sudo apt-get install --reinstall neovim               # Debian/Ubuntu
```

`pacman -Qkk` over everything names the whole blast radius, which is worth
checking before assuming it was only the one binary you noticed:

```console
[box]$ sudo pacman -Qkk 2>&1 | grep 'No such file'
```

## `Note: in a future version of systemd-nspawn ... socket address families`

Gone as of the `address_families` setting: agentbox now states the policy
explicitly rather than leaving systemd-nspawn to warn that its default is about
to change. If you still see it, the box was created by an older agentbox and its
settings file is stale — any `agentbox run`, `shell` or `up` rewrites it.

The default is no filtering, which is deliberate. Narrowing to AF_INET, AF_INET6
and AF_UNIX — what systemd intends to default to — breaks `ip`, `ss`, `udev`,
glibc's resolver and, in `nat` mode, the container's own networkd, all of which
need `AF_NETLINK`. Set `address_families` if you want the restriction anyway.

## AppArmor: the profile is not confining the box, or a box will not launch

The AppArmor profile is optional defense in depth (see
[contrib/apparmor/README.md](../contrib/apparmor/README.md)). agentbox applies it
only when it is actually loaded, so:

* **`apparmor = true` warns it is "not loaded".** The profile has not been loaded
  into the kernel, or AppArmor is off on this host. Load it (`apparmor_parser -r`)
  or install AppArmor; the box still runs, just without the extra wall. Confirm
  with `sudo grep agentbox-nspawn /sys/kernel/security/apparmor/profiles`.
* **`agentbox --dry-run` shows no AppArmor even though it is loaded.** The loaded
  -profile list is root-readable, and `--dry-run` does not escalate. Run
  `sudo agentbox --dry-run shell` to see the real decision.
* **A box fails to launch only after loading the profile.** This is the case to
  watch for, and the reason to load in complain mode first. If `agentbox shell`
  or `up` fails right after you enforced the profile, it is denying something the
  box legitimately needs. Drop back to complain mode
  (`sudo aa-complain agentbox-nspawn`), reproduce, and read `sudo aa-logprof` /
  `dmesg | grep -i apparmor` to see what to allow. As a quick escape, set
  `apparmor = false` to launch without it while you tune the profile.
* **`systemd-run: Unit property AppArmorProfile is not applicable` (or similar).**
  Your systemd rejects `AppArmorProfile=` on a transient scope. Booted boxes
  (`agentbox up`) are unaffected; for `shell`/`run`, set `apparmor = false` and
  file it — the booted path still gives you the profile.

## `cargo build` fails

```console
$ cargo --version     # 1.74 or newer
$ cargo clean && cargo build --release
```

The dependencies are `clap`, `serde`, `toml`, `serde_json`, `sha2`, `libc` and
`anyhow`, all from crates.io. If you are offline and have them vendored,
`cargo build --release --offline` works.

## A mapped subdirectory will not mount, or a saved file fails with `EBUSY`

Both come from nesting one mount inside another; see
[configuration.md](configuration.md#nesting-one-mount-inside-another) for the
rules. Two shapes cause almost all of it:

* An `rw` map inside a `ro` parent whose path does not already exist in the
  parent's source. nspawn creates a missing destination, but by then the parent
  is already read-only, so the launch fails. Create the directory on the host
  first.
* A `ro` map of a single **file** inside an `rw` parent. The file is a mount
  point, so it cannot be renamed over or unlinked — `EBUSY`, not `EROFS`. Any
  program that saves by writing a temporary file and renaming it over the
  original will fail. Map the file's directory read-only instead, or do not map
  the file at all.

`agentbox status` lists mounts in the order agentbox generates them, which is
not the order nspawn applies them — it sorts by destination, parents first.

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
3. Is the destination under a path nspawn owns? See the next entry.

Mount points are prepared on every launch, not only when the box is created, so
adding a mount to `.agentbox.toml` and starting the box again is enough — no
`rm` or `reset` needed.

## `Permission denied` writing *beside* a mapped file, not to it

The mount itself works, but the directory holding it is not writable, so a tool
cannot create its own files next to what you mapped in. Typically:

```
warning-path: Unable to locate data directory derived from $HOME: '~/.local/share/fish'.
warning-path: The error was 'Permission denied (os error 13)'.
```

after mapping `~/.local/share/nvim/lazy`, or claude asking to log in again after
`~/.claude/.credentials.json` was mapped in.

The cause was agentbox creating a bind destination's missing ancestors as *host*
root, which is not in the box's shifted UID range and so shows up inside the box
as an unmapped owner (`nobody`) on a `0755` directory. Mapping any dotfile
conjures one of these under the sandbox user's home — `~/.claude`, `~/.local`,
`~/.local/share`, `~/.config` — and each of them blocked the user from writing
anything alongside the mount.

Ancestors are now given the owner of the host directory of the same name, and an
ancestor left behind as host root is repaired on the next launch, so `agentbox
shell` once with an up-to-date agentbox is the whole fix. Confirm inside the box
with `ls -ld ~/.claude ~/.local/share` — the owner should be you, not `nobody`.

## `cannot map ... systemd-nspawn mounts its own /tmp there`

A project, or a mapped directory, cannot live under `/tmp`, `/run`, `/dev`,
`/proc` or `/sys`. systemd-nspawn mounts its own filesystem over each of those
inside the container, and it does so *before* applying the binds - so the
mount point agentbox prepared underneath is hidden, nspawn creates its own
owned by container root, and `owneridmap` maps you onto that instead of onto
the sandbox user. A mode 700 project then cannot even be entered, which is why
this is refused up front rather than failing later as a `chdir` error.

Move the project somewhere else. `$HOME`, `/srv`, `/var/tmp` and anything else
outside that list are all fine. A *mapped* directory does not have to move: give
it a destination of its own, as in `ro = ["/tmp/sysroot:/sysroot"]`. Only the
project directory is pinned to its real path.

Before this was refused up front, the same cause surfaced much later and much
more confusingly, as

```
Failed to change to specified working directory /tmp/...: Permission denied
```

## `no base image yet - run agentbox build first`

Expected on a fresh install. If you *have* built it, check the current
generation is where the tool looks:

```console
$ sudo cat /var/lib/agentbox/bases/current
$ sudo ls /var/lib/agentbox/bases/$(sudo cat /var/lib/agentbox/bases/current)/usr
```

## Base build fails during bootstrap

The bootstrap runs on the host with the host's own tools, so it is usually the
host's archive setup at fault rather than agentbox.

On an Arch host, the keyring or the mirrors:

```console
$ sudo pacman -Sy archlinux-keyring     # refresh host keyring first
$ sudo pacman-key --refresh-keys        # if signatures are rejected
$ head -5 /etc/pacman.d/mirrorlist      # a dead mirror stalls the bootstrap
```

The build reuses the host's package cache (`/var/cache/pacman/pkg`) and
keyring (`/etc/pacman.d/gnupg`), so a broken host pacman breaks the build.

On a Debian or Ubuntu host, `debootstrap` itself, and what agentbox worked out
to hand it — `agentbox --dry-run build` prints the exact command, which is the
quickest way to see the suite and mirror it chose:

```console
$ agentbox --dry-run build              # the debootstrap line it will run
$ ls /usr/share/debootstrap/scripts/    # the suites this host can bootstrap
```

* *"No such script"* — the host's `debootstrap` is older than the release it
  is being asked for. Name one it has, under `[base] suite`.
* *Nothing is downloaded, or every package 404s* — the mirror. agentbox reads
  it from this host's own `/etc/apt/sources.list`(`.d/<id>.sources`), so a
  mirror that only carries what this host installs (or an internal one the
  build cannot reach) needs `[base] mirror` set explicitly.
* *A package in the list is "not found"* — `base_packages` names something
  this suite or these components do not have. `[base] components` controls the
  latter; Ubuntu needs `universe` for much of the default list.

A
partial generation is safe to ignore: it was never pointed at by `current` (a
build only advances that once every stage has finished), so the next
`agentbox build` sees the same "no complete image yet" state as before the
failed attempt and simply tries again from scratch - no `--force` needed on a
first build. Whatever the failed attempt left behind under
`/var/lib/agentbox/bases/` is cleaned up on its own the next time any agentbox
command runs. If a *previous, successful* build already put a good generation
in place and you're now retrying a failed `--refresh`/`--force`, use the same
flag again.

## `chsh: user "<you>" does not exist`, or `could not create box`

The base image is incomplete: a build that died part way through leaves a
bootstrapped rootfs with no sandbox user in it, and every box built on top then
fails at the point where it tries to set that user's login shell.

`agentbox build` only points `current` at a generation once every stage —
including the one that creates that user — has finished, so a build that died
mid-way leaves its half-built directory orphaned rather than adopted; the
next `agentbox build` (or the next `ls`/`down`/`reset`/`rm`/`remount`) reclaims
it on its own, and `current` still names whatever the last complete build
produced (or nothing, on a fresh install). This entry is now mainly historical,
but check the generation actually in use if you still hit it:

```console
$ sudo cat /var/lib/agentbox/bases/current
$ id=$(sudo cat /var/lib/agentbox/bases/current)
$ sudo test -f /var/lib/agentbox/bases/$id/etc/sudoers.d/00-agentbox && echo complete
$ sudo grep "^$USER:" /var/lib/agentbox/bases/$id/etc/passwd
```

Neither of those is present in a stump. `agentbox build --force` rebuilds from
scratch; `--refresh` will not help, since it copies whatever `current` names
forward and runs the package manager inside it, and a stump was never shifted.

## `pacman` inside the box rejects signatures

The box's own keyring failed to initialise. Fix it in the box:

```console
[box]$ sudo pacman-key --init && sudo pacman-key --populate archlinux
[box]$ sudo pacman -Sy archlinux-keyring
```

If a fresh box has the same problem, the base image is at fault:
`agentbox build --force`.

## `apt-get` inside a Debian/Ubuntu box cannot find a package

The image ships with the archive lists it was built with, exactly as an Arch
image ships with a synced pacman database, and they go stale the same way:

```console
[box]$ sudo apt-get update && sudo apt-get install PKG
```

If the package genuinely is not there, check which components the image was
built with — Ubuntu keeps most of what a dev box wants in `universe`:

```console
[box]$ cat /etc/apt/sources.list.d/agentbox.sources
```

That file is generated at build time from `[base] components`, so widening it
means `agentbox build --force`, not an edit inside a box.

## A `nat` box on Debian/Ubuntu resolves nothing

`nat` boxes run their own `systemd-resolved`, and Debian's copy of it points
`/etc/resolv.conf` at its stub. agentbox generates `ResolvConf=replace-host`,
which overwrites that symlink with the host's resolver config on every launch —
deliberately not `copy-host`, which silently does nothing when the file it
would write is a symlink and so left Debian-family boxes with no resolver at
all. If a box comes up with no DNS but a working route, that is the first thing
to look at:

```console
[box]$ ls -l /etc/resolv.conf ; cat /etc/resolv.conf
[box]$ systemctl status systemd-resolved
```

`agentbox run <box> --network host -- ...` is the quick way to tell a DNS
problem from a routing one: if that resolves and `nat` does not, the box's
resolver is at fault rather than the host's NAT.

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

## `nat` boxes have no network, and `networkctl` shows the veth `unmanaged`

If `networkctl status ve-<box>` reports `Network File: n/a (unmanaged)` and the
journal has

```
ve-<box>: Failed to update link state file /run/systemd/netif/links/NN, ignoring: Permission denied
```

then the host's own networkd cannot write its runtime state, so it never
configures the host side of the veth: no DHCP server, no address and no NAT, and
every `nat` box comes up with no network while `host` mode still works. Check who
owns that state:

```console
$ ls -lnd /run/systemd/netif /run/systemd/netif/links
$ getent passwd systemd-network
```

An owner that is *not* `systemd-network`'s UID means an agentbox built before
this was fixed chowned it. `bootstrap()` used to rbind the host's `/run` and
`/dev` into the image, and the systemd package's install scriptlets then ran
`systemd-tmpfiles --create` inside that chroot; the rules in
`/usr/lib/tmpfiles.d` name owners by user *name*, which resolve against the
*image's* `/etc/passwd`, so the chowns landed on the host's real files with the
image's UIDs. `/run/uuidd`, `/run/tpm2-tss/eventlog` and `/dev/kvm`'s group are
usually hit too.

The build no longer touches either path — it uses a throwaway tmpfs — but a host
already affected has to be repaired, which `systemd-tmpfiles` will do from the
host's own rules:

```console
$ sudo systemd-tmpfiles --create
$ sudo systemctl restart systemd-networkd
```

A reboot does the same thing, since all of it lives in `/run`. No image rebuild
is needed: the damage was to the host, not to the image.

## `agentbox up` fails or the box will not boot

```console
$ systemctl status systemd-nspawn@<box>.service
$ journalctl -u systemd-nspawn@<box>.service -n 50
```

Booted mode needs a real init in the image, which `base` provides. `agentbox
up` mounts the overlay and writes the settings itself, so it does not need a
`shell` first.

If the journal shows the box reaching its banner and then dying:

```
Failed to create /init.scope control group: Permission denied
Failed to allocate manager object: Permission denied
```

PID 1 is not running as container root. A `User=` in
`/etc/systemd/nspawn/<box>.nspawn` does exactly that, since the setting names
the user for the container's *main* process — systemd itself, in booted mode.
agentbox no longer writes one and regenerates the file on every launch, so one
more `agentbox up` clears a stale file left by an older version. See
[design.md](design.md#why-one-generated-nspawn-file).

## A box will not power off

`shell`/`run` power the box off when the last session leaves, but only a box
they started, and only if no session is still attached. A box `agentbox up`
started is kept on purpose — stop it with `agentbox down`. If a launch was
killed hard (SIGKILL) its session file lingers in
`/var/lib/agentbox/boxes/<box>/runtime/sessions/`; the next `shell`/`run`/`down`
sweeps entries whose process is gone, so one more launch (or a `down`) clears a
box wedged that way.

## `memory.max` inside the box says `max`

Look from the host instead. nspawn delegates a subgroup to the container and
the caps sit on the unit above it, so the box's own view of `/sys/fs/cgroup`
correctly reports no limit of its own. Every launch boots the box's unit, so the
caps always live there:

```console
$ agentbox up
$ systemctl show -p MemoryMax -p CPUQuotaPerSecUSec -p TasksMax \
    systemd-nspawn@<box>.service
```

## A program behaves differently when agentbox's output is redirected

By design. When all three of stdin, stdout and stderr are terminals, the box
gets a pseudo-TTY and `/dev/console`. Redirect any of them and it gets the file
descriptors as they are instead, which is what makes `agentbox run` usable in a
pipeline — but a program that wants a terminal will notice, and an init system
will not start without `/dev/console` at all. Booted mode is unaffected:
`agentbox up` starts a unit rather than a pipeline.

Keep the terminal for a program that needs one:

```console
$ agentbox run -- script -qec 'the-tui' /dev/null | tee out.log
```

## Overlay is still mounted after a reboot

It is not — mounts do not survive a reboot, and `agentbox shell` remounts
on demand. `agentbox ls` will show `OVERLAY` as `-` until then. Nothing is
lost; the writes are in `upper` on disk.

## A box is eating disk

```console
$ agentbox ls                                    # WRITES column
$ sudo du -sh /var/lib/agentbox/boxes/*/upper    # per box
$ agentbox reset <box>                           # back to the base image
```

Package caches inside the box are the usual culprit: `[box]$ sudo pacman -Scc`
on an Arch box, `[box]$ sudo apt-get clean` on a Debian or Ubuntu one.

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
