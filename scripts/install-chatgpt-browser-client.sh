#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bin_dir="${TOOMUX_BIN_DIR:-$HOME/.local/bin}"

mkdir -p "$bin_dir"
install -m 755 "$root/scripts/toomux-claude" "$bin_dir/toomux-claude"

echo "installed ChatGPT Browser API client wrapper to $bin_dir/toomux-claude"
