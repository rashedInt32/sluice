#!/usr/bin/env bash
# Build sluice, install the binary to ~/.cargo/bin and the pickers to ~/.config/sluice/bin.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
command -v zmx >/dev/null || { echo "zmx missing: brew install neurosnap/tap/zmx"; exit 1; }
command -v fzf >/dev/null || { echo "fzf missing: brew install fzf"; exit 1; }
cargo install --path "$here" --locked --quiet
bin="${XDG_CONFIG_HOME:-$HOME/.config}/sluice/bin"
mkdir -p "$bin"
for f in sessionizer worktree claude-here claude-picker agents; do
  install -m 755 "$here/scripts/$f" "$bin/$f"
done
# project-dirs is yours to edit, so it is only copied the first time.
[[ -f "$bin/project-dirs" ]] || install -m 755 "$here/scripts/project-dirs" "$bin/project-dirs"
cfg="${XDG_CONFIG_HOME:-$HOME/.config}/sluice/config"
[[ -f "$cfg" ]] || install -m 644 "$here/config.example" "$cfg"
echo "installed: $(command -v sluice), pickers in $bin, config at $cfg"
