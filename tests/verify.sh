#!/usr/bin/env bash
# End-to-end check of what agentbox promises: in-box package installation,
# harmless sudo, working git/jj, rw/ro directory mapping, resource limits,
# booted mode, network modes, that the privilege handover leaves nothing
# behind, that AGENTBOX_STATE cannot redirect a privileged run, and that none
# of it damaged the host.
# Creates a throwaway project under $XDG_CACHE_HOME, and removes its box at the
# end. Not /tmp: systemd-nspawn mounts its own tmpfs there, so a project below
# it cannot be mapped into a box at all (see nspawn::NSPAWN_OWNED). Override
# the location with AGENTBOX_VERIFY_ROOT.
#
#   ~/dev/agentbox/tests/verify.sh              # run everything
#   KEEP=1 ~/dev/agentbox/tests/verify.sh       # keep the box for poking at
#   WITH_BUILD=1 ~/dev/agentbox/tests/verify.sh # also exercise `agentbox build`
#
# WITH_BUILD is opt-in because it builds a second base image from scratch (a few
# minutes, ~3G, and it downloads whatever the host does not already have
# cached) - the same distribution `agentbox build` picks for this host, Arch
# via pacman or Debian/Ubuntu via debootstrap, so this covers whichever
# bootstrap path is actually in use here. It
# is the only section that covers the build path, which is where a mount
# propagation bug once unmounted /dev/pts and /run/user/$UID off the host. It
# builds into a throwaway AGENTBOX_STATE, so the real base image is never
# touched, and leaves the build log behind for a post-mortem.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
AGENTBOX="${AGENTBOX_BIN:-$(command -v agentbox || echo "$here/../target/release/agentbox")}"
[[ -x "$AGENTBOX" ]] || { echo "no agentbox binary; run 'cargo build --release' or 'cargo install --path .' first" >&2; exit 1; }
# Absolutise it. Section 10 runs the build from a scratch directory to prove it
# writes nothing there, and the documented invocation passes a relative
# AGENTBOX_BIN (`AGENTBOX_BIN=target/release/agentbox tests/verify.sh`), which
# stops resolving the moment anything cd's - as an exit 127 several minutes in.
AGENTBOX="$(cd "$(dirname "$AGENTBOX")" && pwd)/$(basename "$AGENTBOX")"
root="${AGENTBOX_VERIFY_ROOT:-${XDG_CACHE_HOME:-$HOME/.cache}/agentbox-verify}"
mkdir -p "$root" || { echo "cannot create $root" >&2; exit 1; }
PROJ="$(mktemp -d "$root/agentbox-verify-XXXXXX")"
REF="$(mktemp -d "$root/agentbox-refonly-XXXXXX")"
LIMITS="$(mktemp -d "$root/agentbox-limits-XXXXXX")"
NAT="$(mktemp -d "$root/agentbox-nat-XXXXXX")"
CPY="$(mktemp -d "$root/agentbox-cpy-XXXXXX")"
CPYSRC="$(mktemp -d "$root/agentbox-cpysrc-XXXXXX")"
SSH="$(mktemp -d "$root/agentbox-ssh-XXXXXX")"
SSHKEYDIR="$(mktemp -d "$root/agentbox-sshkeys-XXXXXX")"
BSTATE=""
FAKE=""
pass=0 fail=0

