# agentbox — Remaining Vulnerabilities Tracker

Working doc for the vectors from the breakout red-team (full detail in
`BREAKOUT_REPORT.md`). Fill in the **Decision / mitigation** blocks as you triage.

**Closed** — implemented on the `worktree-multi-session` line; `cargo build` + `cargo test` (44) green:
- A1 — control-char/newline injection into the generated `.nspawn` file → *fixed*
- A4 — bind `dst` path-traversal → create/chown outside rootfs as root → *fixed*
- A3 — overbroad bind source (`/`, host home, ancestor) → *fixed*
- S1 — ssh-agent forwarding → box-scoped agent, authorized keys only (`ssh_keys`) → *fixed*
- B3 — no LSM → optional AppArmor profile, fail-soft → *fixed*
- G1 — `.git/hooks` executes on host → `agentbox init` maps it read-only for git projects → *fixed*
- A5 / B2 — host-net exposure (X11, host localhost) → **`nat` is now the default** → *fixed*
- A2 / C1 / B1 / G2 — rw-mount / `.claude` trust boundary → *documented* in `docs/security.md` (config, not code); A2's "writable but not the host's" half now also has a code-level answer, the `cpy` mapping type (see A2 below)

Host **runtime** testing is still recommended (nat connectivity + first-run package install, the
box-scoped ssh-agent lifecycle, and the AppArmor drop-in); compile and unit tests pass in-box.

The detail below is retained for reference. The through-line: none was a namespace-containment
failure — each is a *trust boundary* where the box writes data (or shares a network) that a
**host process later consumes**.

---

## Summary

| # | Sev | Vulnerability | Reaches `~/i_win`? | Class |
|---|-----|---------------|--------------------|-------|
| A2 | 🔴 Critical | rw-mounted `~/.claude` → planted `settings.json` hook runs as host user | Yes (host code-exec) | writable config consumed by host tool |
| C1 | 🟠 High | Other host-executed `.claude` assets (MCP servers, statusline `command`, `agents/`, `commands/`) | Yes (host code-exec) | writable config consumed by host tool |
| G2 | 🟠 High\* | `build.rs` / proc-macro / `.cargo/config.toml` runner in the rw project bind | Yes (host code-exec) | writable code consumed by host tool |
| B1 | 🟠 Medium | `~/.claude/.credentials.json` readable in-box | No (credential theft) | secret exposure |
| A5 | 🟠 High | Host X11 abstract socket reachable (`@/tmp/.X11-unix/X0`) | Yes (input injection, auth-gated) | shared host network |
| B2 | 🟠 Medium | Host localhost daemons reachable (127.0.0.1 services) | No (lateral) | shared host network |
| S1 | 🟡 Medium | `ssh_agent` socket forwarding (if enabled) | No (key hijack / lateral) | forwarded host resource |
| B3 | ⚪ Info | No AppArmor/SELinux profile | No (defense-in-depth) | hardening |

\* = untested completeness-critic follow-up (plausible, not yet confirmed).

---

## A2 — rw-mounted `~/.claude` → hook code-exec as host user  🔴 Critical

`.claude` is bind-mounted rw with `owneridmap`, so a file written from inside lands on the host
owned by the real user. Claude Code executes `settings.json` `command` hooks
(SessionStart/PreToolUse/Stop/…) as the invoking user, so the box can plant a hook that runs
host-side — e.g. `touch ~/i_win`. Lowest-friction path in the whole report.

**Suggested mitigation:** don't rw-mount `~/.claude`. Mount read-only, or mount only the
subpaths the box needs, or give the box its own per-box `.claude` overlay separate from the host's.

**Decision / mitigation / notes:**
&nbsp;
Question: this was needed because agents write to ~/.claude. How do you propose this works instead?

