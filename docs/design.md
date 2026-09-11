# Design

## Shape of the thing

```
                     host                                    box
  /var/lib/agentbox/base ────── lowerdir ──┐
                                            ├─ overlayfs ── /var/lib/machines/<box>
  /var/lib/agentbox/boxes/<box>/upper ── upperdir ┘              │
                                                                 │  systemd-nspawn
  ~/dev/myproject ───── Bind=…:owneridmap ───────────────────────►│  /home/you/dev/myproject   (rw)
  ~/dev/reference ───── BindReadOnly=…:owneridmap ───────────────►│  /home/you/dev/reference   (ro)
  uid 1310720000..1310785535 ── PrivateUsers ────────────────────►│  uid 0..65535
```

Three mechanisms carry the whole design: an overlay for cheap per-project
mutability, a user namespace for privilege isolation, and ID-mapped bind mounts
so shared directories have sane ownership on both sides.

## Why an overlay

An agent that can install packages needs a writable `/usr`, which rules out
sharing one read-only rootfs across projects. The alternatives were a full copy
per project (a couple of GB and a slow first launch, since the host root here is
ext4 and gets no reflink) or `machinectl clone` (btrfs only).

overlayfs gives per-project writability at the price of the diff:

* `lowerdir` is the shared base — one copy of Arch for every project.
* `upperdir` holds every write the box has ever made. `agentbox ls` shows its
  size; a box that has installed a few packages costs tens of megabytes.
* `agentbox reset` is `rm -rf upper` and a remount. Roughly a second, and it
  cannot touch your code, which is a bind mount rather than part of the image.
* The base lives as a sequence of *generations* under
  `/var/lib/agentbox/bases/<id>/`, not one directory mutated in place, because
  overlayfs never revalidates its lower layer: a mount that straddled a rebuild
  would go on serving the view it cached - a file the rebuild added can be
  listed by `readdir` and still `ENOENT` on open, for as long as that mount
  lives. `agentbox build --refresh` copies the current generation forward and
  runs `pacman -Syu` on the copy; `--force` (and the first build) bootstraps a
  new generation from scratch. Either way it swaps a `current` pointer to the
  new generation and never touches the directory any live box already opened
  as its lowerdir, so a build no longer has to refuse or unmount anything
  running.
* A generation sticks around exactly as long as `current` names it, or some
  *mounted* box's `overlay.id` does; `base::gc_generations` deletes anything
  else, run opportunistically by `build`, `down`, `reset`, `rm`, `remount` and
  `ls` rather than as a separate step a user has to remember. An idle box pins
  nothing even if its own record names an old generation, since its next mount
  always targets `current` anyway.
* `agentbox ls`'s `stale` means a box's recorded generation is missing from
  disk entirely - a GC race, or a mount left by a version of agentbox that
  predates generations - not "a newer generation exists"; a box happily
  running on an older-but-still-present generation is the ordinary case, shown
  as `mounted (g..., current g...)`. Either way, a launch quietly repairs it and
  `agentbox remount` repairs it on demand, by dropping the overlay and
  mounting fresh against `current`.

The mount is deliberately conservative: `index=off,metacopy=off,redirect_dir=off,xino=off`.
Those features change how the upper layer refers to lower files and interact
badly with an image whose ownership was shifted; turning them off costs a
little copy-up work and buys predictability.

The merged mount lives at `/var/lib/machines/<box>` so `machinectl` and
`systemd-nspawn@.service` find it by name with no extra configuration.

## Why the image is pre-shifted

With `PrivateUsers=<base>:65536`, container UID 0 is host UID `<base>`. The
files in the image have to line up with that mapping somehow. systemd offers
three ways, and the choice matters a lot here:

| `PrivateUsersOwnership=` | What it does | Why not |
| --- | --- | --- |
| `chown` | Recursive chown of the tree at every start | On an overlay, that copies the entire base into the upper layer. Fatal. |
| `map` | ID-mapped mount of the rootfs | overlayfs cannot be the target of an ID-mapped mount. |
| `off` | Trust the on-disk ownership | Correct, if the ownership is already shifted. |

So `agentbox build` shifts the base image once, at the end of the build, and
every box then starts with `PrivateUsersOwnership=off` and zero per-launch
work. The consequence is that all boxes share one UID range (`uid_base`,
default 1310720000) - *per generation*: each build stamps the `uid_base` it
shifted for beside its generation directory, and `nspawn::mount` refuses to
mount a box against a generation shifted for a different one rather than let
the mismatch through silently. That matters more once generations make
rebuilding routine than it did with one base built once: editing the global
`uid_base` and rebuilding produces a generation only boxes without a
conflicting `uid_base` of their own can use. Each box is fully isolated from
the *host*; boxes are not
isolated from each other's UIDs. Since they cannot see each other's
filesystems, that only matters if you also hand two boxes a common read-write
directory, and it is the deliberate trade for instant startup. Give a box its
own `uid_base` and rebuild if you need more.

## Why `owneridmap` on the binds

