#!/usr/bin/env bash
# End-to-end check of the four things agentbox promises: in-box package
# installation, harmless sudo, working git/jj, and rw/ro directory mapping.
# Creates a throwaway project under /tmp, and removes its box at the end.
#
#   ~/dev/agentbox/tests/verify.sh          # run everything
#   KEEP=1 ~/dev/agentbox/tests/verify.sh   # keep the box for poking at
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
AGENTBOX="${AGENTBOX_BIN:-$(command -v agentbox || echo "$here/../target/release/agentbox")}"
[[ -x "$AGENTBOX" ]] || { echo "no agentbox binary; run install.sh first" >&2; exit 1; }
root="${AGENTBOX_VERIFY_ROOT:-/tmp}"
PROJ="$(mktemp -d "$root/agentbox-verify-XXXXXX")"
REF="$(mktemp -d "$root/agentbox-refonly-XXXXXX")"
LIMITS="$(mktemp -d "$root/agentbox-limits-XXXXXX")"
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
}
trap cleanup EXIT

printf 'project  %s (%s)\nref (ro) %s\n\n' \
  "$PROJ" "$(findmnt -no FSTYPE -T "$PROJ")" "$REF"

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
check 'pacman installed a package' "$(box sh -c 'command -v cowsay')" /usr/bin/cowsay
check 'the package actually runs'  "$(box sh -c 'cowsay -f default moo | tail -1 | tr -d " "')" '||'
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
check 'installed package persists across launches' \
  "$(box sh -c 'command -v cowsay')" /usr/bin/cowsay
"$AGENTBOX" reset --dir "$PROJ" -y >/dev/null 2>&1
check 'reset removed the package'  "$(box sh -c 'command -v cowsay || echo gone')" gone
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
check 'MemoryMax applied' "$(lim cat /sys/fs/cgroup/memory.max 2>/dev/null)" 2147483648
check 'CPUQuota applied'  "$(lim sh -c 'cut -d" " -f1 /sys/fs/cgroup/cpu.max' 2>/dev/null)" 150000
check 'TasksMax applied'  "$(lim cat /sys/fs/cgroup/pids.max 2>/dev/null)" 512

echo
echo '--- 6. booted mode (systemd as PID 1 inside) ---'
LBOX="$("$AGENTBOX" status --dir "$LIMITS" | awk '/^box /{print $2}')"
SERVICE="systemd-nspawn@$LBOX.service"
"$AGENTBOX" up --dir "$LIMITS" >/dev/null 2>&1
for _ in $(seq 30); do
  [[ "$(systemctl is-active "$SERVICE" 2>/dev/null)" == active ]] && break
  sleep 0.5
done
check 'boots'                    "$(systemctl is-active "$SERVICE" 2>/dev/null)" active
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

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ -n "${KEEP:-}" ]] && printf 'kept: %s (agentbox rm --dir %s)\n' "$BOXNAME" "$PROJ"
exit $((fail > 0))
