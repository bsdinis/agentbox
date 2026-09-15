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

command -v pacman >/dev/null || {
  echo 'install-deps.sh: no pacman on this host - agentbox build is Arch-only, see docs/setup.md' >&2
  exit 1
}

missing=()
command -v systemd-nspawn >/dev/null || missing+=(systemd-container)
command -v cargo >/dev/null || missing+=(rust)
# socat relays the box-scoped ssh-agent's socket: a box's own user namespace
# gives it a different real UID than the invoking user, which ssh-agent's own
# peer-UID check would otherwise reject outright. Only needed when a project
# sets `ssh_keys`, but cheap enough to always install.
command -v socat >/dev/null || missing+=(socat)

if (( ${#missing[@]} )); then
  echo "installing: ${missing[*]}"
  sudo pacman -S --needed "${missing[@]}"
else
  echo "systemd-container and a Rust toolchain are already installed"
fi

# nat (the default network mode) needs systemd-networkd to DHCP and NAT each
# box's veth; it is not enabled by default on Arch.
if command -v systemd-networkd >/dev/null && ! systemctl is-enabled --quiet systemd-networkd 2>/dev/null; then
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

cat <<'MSG'

Next:
  cargo install --path .          # builds and installs agentbox
  agentbox init --global          # writes ~/.config/agentbox/config.toml
  agentbox build                  # one-time: build the shared base image
  cd <project> && agentbox init && agentbox shell
MSG