ok()   { printf '\033[32mPASS\033[0m %s\n' "$1"; pass=$((pass+1)); }
no()   { printf '\033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
skip() { printf '\033[33mSKIP\033[0m %s\n' "$1"; }
check(){ if [[ "$2" == "$3" ]]; then ok "$1"; else no "$1 (got '$2', want '$3')"; fi; }
box()  { "$AGENTBOX" run "$PROJ" -- "$@"; }

cleanup() {
  [[ -n "${KEEP:-}" ]] && return
  "$AGENTBOX" down "$LIMITS" >/dev/null 2>&1
  "$AGENTBOX" rm "$PROJ" -y >/dev/null 2>&1
  "$AGENTBOX" rm "$LIMITS" -y >/dev/null 2>&1
  "$AGENTBOX" rm "$NAT" -y >/dev/null 2>&1
  "$AGENTBOX" rm "$CPY" -y >/dev/null 2>&1
  "$AGENTBOX" rm "$SSH" -y >/dev/null 2>&1
  rm -rf "$PROJ" "$REF" "$LIMITS" "$NAT" "$CPY" "$CPYSRC" "$SSH" "$SSHKEYDIR"
  [[ -n "${cwdprobe:-}" ]] && rm -rf "$cwdprobe"
  # The throwaway image is owned by the shifted container UIDs, so it needs root.
  [[ -n "$FAKE" ]] && sudo rm -rf "$FAKE"
  [[ -n "$BSTATE" ]] && sudo rm -rf "$BSTATE"
}
trap cleanup EXIT

printf 'project  %s (%s)\nref (ro) %s\n\n' \
  "$PROJ" "$(findmnt -no FSTYPE -T "$PROJ")" "$REF"

# Mount propagation is the one failure mode that damages the *host* rather than
# the box: a shared subtree bind-mounted into an image and then unmounted takes
# the host's own mounts with it. Snapshot the ones that would go first.
WATCH=(/dev/pts /dev/shm /dev/mqueue "/run/user/$(id -u)")
still_mounted() {
  local m
  for m in "${WATCH[@]}"; do
    findmnt -rno TARGET "$m" >/dev/null 2>&1 && printf '%s\n' "$m"
  done
}
MOUNTS_BEFORE="$(still_mounted)"

# Which of those are gone, or 'none lost'. Spelled out rather than piped
# through `comm | tr | sed`: comm needs sorted input, and when nothing is
# missing the pipeline carries a zero-line stream, which `sed s/^$/.../`
# cannot substitute into - so the check could never report success.
lost_mounts() {
  local now m missing=()
  now="$(still_mounted)"
  while IFS= read -r m; do
    [[ -n "$m" ]] || continue
    grep -qxF "$m" <<< "$now" || missing+=("$m")
  done <<< "$MOUNTS_BEFORE"
  if (( ${#missing[@]} == 0 )); then
    printf 'none lost\n'
  else
    printf '%s\n' "${missing[*]}"
  fi
}

echo 'reference material, do not edit' > "$REF/NOTES.md"
cat > "$PROJ/.agentbox.toml" <<TOML
# Pinned, not redundant: section 7 asserts the box reaches the network, and the
# tester's global config is free to default to nat or none.
network = "host"
# $REF only. ~/.gitconfig is a built-in default, and the git section below
# checks the host identity applies, so it fails if that stops happening.
ro = ["$REF"]
TOML

# ---------------------------------------------------------------- lifecycle
echo '--- creating box ---'
"$AGENTBOX" run "$PROJ" -- true || { echo 'could not create box'; exit 1; }
BOXNAME="$("$AGENTBOX" status "$PROJ" | awk '/^box /{print $2}')"
UIDBASE="$("$AGENTBOX" status "$PROJ" | awk '/^uid range/{split($3,a,"[.][.]");print a[1]}')"
printf 'box %s, uid base %s\n\n' "$BOXNAME" "$UIDBASE"

# The same box, named rather than pointed at: `agentbox ls` prints these names,
# and every management command takes one in place of the project directory.
check 'a box can be named instead of its project directory' \
  "$("$AGENTBOX" status "$BOXNAME" | awk '/^project /{print $2}')" "$PROJ"

echo '--- identity and paths ---'
check 'project mounted at its real host path' "$(box pwd)" "$PROJ"
check 'runs as the host user'                 "$(box id -un)" "$USER"
check 'same uid as on the host'               "$(box id -u)"  "$(id -u)"

# The image follows the host (src/distro.rs), so everything below that names a
# package manager or a package has to follow the image. Read it from the box
# rather than from the host: that is the thing under test, and a box built
# against an older generation can legitimately differ from what this host
# would build today.
case " $(box sh -c '. /etc/os-release 2>/dev/null; echo "${ID:-} ${ID_LIKE:-}"') " in
  *" arch "*) PKG=pacman ;;
  *" debian "*|*" ubuntu "*) PKG=apt ;;
  *) PKG=unknown ;;
esac
printf 'guest package manager: %s\n' "$PKG"
install_in_box() {
  case "$PKG" in
    pacman) box sudo pacman -Sy --noconfirm --needed "$@" ;;
    apt)    box sudo env DEBIAN_FRONTEND=noninteractive \
              apt-get install -y --no-install-recommends "$@" ;;
    *)      return 1 ;;
  esac
}

# The canary package used below to prove that an install persists, survives a
# remount and is undone by `reset`. `cowsay` is in both archives, but Debian
# puts it in /usr/games, which is not on the PATH nspawn hands a payload (only
# a *login* shell picks it up, via Debian's /etc/profile) - so the command has
# to be named by path there or every check reads as "not installed" while the
# package is in fact present and working.
CANARY=cowsay
case "$PKG" in
  apt) CANARY_CMD=/usr/games/cowsay ;;
  *)   CANARY_CMD=cowsay ;;
esac

echo
echo '--- 1. package installation inside the box ---'
install_in_box "$CANARY" >/dev/null 2>&1
installed() { box sh -c "command -v $CANARY_CMD >/dev/null && echo installed || echo gone"; }
check "$PKG installed a package" "$(installed)" installed
check 'the package actually runs'  "$(box sh -c "$CANARY_CMD moo | grep -c moo")" 1
check 'host is unaffected'         "$(command -v "$CANARY" || echo none)" none

