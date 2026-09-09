# User guide

## The 30-second version

```console
$ cd ~/dev/myproject
$ agentbox init          # writes .agentbox.toml; edit the rw/ro lists
$ agentbox shell         # first run creates the box, then drops you in
```

Inside, you are your own username, in your project directory, with `sudo`,
`pacman`, `git` and `jj`. Exit with `exit`; the box and everything it installed
stays until you `agentbox reset` or `agentbox rm`.

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
[box]$ sudo pacman -S cargo-nextest postgresql
[box]$ npm install -g @anthropic-ai/claude-code
[box]$ pip install --user ruff        # or use a venv; both work
```

Writes go to that box's overlay, never to the shared base image and never to
the host. To make packages part of the box from the start, list them in
`.agentbox.toml` — they are installed the first time the box is created:

```toml
packages = ["cargo-nextest", "postgresql", "redis"]
```

To add a package to *every* box, put it in `base_packages` in
`~/.config/agentbox/config.toml` and run `agentbox build --refresh`.

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
$ agentbox shell --ssh-agent            # forward $SSH_AUTH_SOCK, no keys on disk
```

```toml
# .agentbox.toml - forward a token instead
pass_env = ["TERM", "GITHUB_TOKEN"]
```

```toml
# or map keys read-only, if you accept that the agent can read them
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
network = "host"   # default: shares the host network namespace
network = "none"   # no network at all
network = "nat"    # private namespace, veth pair, NAT via systemd-networkd
```

`host` is simplest and what you usually want: `pacman`, `npm` and API calls all
work with zero setup. The cost is that the box can reach services bound to the
host's `localhost` and can bind host ports.

`none` is the strong option for a review or refactor task that needs no
network. Package installs will fail, so bake what you need into `packages`
first.

`nat` gives the box its own network namespace, so host `localhost` is out of
reach. It requires `systemd-networkd` enabled on the host, which is not the
default on Arch with NetworkManager:

```console
$ sudo systemctl enable --now systemd-networkd
$ agentbox up      # nat needs booted mode so the box can configure host0
```

`agentbox` masks `systemd-networkd` and `systemd-resolved` inside boxes that
use host networking, precisely so a booted box can never reconfigure your
host's interfaces. In `nat` mode they are left enabled.

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
packages = ["nodejs", "npm"]        # .agentbox.toml, installed on box creation
pass_env = ["ANTHROPIC_API_KEY"]    # TERM, COLORTERM and LANG come as standard
```

```toml
# ~/.config/agentbox/config.toml - in every box, baked into the base image
base_packages = ["base", "base-devel", "sudo", "git", "jujutsu", "nodejs", "npm"]
```

If you want the agent to reuse your host login rather than an API key, map its
credential file read-only and accept that the box can read it:

```toml
ro = ["~/.claude/.credentials.json"]
```

Read-only also means the box cannot rewrite the file when the token expires, so
that login stops working until you refresh it on the host. An API key through
`pass_env` has no such expiry. Leave `~/.claude.json` unmapped either way: the
agent writes to it constantly, and the box keeps its own.

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

`reset` is the one to reach for when an agent has mangled the box's `/etc` or
installed something poisonous: it deletes the overlay and rebuilds the box from
the shared base in about a second. Your project directory is a bind mount, so
`reset` never touches your code.
