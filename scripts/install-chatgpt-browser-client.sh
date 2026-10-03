#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bin_dir="${TOOMUX_BIN_DIR:-$HOME/.local/bin}"
toomux_bin="${TOOMUX_BIN:-$(command -v toomux 2>/dev/null || true)}"

mkdir -p "$bin_dir"
install -m 755 "$root/scripts/toomux-claude" "$bin_dir/toomux-claude"

echo "installed ChatGPT Browser API client wrapper to $bin_dir/toomux-claude"

if [ -n "$toomux_bin" ] && [ -x "$toomux_bin" ]; then
  if ! "$toomux_bin" provider --configure-client; then
    echo "warning: could not register chatgpt-browser with Claude Code; update toomux and rerun this installer" >&2
  fi
else
  echo "warning: toomux binary not found; run 'toomux provider --configure-client' after installing toomux" >&2
fi
