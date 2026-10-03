#!/bin/sh
# Build the private tmux runtime shipped inside a self-contained toomux bundle.
# Usage: build-tmux-runtime.sh <output-dir>
set -eu

out=${1:?usage: build-tmux-runtime.sh <output-dir>}
TMUX_VERSION=3.4
LIBEVENT_VERSION=2.1.12-stable
NCURSES_VERSION=6.5
TMUX_SHA256=551ab8dea0bf505c0ad6b7bb35ef567cdde0ccb84357df142c254f35a23e19aa
LIBEVENT_SHA256=92e6de1be9ec176428fd2367677e61ceffc2ee1cb119035037a27d346b0403bb
NCURSES_SHA256=136d91bc269a9a5785e5f9e980bc76ab57428f604ce3e5a5a90cebc767971cc6

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "build-tmux-runtime: missing build tool: $1" >&2
    exit 1
  }
}

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

fetch() {
  name=$1
  url=$2
  want=$3
  file="$work/$name.tar.gz"
  curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$file"
  got=$(hash_file "$file")
  [ "$got" = "$want" ] || {
    echo "build-tmux-runtime: checksum mismatch for $name: $got != $want" >&2
    exit 1
  }
  mkdir "$work/$name-src"
  tar xzf "$file" -C "$work/$name-src" --strip-components=1
}

need curl
need tar
need make
need pkg-config

os=$(uname -s)
case "$os" in
  Linux)
    need musl-gcc
    need yacc
    cc=musl-gcc
    ;;
  Darwin)
    need cc
    cc=${CC:-cc}
    ;;
  *)
    echo "build-tmux-runtime: Linux and macOS only" >&2
    exit 1
    ;;
esac

jobs=${TOOMUX_BUILD_JOBS:-}
if [ -z "$jobs" ]; then
  if command -v nproc >/dev/null 2>&1; then
    jobs=$(nproc)
  else
    jobs=$(sysctl -n hw.ncpu 2>/dev/null || echo 2)
  fi
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
prefix="$work/prefix"
mkdir -p "$prefix" "$out/bin" "$out/share"

fetch tmux "https://github.com/tmux/tmux/releases/download/$TMUX_VERSION/tmux-$TMUX_VERSION.tar.gz" "$TMUX_SHA256"
fetch libevent "https://github.com/libevent/libevent/releases/download/release-$LIBEVENT_VERSION/libevent-$LIBEVENT_VERSION.tar.gz" "$LIBEVENT_SHA256"
fetch ncurses "https://invisible-island.net/archives/ncurses/ncurses-$NCURSES_VERSION.tar.gz" "$NCURSES_SHA256"

(
  cd "$work/ncurses-src"
  CC="$cc" ./configure \
    --prefix="$prefix" \
    --without-shared \
    --with-normal \
    --with-termlib \
    --without-debug \
    --without-ada \
    --without-cxx \
    --without-cxx-binding \
    --without-tests \
    --enable-widec
  make -j"$jobs"
  make install
)
ln -sf libtinfow.a "$prefix/lib/libtinfo.a"

(
  cd "$work/libevent-src"
  CC="$cc" ./configure \
    --prefix="$prefix" \
    --disable-shared \
    --enable-static \
    --disable-openssl \
    --disable-samples \
    --disable-libevent-regress
  make -j"$jobs"
  make install
)

(
  cd "$work/tmux-src"
  export PKG_CONFIG_PATH="$prefix/lib/pkgconfig"
  export CPPFLAGS="-I$prefix/include -I$prefix/include/ncursesw"
  if [ "$os" = Linux ]; then
    export LDFLAGS="-L$prefix/lib -static"
    ac_cv_search_forkpty='none required' CC="$cc" ./configure --prefix="$prefix"
  else
    export LDFLAGS="-L$prefix/lib"
    CC="$cc" ./configure --prefix="$prefix"
  fi
  make -j"$jobs"
  cp tmux "$out/bin/tmux"
)

chmod 755 "$out/bin/tmux"
rm -rf "$out/share/terminfo"
cp -R "$prefix/share/terminfo" "$out/share/terminfo"

if [ "$os" = Linux ]; then
  strip "$out/bin/tmux" 2>/dev/null || true
  file "$out/bin/tmux" | grep -q 'statically linked' || {
    echo "build-tmux-runtime: Linux tmux is not static" >&2
    file "$out/bin/tmux" >&2
    exit 1
  }
  if ldd "$out/bin/tmux" >/dev/null 2>&1; then
    echo "build-tmux-runtime: Linux tmux unexpectedly has dynamic dependencies" >&2
    ldd "$out/bin/tmux" >&2
    exit 1
  fi
else
  strip -x "$out/bin/tmux" 2>/dev/null || true
  if command -v codesign >/dev/null 2>&1; then
    codesign --force --sign - "$out/bin/tmux"
  fi
  if otool -L "$out/bin/tmux" | grep -E '/(opt/homebrew|usr/local/opt|Cellar)/' >/dev/null; then
    echo "build-tmux-runtime: macOS tmux still depends on package-manager libraries" >&2
    otool -L "$out/bin/tmux" >&2
    exit 1
  fi
fi

[ "$("$out/bin/tmux" -V)" = "tmux $TMUX_VERSION" ] || {
  echo "build-tmux-runtime: bundled tmux failed its version check" >&2
  exit 1
}

cat > "$out/SOURCES.txt" <<EOF
tmux $TMUX_VERSION
https://github.com/tmux/tmux/releases/download/$TMUX_VERSION/tmux-$TMUX_VERSION.tar.gz
sha256 $TMUX_SHA256

libevent $LIBEVENT_VERSION
https://github.com/libevent/libevent/releases/download/release-$LIBEVENT_VERSION/libevent-$LIBEVENT_VERSION.tar.gz
sha256 $LIBEVENT_SHA256

ncurses $NCURSES_VERSION
https://invisible-island.net/archives/ncurses/ncurses-$NCURSES_VERSION.tar.gz
sha256 $NCURSES_SHA256
EOF

echo "private runtime: $("$out/bin/tmux" -V) -> $out"
