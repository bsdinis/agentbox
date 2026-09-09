# agentbox

A `systemd-nspawn` sandbox for running coding agents against a project directory.
Single crate, one binary, no workspace, no features, `rust-version = 1.74`.

The canonical documentation lives in `docs/` — this file exists to point at it and to
record the invariants that a plausible-looking change would quietly break. When the two
disagree, `docs/` wins:

- `docs/design.md` — architecture and the reasoning behind every mechanism here
- `docs/security.md` + `REMAINING_VULNS.md` — the threat model and what is still open
- `docs/configuration.md` — every config key and how layering resolves it
- `docs/setup.md` — host requirements, the dev loop, uninstall
- `docs/usage.md`, `docs/troubleshooting.md`

## Commands

The dev loop (`docs/setup.md:42-46`):

```
cargo test      # config layering, $VAR/~ expansion, escaped-colon src:dst splitting,
                # box-name sanitising, bind escaping
cargo clippy
cargo fmt       # plain defaults; there is no rustfmt.toml (7ad7f04)
tests/verify.sh # end-to-end, against a real host
```

Unit tests are inline `#[cfg(test)]` modules only, and deliberately pure: they cover the
parts where a silent mistake would be expensive, and everything that is I/O against
systemd is covered by `tests/verify.sh` instead (`docs/design.md:210-213`). Don't add
tests that need a container to `cargo test`.

**What needs root.** `cargo *`, `agentbox init`, `agentbox config`, and
`agentbox --dry-run <anything>` do not. Every other subcommand re-execs itself under
`sudo` and mutates host state (`needs_root()`, `src/main.rs:204`). `--dry-run` prints
the full launch line and the generated settings file, and is the intended way to audit a
change without touching the host.

**`tests/verify.sh` gotchas**, from its header:

- It tests the *installed* binary unless you set `AGENTBOX_BIN`. After `cargo build`,
  use `AGENTBOX_BIN=target/release/agentbox tests/verify.sh`.
- Its scratch root (`AGENTBOX_VERIFY_ROOT`, default under `~/.cache`) must not sit under
  `/tmp` — nspawn owns `/tmp`, so a project there cannot be boxed at all.
- `WITH_BUILD=1` is opt-in: it builds a second ~3G Arch image and takes minutes.
- `AGENTBOX_STATE` is read from the current process and never from the sudo handover, so
  it cannot aim a privileged agentbox. Tests that relocate state need
  `sudo env AGENTBOX_STATE=... agentbox ...` — plain `sudo VAR=x` is refused by sudoers.

Any change to the bootstrap or teardown in `src/base.rs` must be exercised with
`WITH_BUILD=1`. A shared-subtree unmount there once took the host's own `/dev/pts` and
`/run/user/$UID` down with it (`5def219`); verify.sh sections 10-11 assert those
survived.

## Layout

`README.md`'s "Layout" section is stale — it lists a `contrib/` Python prototype that
`f016655` removed, and omits two modules. Use this instead:

| File | Responsibility |
| --- | --- |
| `src/main.rs` | clap derive (`Cli`/`Command`), global `--dry-run`, hidden `--internal-handover`, one dispatch arm per subcommand |
| `src/host.rs` | the privilege boundary: passwd lookup, sudo re-exec with env handover, the `argv!` macro and the `sh()` runner everything logs through |
| `src/config.rs` | layered TOML, `DEFAULT_BASE_PACKAGES`, `UID_RANGE`, `expand()` |
| `src/sandbox.rs` | one box: naming, paths, bind list, env, uid shift, `split_spec`, `overbroad_reason` |
| `src/nspawn.rs` | the big one: overlay mount/umount and staleness, generated `.nspawn` text, unit drop-ins, safety checks, box-scoped ssh-agent, launch/attach |
| `src/base.rs` | the base image: five build stages, `release_boxes()`, the `base.id` identity stamp |
| `src/session.rs` | who owns a running box and when it powers off |
| `src/cmds.rs` | subcommand bodies plus `[BOX]` argument resolution (`classify`/`resolve`/`load`) |