# A remount is the cure for a box whose image moved underneath it, and it is
# only usable as a cure if it costs the box nothing: its writes live in `upper`
# on disk, not in the mount, so all a remount discards is the kernel's cached
# view of the layer below. The package installed a moment ago is exactly the
# state that has to survive.
"$AGENTBOX" remount "$PROJ" >/dev/null 2>&1
check 'remount keeps what the box installed' "$(installed)" installed
check 'remount leaves the box on a current overlay' \
  "$("$AGENTBOX" ls | awk -v b="$BOXNAME" '$1 == b {print $3}')" mounted
# Without the preflight this is nspawn's `execv(...) failed`, printed after the
# box is already up and with nothing to say the program was never installed.
MISSING="$("$AGENTBOX" run "$PROJ" -- agentbox-no-such-program 2>&1)"
check 'a payload missing from the box is named before launch' \
  "$(grep -c 'not found in box' <<< "$MISSING")" 1

echo
echo '--- 2. sudo, and its blast radius ---'
check 'sudo needs no password'   "$(box sudo -n id -un)" root
check 'container root is uid 0'  "$(box sudo -n id -u)"  0
box sudo -n sh -c 'echo owned > /etc/agentbox-was-here'
check 'container root wrote to its own /etc' \
  "$(box sh -c 'cat /etc/agentbox-was-here')" owned
check 'that write did not reach the host /etc' \
  "$([[ -e /etc/agentbox-was-here ]] && echo leaked || echo contained)" contained
HOSTOWNER="$(sudo stat -c %u "/var/lib/agentbox/boxes/$BOXNAME/upper/etc/agentbox-was-here")"
check 'container root maps to an unprivileged host uid' "$HOSTOWNER" "$UIDBASE"
check 'unmapped host directories are invisible' \
  "$(box sh -c "ls $HOME/.ssh >/dev/null 2>&1 && echo visible || echo hidden")" hidden

echo
echo '--- 3. git and jj ---'
check 'git present'               "$(box sh -c 'git --version | cut -d" " -f1-2')" 'git version'
check 'host git identity applies' "$(box git config --get user.email)" "$(git config --get user.email)"
box sh -c 'git init -q . && echo hello > file.txt && git add file.txt &&
           git -c commit.gpgsign=false commit -qm "from inside the box"' >/dev/null 2>&1
check 'commit made in the box is visible on the host' \
  "$(git -C "$PROJ" log --format=%s -1 2>/dev/null)" 'from inside the box'
check 'file written in the box is owned by you on the host' \
  "$(stat -c %U "$PROJ/file.txt" 2>/dev/null)" "$USER"
check 'git sees no dubious ownership' \
  "$(box sh -c 'git status --porcelain=v1 >/dev/null 2>&1 && echo clean || echo error')" clean
# jj is in the Arch image's package list but in no Debian stable release or
# Ubuntu archive, so a Debian-family box legitimately has none
# (DEFAULT_BASE_PACKAGES_DEBIAN, and docs/setup.md says so). Skip rather than
# fail: what these cover is that a VCS writing through the owneridmap'd project
# dir lands on the host, and the git checks above already assert that.
if [[ "$(box sh -c 'command -v jj >/dev/null && echo yes || echo no')" == yes ]]; then
  check 'jj present'                "$(box sh -c 'jj --version | cut -d" " -f1')" 'jj'
  box sh -c 'jj git init --colocate . >/dev/null 2>&1 && jj st >/dev/null 2>&1 && echo ok' >/dev/null 2>&1
  check 'jj works on the mapped repo' \
    "$(box sh -c 'jj st >/dev/null 2>&1 && echo ok || echo err')" ok
  box sh -c 'jj new -m "made by jj inside the box"' >/dev/null 2>&1
  check 'jj operation from the box is visible on the host' \
    "$(jj -R "$PROJ" log -r 'all()' --no-graph -T 'description.first_line() ++ "\n"' \
       2>/dev/null | grep -c 'made by jj inside the box')" 1
else
  skip "jj is not in this guest's image ($PKG has no jujutsu package); git above covers the same path"
fi

echo
echo '--- 4. read-write and read-only mapping ---'
check 'rw mount is writable' \
  "$(box sh -c 'echo x > rw-probe && echo written')" written
check 'and lands on the host' "$([[ -f "$PROJ/rw-probe" ]] && echo yes)" yes
check 'ro mount is readable'  "$(box sh -c "head -c 9 $REF/NOTES.md")" 'reference'
check 'ro mount rejects the sandbox user' \
  "$(box sh -c "echo x > $REF/NOTES.md 2>/dev/null && echo wrote || echo refused")" refused
