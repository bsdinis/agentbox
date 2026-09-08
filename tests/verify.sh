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
# WITH_BUILD is opt-in because it builds a second Arch image from scratch (a few
# minutes, and it downloads packages). It is the only section that covers the
# build path, which is where a mount-propagation bug once unmounted /dev/pts and
# /run/user/$UID off the host. It builds into a throwaway AGENTBOX_STATE, so the
# real base image is never touched.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
AGENTBOX="${AGENTBOX_BIN:-$(command -v agentbox || echo "$here/../target/release/agentbox")}"
[[ -x "$AGENTBOX" ]] || { echo "no agentbox binary; run install.sh first" >&2; exit 1; }
root="${AGENTBOX_VERIFY_ROOT:-${XDG_CACHE_HOME:-$HOME/.cache}/agentbox-verify}"
mkdir -p "$root" || { echo "cannot create $root" >&2; exit 1; }
PROJ="$(mktemp -d "$root/agentbox-verify-XXXXXX")"
REF="$(mktemp -d "$root/agentbox-refonly-XXXXXX")"
LIMITS="$(mktemp -d "$root/agentbox-limits-XXXXXX")"
BSTATE=""
FAKE=""
pass=0 fail=0

ok()   { printf '\033[32mPASS\033[0m %s\n' "$1"; pass=$((pass+1)); }
no()   { printf '\033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
check(){ if [[ "$2" == "$3" ]]; then ok "$1"; else no "$1 (got '$2', want '$3')"; fi; }
box()  { "$AGENTBOX" run --dir "$PROJ" -- "$@"; }

cleanup() {
  [[ -n "${KEEP:-}" ]] && return
  "$AGENTBOX" down --dir "$LIMITS" >/dev/null 2>&1
  "$AGENTBOX" rm --dir "$PROJ" -y >/dev/null 2>&1
  "$AGENTBOX" rm --dir "$LIMITS" -y >/dev/null 2>&1
  rm -rf "$PROJ" "$REF" "$LIMITS"
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
network = "host"
ro = ["~/.gitconfig", "~/.config/jj", "$REF"]
TOML

# ---------------------------------------------------------------- lifecycle
echo '--- creating box ---'
"$AGENTBOX" run --dir "$PROJ" -- true || { echo 'could not create box'; exit 1; }
BOXNAME="$("$AGENTBOX" status --dir "$PROJ" | awk '/^box /{print $2}')"
UIDBASE="$("$AGENTBOX" status --dir "$PROJ" | awk '/^uid range/{split($3,a,"[.][.]");print a[1]}')"
printf 'box %s, uid base %s\n\n' "$BOXNAME" "$UIDBASE"

echo '--- identity and paths ---'
check 'project mounted at its real host path' "$(box pwd)" "$PROJ"
check 'runs as the host user'                 "$(box id -un)" "$USER"
check 'same uid as on the host'               "$(box id -u)"  "$(id -u)"

echo
echo '--- 1. package installation inside the box ---'
box sudo pacman -Sy --noconfirm --needed cowsay >/dev/null 2>&1
installed() { box sh -c 'command -v cowsay >/dev/null && echo installed || echo gone'; }
check 'pacman installed a package' "$(installed)" installed
check 'the package actually runs'  "$(box sh -c 'cowsay moo | grep -c moo')" 1
check 'host is unaffected'         "$(command -v cowsay || echo none)" none

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
check 'jj present'                "$(box sh -c 'jj --version | cut -d" " -f1')" 'jj'
check 'host git identity applies' "$(box git config --get user.email)" "$(git config --get user.email)"
box sh -c 'git init -q . && echo hello > file.txt && git add file.txt &&
           git -c commit.gpgsign=false commit -qm "from inside the box"' >/dev/null 2>&1
check 'commit made in the box is visible on the host' \
  "$(git -C "$PROJ" log --format=%s -1 2>/dev/null)" 'from inside the box'
check 'file written in the box is owned by you on the host' \
  "$(stat -c %U "$PROJ/file.txt" 2>/dev/null)" "$USER"
check 'git sees no dubious ownership' \
  "$(box sh -c 'git status --porcelain=v1 >/dev/null 2>&1 && echo clean || echo error')" clean
box sh -c 'jj git init --colocate . >/dev/null 2>&1 && jj st >/dev/null 2>&1 && echo ok' >/dev/null 2>&1
check 'jj works on the mapped repo' \
  "$(box sh -c 'jj st >/dev/null 2>&1 && echo ok || echo err')" ok
box sh -c 'jj new -m "made by jj inside the box"' >/dev/null 2>&1
check 'jj operation from the box is visible on the host' \
  "$(jj -R "$PROJ" log -r 'all()' --no-graph -T 'description.first_line() ++ "\n"' \
     2>/dev/null | grep -c 'made by jj inside the box')" 1

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

echo
echo '--- persistence and reset ---'
check 'installed package persists across launches' "$(installed)" installed
"$AGENTBOX" reset --dir "$PROJ" -y >/dev/null 2>&1
check 'reset removed the package'  "$(installed)" gone
check 'reset kept the code intact' "$(cat "$PROJ/file.txt" 2>/dev/null)" hello

echo
echo '--- 5. resource limits reach the container cgroup ---'
cat > "$LIMITS/.agentbox.toml" <<TOML
memory_max = "2G"
cpu_quota = "150%"
tasks_max = "512"
TOML
lim() { "$AGENTBOX" run --dir "$LIMITS" -- "$@"; }
lim true >/dev/null 2>&1
LBOX="$("$AGENTBOX" status --dir "$LIMITS" | awk '/^box /{print $2}')"

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
"$AGENTBOX" up --dir "$LIMITS" >/dev/null 2>&1

# `systemctl is-active` on the service says only that nspawn was started; the
# box's own systemd may still be coming up, or may already have died. Waiting
# for the container to answer on its own bus is what distinguishes a box that
# booted from one that started and immediately exited.
container_state() { sudo systemctl -M "$LBOX" is-system-running 2>/dev/null; }
for _ in $(seq 60); do
  case "$(container_state)" in running | degraded) break ;; esac
  [[ "$(systemctl is-active "$SERVICE" 2>/dev/null)" == active ]] || break
  sleep 0.5
done
check 'boots' "$(systemctl is-active "$SERVICE" 2>/dev/null)" active
check "the box's own systemd is up" \
  "$(case "$(container_state)" in running | degraded) echo up ;; *) echo down ;; esac)" up
check 'registers with machined'  "$(machinectl list --no-legend 2>/dev/null | awk -v b="$LBOX" '$1==b{print "listed"}')" listed
check 'the settings file applied to the booted box' \
  "$(sudo systemd-run -M "$LBOX" --pipe --quiet --wait /usr/bin/hostname 2>/dev/null)" "$LBOX"
"$AGENTBOX" down --dir "$LIMITS" >/dev/null 2>&1
for _ in $(seq 30); do
  [[ "$(systemctl is-active "$SERVICE" 2>/dev/null)" == active ]] || break
  sleep 0.5
done
check 'powers off' "$(systemctl is-active "$SERVICE" 2>/dev/null)" inactive

echo
echo '--- 7. network modes ---'
check 'host networking reaches the network' \
  "$(box sh -c 'getent hosts archlinux.org >/dev/null 2>&1 && echo up || echo down')" up
check 'network = none exposes only loopback' \
  "$("$AGENTBOX" run --dir "$PROJ" --network none -- \
     sh -c 'ls /sys/class/net | xargs echo' 2>/dev/null)" lo
check 'network = none cannot resolve' \
  "$("$AGENTBOX" run --dir "$PROJ" --network none -- \
     sh -c 'getent hosts archlinux.org >/dev/null 2>&1 && echo up || echo down' 2>/dev/null)" down

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
  "$(env AGENTBOX_STATE="$FAKE" "$AGENTBOX" run --dir "$PROJ" -- \
     sh -c 'echo real-image' 2>/dev/null)" real-image
check 'nothing was created in the decoy state dir' \
  "$(find "$FAKE" -mindepth 1 2>/dev/null | wc -l)" 0

if [[ -n "${WITH_BUILD:-}" ]]; then
  echo
  echo '--- 10. the build path, in a throwaway state directory ---'
  BSTATE="$(mktemp -d "$root/agentbox-buildstate-XXXXXX")"
  echo "building into $BSTATE (a few minutes)"
  # `sudo env VAR=...` rather than `sudo VAR=...`: the latter is refused by
  # default sudoers. Running the build already-root means AGENTBOX_STATE is read
  # from this process rather than being stripped on the way through sudo.
  sudo env AGENTBOX_STATE="$BSTATE" "$AGENTBOX" build --force >/dev/null 2>&1
  check 'build succeeded' "$?" 0
  check 'build left every host mount in place' "$(lost_mounts)" 'none lost'
  check 'the real base image was untouched' \
    "$([[ -e "$BSTATE/base" && "$BSTATE" != /var/lib/agentbox ]] && echo isolated)" isolated
  check 'keyring was initialised' \
    "$(sudo test -d "$BSTATE/base/etc/pacman.d/gnupg" && echo yes || echo no)" yes
  check 'git is in the image'  "$(sudo test -x "$BSTATE/base/usr/bin/git" && echo yes || echo no)" yes
  check 'jj is in the image'   "$(sudo test -x "$BSTATE/base/usr/bin/jj"  && echo yes || echo no)" yes
  check 'sudoers rule was written' \
    "$(sudo test -f "$BSTATE/base/etc/sudoers.d/00-agentbox" && echo yes || echo no)" yes
  check 'image ownership was shifted out of the host root range' \
    "$(sudo stat -c %u "$BSTATE/base/usr/bin/bash" 2>/dev/null)" "$UIDBASE"
fi

echo
echo '--- 11. the host itself is unharmed ---'
check 'host mounts under /dev and /run survived' "$(lost_mounts)" 'none lost'

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ -n "${KEEP:-}" ]] && printf 'kept: %s (agentbox rm --dir %s)\n' "$BOXNAME" "$PROJ"
exit $((fail > 0))
