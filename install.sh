#!/bin/sh
# Install toomux: the latest release for this machine, or $TOOMUX_VERSION when
# supplied, checked against its
# checksum, into ~/.local/bin (or $TOOMUX_BIN_DIR). Nothing else is changed;
# `toomux init --apply` sets it up afterwards, and says what it will do.
set -eu

repo="meshbergio/toomux"
dir="${TOOMUX_BIN_DIR:-$HOME/.local/bin}"
version="${TOOMUX_VERSION:-latest}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux | Darwin) ;;
  MINGW* | MSYS* | CYGWIN*) echo "on Windows, toomux runs inside WSL 2: run this in your WSL terminal." >&2; exit 1 ;;
  *) echo "toomux runs on Linux and macOS (on Windows, inside WSL 2)." >&2; exit 1 ;;
esac
# A shell under Rosetta says x86_64 on Apple silicon; the arm64 build is the one to have.
if [ "$os" = Darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
  arch=arm64
fi
case "$os/$arch" in
  Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-musl ;;
  Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
  *) echo "no toomux build for $arch; cargo install --git https://github.com/$repo builds one." >&2; exit 1 ;;
esac

need() { command -v "$1" >/dev/null 2>&1 || { echo "toomux's installer needs $1." >&2; exit 1; }; }
need curl
need tar
if command -v sha256sum >/dev/null 2>&1; then
  check() { sha256sum -c --quiet -; }
elif command -v shasum >/dev/null 2>&1; then
  check() { shasum -a 256 -c - >/dev/null; }
elif command -v openssl >/dev/null 2>&1; then
  check() {
    want="$(cut -d' ' -f1)"
    got="$(openssl dgst -sha256 <toomux.tar.gz | sed 's/^.*= //')"
    [ "$want" = "$got" ] || { echo "toomux.tar.gz: checksum doesn't match" >&2; return 1; }
  }
else
  echo "toomux's installer needs sha256sum, shasum or openssl." >&2; exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
case "$version" in
  latest) base="https://github.com/$repo/releases/latest/download" ;;
  v*) base="https://github.com/$repo/releases/download/$version" ;;
  *) base="https://github.com/$repo/releases/download/v$version" ;;
esac
url="$base/toomux-$target.tar.gz"
echo "downloading $url"
curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$tmp/toomux.tar.gz"
curl --proto '=https' --tlsv1.2 -fsSL "$url.sha256" -o "$tmp/toomux.tar.gz.sha256"
(cd "$tmp" && sed "s/ .*/  toomux.tar.gz/" toomux.tar.gz.sha256 | check)
tar xzf "$tmp/toomux.tar.gz" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/toomux-$target/toomux" "$dir/toomux"
echo "installed $("$dir/toomux" --version) to $dir/toomux"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "note: $dir is not on your PATH yet." ;;
esac
if ! command -v tmux >/dev/null 2>&1; then
  if [ "$os" = Darwin ]; then echo "note: toomux needs tmux 3.2 or later: brew install tmux"; else echo "note: toomux needs tmux 3.2 or later."; fi
fi
echo
echo "next: toomux init --apply    (sets up tmux and Claude Code, and lists what toomux does by itself)"
echo "then: toomux account setup   (names your Claude Code accounts and who shares history)"
