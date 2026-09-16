# User guide

## The 30-second version

```console
$ cd ~/dev/myproject
$ agentbox init          # writes .agentbox.toml; edit the rw/ro lists
$ agentbox shell         # first run creates the box, then drops you in
```

Inside, you are your own username, in your project directory, with `sudo`, a
package manager and `git`. The box runs the same distribution the host does
([setup.md](setup.md#which-distribution-is-inside-the-box)), so that is
`pacman` and `jj` on an Arch host, `apt-get` on a Debian or Ubuntu one (where
`jj` is not packaged, and has to be added to `packages` or installed by hand).
Exit with `exit`; the box and everything it installed stays until you
`agentbox reset` or `agentbox rm`.

## Daily workflow

```console
$ agentbox shell                       # interactive shell in this project's box
$ agentbox run -- cargo test           # one command, then exit
$ agentbox run -- claude --dangerously-skip-permissions   # once it is in the box
$ agentbox shell --root                # container root, for poking at /etc
$ agentbox status                      # what is mapped where
$ agentbox ls                          # every box, and how much each has written
$ agentbox reset                       # forget everything the container wrote
```

`run` and `shell` are the same command; use whichever reads better. Anything
after `--` is the command line, so quoting works normally:

```console
$ agentbox run -- bash -lc 'cd src && make -j8 2>&1 | tail -40'
```

A redirected `run` is a normal pipeline component: when its input or output is
not a terminal it hands the box the file descriptors as they are, so output
comes back byte for byte, stderr stays separate from stdout, and EOF crosses
the pipe in both directions.

```console
$ agentbox run -- cargo metadata --format-version 1 | jq -r .packages[].name
$ git diff | agentbox run -- git apply -
$ agentbox run -- ./flaky-test 2>errors.log
```

## Installing packages

Just do it. The box has a working keyring and its own writable `/usr`.

```console
[box]$ sudo pacman -S cargo-nextest postgresql        # Arch box
[box]$ sudo apt-get install postgresql redis-server   # Debian/Ubuntu box
[box]$ npm install -g @anthropic-ai/claude-code
[box]$ pip install --user ruff        # or use a venv; both work
```

Package *names* are the one thing that does not carry across: `packages` in a
`.agentbox.toml` is a list for whichever archive the image was built from, so
a project file shared between an Arch host and a Debian one needs the names
its own host understands.

Writes go to that box's overlay, never to the shared base image and never to
the host. To make packages part of the box from the start, list them in
`.agentbox.toml` — they are installed the first time the box is created:

```toml
packages = ["cargo-nextest", "postgresql", "redis"]
```

To add a package to *every* box, put it in `base_packages` in
`~/.config/agentbox/config.toml` and rebuild the shared image. That list
replaces the built-in one rather than adding to it, and the rebuild needs every
box powered off, so it has a short procedure of its own:
[setup.md](setup.md#adding-a-package-to-every-box).

## sudo

`wheel` is `NOPASSWD` inside the box and your user is in it, so `sudo` works
with no password and no prompt. It cannot affect the host: container UID 0 is
an unmapped, unprivileged UID on the host side, so `sudo rm -rf /` inside the
box destroys the box's overlay and nothing else. Recover with:

```console
$ agentbox reset
```

## git and jj

Both are preinstalled and behave normally. The pieces that make this pleasant:

* Your `~/.gitconfig`, `~/.config/git` and `~/.config/jj` are mapped read-only
  by default, so `user.name`, `user.email`, aliases and jj settings all apply.
* Bind mounts use `owneridmap`, which maps the host owner of the source onto
  the sandbox user. Files inside show as owned by you, so git never complains
  about `dubious ownership` and you do not have to add `safe.directory`.
* `.git`/`.jj` live inside the mapped project directory, so commits you make in
  the box are immediately visible on the host, and vice versa.

A colocated jj repo works as expected:

```console
[box]$ jj st
[box]$ jj new -m 'wip'
[box]$ jj git push -c @-       # needs credentials, see below
```

### Credentials for push

Nothing that can authenticate to a remote is mapped in by default. Pick the
level you are comfortable with:

```console
$ agentbox shell --ssh-key ~/.ssh/id_ed25519   # box-scoped agent, this key only
```

This starts a **dedicated** ssh-agent for the box holding only the keys you
name, and forwards that agent's socket in - never the host's own
`$SSH_AUTH_SOCK`, which would expose every key it holds. Each key is added with
`ssh-add -c`, so the box using it prompts you on the host to confirm. Set it
persistently in the project file:

```toml
# .agentbox.toml - only these keys, via a box-scoped agent
ssh_keys = ["~/.ssh/id_ed25519"]
```

```toml
# .agentbox.toml - forward a token instead
pass_env = ["TERM", "GITHUB_TOKEN"]
```

```toml
# or map keys read-only, if you accept that the box can read the key material
ro = ["~/.ssh/id_ed25519", "~/.ssh/known_hosts"]
```

The agent-friendly pattern is to let the agent commit inside the box and push
from the host yourself. Then nothing in the box can write to a remote, and you
review before anything leaves the machine.

## Mapping directories

The project directory is always mounted read-write at its real path. Everything
else is explicit:

```toml
rw = [
  "~/dev/shared-lib",                    # same path inside and out
  "~/scratch:/scratch",                  # host:container
]
ro = [
  "~/dev/reference-impl",                # read it, cannot change it
  "~/.cargo/registry",                   # warm caches without letting it write
  "~/dev/vendor/protos",
]
```

Rules:

* `~` is your host home on the source side, and the sandbox user's home on the
  destination side — the same string, since the usernames match.
* A bare path maps to the identical path inside. That matters for path
  dependencies: a `Cargo.toml` with `path = "../shared-lib"`, a `go.work`
  entry, a `pyproject.toml` editable install or a tsconfig `paths` alias all
  resolve because the layout on both sides is identical.
* Read-only really is read-only: the mount is `BindReadOnly=`, enforced by the
  kernel, not by permissions the container root could change.
* Destinations under `/tmp`, `/run`, `/dev`, `/proc` or `/sys` are refused.
  systemd-nspawn covers each of those with a mount of its own, so a bind
  underneath it would be hidden and end up owned by container root rather than
  by you. This applies to the project directory too: a project living under
  `/tmp` cannot be put in a box. Move it, or map it to a destination elsewhere
  with `~/scratch:/scratch`-style syntax.
* Sources that do not exist are skipped with a warning rather than failing.
* One-off maps without editing config: `--map` (ro) and `--rw-map` (rw),
  repeatable.

```console
$ agentbox shell --map ~/dev/other-repo --rw-map ~/tmp/out
```

## Networking

```toml
network = "nat"    # default: private namespace, veth pair, NAT via systemd-networkd
network = "host"   # shares the host network namespace
network = "none"   # no network at all
```

`nat` is the default: the box gets its own network namespace with working
outbound connectivity (package installs, `npm` and API calls all work), while the host's
`localhost` services and abstract-namespace sockets (e.g. the X11 display) stay
out of reach.

`host` shares the host network namespace: simplest and zero setup, but the box
can reach services on the host's `localhost`, bind host ports, and connect to
host abstract sockets such as the X11 server. Opt into it only for a trusted
box — see [security.md](security.md).

`none` is the strong option for a review or refactor task that needs no
network. Package installs will fail, so bake what you need into `packages`
first.

Because `nat` is the default, note it needs a little host setup: `systemd-networkd`
enabled on the host, and - if you use NetworkManager - NM told to leave the
container veths (`ve-*`/`vz-*`) alone. See [setup.md](setup.md#nat-networking)
for the exact steps. If a nat box comes up without a route, `agentbox` warns on
launch and points there.

`agentbox` masks `systemd-networkd` and `systemd-resolved` inside boxes that use
host networking, so a booted box can never reconfigure your host's interfaces.
In `nat` mode it enables them *in the box* instead, so the box configures its own
`host0` and DNS.

## Resource limits

Real cgroup limits, not advisory:

```toml
memory_max = "16G"
cpu_quota  = "800%"    # 8 cores' worth
tasks_max  = "4096"
```

A runaway `make -j$(nproc)` or a memory-leaking test then hits a wall instead
of your host. Every launch boots the box's `systemd-nspawn@<box>.service`, and
the caps sit on a drop-in on that unit, so they apply the same to `run`,
`shell` and `up` alike, covering every session attached to the box at once.

To see them in force, look from the host rather than from inside — the box's
own `/sys/fs/cgroup` is a subgroup below the capped one, so it reads `max`:

```console
$ agentbox up
$ systemctl show -p MemoryMax -p CPUQuotaPerSecUSec -p TasksMax \
    systemd-nspawn@<box>.service
```

## Hardening with AppArmor

Beyond the user namespace, empty capability set, seccomp filter and mount plan,
agentbox can apply an optional AppArmor profile as a mandatory-access-control
second wall — it stays up even if one of the other layers regresses. It ships in
[contrib/apparmor/](../contrib/apparmor/), targets AppArmor (so Arch/Debian/
Ubuntu/SUSE, not SELinux distros), and is off until you load it:

```console
$ sudo install -Dm644 contrib/apparmor/agentbox-nspawn /etc/apparmor.d/agentbox-nspawn
$ sudo apparmor_parser -r -C /etc/apparmor.d/agentbox-nspawn   # complain mode first
```

Once loaded, agentbox applies it automatically (the `apparmor` key defaults to
on-if-available) and still launches fine without it. Test in complain mode and
review `sudo aa-logprof` before `sudo aa-enforce` — see
[contrib/apparmor/README.md](../contrib/apparmor/README.md).

## One box, several sessions

A box is a booted machine: `shell` or `run` boots it (systemd as PID 1 inside,
registered with machined) the first time, and every later `shell`/`run` on the
same project attaches another session to the *same* running box. So a second
terminal can join a box an agent is already working in:

```console
$ agentbox run -- claude       # boots the box, runs the agent in it
$ agentbox shell               # from another terminal: a shell in the same box
$ machinectl list              # the box shows up as a machine
$ journalctl -M <box>          # its journal
```

Who first started the box decides when it stops. A box booted by `shell`/`run`
exists to carry sessions, so it powers off when the last session leaves —
whichever session that turns out to be, not necessarily the first. A box booted
by `agentbox up` is kept: it stays running until `agentbox down`, however many
sessions come and go (and `up` on a box a `shell` already started promotes it to
kept). Use `up` when you want systemd services — timers, socket activation, a
database — running in the box with no session attached.

Every session reads the same generated `/etc/systemd/nspawn/<box>.nspawn`, so
the mounts, UID map and network mode are identical throughout. Who you are is
not in that file: a box starts systemd as container root, as PID 1 must be, and
each `shell`/`run` attaches as you (or as root with `--root`) afterwards, in the
project directory.

## Running agents inside

The point of the exercise. Two things make agents behave better in here:

1. **Skip permission prompts safely.** The blast radius is the box's overlay
   plus whatever you mapped read-write, so the aggressive flags stop being
   reckless: `claude --dangerously-skip-permissions`, `codex --full-auto`, and
   equivalents.
2. **Keep agent state per project.** The sandbox user's home is per box, so
   `~/.claude`, `~/.npm` and friends persist for that project and do not leak
   between projects.

Nothing you installed on the host is in the box, so an agent CLI has to be put
there. `agentbox run -- claude ...` before that says so:
`claude: not found in box <name>`. Four ways in, cheapest to most reproducible:

```console
[box]$ sudo npm install -g @anthropic-ai/claude-code    # ad hoc, this box only
```

```toml
# .agentbox.toml - map the copy already on the host, read-only. No download,
# and the box tracks whatever the host has. Map the whole install, not just the
# launcher: an Arch claude-code package is a wrapper in /usr/bin execing a
# binary in /opt.
ro = ["/usr/bin/claude", "/opt/claude-code"]
```

```toml
packages = ["nodejs", "npm"]              # .agentbox.toml, installed on box creation
pass_env = ["CLAUDE_CODE_OAUTH_TOKEN"]    # TERM, COLORTERM and LANG come as standard
```

```toml
# ~/.config/agentbox/config.toml - in every box, baked into the base image.
# This list REPLACES the built-in one rather than adding to it, so start from
# the copy in config.example.toml, which is the built-in list verbatim, and add
# to that. A short list here is a short image the next time you `build --force`.
base_packages = [
  "base", "base-devel", "sudo", ...,      # the built-in list, unchanged
  "claude-code",                          # and what this host wants on top
]
```

If you want the agent to use your subscription rather than an API key, mint a
long-lived token on the host and forward that:

```console
$ claude setup-token
```

```toml
pass_env = ["CLAUDE_CODE_OAUTH_TOKEN"]
```

Do **not** reach for `ro = ["~/.claude/.credentials.json"]` instead. It looks
like it should work and does not: that file holds only the tokens, while the
account they belong to lives in `~/.claude.json`, which has to stay unmapped
because the agent rewrites it constantly. The box ends up with tokens and no
account, and Claude Code asks you to log in. See
[security.md](security.md#authentication).

### The first-run wizard in a fresh box

A working token is not the whole story. Interactive Claude Code runs its
first-run wizard before it gets as far as using one, and the state saying you
are past that wizard lives in `~/.claude.json` - which a fresh box does not
have. So a new box walks you through picking a theme, then **signing in**, then
whether you trust the project directory.

That middle step is the trap. A box cannot complete a browser sign-in, so a box
holding a perfectly good token still asks you to log in, and answering the
wizard is not a way through it. It reads as a broken token and is not one:

```console
[box]$ claude auth status
logged in
```

`auth status` reporting `logged in` while the interactive session still asks is
the tell - authentication is fine, the wizard is what is in the way.

Seed the one key that retires it instead. From inside the box:

```console
[box]$ echo '{"hasCompletedOnboarding":true}' > ~/.claude.json
```

That single key skips the theme picker *and* the sign-in step; the box's token is
used as it already was. The remaining "do you trust this folder?" prompt is
answerable in the box and persists, since `~/.claude.json` is writable there.
Both survive until `agentbox reset` or `rm`.

The recommended way to make every *new* box start past both prompts is `cpy`:
seed a dedicated host file once, and agentbox copies it in the first time each
box boots - never as a live bind, so nothing the box subsequently writes to
`~/.claude.json` (session ids, costs, timings, MCP state - it rewrites this
file constantly) ever reaches your host copy:

```toml
cpy = ["~/.agentbox-claude/myproject.json:~/.claude.json"]
```

If instead you want that state to persist *across* an `agentbox reset` (`cpy`
content is wiped along with the rest of the box's overlay on reset, and only
re-copied on the box's next boot), map a **dedicated** host file read-write
instead, not your own `~/.claude.json`:

```toml
rw = ["~/.agentbox-claude/myproject.json:/home/you/.claude.json"]
```

`rw`, not `ro`: Claude Code rewrites this file on every session, and a
read-only mount makes it fail. That is a second reason it has to be a file of
your own rather than the host's - and, either way, the source is a dedicated
file that only agentbox and your seeding ever touch.

Seed it once and every box after that starts at the prompt, trust prompt
included:

```json
{
  "hasCompletedOnboarding": true,
  "projects": {
    "/home/you/dev/myproject": { "hasTrustDialogAccepted": true }
  }
}
```

Those two keys are the whole seed - `theme` and `lastOnboardingVersion` appear
in a host `~/.claude.json` but are not load-bearing here, and Claude Code falls
back to its default theme without them. The `projects` key is the path *inside*
the box, which agentbox spells the same as on the host, so it is your real
project path; a file shared between boxes needs one entry per project
directory. This is the `~/.claude.json` analogue of the dedicated `.claude`
directory in [security.md](security.md), and it is safe for the same reason: it
is a file of your own that the host's Claude Code never reads, so the `mcpServers`
the agent may write into it are never host-executed config. Mapping your real
`~/.claude.json` is what stays off the table.

A reasonable default for unattended runs:

```toml
network = "host"
memory_max = "16G"
cpu_quota = "600%"
ro = ["~/dev/reference"]   # on top of the built-in ~/.gitconfig and friends
rw = []
```

Then `agentbox run -- claude --dangerously-skip-permissions -p "$TASK"` and let
it work. Review the diff on the host with `jj diff` afterwards.

## Cleaning up

```console
$ agentbox reset      # keep the box, discard every write it made
$ agentbox rm         # delete the box entirely
$ agentbox ls         # WRITES column shows what each box is costing you
```

Those act on the box for the current directory. To clean up from anywhere, name
the box `agentbox ls` printed:

```console
$ agentbox rm myproj-1a2b3c4 -y
```

`reset` is the one to reach for when an agent has mangled the box's `/etc` or
installed something poisonous: it deletes the overlay and rebuilds the box from
the shared base in about a second. Your project directory is a bind mount, so
`reset` never touches your code.