check 'ro mount rejects container root too' \
  "$(box sudo -n sh -c "echo x > $REF/NOTES.md 2>/dev/null && echo wrote || echo refused")" refused
check 'ro mount survived intact' "$(tail -c 9 "$REF/NOTES.md")" 'not edit'

# The one place this suite wants /tmp: a project under a path nspawn covers
# with a mount of its own cannot be mapped at all, and must be refused before
# anything is created rather than after the overlay is already mounted.
UNSUP="$(mktemp -d /tmp/agentbox-unsupported-XXXXXX)"
unsup_box="$("$AGENTBOX" status --dry-run "$UNSUP" 2>/dev/null | awk '/^box /{print $2}')"
unsup_out="$("$AGENTBOX" run "$UNSUP" -- true 2>&1)"
check 'a project under /tmp is refused' \
  "$(grep -c 'cannot be mapped into a box' <<< "$unsup_out")" 1
check 'the refusal names the path nspawn owns' \
  "$(grep -c 'is under /tmp' <<< "$unsup_out")" 1
check 'the refusal leaves no box behind' \
  "$(sudo test -e "/var/lib/agentbox/boxes/$unsup_box" && echo left || echo none)" none
check 'the refusal mounts no rootfs' \
  "$(findmnt -rno TARGET "/var/lib/machines/$unsup_box" >/dev/null 2>&1 && echo mounted || echo none)" none
rm -rf "$UNSUP"

echo
echo '--- persistence and reset ---'
check 'installed package persists across launches' "$(installed)" installed
"$AGENTBOX" reset "$PROJ" -y >/dev/null 2>&1
check 'reset removed the package'  "$(installed)" gone
check 'reset kept the code intact' "$(cat "$PROJ/file.txt" 2>/dev/null)" hello

echo
echo '--- 5. resource limits reach the container cgroup ---'
cat > "$LIMITS/.agentbox.toml" <<TOML
memory_max = "2G"
cpu_quota = "150%"
tasks_max = "512"
TOML
lim() { "$AGENTBOX" run "$LIMITS" -- "$@"; }
lim true >/dev/null 2>&1
LBOX="$("$AGENTBOX" status "$LIMITS" | awk '/^box /{print $2}')"

# Read the caps from the host while a box is running, rather than from inside
# it. nspawn delegates a subgroup to the container - the stock unit spells it
# out with DelegateSubgroup=supervisor - and the caps sit on the scope above
# that, so the box's own /sys/fs/cgroup/memory.max reads `max` however well the
# limits were applied. Deriving the path from the nspawn process rather than
# from a unit name keeps this independent of how systemd names the scope.
lim sleep 8 >/dev/null 2>&1 &
limpid=$!
cg=""
for _ in $(seq 60); do
  nspid="$(pgrep -f "systemd-nspawn.*$LBOX" | head -1)"
  if [[ -n "$nspid" ]]; then
    cg="$(awk -F: '{print $3}' "/proc/$nspid/cgroup" 2>/dev/null)"
    cg="${cg%/supervisor}"
    cg="${cg%/payload}"
    [[ -n "$cg" && -e "/sys/fs/cgroup$cg/memory.max" ]] && break
    cg=""
  fi
  sleep 0.25
done
# Non-empty is not enough: if nspawn allocates no unit of its own, the box
# simply runs in the caller's cgroup, where --property has nowhere to land and
# every cap below reads as the session default.
own_cg="$(awk -F: '{print $3}' /proc/self/cgroup)"
check 'the launch got a cgroup of its own' \
  "$([[ -n "$cg" && "$cg" != "$own_cg" ]] && echo yes || echo no)" yes
check 'MemoryMax reached the cgroup' "$(cat "/sys/fs/cgroup$cg/memory.max" 2>/dev/null)" 2147483648
check 'CPUQuota reached the cgroup' \
  "$(cut -d' ' -f1 "/sys/fs/cgroup$cg/cpu.max" 2>/dev/null)" 150000
check 'TasksMax reached the cgroup' "$(cat "/sys/fs/cgroup$cg/pids.max" 2>/dev/null)" 512
wait "$limpid" 2>/dev/null

echo
echo '--- 6. booted mode (systemd as PID 1 inside) ---'
SERVICE="systemd-nspawn@$LBOX.service"
"$AGENTBOX" up "$LIMITS" >/dev/null 2>&1