**Reply (Claude):** The box genuinely needs a *writable* `.claude` (session transcripts, todos,
history, shell snapshots) — but not the *host's*. Proposal: give each box its own writable
`.claude` on the overlay (per-box, never touches the host), and bind in from the host only what the
agent must *read*, read-only. Two sub-parts need your call:
  (a) **Auth** — today the box authenticates via the host's `.credentials.json`. Prefer a token via
      `pass_env` (e.g. `ANTHROPIC_API_KEY`) or a scoped/short-lived cred, so no host credential file
      is writable or exposed. If OAuth must be shared, mount only `.credentials.json` read-only
      (accepts B1).
  (b) **Config the agent reads** — mount specific files (e.g. `settings.json`) read-only, so the box
      can read but never plant hooks.
  → Open decision: which auth path (env token vs ro credential)? That choice fixes the exact mount set.

**Decision (maintainer):** solve by *configuration + documentation*, not agentbox code. `.claude` is a
special case of the rw-mount trust issue. Document how to set up each auth option and how the mounts
should be configured.
**Status (Claude):** ✅ DONE, integrated — `docs/security.md` §"Running Claude Code inside a box" (own writable `.claude`; API-key vs OAuth mount setups).

**Update:** the `cpy` mapping type (`src/sandbox.rs`, `src/nspawn.rs`) has since landed as a real
code-level resolution of the "(a) writable, but not the host's" half of the proposal above — the
part this decision previously only approximated via a dedicated host directory bound `rw` (a
decoy `~/.claude`/`~/.claude.json` you kept separate from your real one, still a live bind and
still a trust call resting entirely on you never mixing the two up). `cpy` instead copies a seed
file (`~/.claude.json` in particular) into the box's own overlay once, on first boot with the
destination absent, and never mounts anything: the box's writes never propagate to any host file
at all, decoy or otherwise. The dedicated-`rw`-directory pattern is still the right answer when
state needs to persist *across* `agentbox reset` (`cpy` content is wiped with the rest of the
box's overlay on reset), so both remain documented in `docs/security.md`, with `cpy` now the
preferred default and the `rw` directory kept for that one remaining case. See
`docs/configuration.md`#`cpy` and `docs/design.md` ("Why `cpy` is a one-time copy, not a nested
overlay") for the mechanism.
&nbsp;

---

## C1 — other host-executed `.claude` assets  🟠 High

Same rw `.claude` bind. Beyond `settings.json` hooks, host Claude Code also reads and acts on
`settings.local.json`, MCP server definitions, the statusline `command`, `agents/`, `commands/`,
and output styles — each a further host-code-exec surface.

**Suggested mitigation:** folded into A2 — a read-only or narrowed `.claude` mount closes all of
these at once. Track separately only if A2 lands as a partial fix.

**Decision / mitigation / notes:**
<!-- write here -->
&nbsp;
Question: What is the actual exploit here? Is it just leakage between agents?

**Reply (Claude):** Not leakage between agents — host code-execution. The rw `.claude` lets the box
write files the *host's* Claude Code later runs as you: an MCP server entry with a `command`, the
statusline `command`, a custom slash-command / `agents/` definition, an output style. Your next host
session executes them as your user → same outcome as A2's `settings.json` hook. So C1 is "A2, but
more file types," and the same fix (no write access to the host `.claude`) closes all of them. Folds
into A2.
&nbsp;

---

## G2 — `build.rs` / cargo execution in the rw project bind  🟠 High (untested)

The project dir is a rw `owneridmap` bind. If the host user later runs `cargo build`/`test` in
the repo, a planted `build.rs`, a proc-macro, a path/`git` dependency, or a `.cargo/config.toml`
`runner=` executes on the host as that user. Same class as G1 but for build tooling, not git.

**Suggested mitigation:** treat the project bind as tainted — don't run host build tooling against
a box-shared repo, or copy (not bind) the project for untrusted agents. Consider documenting
sharply. (Confirm the vector first — untested.)

**Decision / mitigation / notes:**
<!-- write here -->
&nbsp;
Question: is the vector here that code ran in the host (possibly unintentionally as via build.rs); that could allow the box to be breached after. 1st: how exactly? 2nd: isn't this a fundamental trust model issue?

**Reply (Claude):** (1) *How exactly:* the box has rw (idmapped) write access to the project. A
compromised agent edits `build.rs` / a proc-macro / `.cargo/config.toml` (`runner=`,
`rustc-wrapper=`). Later, when *you* run `cargo build/test/run` on the host in that repo (outside the
box), cargo executes that code as your host user → host code-exec. It doesn't breach the box; it
escapes *from* the box via a delayed host trigger. (2) *Yes — fundamentally.* Unlike G1 (git hooks
live in `.git/hooks`, which can be a ro mount), `build.rs` *is* project source and must stay
writable, so there is no clean ro fix. The real answer is trust-model: once a box has touched a repo,
treat it as tainted — build *inside* a box, or copy-not-bind the project for untrusted agents. So
this is "document + optional copy-mode," not a code patch. Needs your call on which.

**Decision (maintainer):** documentation only. Copy-mode is rejected — we *want* the agent's
output visible on the host. This is an underlying trust property of rw mounts (`.git/hooks`,
`.cargo`, `build.rs`, and similar): an agent can weaponize innocuous-looking files that the host
later executes. Document it so users know a box-touched repo may be tainted when used outside.
**Status (Claude):** ✅ DONE, integrated — `docs/security.md` (rw-mount trust boundary + weaponized-file list; copy-mode rejected).

Note: the `cpy` mapping type added since (see A2's update above) does not revisit this decision.
`cpy` is a one-time copy *into* the box for auxiliary files where write-back is unwanted; it is
not a copy of the project *out of* a live bind, so it does nothing for a project directory that
must stay a writable, host-visible bind for the workflow to make sense. `docs/security.md` now
says this explicitly, so a reader of the `cpy` docs doesn't mistake it for the rejected proposal.
&nbsp;

---

## B1 — `~/.claude/.credentials.json` readable in-box  🟠 Medium

Bind-mounted read-only + idmapped, so the host Claude OAuth tokens (access/refresh) are readable
as the user from inside the box. Enables account takeover / API use; no goal-write by itself.

**Suggested mitigation:** never expose the credential file to an untrusted box — exclude the path
entirely, or mount a `.claude` view that omits it. Naturally covered if A2 narrows the mount.

**Decision / mitigation / notes:**
<!-- write here -->
&nbsp;
Question: what do you mean account takeover?

**Reply (Claude):** `.credentials.json` holds your Claude OAuth session — an access token and a
longer-lived refresh token. Anything that reads and exfiltrates it can then act as your Claude
account from anywhere until you revoke it or it expires: make Claude/API requests authenticated and
billed as you, and reach your account's data. "Takeover" = control of your Claude account/session,
not just this box. Mitigation ties to A2 (don't expose the file to an untrusted box).
&nbsp;

---

## A5 — host X11 abstract socket reachable  🟠 High

`network = "host"` shares the host network namespace, and abstract-namespace unix sockets live in
the netns (not the filesystem), so the host Xorg's `@/tmp/.X11-unix/X0` is connectable. An X client
could use XTEST to inject synthetic keystrokes into the host session. Connectivity confirmed;
exploitation is gated today only by the MIT-MAGIC-COOKIE (an X-auth check, not a namespace boundary).

**Suggested mitigation:** default to `network = "none"`/`nat` rather than host. If host-net is
required, document the X11 exposure and advise users never to `xhost +`; consider an X11-isolation
option.

**Decision / mitigation / notes:**
<!-- write here -->
&nbsp;
Question: with network = nat, can the agent still use the internet to fetch things?

**Reply (Claude):** Yes. `network = "nat"` gives the box its *own* netns with a veth + NAT through
the host, so outbound internet works (crates, npm, git clone) — it just no longer *shares* the
host's netns, so host abstract sockets (X11) and host `127.0.0.1` services become unreachable. That
is exactly the isolation we want. (`network = "none"` is the fully-offline one that breaks fetching.)
So NAT keeps the internet and closes A5 + B2. If NAT-as-default is OK with you, say so and I'll spawn
an agent to make it the default + document the host-net exposure.

Decision: make nat the default
**Status (Claude):** ✅ DONE — `nat` is the default (src/config.rs). The multi-session boot model + install-packages-after-boot close the old shell/run and create-time-install gaps; `host` remains an explicit opt-in. Build + tests green; also closes B2. (Host runtime test recommended.)
&nbsp;

---

## B2 — host localhost daemons reachable via shared netns  🟠 Medium

Same shared netns exposes every host `127.0.0.1` service to the box (observed: an unknown tcp/59877
daemon, Spotify tcp+udp/57621, mDNS, SSDP). Lateral attack surface against host daemons; not
fingerprinted or attacked.

**Suggested mitigation:** same as A5 — private/NAT networking removes it. If host-net stays,
document which loopback services become reachable.

**Reply (Claude):** No separate decision needed — B2 is closed by the same networking choice as A5
(default to `nat`).

**Status (Claude):** ✅ DONE — closed by the `nat` default (see A5).

**Decision / mitigation / notes:**
<!-- write here -->
&nbsp;

&nbsp;

---

## S1 — `ssh_agent` socket forwarding (if enabled)  🟡 Medium

`binds()` conditionally forwards `$SSH_AUTH_SOCK` into the box. When enabled, an in-box process can
use the host agent's keys → agent hijacking / lateral movement as the user. Config-dependent; off by
default.

**Suggested mitigation:** keep `ssh_agent` off by default; when enabled, warn that the box can use
your keys, and prefer a scoped or confirm-on-use agent.

**Decision / mitigation / notes:**
<!-- write here -->
&nbsp;
Correct. Off by default, explicit authorization for a box to use particular keys, enabled by the configuration.

**Reply (Claude):** Agreed — **spawning an implementation agent now** (worktree `ssh-agent`). Design
it's taking: keep `ssh_agent = false` as the default; replace "forward the whole host agent" with a
config that names the specific keys a box may use (e.g. `ssh_keys = ["~/.ssh/id_ed25519_x"]`);
agentbox starts a dedicated, box-scoped `ssh-agent` holding only those keys (with `ssh-add -c`
confirm-on-use) and forwards *that* socket, so the host's full agent is never exposed.
&nbsp;

---

## B3 — no AppArmor/SELinux profile  ⚪ Info / hardening

Containment rests solely on user namespace + empty cap set + seccomp + mount masking. No LSM layer,
so a future capability or mount regression has no second line of defense.

**Suggested mitigation:** ship an nspawn AppArmor/SELinux profile as defense-in-depth.

**Decision / mitigation / notes:**
<!-- write here -->
&nbsp;
Question: Elaborate. What do these provide, how would this differ.

**Reply (Claude):** AppArmor/SELinux are Linux Security Modules — a mandatory-access-control layer
the kernel enforces *independently* of uid / namespaces / capabilities. A profile for the nspawn
payload whitelists exactly what the box may do (paths it can read/write/exec, capabilities, mounts,
syscalls beyond seccomp), so even if a namespace/cap/mount bug regressed, the LSM would still deny
the out-of-policy action. Today containment is single-layered (userns + empty caps + seccomp + mount
masking); an LSM adds a second, orthogonal wall. It differs from the current controls in being
policy-based and kernel-enforced *regardless of privilege*, rather than relying on the namespace
boundaries holding. Cost: a profile must be written and maintained per distro (Arch ships AppArmor
but no default nspawn profile). Hardening, not a specific-hole fix.

Decision: spin off a worker to implement this in a worktree
**Status (Claude):** ✅ DONE, integrated — `contrib/apparmor/agentbox-nspawn` profile + `apparmor` config toggle, applied on both launch paths, fail-soft (needs host load+test).
&nbsp;

---

### Two changes covered most of the table — status
1. **rw-mount / `.claude` trust boundary** → A2, C1, B1, G2 documented in `docs/security.md`; the
   automatable slice — `.git/hooks` read-only (G1) — is implemented. `.cargo`/`build.rs` (G2) stay
   documented (no clean code fix; copy-mode rejected).
2. **Default to private/NAT networking** → A5, B2 → **done** (`nat` is the default).
