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

### What can and cannot be locked down

One case *can* be closed off: `.git/hooks` never needs to be writable by the
agent, so you can map it read-only over the copy in the working tree:

```toml
# .agentbox.toml
ro = [".git/hooks"]
```

With that, the box can read the hooks but cannot plant new ones for `git` on the
host to run. Note this is a **recommended config entry you add**, not something
`agentbox` does for you: `agentbox init` does *not* currently emit a
`.git/hooks` read-only map (automatic support is planned but not yet in place),
so add it yourself if you want it.

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

Never point the destination's source at your host `~/.claude`.

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

**OAuth / subscription.** Here the box has to read a token file, so mount only
the credentials file, **read-only**:

```toml
# .agentbox.toml (or ~/.config/agentbox/config.toml)
ro = ["~/.claude/.credentials.json"]
```

Read-only means the box cannot rewrite your credentials or plant anything
alongside them. It does **not** hide the token: the file is readable inside the
box, so an untrusted agent could exfiltrate it, and a leaked OAuth token can act
as your Claude account until you revoke it or it expires. This residual risk is
inherent whenever the box authenticates *as you* - the API-key setup carries the
same exposure for `ANTHROPIC_API_KEY`. If that matters, prefer a key you can
scope and rotate, and revoke it when you are done.

### Reading host Claude settings

If the box needs specific host Claude settings (not credentials), mount those
specific files **read-only** - never read-write:

```toml
# .agentbox.toml
ro = [
  "~/.claude/settings.json",   # box may read it; cannot rewrite it
  "~/.claude/.credentials.json",
]
pass_env = ["ANTHROPIC_API_KEY"]   # if using the API-key setup instead
```

Read-only is what makes this safe: any hooks or `command`s in a settings file
mounted this way run *inside the box* (contained), and the box cannot edit the
file to plant new host-executed config. The principle to carry away:

* **writable, box-local `.claude`** for the agent's own runtime state,
* **read-only (or simply absent)** for any host `.claude` bits the box needs to
  read,
* **credentials read-only, or supplied via `pass_env`** - never read-write, and
  never the whole host `~/.claude`.