A plain bind mount into a user namespace shows every host file as `nobody`
(65534): the host owner is not in the container's map. Read-only reference
material would still be readable, but the project directory would be unusable —
and `git` and `jj` would balk at ownership they do not recognise.

`owneridmap` (systemd 256+, needs kernel ID-mapped mount support in the source
filesystem) maps *the owner of the mount destination inside the container* to
*the owner of the source inode on the host*. `agentbox` therefore pre-creates
every mount point inside the box owned by the sandbox user, whose UID equals
yours. The result: your files show up as yours, writes land on the host owned
by you, and no `safe.directory` incantations are needed.

Host files inside a mapped directory owned by some *other* host user still show
as `nobody`, which is the correct and expected outcome.

## Why `cpy` is a one-time copy, not a nested overlay

`rw`/`ro` give a box two ways to see a host path live: a read-write bind or a
read-only one. `cpy` is deliberately a third, different mechanism - a one-time
`cp -a` into the box's own overlay upper layer, done once per box lifetime
(reset resets it), never a mount at all.

The alternative that looks obvious - mount the host directory as a nested
overlay's `lowerdir`, upper layer inside the box - was rejected for the exact
reason generations exist for the base image: **overlayfs never revalidates its
lower layer.** A mount straddling a rewrite of its `lowerdir` goes on serving
whatever it cached for the life of that mount - a file the rewrite added can be
listed by `readdir` and still `ENOENT` on open (see "Why an overlay" above, and
`b41f4b8`). A `cpy` source is exactly the kind of host path a project keeps
editing on the host side after the box has been booted for a while - a seed
config, a vendored cache - so a nested overlay mount would reintroduce that bug
class the moment someone touched the host source while a box's mount was live,
with no generation scheme protecting it the way the base image is protected.

