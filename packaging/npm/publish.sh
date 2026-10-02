#!/bin/sh
# publish.sh <version> <dir with toomux-<target>.tar.gz> [--dry-run]
# Builds the npm packages from a release's binaries and publishes them: one
# @toomux/<os>-<cpu> package per platform, then the toomux launcher that lists
# them as optional dependencies, so npm installs only the one that fits.
set -eu
version=$1
dir=$(cd "$2" && pwd)
dry=${3:-}
here=$(cd "$(dirname "$0")" && pwd)
out=$(mktemp -d)
repo='"repository": { "type": "git", "url": "git+https://github.com/meshbergio/toomux.git" }'
common="\"version\": \"$version\", \"license\": \"MIT OR Apache-2.0\", \"homepage\": \"https://toomux.com\", $repo"
deps=""

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

verify_archive() {
  archive=$1
  sum="$archive.sha256"
  [ -f "$sum" ] || { echo "missing checksum: $sum" >&2; exit 1; }
  want=$(awk 'NR == 1 { print $1 }' "$sum")
  got=$(hash_file "$archive")
  [ -n "$want" ] && [ "$want" = "$got" ] || {
    echo "checksum mismatch: $archive" >&2
    exit 1
  }
}

for pair in x86_64-unknown-linux-musl:linux:x64 aarch64-unknown-linux-musl:linux:arm64 \
            x86_64-apple-darwin:darwin:x64 aarch64-apple-darwin:darwin:arm64; do
  target=${pair%%:*}; rest=${pair#*:}; os=${rest%%:*}; cpu=${rest#*:}
  name="@toomux/$os-$cpu"
  p="$out/$os-$cpu"
  mkdir -p "$p/bin"
  verify_archive "$dir/toomux-$target.tar.gz"
  tar xzf "$dir/toomux-$target.tar.gz" -C "$out"
  mv "$out/toomux-$target/toomux" "$p/bin/toomux"
  chmod 755 "$p/bin/toomux"
  cp "$out/toomux-$target"/LICENSE-* "$p/"
  # shellcheck disable=SC2016 # the Markdown backticks are deliberately literal
  printf 'The toomux binary for %s %s. Install `toomux`, not this: https://toomux.com\n' "$os" "$cpu" > "$p/README.md"
  cat > "$p/package.json" <<JSON
{ "name": "$name", $common,
  "description": "The toomux binary for $os $cpu. Install toomux, not this.",
  "os": ["$os"], "cpu": ["$cpu"], "files": ["bin/toomux", "LICENSE-MIT", "LICENSE-APACHE"], "preferUnplugged": true }
JSON
  deps="$deps${deps:+, }\"$name\": \"$version\""
done
cp -R "$here/toomux" "$out/toomux"
cp "$out"/toomux-x86_64-unknown-linux-musl/LICENSE-* "$out/toomux/"
cat > "$out/toomux/package.json" <<JSON
{ "name": "toomux", $common,
  "description": "Claude Code, minus the babysitting: every session and account in one place, with lossless handover, shared memory and tmux-native control.",
  "keywords": ["claude-code", "claude", "anthropic", "tmux", "tui", "ai-agents", "terminal"],
  "bin": { "toomux": "bin/toomux.js" },
  "files": ["bin/toomux.js", "LICENSE-MIT", "LICENSE-APACHE"],
  "engines": { "node": ">=16" },
  "optionalDependencies": { $deps } }
JSON
for p in linux-x64 linux-arm64 darwin-x64 darwin-arm64 toomux; do
  (cd "$out/$p" && node -e 'JSON.parse(require("fs").readFileSync("package.json"))' && npm publish --access public ${dry:+--dry-run} ${NPM_PROVENANCE:+--provenance})
done
echo "$out"
