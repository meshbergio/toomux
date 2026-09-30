#!/bin/sh
set -eu

repo=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cd "$repo"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "need $1" >&2
    exit 1
  }
}
need cargo
need tmux
need ps
need awk

cargo build --release --locked >/dev/null
bin="$repo/target/release/toomux"
tmp=$(mktemp -d)
server="toomux-bench-$$"
cleanup() {
  tmux -L "$server" kill-server >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT HUP INT TERM

tmux -L "$server" new-session -d -x 160 -y 50 -s bench "exec env TOOMUX_HOME='$tmp/home' '$bin'"
sleep 2
pid=$(tmux -L "$server" display-message -p '#{pane_pid}')

samples="$tmp/samples"
: > "$samples"
i=0
while [ "$i" -lt 10 ]; do
  ps -o rss=,pcpu= -p "$pid" >> "$samples"
  i=$((i + 1))
  sleep 1
done

size=$(wc -c < "$bin" | tr -d ' ')
awk -v size="$size" '
  { rss += $1; cpu += $2; n += 1 }
  END {
    if (!n) exit 1
    printf "release binary: %.2f MiB\n", size / 1048576
    printf "mean RSS:       %.2f MiB\n", rss / n / 1024
    printf "mean process CPU over samples: %.2f%%\n", cpu / n
    printf "samples:        %d (1 second apart)\n", n
  }
' "$samples"
