#!/bin/sh
# Package a self-contained toomux release bundle without changing the
# binary-only artifacts consumed by Homebrew and npm.
# Usage: package.sh <target> <toomux-bin> <runtime-dir> <output-dir> <version> <git-sha>
set -eu

target=${1:?target}
toomux_bin=${2:?toomux binary}
runtime=${3:?runtime dir}
out=${4:?output dir}
version=${5:?version}
git_sha=${6:?git sha}

case "$target" in
  x86_64-unknown-linux-musl|aarch64-unknown-linux-musl|x86_64-apple-darwin|aarch64-apple-darwin) ;;
  *) echo "package bundle: unsupported target: $target" >&2; exit 1 ;;
esac
case "$version" in
  ''|*[!0-9A-Za-z._+-]*) echo "package bundle: unsafe version: $version" >&2; exit 1 ;;
esac
case "$git_sha" in
  ''|*[!0-9a-f]*) echo "package bundle: unsafe git sha: $git_sha" >&2; exit 1 ;;
esac

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

root="toomux-bundle-$target"
stage="$out/$root"
archive="$out/$root.tar.gz"
rm -rf "$stage" "$archive" "$archive.sha256"
mkdir -p "$stage/bin" "$stage/share"

cp "$toomux_bin" "$stage/bin/toomux"
cp "$runtime/bin/tmux" "$stage/bin/tmux"
cp -R "$runtime/share/terminfo" "$stage/share/terminfo"
cp "$runtime/SOURCES.txt" "$stage/RUNTIME-SOURCES.txt"
cp README.md LICENSE-MIT LICENSE-APACHE "$stage/"
chmod 755 "$stage/bin/toomux" "$stage/bin/tmux"

case "$target" in
  *-unknown-linux-musl)
    file "$stage/bin/toomux" | grep -Eq 'statically linked|static-pie linked' || {
      echo "package bundle: Linux toomux is not a static musl binary" >&2
      file "$stage/bin/toomux" >&2
      exit 1
    }
    ;;
  *-apple-darwin)
    if command -v codesign >/dev/null 2>&1; then
      codesign --force --sign - "$stage/bin/toomux"
      codesign --force --sign - "$stage/bin/tmux"
    fi
    if otool -L "$stage/bin/toomux" | grep -E '/(opt/homebrew|usr/local/opt|Cellar)/' >/dev/null; then
      echo "package bundle: macOS toomux depends on package-manager libraries" >&2
      otool -L "$stage/bin/toomux" >&2
      exit 1
    fi
    ;;
esac

toomux_sha=$(hash_file "$stage/bin/toomux")
tmux_sha=$(hash_file "$stage/bin/tmux")
tmux_version=$("$stage/bin/tmux" -V | awk '{print $2}')

cat > "$stage/manifest.txt" <<EOF
bundle_format=1
version=$version
target=$target
git_sha=$git_sha
tmux_version=$tmux_version
libevent_version=2.1.12-stable
ncurses_version=6.5
toomux_sha256=$toomux_sha
tmux_sha256=$tmux_sha
EOF

COPYFILE_DISABLE=1 tar czf "$archive" -C "$out" "$root"
sum=$(hash_file "$archive")
printf '%s  %s\n' "$sum" "$(basename "$archive")" > "$archive.sha256"
echo "$archive"