## CLI shape

Boxes are named, not `--dir`'d (`b02e35e`; `--dir` no longer exists). Every command
except `build`, `ls` and `init` takes an optional positional `[BOX]` that is either a
box name or a project directory, defaulting to the current directory's box. A spec
containing `/` is always a path; otherwise an existing box beats a same-named directory
(`cmds::classify`). A box name is `<name>-<6 hex of project path>`, and `meta.json` maps
it back to its project so `down`/`reset`/`rm` keep working after the project moved.

A `run` payload goes after `--`: `agentbox run mybox -- cmd`.

## Invariants

Most of these are fixes for real failures. Read the reason before changing the code.

**Overlayfs never revalidates its lower layer.** The base image is the `lowerdir` of
every mounted box, so a mount that straddles a `build` keeps serving its cached view for
the life of the mount — a file the build added is listed by `readdir` and `ENOENT` on
open (`b41f4b8`). Hence: `build --refresh|--force` refuses by name while any box is
running, and unmounts every idle mounted box first (`base::release_boxes`); unmounting an
idle box is free, since its writes live in `upper` on disk. As a second line of defence
each build stamps `base.id` and each mount records `overlay.id`; a mismatch, or no record
at all, shows as `stale` in `agentbox ls`, and a launch or `agentbox remount` cures it
losslessly. `base.id` lives *beside* the image, not inside it — a box would otherwise
read its own cached copy from the stale lower layer.

Overlay options stay pinned conservative — `index=off,metacopy=off,redirect_dir=off,
xino=off` — because they interact badly with an image whose UIDs are shifted.

**The image is pre-shifted once at build time**, so every box launches with
`PrivateUsersOwnership=off` and zero per-launch work (`docs/design.md:52-72`). `chown`
mode would copy the whole base into the upper layer and `map` mode cannot target
overlayfs, so don't switch modes. The consequence is that all boxes share one UID range:
boxes are isolated from the host, not from each other. `uid_base` only means anything
before `build`.

**Networking defaults to `nat`** (`49b430c`). That default is what isolates the host's
`localhost` services and abstract-namespace sockets, X11 among them — it is the fix for
`REMAINING_VULNS.md` A5/B2, so `host` mode is a per-project opt-in, not a convenience.
`nat` needs `systemd-networkd` on the host; `none` makes package installs fail, so bake
`packages` into the image first.

**Packages install after boot**, not during bootstrap (`dea7575`): a pre-boot install has
no network under `nat`. `install_packages` runs after `wait_attachable` and `canary`,
gated on a fresh box, and `check_payload` runs after it because the payload may be one of
those packages. A non-zero pacman exit is fatal by design.

**The mount guards are red-team fixes, not style.** All of them refuse rather than
sanitise:

- `unsafe_dst` — a bind `dst` must be absolute and lexically normalized. The mount point
  is created and chowned *as root on the host* before nspawn starts, so a `..` in `dst`
  is an arbitrary host path (V3).
- `overbroad_reason` (`src/sandbox.rs:293`) — refuses `/`, the host home or any ancestor,
  and the state dir or any ancestor; the project dir and `internal` binds are the
  exceptions (V1).
- `reject_control_chars` — a newline in a config-derived value would inject settings into
  the trusted `.nspawn` file, and `escape()` depends on this (V2).
- `NSPAWN_OWNED` = `/tmp`, `/run`, `/dev`, `/proc`, `/sys` — nspawn mounts over these
  *after* custom binds, hiding the prepared mount point and mismapping `owneridmap`. A
  project under one of them cannot be boxed; this is why the ssh-agent socket lands at
  `~/.agentbox/ssh-agent.sock`.

`check_supported` runs before anything is created or mounted, so a doomed launch leaves
no overlay, box dir or settings file behind, and `status` and `--dry-run` report the same
verdict.