# `systemctl is-active` on the service says only that nspawn was started; the
# box's own systemd may still be coming up, or may already have died. Waiting
# for the container to answer on its own bus is what distinguishes a box that
# booted from one that started and immediately exited.
container_state() { sudo systemctl -M "$LBOX" is-system-running 2>/dev/null; }
# `is-active` has transitional answers, so waiting for "not active" settles
# nothing: it is satisfied by `activating` on the way up and by `deactivating`
# on the way down. Wait for a state the unit can stay in.
settled() {
  case "$(systemctl is-active "$1" 2>/dev/null)" in
    active | activating | deactivating | reloading) return 1 ;;
    *) return 0 ;;
  esac
}
for _ in $(seq 60); do
  case "$(container_state)" in running | degraded) break ;; esac
  settled "$SERVICE" && break   # it died; let the checks below report it
  sleep 0.5
done
check 'boots' "$(systemctl is-active "$SERVICE" 2>/dev/null)" active
check "the box's own systemd is up" \
  "$(case "$(container_state)" in running | degraded) echo up ;; *) echo down ;; esac)" up
check 'registers with machined'  "$(machinectl list --no-legend 2>/dev/null | awk -v b="$LBOX" '$1==b{print "listed"}')" listed
check 'the settings file applied to the booted box' \
  "$(sudo systemd-run -M "$LBOX" --pipe --quiet --wait /usr/bin/hostname 2>/dev/null)" "$LBOX"

# Stale check removed: this used to assert that `build` *refuses* while a box
# is running, back when the base image was one mutable directory that a
# refresh rewrote in place. Since the generations refactor (see base.rs's
# module doc), `build`/`--refresh` write to a brand new `bases/<id>/` and only
# repoint `bases/current` once done - a live box's overlay keeps its lowerdir
# open on the old generation, untouched, for the life of the mount, so there is
# nothing left to refuse. The decoy state dir this used (a lone `base/`
# directory) predates that refactor too and has no `bases/current` to find, so
# `build --refresh` against it fell through to bootstrapping a whole new
# generation from scratch - slow, unconditionally, on every verify.sh run,
# which is exactly what WITH_BUILD exists to gate. The actual guarantee (a
# build never touches a running box's generation) is exercised by section 10
# instead, under WITH_BUILD.

"$AGENTBOX" down "$LIMITS" >/dev/null 2>&1
for _ in $(seq 60); do
  settled "$SERVICE" && break
  sleep 0.5
done
check 'powers off' "$(systemctl is-active "$SERVICE" 2>/dev/null)" inactive

echo
echo '--- 7. network modes ---'
check 'host networking reaches the network' \
  "$(box sh -c 'getent hosts example.com >/dev/null 2>&1 && echo up || echo down')" up
check 'network = none exposes only loopback' \
  "$("$AGENTBOX" run "$PROJ" --network none -- \
     sh -c 'ls /sys/class/net | xargs echo' 2>/dev/null)" lo
check 'network = none cannot resolve' \
  "$("$AGENTBOX" run "$PROJ" --network none -- \
     sh -c 'getent hosts example.com >/dev/null 2>&1 && echo up || echo down' 2>/dev/null)" down

# nat gives the box its own network namespace; it must still reach the internet
# through the host's NAT, and a fresh box's `packages` must install after boot.
#
# Both need host setup that agentbox cannot do for itself, and there are two
# independent ways for it to be missing, so the gate probes the thing actually
# required - a packet leaving the box - rather than any one precondition:
#
#   * no default route: NetworkManager still owns `ve-*`/`vz-*`, so networkd
#     never DHCPs the veth and host0 stays link-local;
#   * a route but no egress: Docker sets `iptables -P FORWARD DROP` and
#     whitelists only `docker0`, so the box is addressed and routed and still
#     reaches nothing.
#
# Skip with a pointer either way rather than failing the suite on a host-config
# gap. The probe is by IP, resolved here on the host, so that it answers
# "can a packet get out" and not "does DNS work" - which is the next check's
# job. `bash`'s /dev/tcp needs no extra tool in the box.
natprobe="$(getent hosts example.com | awk '{print $1; exit}')"
if [[ -n "$natprobe" ]] && "$AGENTBOX" run "$PROJ" --network nat -- \
     bash -c "exec 3<>/dev/tcp/$natprobe/443" >/dev/null 2>&1; then
  check 'network = nat resolves a name' \
    "$("$AGENTBOX" run "$PROJ" --network nat -- \
       sh -c 'getent hosts example.com >/dev/null 2>&1 && echo up || echo down' 2>/dev/null)" up
  check 'network = nat opens an outbound connection' \
    "$("$AGENTBOX" run "$PROJ" --network nat -- \
       bash -c 'exec 3<>/dev/tcp/example.com/443 && echo ok || echo fail' 2>/dev/null)" ok
  cat > "$NAT/.agentbox.toml" <<TOML
