#!/usr/bin/env bash
# Build agentbox and install it, plus a global config if you have none.
#
#   ./install.sh              # build, install to ~/.local/bin
#   ./install.sh --system     # build, install to /usr/local/bin (needs sudo)
set -euo pipefail

src="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
system=0
[[ "${1:-}" == "--system" ]] && system=1

command -v cargo >/dev/null || { echo 'install.sh: cargo not found' >&2; exit 1; }
cargo build --release --manifest-path "$src/Cargo.toml"

if (( system )); then
  bindir=/usr/local/bin
  sudo install -Dm755 "$src/target/release/agentbox" "$bindir/agentbox"
else
  bindir="${XDG_BIN_HOME:-$HOME/.local/bin}"
  install -Dm755 "$src/target/release/agentbox" "$bindir/agentbox"
fi
echo "installed $bindir/agentbox"

confdir="${XDG_CONFIG_HOME:-$HOME/.config}/agentbox"
mkdir -p "$confdir"
if [[ ! -e "$confdir/config.toml" ]]; then
  cp "$src/config.example.toml" "$confdir/config.toml"
  echo "wrote $confdir/config.toml"
else
  echo "kept existing $confdir/config.toml"
fi

case ":$PATH:" in
  *":$bindir:"*) ;;
  *) echo "warning: $bindir is not on your PATH" >&2 ;;
esac

cat <<MSG

Next:
  agentbox build          # one-time, builds the shared base image
  cd <project> && agentbox init && agentbox shell

agentbox re-execs itself under sudo, so each launch asks for your password
unless sudo still has a valid timestamp. To skip the prompt:

  printf '%s ALL=(root) NOPASSWD: %s\\n' "\$USER" "$bindir/agentbox" \\
    | sudo install -m 440 /dev/stdin /etc/sudoers.d/50-agentbox

MSG

if (( ! system )); then
  cat <<'MSG'
If you do that, install with --system first. A NOPASSWD rule pointing at a
binary in a directory you can write to is a free root shell for anything
running as you. docs/setup.md spells out the trade-off.
MSG
fi