**One generated `.nspawn` file serves two launch paths.** `/etc/systemd/nspawn/<box>.nspawn`
is regenerated from TOML on every launch, so hand-editing it is pointless. It is used by
both the bootstrap launch and the booted `systemd-nspawn@.service`, so nothing describing
a single payload may go in it: `User=` there gave PID 1 the sandbox user's UID and a
container that died a second after `up` reported success. User and cwd go on the command
line. Resource caps and `AppArmorProfile=` are *unit* properties with no settings key, so
they go in a drop-in under `/etc/systemd/system/systemd-nspawn@<box>.service.d/` or as
`--property=` on the transient scope. AppArmor is always applied with a leading `-` and
only when the profile is loaded — defense in depth, never a launch gate.

**Config layering** (`docs/configuration.md:10-15`): lists (`rw`, `ro`, `packages`,
`pass_env`, `ssh_keys`) accumulate across layers, tables merge, scalars replace. There is
no way to subtract, so a project can add mounts but never drop the built-in ones.
`base_packages` is the sole exception — it *replaces* the built-in list, and its commented
copy in `config.example.toml` is pinned to `DEFAULT_BASE_PACKAGES` by a unit test, so
editing one means editing both. `deny_unknown_fields` makes typos hard errors. In a
`.agentbox.toml`, `[env]` must come last: any scalar written below that table header
silently becomes an environment variable.

**`ssh_keys` fails closed.** A missing key aborts the launch, the host's own
`SSH_AUTH_SOCK` is never forwarded, and keys are added with `ssh-add -c`
(confirm-on-use).

**Session ownership** (`src/session.rs`): a box booted by `shell` or `run` is
`Session`-owned and powers off when the *last* session leaves; one booted by `up` is
`Up`-owned and survives until `down`, and `up` promotes a `Session` box. Sessions are a
flock-guarded *set* of files, one per attach, with stale-PID sweeping — do not turn it
into a counter.

**The project directory is tainted output.** It is always rw and mapped with
`owneridmap`, so anything the agent writes is a normal host file owned by you:
`.git/hooks`, `build.rs`, `.cargo/config.toml`, `Makefile`, `package.json` scripts,
`.envrc`, `.vscode/` and `.pre-commit-config.yaml` all execute **as you, on the host**,
later. Review the diff before trusting a repo an agent has worked in. Copy-mode is
explicitly rejected as a fix (it would defeat the workflow), and `agentbox init`
implements the one automatable slice: a read-only `.git/hooks` for git repos, skipped
when `.git` is a file (worktrees and submodules).

## Host state and porting

Everything below is root-owned: `/var/lib/agentbox/{base,base.id}`,
`/var/lib/agentbox/boxes/<box>/{upper,work,meta.json,overlay.id,runtime/}`,
`/var/lib/machines/<box>` (the overlay mountpoint),
`/etc/systemd/nspawn/<box>.nspawn`, and
`/etc/systemd/system/systemd-nspawn@<box>.service.d/` plus a `daemon-reload`. To clear
test debris: `agentbox rm <box>`, then `sudo rm -rf /var/lib/agentbox` and
`sudo rm -f /etc/systemd/nspawn/*.nspawn` (`docs/setup.md:170-178`).

Host requirements: systemd >= 256, Linux >= 5.12, `systemd-container`, host `pacman`
(so `build` is Arch-only), an ID-mapped-mount-capable filesystem under the code, `sudo`.
`systemd-networkd` for `nat`. To port the guest distro, replace `bootstrap()` in
`src/base.rs` and `DEFAULT_BASE_PACKAGES` in `src/config.rs` — nothing else knows what
the guest is.

## Repo conventions

Colocated git + jj: both `.git` and `.jj` are present. `.claude/worktrees/multi-session/`
is a checked-out worktree holding a full second copy of the tree; it is gitignored, but
exclude it explicitly if you search with a tool that ignores `.gitignore`.

Commits are `scope: imperative summary` in lowercase, with a wrapped body that explains
the failure mode rather than the diff, and a `Co-Authored-By:` trailer.