network = "nat"
packages = ["$CANARY"]
TOML
  "$AGENTBOX" run "$NAT" -- true >/dev/null 2>&1
  check 'a fresh nat box installs its packages after boot' \
    "$("$AGENTBOX" run "$NAT" -- \
       sh -c "command -v $CANARY_CMD >/dev/null && echo installed || echo gone" 2>/dev/null)" installed
else
  skip 'network = nat connectivity (nothing gets out of a nat box on this host: no route, or a FORWARD DROP policy from Docker - see docs/setup.md, "nat networking")'
  skip 'a fresh nat box installs its packages after boot (nat not reachable on this host)'
fi

echo
echo '--- 8. privilege handover hygiene ---'
# ensure_root() writes the caller's environment - tokens included - to a 0600
# file for the root re-exec to consume. The child normally eats it; a sudo that
# never authenticates does not, and in $HOME that leftover outlives a reboot.
# Mirror the directory choice made in ensure_root: first writable candidate.
hdir=""
for d in "${XDG_RUNTIME_DIR:-}" "/run/user/$(id -u)" "$HOME"; do
  [[ -n "$d" && -w "$d" && -x "$d" ]] && { hdir="$d"; break; }
done
if [[ -z "$hdir" ]]; then
  # ensure_root() would have failed outright, so the box above could not exist.
  no 'no writable directory for handover files (cannot check)'
else
  strays() { find "$hdir" -maxdepth 1 -name '.agentbox-handover-*.json' | wc -l; }
  box true >/dev/null 2>&1
  check 'a normal run leaves no handover file behind' "$(strays)" 0

  # A pid that has certainly exited, so the sweep finds no /proc entry for it.
  sleep 0 & dead=$!; wait "$dead" 2>/dev/null
  : > "$hdir/.agentbox-handover-$dead.json"
  box true >/dev/null 2>&1
  check 'a handover file from a dead process is swept' \
    "$([[ -e "$hdir/.agentbox-handover-$dead.json" ]] && echo left || echo swept)" swept
  rm -f "$hdir/.agentbox-handover-$dead.json"
fi

echo
echo '--- 9. AGENTBOX_STATE cannot aim a privileged agentbox ---'
# state_dir() reads AGENTBOX_STATE from its own environment, never from the
# handover, so sudo's env_reset strips it on the way to root. That is a claim
# about this host's sudoers as much as about the code, so assert it: an
# unprivileged caller setting it must not move where boxes and the image live.
FAKE="$(mktemp -d "$root/agentbox-fakestate-XXXXXX")"
check 'the run still finds the real base image' \
  "$(env AGENTBOX_STATE="$FAKE" "$AGENTBOX" run "$PROJ" -- \
     sh -c 'echo real-image' 2>/dev/null)" real-image
check 'nothing was created in the decoy state dir' \
  "$(find "$FAKE" -mindepth 1 2>/dev/null | wc -l)" 0

