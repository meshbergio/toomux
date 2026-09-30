#!/bin/sh
# render.sh <version> <dir with toomux-*.tar.gz.sha256>: the tap's formula on stdout.
set -eu
version=$1
dir=$2
here=$(dirname "$0")
args="-e s/@VERSION@/$version/g"
for t in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
  sha=$(cut -d' ' -f1 "$dir/toomux-$t.tar.gz.sha256")
  case "$sha" in *[!0-9a-f]* | "") echo "no checksum for $t" >&2; exit 1 ;; esac
  [ ${#sha} -eq 64 ] || { echo "bad checksum for $t" >&2; exit 1; }
  args="$args -e s/@SHA_$t@/$sha/g"
done
# shellcheck disable=SC2086
sed $args "$here/toomux.rb.in"
