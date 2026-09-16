#!/usr/bin/env bash
# Install the host packages agentbox needs and set up nat networking. It does
# not build or install agentbox itself - that's a plain cargo install:
#
#   cargo install --path .      # from this checkout
#   cargo install agentbox      # from crates.io, once published
#
#   ./install-deps.sh           # install missing packages, print next steps
set -euo pipefail

src="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Which host this is decides both how packages are installed and what the base
# image is bootstrapped with: pacman installs into an empty root on Arch,
# debootstrap does the same job on a Debian-family host. agentbox itself picks
# the same way (src/distro.rs), so the two stay in step.
id=$(. /etc/os-release 2>/dev/null && echo "${ID:-} ${ID_LIKE:-}")
case " $id " in
  *" arch "*)                family=arch;   guest="an Arch" ;;
  *" debian "*|*" ubuntu "*) family=debian; guest="a Debian-family" ;;
  *)
    echo "install-deps.sh: unrecognised host (os-release ID=${id% *}); agentbox" >&2
    echo "  bootstraps its base image with pacman or debootstrap - see docs/setup.md" >&2
    exit 1 ;;
esac

# The base image needs systemd's `owneridmap` bind option, which landed in
# systemd 256. Everything below installs fine on an older host and then fails
# at the first `agentbox shell`, so say so now.
version=$(systemctl --version | awk 'NR==1 {print $2}')
if [[ "$version" =~ ^[0-9]+$ ]] && (( version < 256 )); then
  cat >&2 <<MSG
install-deps.sh: this host runs systemd $version, and agentbox needs 256 or
newer - the base image is mapped into each box with the `owneridmap` bind
option, which does not exist before then. Ubuntu 24.04 (systemd 255) is the
common case; 24.10 and later, and Debian trixie, are new enough.

Continuing anyway - everything below is still correct, but boxes will not run
until the host's systemd is new enough.

MSG
fi

# What agentbox needs, as the command that proves it present and the package
# that provides it. Two parallel arrays rather than one list per family: the
# same tool names are re-checked after the install to decide whether it
# worked, and a spelling that drifted between the two lists would make that
# check either vacuous or permanently unsatisfiable.
#
# socat relays the box-scoped ssh-agent's socket: a box's own user namespace
# gives it a different real UID than the invoking user, which ssh-agent's own
# peer-UID check would otherwise reject outright. Only needed when a project
# sets `ssh_keys`, but cheap enough to always install.
tools=(systemd-nspawn cargo socat)
if [[ $family == debian ]]; then
  # Same set, spelled the way this archive spells it, plus the bootstrapper:
  # an Arch host already has pacman, a Debian one does not ship debootstrap.
  pkgs=(systemd-container cargo socat debootstrap)
  tools+=(debootstrap)
  install_cmd=(sudo apt-get install -y)
else
  pkgs=(systemd-container rust socat)
  install_cmd=(sudo pacman -S --needed)
fi

missing=()
for i in "${!tools[@]}"; do
  command -v "${tools[$i]}" >/dev/null || missing+=("${pkgs[$i]}")
done

if (( ${#missing[@]} )); then
  echo "installing: ${missing[*]}"
  # Judge the result by what landed, not by the exit status. A Debian host
  # part-way through a release upgrade has other packages waiting to be
  # configured, and `apt-get install` configures those too - so one broken
  # kernel postinst or DKMS module belonging to something else fails this
  # command long after the packages asked for here are unpacked and set up.
  # Reporting that as "agentbox's dependencies failed to install" sends you
  # hunting through a wall of someone else's build log for a problem that is
  # not in the way.
  rc=0
  "${install_cmd[@]}" "${missing[@]}" || rc=$?
  still_missing=()
  for i in "${!tools[@]}"; do
    command -v "${tools[$i]}" >/dev/null || still_missing+=("${tools[$i]} (${pkgs[$i]})")
  done
  if (( ${#still_missing[@]} )); then
    echo >&2
    echo "install-deps.sh: still missing after the install: ${still_missing[*]}" >&2
    echo "  The package manager exited $rc. Fix that, then run this again." >&2
    exit 1
  fi
  if (( rc != 0 )); then
    cat >&2 <<MSG

install-deps.sh: the package manager exited $rc, but everything agentbox needs
is installed - the failure was about other packages on this host, which
\`apt-get install\` configures along the way on a system with a backlog. Nothing
below is blocked by it, and \`dpkg -l | grep -v '^ii'\` will show you what is
still unconfigured.

MSG
  fi
else
  echo "the host packages agentbox needs are already installed"
fi

# nat (the default network mode) needs systemd-networkd to DHCP and NAT each
# box's veth; it is enabled by default on neither Arch nor Debian.
if ! systemctl is-enabled --quiet systemd-networkd 2>/dev/null; then
  cat <<'MSG'

agentbox's default network mode (`nat`) needs systemd-networkd running.
Enable it:

  sudo systemctl enable --now systemd-networkd

Skip this if every project will use `network = "host"` or `network = "none"`
instead - see docs/setup.md.
MSG
fi

# NetworkManager claims the box's veth before networkd if it isn't told to
# leave container interfaces alone, which leaves the box with no route.
if systemctl is-active --quiet NetworkManager 2>/dev/null; then
  cat <<'MSG'

NetworkManager is running and will race systemd-networkd for each box's veth
unless told to ignore them:

  printf '[keyfile]\nunmanaged-devices=interface-name:ve-*;interface-name:vz-*\n' \
    | sudo tee /etc/NetworkManager/conf.d/agentbox-nspawn.conf
  sudo systemctl reload NetworkManager
MSG
fi

# Optional AppArmor hardening. Only mention it where the tooling exists;
# loading the profile needs root and is left to the user (test in complain
# mode first). agentbox runs fine without it - this is defense in depth, never
# a launch gate.
if command -v apparmor_parser >/dev/null; then
  cat <<MSG

Optional: an AppArmor profile ships in contrib/apparmor/ as a second wall
behind the user namespace, seccomp and mount plan. agentbox applies it
automatically once it is loaded. Test it in complain mode first:

  sudo install -Dm644 "$src/contrib/apparmor/agentbox-nspawn" \\
    /etc/apparmor.d/agentbox-nspawn
  sudo apparmor_parser -r -C /etc/apparmor.d/agentbox-nspawn   # complain mode
  # exercise a box, review: sudo aa-logprof ; then: sudo aa-enforce agentbox-nspawn

See contrib/apparmor/README.md for the full walkthrough.
MSG
fi

cat <<MSG

This host builds $guest base image; \`agentbox build\` picks the same
release the host runs. Override it in ~/.config/agentbox/config.toml under
[base] - see config.example.toml.

Next:
  cargo install --path .          # builds and installs agentbox
  agentbox init --global          # writes ~/.config/agentbox/config.toml
  agentbox build                  # one-time: build the shared base image
  cd <project> && agentbox init && agentbox shell
MSG