A one-time copy sidesteps the problem instead of managing it: once `cp -a` has
run, the destination is an ordinary file in the box's own upper layer with no
lower layer of its own to go stale. There is nothing left to revalidate,
because there is no longer a live relationship to the host path at all -
which is also why `cpy` needs no `owneridmap`/ID-mapped-mount support: the
copy is chowned once, to the box's own shifted sandbox user, the same
`shift`/`map_id` math a bind's mount point already uses. The cost is the
copy-if-absent semantics this implies: a `cpy` destination that already
exists inside the box (a previous copy, or the box's own later write) is left
alone rather than resynced, so `agentbox reset` - which already exists to
empty a box's upper layer - is what makes the next boot copy fresh again. No
new staleness-tracking machinery was needed because `cpy` never holds a mount
open past the copy.

## Why the sandbox user mirrors you

Same username, same UID and GID, home at `/home/<you>`. Paths are then spelled
identically inside and outside, which is what makes relative path dependencies
(`path = "../lib"`, `go.work`, editable installs, tsconfig aliases) resolve, and
makes compiler output clickable in a host editor. The home directory is the
box's own — it lives in the overlay — so the agent gets a normal writable `~`
for tool state without seeing yours.

## Why one generated `.nspawn` file

`agentbox` writes `/etc/systemd/nspawn/<box>.nspawn` and treats it as the
single source of truth for binds, UID mapping, environment and network mode.
Two consequences worth knowing:

* The bootstrap launch that assembles a box (installing packages, setting the
  login shell) runs `systemd-nspawn --settings=yes`, so the file applies in full
  (files under `/etc/systemd/nspawn/` are trusted) while explicit command line
  flags such as `-u root` still win.
* Every launch a user asks for — `shell`, `run`, `up` — boots the box through
  the stock `systemd-nspawn@.service`, which forces `-U --network-veth
  --settings=override`. Because `override` gives the file precedence, our
  `PrivateUsers=` and `[Network]` settings take effect anyway, and the booted
  box ends up identical to what the bootstrap launch saw.

The flip side of one file serving both the bootstrap launch and the booted
service is that nothing describing a single *payload* may go in it. `User=`
names the user to invoke the container's main process as, and for a booted box
that process is systemd itself: setting it there gave PID 1 the sandbox user's
UID, no way to create `/init.scope`, and a container that died a second after
`agentbox up` reported success. The user and working directory are named on the
command line instead — by the bootstrap launch, and by each `systemd-run`
attach into the booted box — and the file carries only what is true of the box
however it starts.

Resource caps are the mirror image. They are properties of a *unit*, which the
file cannot express at all. Every launch a user asks for boots the box's
`systemd-nspawn@<box>.service` and attaches to it, so the caps go where they
belong: a drop-in on that unit, covering the whole container and every session
in it. (The only launch that is not a boot is the internal bootstrap that
installs packages while the box is still being assembled; it is short-lived and
runs uncapped.)

The file is regenerated from the TOML on every launch, so editing it by hand is
pointless.

## Security model

What the boundary actually is:

* **User namespace.** Container UID 0 is host UID `uid_base`, which owns
  nothing outside the box's own image. `sudo` inside is unprivileged outside.
  Capabilities are namespaced, so `CAP_SYS_ADMIN` in the box does not imply it
  on the host.
* **Mount namespace.** Only the base overlay and the paths you listed are
  visible. `BindReadOnly=` is enforced by the kernel; container root cannot
  remount it read-write.
* **PID, IPC, UTS namespaces.** Host processes are invisible and unkillable
  from inside.
* **cgroup limits.** `memory_max`, `cpu_quota` and `tasks_max` are real
  `MemoryMax=`/`CPUQuota=`/`TasksMax=` on the container's scope.
* **seccomp.** nspawn's default filter blocks the usual dangerous syscall
  families (`kexec_load`, `open_by_handle_at`, raw `bpf`, and so on).
* **AppArmor (optional, defense in depth).** None of the above is a mandatory
  -access-control layer, so a future capability, mount or namespace regression
  would have no second wall. An optional AppArmor profile
  ([contrib/apparmor/](../contrib/apparmor/)) is that wall: it denies a small set
  of host-catastrophic operations no box needs (loading kernel modules, writing
  `/boot`, raw disk access, altering LSM policy, `/dev/mem`, magic SysRq, ...),
  independent of uid/caps/namespaces. agentbox applies it — to the direct-launch
  scope and to the booted-box unit — only when the profile is loaded, and always
  best-effort (a leading `-`), so it never turns into a launch gate. It targets
  AppArmor because the host distro (Arch) ships AppArmor, not SELinux.

What it is not:

* **Not a VM.** One shared kernel. A kernel LPE reachable from a user namespace
  breaks out. Run genuinely hostile code in a VM.
* **Not a network boundary, in the default mode.** `network = "host"` shares
  your network namespace, so the box reaches anything on your `localhost` and
  can bind host ports. Use `none` or `nat` when that matters.
* **Not a secret boundary you get for free.** Anything you map in, forward via
  `pass_env`, or expose through the SSH agent is available to whatever runs in
  the box. Default to mapping nothing that can authenticate, and let the agent
  commit while you push.
* **Not protection for read-write mounts.** An agent with a read-write mount can
  delete everything in it. Keep those mounts to committed working trees.
* **Not a time bomb defuser.** `/dev/shm`, `/tmp` and the box's overlay are the
  agent's to fill; set `memory_max` and watch `agentbox ls` if you care.

`--dry-run` prints the full launch line and the generated settings, which is
the fastest way to audit what a given project's config will actually expose.

## Rejected alternatives

* **bubblewrap** (what ai-jail uses) is lighter and needs no root, but the
  sandbox shares the host's `/usr`, so in-sandbox package installation is not
  really on the table, and neither is a working `sudo`. Those were requirements.
* **Docker/Podman** would work, but a `Dockerfile` per project plus image
  rebuild cycles is a heavier UX than one TOML file, and `pacman -S` inside a
  container whose changes vanish on exit trains bad habits. nspawn's persistent
  machine model fits an agent's long-lived workspace better.
* **A VM per project** is the right answer for hostile code and the wrong one
  for the common case: minutes to boot, a fixed RAM tax, and painful directory
  sharing.
* **`--volatile=overlay`** gives a throwaway upper layer in tmpfs. Attractive
  for one-shot runs, useless for a box that should remember what it installed.
  It is a two-line change in `nspawn::mount` if you want that mode.

## Code map

| File | Responsibility |
| --- | --- |
| `src/main.rs` | The CLI surface (clap), and which commands need root. |
| `src/host.rs` | The privilege boundary: passwd lookup, the sudo re-exec, the environment handover, and the command runner that dry-run and logging hang off. |
| `src/config.rs` | Layered TOML: defaults, global file, project file, CLI overrides. Lists accumulate, tables merge, scalars replace. Unknown keys are an error, so typos surface immediately. |
| `src/sandbox.rs` | One box: its name (project basename plus a hash of the path), its paths, its bind list, its environment, its UID shift. |
| `src/nspawn.rs` | The overlay mount, the generated `.nspawn` file, mount-point preparation for `owneridmap`, and launching. |
| `src/base.rs` | The five build stages, from `pacman --root` to the final ownership shift. |

`cargo test` covers the parts where a silent mistake would be expensive:
config layering, `$VAR`/`~` expansion, `src:dst` splitting with escaped
colons, box-name sanitising, and bind-path escaping. Everything else is I/O
against systemd and is covered by `tests/verify.sh` instead.

### Passing the caller's identity through sudo

`agentbox` needs root, and re-execs itself under `sudo` rather than making you
remember to type it. That creates a problem: `sudo` resets the environment, so
`pass_env = ["ANTHROPIC_API_KEY"]` would silently forward nothing, and `~`
would resolve to root's home.

`sudo VAR=x cmd` is not a fix — default sudoers rejects command-line
environment assignments without a `SETENV` tag. Nor is argv, which is
world-readable through `/proc`, and these values are exactly the kind of thing
that should not be. So the unprivileged half writes its environment, cwd and
UID to a `0600` file under `$XDG_RUNTIME_DIR`, passes the path as a hidden
`--internal-handover` flag, and the privileged half reads the file, checks it
is owned by `SUDO_UID`, and unlinks it before doing anything else.
