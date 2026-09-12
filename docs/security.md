# Security model

`agentbox` runs a coding agent in a per-project systemd-nspawn box. This
document is about where the trust boundary actually falls: what the box
contains, what it deliberately does *not*, and how to run Claude Code inside one
without handing your host or your credentials to whatever the agent does.

Read [design.md](design.md#security-model) for the mechanics of the
containment; this file is about the trust decisions you make on top of it.

## What the box contains

The box's *running process* is held by systemd-nspawn:

* **A private user namespace** (`PrivateUsers=<uid_base>:65536`). Container
  UID 0 is host UID `uid_base` (default 1310720000), which owns nothing outside
  the box's own image. `sudo` inside the box is unprivileged outside it, and any
  capability the box holds is namespaced - it applies only inside that user
  namespace, never to the host. This is the load-bearing boundary.
* **systemd-nspawn's default seccomp filter**, which blocks the usual dangerous
  syscall families (`kexec_load`, `open_by_handle_at`, raw `bpf`, and so on),
  and its default handling of capabilities inside the container.
* **Mount masking.** nspawn lays down its own `/tmp`, `/run`, `/dev`, `/proc`
  and `/sys` over the rootfs, so the box never sees the host's. Only the base
  overlay and the paths you explicitly map are visible.
* **ID-mapped binds** (`owneridmap`). Every host directory you map in is mapped
  so its files show up owned by *you* inside the box, and writes land back on
  the host owned by you - no `nobody`, no `safe.directory` dance.

That contains the box's process. It is not a VM: one shared kernel, so a kernel
LPE reachable from a user namespace breaks out. Run genuinely hostile code in a
VM. The rest of this document is about a boundary that namespaces do **not**
cover at all.

## The read-write mount trust boundary

The single most important property to understand:

> **An agent in the box can modify anything in a read-write mount, and the
> project directory is always mounted read-write.** Those changes are written
> straight back to the host, owned by you, via `owneridmap`.

The box's own process is contained. But the *files it wrote* are ordinary files
on your host, sitting in your working tree. When **you** later run ordinary
tooling on the host against those files, files that look innocuous can have been
edited to execute code **as you, on the host** - outside the box, with none of
the containment above.

This is not hypothetical, and it is not one file. Almost every dev tool will run
code out of the repository as a side effect of a normal command:

| File the agent can write | Fires when *you* run |
| --- | --- |
| `.git/hooks/*` (`pre-commit`, `post-checkout`, `post-merge`, `pre-push`, ...) | `git commit`, `git checkout`, `git merge`, `git push` |
| `.cargo/config.toml` - `[target.*] runner = ...`, `[build] rustc-wrapper = ...`, and similar | `cargo build`, `cargo test`, `cargo run` |
| `build.rs`, proc-macro crates, `path`/`git` dependencies in `Cargo.toml` | `cargo build`, `cargo test`, `cargo run` |
| `Makefile` | `make` |
| `package.json` lifecycle scripts (`preinstall`, `postinstall`, `prepare`, ...) | `npm install`, `yarn`, `pnpm install` |
| `.envrc` | entering the directory with `direnv` allowed |
| `.vscode/`, `.idea/` task and settings files | opening the project in the editor/IDE |
| `.pre-commit-config.yaml` and the hooks it names | `pre-commit run`, or a `git commit` with pre-commit installed |

The box being contained buys you nothing here, because none of these fire
inside the box - they fire later, on the host, under your uid.

**The takeaway: treat a repo an agent has worked in as potentially tainted.
Review the diff before running host tooling against it.** `git diff`, read what
changed, and only then build, test, commit, or open it in your editor on the
host. If you want to run the result without reading it first, run it *inside the
box*, where it is contained.

### Copy-mode is deliberately not the answer

It is tempting to think the fix is to copy the project into the box instead of
bind-mounting it, so the agent's writes never touch the host. `agentbox` does
not do this on purpose. The entire point is that the agent's output is a normal
part of your working tree - visible, diffable, committable, usable by your host
tools the moment the agent is done. A one-way copy would defeat the workflow
this tool exists for. The trade is real and it is intentional: you get the
agent's work in place, and in return you own the job of reviewing it before you
execute it.

**This is not what `cpy` is for.** `cpy` (see
[configuration.md](configuration.md#cpy-one-time-copy-if-absent)) is a
one-time, copy-if-absent snapshot available as a config option, but it exists
for the opposite goal from the one rejected above. The project directory is
copy-mode's proposed target *because* you want the agent's writes to land on
the host; `cpy` is for small, auxiliary config/credential-shaped paths -
`~/.claude.json` is the running example in this document - where you
specifically do **not** want the box's writes ever reaching the host. Reaching
for `cpy` on the project directory itself would reproduce exactly the trade
this section rejects; reaching for it on a seed file like `~/.claude.json` is
the intended use, since nothing about that file benefits from write-back.

### What can and cannot be locked down

One case *can* be closed off: `.git/hooks` never needs to be writable by the
agent, so you can map it read-only over the copy in the working tree:

```toml
# .agentbox.toml
ro = [".git/hooks"]
```

With that, the box can read the hooks but cannot plant new ones for `git` on the
host to run. `agentbox init` now does this for you: run in a git repository, it
emits a read-only `.git/hooks` entry in the generated `.agentbox.toml` by
default, so a fresh project is protected without your having to add it. (A git
worktree or submodule, whose `.git` is a file rather than a directory, has no
local `.git/hooks` and is skipped; add the entry yourself there if you need it.)

Most of the other entries in the table above **cannot** be locked down this way,
because they are project source that has to stay writable for the agent to do
its job - `Cargo.toml`, `build.rs`, `Makefile`, `package.json` and the rest are
exactly the files you are asking the agent to edit. There is no config that both
lets the agent write your build files and stops those build files from running
on the host. This is an inherent trust property of running an agent against a
working tree, documented here so you can make an informed choice - not a bug
with a code fix.

## Running Claude Code inside a box

`~/.claude` is a sharp instance of the boundary above, because it holds **two**
dangerous things at once:

* **Executable configuration that the host's Claude Code runs as you** -
  `settings.json` `command` hooks, MCP server `command`s, the statusline
  `command`, `agents/`, custom slash-commands, and output styles. If the box can
  write these, it can plant code that fires the next time *you* run Claude Code
  on the host.
* **Your credentials** - the token that authenticates as your Claude account.

So the rule is: **never read-write mount the host `~/.claude` into a box.** Doing
so lets an untrusted agent plant host-executed config directly in the place your
host Claude Code reads it from.

### Give the box its own writable `.claude`

The box's home directory is its own - it lives in the overlay, not on your host
(see [design.md](design.md#why-the-sandbox-user-mirrors-you)) - so a box already
has an empty, writable `~/.claude` for the agent's runtime writes (sessions,
todos, history). Nothing to configure; just do not replace it with the host's.

If you want that runtime state to survive `agentbox reset`, map a **per-box**
host directory (one you keep separate from your real `~/.claude`) read-write:

```toml
# .agentbox.toml - a dedicated per-box directory, NOT the host ~/.claude
rw = ["~/.agentbox-claude/myproject:/home/you/.claude"]
```

Never point the destination's source at your host `~/.claude`. That workaround
covers state you want to survive a `reset`; if you don't need that, `cpy` is
the better tool for the one file most boxes actually need seeded:

```toml
# .agentbox.toml - seed the first-run wizard state, never a live host bind
cpy = ["~/.agentbox-claude/myproject.json:~/.claude.json"]
```

That is how you skip the first-run wizard in every new box (see
[usage.md](usage.md#the-first-run-wizard-in-a-fresh-box)) without ever
bind-mounting a credentials-adjacent file into the box: the box gets a
one-time copy it can freely rewrite, and nothing it writes ever reaches the
host file. The trade is that `cpy` content does not survive `agentbox reset`
(reset empties the box's overlay, so the *next* boot copies fresh from the
host source again) - if you specifically want `.claude.json` state to persist
*across* a reset, that is the real use case the dedicated-`rw`-directory
workaround above still covers and `cpy` does not. Either way, your real
`~/.claude.json` stays out of the mapping - only a dedicated file of your own
is ever named as the source.

### Authentication

Pick one of two setups. Both keep the host `~/.claude` out of the box's
read-write reach.

**API key (cleanest).** Forward the key through the environment and mount no
credential file at all. Nothing sensitive is written into the box's filesystem:

```toml
# .agentbox.toml (or ~/.config/agentbox/config.toml)
pass_env = ["ANTHROPIC_API_KEY"]
```

`pass_env` forwards the variable only if it is set in your host environment;
because `agentbox` re-execs under `sudo`, it carries the value across the sudo
boundary for you (see [design.md](design.md), "Passing the caller's identity
through sudo"). Nothing to mount.

**OAuth / subscription.** Mint a long-lived token on the host and forward that,
the same shape as the API key - again mounting no credential file:

```console
$ claude setup-token          # on the host; prints a long-lived OAuth token
```

```toml
# .agentbox.toml (or ~/.config/agentbox/config.toml)
pass_env = ["CLAUDE_CODE_OAUTH_TOKEN"]
```

Export that token in your host shell and every box picks it up.

**Mounting `~/.claude/.credentials.json` instead does not work, and is worth
knowing why.** It is the obvious thing to try, and the box really can read the
file, so the failure is confusing: Claude Code asks you to log in anyway. The
credentials file holds only the *tokens*. Which account they belong to, and the
fact that you have logged in at all, live in `~/.claude.json`
(`oauthAccount`, `hasCompletedOnboarding`) - and that file must stay unmapped,
since the agent rewrites it constantly and it also carries MCP server `command`s
that would then be host-executed config the box can edit. So the box has tokens
with no account context, and runs onboarding. Use `setup-token`.

Neither setup hides the secret from the agent: whatever authenticates the box
authenticates *as you*, so an untrusted agent could exfiltrate it, and a leaked
token acts as your Claude account until it expires or you revoke it. That
residual risk is inherent. A `setup-token` token is at least revocable on its
own, and an API key can additionally be scoped and rotated.

### Reading host Claude settings

If the box needs specific host Claude settings (not credentials), mount those
specific files **read-only** - never read-write:

```toml
# .agentbox.toml
ro = [
  "~/.claude/settings.json",   # box may read it; cannot rewrite it
]
pass_env = ["CLAUDE_CODE_OAUTH_TOKEN"]   # or ANTHROPIC_API_KEY; see above
```

Read-only is what makes this safe: any hooks or `command`s in a settings file
mounted this way run *inside the box* (contained), and the box cannot edit the
file to plant new host-executed config. The principle to carry away:

* **writable, box-local `.claude`** for the agent's own runtime state,
* **read-only (or simply absent)** for any host `.claude` bits the box needs to
  read,
* **credentials read-only, or supplied via `pass_env`** - never read-write, and
  never the whole host `~/.claude`.

## perf inside a box

`perf_event_open(2)` fails with `EPERM` inside a box by default, and the reason
is worth understanding before reaching for `perf = true`: it is not a missing
capability, it is the box's user namespace itself.

Perf's own permission check is `perfmon_capable()`:

```c
// include/linux/capability.h
static inline bool perfmon_capable(void)
{
    return capable(CAP_PERFMON) || capable(CAP_SYS_ADMIN);
}
// kernel/capability.c
bool capable(int cap) { return ns_capable(&init_user_ns, cap); }
```

`capable()` checks the capability against `init_user_ns` - the host's own root
user namespace - specifically, not against whatever namespace the calling
process happens to be in. `cap_capable()` (`security/commoncap.c`) walks from
the target namespace toward the caller's; when the target is `init_user_ns`
and the caller lives in a box's private, nested user namespace, that walk
returns `-EPERM` immediately, unconditionally. **No capability the box grants
itself inside its own namespace can ever satisfy this check** - `Capability=
CAP_PERFMON` in the `.nspawn` file would be a pure no-op, so agentbox does not
offer it. The only way to satisfy `perfmon_capable()` from inside a box would
be `PrivateUsers=no` - not creating a private user namespace for the box at
all - which is not what `perf = true` does, because it would mean container
root literally is host root, discarding the boundary the whole rest of this
document is written to describe. That trade is not worth perf.

What `perf = true` actually does instead is narrower: it only unblocks the
*syscall* (`SystemCallFilter=perf_event_open`, which is not in nspawn's
default allow list). Whether that gets you anything depends entirely on the
kernel's own gates in `perf_allow_cpu()` / `perf_allow_kernel()` /
`perf_allow_tracepoint()` (`kernel/events/core.c`), each of which only calls
`perfmon_capable()` - the check that always fails in a box - once the
**host's** `kernel.perf_event_paranoid` sysctl is above a threshold:

| Host `perf_event_paranoid` | What works from inside a namespaced box |
| --- | --- |
| `2` or more (many distros' default) | Nothing beyond self-only software counters (`PERF_COUNT_SW_*`: page faults, context switches, cpu-clock) - these are never paranoid-gated. |
| `1` | Also CPU hardware events (cycles, instructions) on the box's own process. |
| `0` | Also kernel-symbol/call-graph profiling. |
| `-1` | Also raw tracepoints. |

That sysctl is global - it is not namespaced, agentbox cannot set it on a
box's behalf, and lowering it relaxes the same thing for every process on the
host, not just the box. That is the real, honest cost of making `perf = true`
useful: broader performance-counter visibility for everyone on the machine.
It is a much narrower cost than defeating the user namespace would be - it
grants observability, not a DAC bypass, mount capability, or anything else
`CAP_SYS_ADMIN` would otherwise imply - but it is not free, and it is yours to
make, on the host, outside of any project's `.agentbox.toml`:

```console
$ sudo sysctl kernel.perf_event_paranoid=1     # or lower, depending on what you need
```

One further residual risk once that sysctl is lowered: `perf_event_open`'s
"CPU-wide" mode (a specific CPU core, every process on it, rather than one
target process) is not mediated by any namespace at all - a box in this mode
can observe activity from processes outside its own PID namespace, including
the host's, via cycle-counting side channels. Per-process monitoring stays
bounded by the box's PID namespace regardless of the sysctl.

**The AppArmor profile does not cover any of this.** `contrib/apparmor/
agentbox-nspawn`'s broad `capability,` grant is irrelevant here - AppArmor
implements no `perf_event_*` LSM hooks at all (unlike SELinux, which has a
dedicated `perf_event` object class with `PERF_EVENT__CPU`/`PERF_EVENT__KERNEL`/
`PERF_EVENT__TRACEPOINT` permissions). There is no profile rule that could gate this
syscall even if you wanted one. `SystemCallFilter=perf_event_open` is the
*only* control agentbox applies, with no defense-in-depth behind it - another
reason `perf` defaults to off and is meant to be turned on deliberately, per
project, rather than left on everywhere.
