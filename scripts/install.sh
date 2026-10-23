#!/usr/bin/env bash
# Build sluice, install the binary to ~/.cargo/bin and the pickers to ~/.config/sluice/bin.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
command -v zmx >/dev/null || { echo "zmx missing: brew install neurosnap/tap/zmx"; exit 1; }
command -v fzf >/dev/null || { echo "fzf missing: brew install fzf"; exit 1; }
command -v cargo >/dev/null || { echo "cargo missing: install Rust from https://rustup.rs"; exit 1; }
cargo install --path "$here" --locked --quiet
cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
# macOS checks a binary the first time it runs, 0.3 to 1 s. A sluice already
# running hands its scripts this path as $SLUICE, so without this run the
# first prefix+s or prefix+u after an upgrade would wait out that check.
"$cargo_bin/sluice" --version >/dev/null
bin="${XDG_CONFIG_HOME:-$HOME/.config}/sluice/bin"
mkdir -p "$bin"
for f in sessionizer worktree claude-here claude-picker agents; do
  install -m 755 "$here/scripts/$f" "$bin/$f"
done
cfg="${XDG_CONFIG_HOME:-$HOME/.config}/sluice/config"
[[ -f "$cfg" ]] || install -m 644 "$here/config.example" "$cfg"
echo "installed: $cargo_bin/sluice, pickers in $bin, config at $cfg"
case ":$PATH:" in
  *":$cargo_bin:"*) ;;
  *) echo "note: $cargo_bin is not on your PATH; add it so \`sluice\` is found" ;;
esac
command -v jq >/dev/null || echo "note: prefix+u (the Claude agent picker) needs jq: brew install jq"