if [[ -n "${WITH_BUILD:-}" ]]; then
  echo
  echo '--- 10. the build path, in a throwaway state directory ---'
  avail_mb=$(( $(df -Pk "$root" | awk 'NR == 2 {print $4}') / 1024 ))
  if (( avail_mb < 3072 )); then
    # A build that runs out of space part way leaves a half-populated image,
    # which fails much later and far less legibly - as a box whose useradd
    # never ran. Say it up front instead.
    no "need ~3G under $root for a throwaway image, have ${avail_mb}M"
  else
    BSTATE="$(mktemp -d "$root/agentbox-buildstate-XXXXXX")"
    buildlog="$root/agentbox-build.log"
    echo "building into $BSTATE (a few minutes; log in $buildlog)"
    # `agentbox build --force` deletes the image it is aimed at, so the
    # isolation here is load-bearing: snapshot the real image and prove the
    # build never reached it.
    realbase="$(stat -c '%i %Y' /var/lib/agentbox/base 2>/dev/null || echo absent)"
    # Run the build from a directory of our own, and prove it stays empty. Every
    # stage of a build runs as root, so anything a child writes relative to the
    # cwd becomes a root-owned file in whatever directory the user typed
    # `agentbox build` in - a git repo, usually, where they cannot even be
    # deleted without sudo. debootstrap's internal wget did exactly that,
    # leaving `wget-log`, `wget-log.1`, ... behind. No other section would
    # notice.
    #
    # `sudo env VAR=...` rather than `sudo VAR=...`: the latter is refused by
    # default sudoers. Running the build already-root means AGENTBOX_STATE is
    # read from this process rather than being stripped on the way through sudo.
    cwdprobe="$(mktemp -d "$root/agentbox-cwd-XXXXXX")"
    ( cd "$cwdprobe" && sudo env AGENTBOX_STATE="$BSTATE" "$AGENTBOX" build --force ) \
      >"$buildlog" 2>&1
    rc=$?
    check 'build succeeded' "$rc" 0
    (( rc == 0 )) || { echo "--- tail of $buildlog ---"; tail -20 "$buildlog"; }
    check 'the build wrote nothing into the directory it ran from' \
      "$(ls -A "$cwdprobe" | wc -l)" 0
    check 'build left every host mount in place' "$(lost_mounts)" 'none lost'
    # The bootstrap rbinds /proc, /sys, /dev and /run into the half-built image
    # and tears them down again. Anything still mounted under it means either
    # the teardown failed or the guard refused to unmount a still-shared
    # subtree and left it behind deliberately - both worth knowing about.
    check 'the bootstrap binds were torn down' \
      "$(findmnt -rno TARGET | awk -v p="$BSTATE/bases/" 'index($0, p) == 1' | wc -l)" 0
    check 'the real base image was untouched' \
      "$(stat -c '%i %Y' /var/lib/agentbox/base 2>/dev/null || echo absent)" "$realbase"
    # Every generation lives in a directory of its own, named by `current`.
    gen="$BSTATE/bases/$(sudo cat "$BSTATE/bases/current" 2>/dev/null)"
    check 'the build stamped a current generation' \
      "$(sudo test -d "$gen/usr" && echo yes || echo no)" yes
    # What the guest is is the build's own decision, so read it back rather
    # than assuming: the per-family stages differ, and only one of them ran.
    guest="$(sudo sh -c ". '$gen/etc/os-release' 2>/dev/null; echo \${ID:-unknown}")"
    check 'the generation is stamped with its distribution' \
      "$(sudo cat "$gen.distro" 2>/dev/null)" \
      "$(case "$guest" in arch) echo arch ;; *) echo debian ;; esac)"
    if [[ "$guest" == arch ]]; then
      check 'keyring was initialised' \
        "$(sudo test -d "$gen/etc/pacman.d/gnupg" && echo yes || echo no)" yes
      check 'jj is in the image' \
        "$(sudo test -x "$gen/usr/bin/jj" && echo yes || echo no)" yes
    else
      # debootstrap leaves a one-component list with no -updates pocket behind;
      # the image is only usable if the full list replaced it.
      check 'the full archive list replaced the bootstrap one' \
        "$(sudo test -f "$gen/etc/apt/sources.list.d/agentbox.sources" && echo yes || echo no)" yes
      check 'the bootstrap archive list is gone' \
        "$(sudo test -e "$gen/etc/apt/sources.list" && echo left || echo gone)" gone
      # Without these the image boots to nothing and no session can attach.
      check 'the image can boot' \
        "$(sudo test -x "$gen/usr/lib/systemd/systemd" && echo yes || echo no)" yes
      # policy-rc.d blocks service starts during the build only; left behind,
      # it silently blocks them in every box forever.
      check 'the build-time service block was removed' \
        "$(sudo test -e "$gen/usr/sbin/policy-rc.d" && echo left || echo gone)" gone
    fi
    check 'git is in the image'  "$(sudo test -x "$gen/usr/bin/git" && echo yes || echo no)" yes
    check 'sudoers rule was written' \
      "$(sudo test -f "$gen/etc/sudoers.d/00-agentbox" && echo yes || echo no)" yes
    # The image the crash left behind stopped just short of this, and every
    # later failure was a box whose chsh could not find the user.
    check 'the sandbox user is in the image' \
      "$(sudo grep -c "^$USER:" "$gen/etc/passwd" 2>/dev/null)" 1
    check 'image ownership was shifted out of the host root range' \
      "$(sudo stat -c %u "$gen/usr/bin/bash" 2>/dev/null)" "$UIDBASE"
  fi
fi

echo
echo '--- 11. the host itself is unharmed ---'
check 'host mounts under /dev and /run survived' "$(lost_mounts)" 'none lost'

echo
echo '--- 12. cpy: copy-if-absent, not a live mount ---'
# A cpy entry with no explicit dst lands at the same absolute path inside the
# box as its host source, same as an rw/ro entry - so CPYSRC never has to sit
# under either sandbox user's home.
echo 'seed-v1' > "$CPYSRC/seed.txt"
cat > "$CPY/.agentbox.toml" <<TOML
network = "host"
cpy = ["$CPYSRC"]
TOML
cpy() { "$AGENTBOX" run "$CPY" -- "$@"; }
# A fresh box's first launch also runs the one-time login-shell setup (chsh),
# which prints straight to this same stdout; warm it up first so that noise
# lands here instead of in the first real check's captured output, same as
# $PROJ/$LIMITS/$NAT above.
cpy true >/dev/null 2>&1
check 'first boot copies the host source in' \
  "$(cpy cat "$CPYSRC/seed.txt")" 'seed-v1'
cpy sh -c "echo edited > $CPYSRC/seed.txt"
check 'a write inside the box lands on its own copy' \
  "$(cpy cat "$CPYSRC/seed.txt")" 'edited'
check 'the write never reached the host source' \
  "$(cat "$CPYSRC/seed.txt")" 'seed-v1'
# Rewrite the host source while the box's copy still exists: copy-if-absent
# means this is never resynced - the exact overlay-staleness bug class a live
# mount of a mutable host directory would hit (see "Overlayfs never
# revalidates its lower layer" in CLAUDE.md), sidestepped here by never
# mounting at all.
echo 'seed-v2' > "$CPYSRC/seed.txt"
check 'a later host rewrite is not resynced into the box' \
  "$(cpy cat "$CPYSRC/seed.txt")" 'edited'
"$AGENTBOX" reset "$CPY" -y >/dev/null 2>&1
check 'reset makes the destination absent again' \
  "$(cpy cat "$CPYSRC/seed.txt")" 'seed-v2'

echo
echo '--- 13. ssh_keys: a dedicated agent behind a relay ---'
# Regression test for a box's own user namespace defeating a naive bind: a box
# process's real, host-global UID is uid_base + <its in-box uid>, never the
# invoking user's actual UID - owneridmap only translates file ownership
# metadata (what stat/ls see), not process credentials. ssh-agent checks the
# connecting peer's real UID via getsockopt(SO_PEERCRED) and closes the
# connection if it doesn't match its own, so binding the agent's own socket
# straight into the box makes every ssh-add there die with SIGPIPE
# ("communication with agent failed") even though the socket and its
# permissions look entirely correct. See nspawn::spawn_ssh_agent for the fix:
# a socat relay, run as the same user as the agent, sits in front of it.
if command -v socat >/dev/null 2>&1; then
  ssh-keygen -q -t ed25519 -N '' -C 'agentbox-verify' -f "$SSHKEYDIR/id_ed25519" </dev/null
  cat > "$SSH/.agentbox.toml" <<TOML
network = "host"
ssh_keys = ["$SSHKEYDIR/id_ed25519"]
TOML
  sshbox() { "$AGENTBOX" run "$SSH" -- "$@"; }
  # `run` alone is Session-owned and powers the box off - agent, relay and
  # all - the moment its one-shot session ends, which is immediately after
  # each `sshbox` call below returns. `up` first keeps it running underneath
  # them, the way an interactive `shell` session would, so the host-side
  # process checks below see it still alive rather than already torn down.
  "$AGENTBOX" up "$SSH" >/dev/null 2>&1
  SSHBOXNAME="$("$AGENTBOX" status "$SSH" | awk '/^box /{print $2}')"
  check 'the box-scoped agent serves the configured key' \
    "$(sshbox sh -c 'ssh-add -l 2>&1' | grep -c 'agentbox-verify')" 1
  check 'SSH_AUTH_SOCK points at the forwarded socket' \
    "$(sshbox sh -c 'echo $SSH_AUTH_SOCK')" \
    "$(sshbox sh -c 'echo $HOME/.agentbox/ssh-agent.sock')"
  check 'a host-side agent is running for the box' \
    "$(pgrep -f "ssh-agent -s -a .*/boxes/$SSHBOXNAME/ssh-agent/agent\.sock" | wc -l)" 1
  check 'a host-side relay is running for the box' \
    "$(pgrep -f "socat UNIX-LISTEN:.*/boxes/$SSHBOXNAME/ssh-agent/agent-relay\.sock" | wc -l)" 1

  "$AGENTBOX" down "$SSH" >/dev/null 2>&1
  for _ in $(seq 20); do
    pgrep -f "ssh-agent -s -a .*/boxes/$SSHBOXNAME/ssh-agent/agent\.sock" >/dev/null 2>&1 || break
    sleep 0.25
  done
  check 'down stops the host-side agent' \
    "$(pgrep -f "ssh-agent -s -a .*/boxes/$SSHBOXNAME/ssh-agent/agent\.sock" | wc -l)" 0
  check 'down stops the relay too' \
    "$(pgrep -f "socat UNIX-LISTEN:.*/boxes/$SSHBOXNAME/ssh-agent/agent-relay\.sock" | wc -l)" 0

  sshbox true >/dev/null 2>&1
  check 'a fresh boot after down still serves the key' \
    "$(sshbox sh -c 'ssh-add -l 2>&1' | grep -c 'agentbox-verify')" 1
else
  skip 'ssh_keys (socat not installed on this host)'
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ -n "${KEEP:-}" ]] && printf 'kept: %s (agentbox rm %s)\n' "$BOXNAME" "$BOXNAME"
exit $((fail > 0))
