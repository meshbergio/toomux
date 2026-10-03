#!/bin/sh
# Canonical self-contained toomux installer.
#
# Installs an immutable verified bundle under ~/.local/lib/toomux and an
# atomic launcher under ~/.local/bin. The bundle carries toomux, a private
# tmux runtime and its terminfo database. It never installs Claude Code or
# ByteTraverse and never overwrites an unrelated tmux.
set -eu

repo="meshbergio/toomux"
version_request="${TOOMUX_VERSION:-latest}"
install_root="${TOOMUX_INSTALL_ROOT:-$HOME/.local/lib/toomux}"
bin_dir="${TOOMUX_BIN_DIR:-$HOME/.local/bin}"
release_base="${TOOMUX_RELEASE_BASE:-}"

fail() {
  echo "toomux installer: $*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "needs $1"
}

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$1" | sed 's/^.*= //'
  else
    fail "needs sha256sum, shasum or openssl"
  fi
}

download() {
  source=$1
  dest=$2
  case "$source" in
    file://*) cp "${source#file://}" "$dest" ;;
    /*) cp "$source" "$dest" ;;
    *) curl --proto '=https' --tlsv1.2 -fsSL "$source" -o "$dest" ;;
  esac
}

manifest_field() {
  key=$1
  file=$2
  awk -F= -v key="$key" '$1 == key { sub(/^[^=]*=/, ""); print; exit }' "$file"
}

safe_absolute_dir() {
  path=$1
  case "$path" in
    /*) ;;
    *) return 1 ;;
  esac
  case "/$path/" in
    */../*|*/./*) return 1 ;;
  esac
  return 0
}

atomic_link() {
  target=$1
  link=$2
  tmp="$link.toomux-new.$"
  rm -f "$tmp"
  ln -s "$target" "$tmp"

  # GNU mv follows a destination symlink-to-directory unless -T is used.
  # BSD/macOS mv uses -h for the same no-follow replacement. Both perform
  # rename(2) on this same-filesystem temporary link.
  if mv -Tf "$tmp" "$link" 2>/dev/null; then
    return
  fi
  if mv -fh "$tmp" "$link" 2>/dev/null; then
    return
  fi
  rm -f "$tmp"
  fail "this platform cannot atomically replace $link"
}

owned_link() {
  link=$1
  [ -L "$link" ] || return 1
  target=$(readlink "$link" 2>/dev/null || true)
  case "$target" in
    "$install_root"/*|"$install_root"/current/*|"$install_root"/versions/*) return 0 ;;
    *) return 1 ;;
  esac
}

safe_absolute_dir "$install_root" || fail "TOOMUX_INSTALL_ROOT must be an absolute path without . or .."
safe_absolute_dir "$bin_dir" || fail "TOOMUX_BIN_DIR must be an absolute path without . or .."
case "$install_root" in
  /|"$HOME"|"$HOME/.local"|"$HOME/.local/lib"|"$HOME/.local/share"|"$HOME/.local/share/toomux")
    fail "refusing unsafe install root: $install_root"
    ;;
esac
[ "$install_root" != "$bin_dir" ] || fail "install root and bin directory must be different"

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux|Darwin) ;;
  MINGW*|MSYS*|CYGWIN*)
    fail "on Windows, run this inside WSL 2"
    ;;
  *) fail "toomux runs on Linux and macOS (Windows through WSL 2)" ;;
esac
if [ "$os" = Darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
  arch=arm64
fi
case "$os/$arch" in
  Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-musl ;;
  Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-musl ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  Darwin/arm64|Darwin/aarch64) target=aarch64-apple-darwin ;;
  *) fail "no self-contained bundle for $os/$arch" ;;
esac

need tar
need awk
need curl

if [ -z "$release_base" ]; then
  case "$version_request" in
    latest) release_base="https://github.com/$repo/releases/latest/download" ;;
    v*) release_base="https://github.com/$repo/releases/download/$version_request" ;;
    *) release_base="https://github.com/$repo/releases/download/v$version_request" ;;
  esac
fi

asset="toomux-bundle-$target.tar.gz"
tmp=$(mktemp -d)
stage=
trap 'rm -rf "$tmp" "${stage:-}"' EXIT INT TERM

echo "toomux: downloading $asset"
download "$release_base/$asset" "$tmp/$asset"
download "$release_base/$asset.sha256" "$tmp/$asset.sha256"

want=$(awk 'NR == 1 { print $1 }' "$tmp/$asset.sha256")
got=$(hash_file "$tmp/$asset")
case "$want" in
  ????????????????????????????????????????????????????????????????) ;;
  *) fail "published bundle checksum is malformed" ;;
esac
[ "$want" = "$got" ] || fail "bundle checksum does not match"

bundle_root="toomux-bundle-$target"
tar tzf "$tmp/$asset" | while IFS= read -r member; do
  case "$member" in
    "$bundle_root"|"$bundle_root/"|"$bundle_root"/*) ;;
    *) echo "toomux installer: unsafe archive member: $member" >&2; exit 1 ;;
  esac
  case "/$member/" in
    */../*|*/./*) echo "toomux installer: unsafe archive path: $member" >&2; exit 1 ;;
  esac
done

tar xzf "$tmp/$asset" -C "$tmp"
bundle="$tmp/$bundle_root"
manifest="$bundle/manifest.txt"
[ -f "$manifest" ] || fail "bundle has no manifest"
[ "$(manifest_field bundle_format "$manifest")" = 1 ] || fail "unsupported bundle format"
[ "$(manifest_field target "$manifest")" = "$target" ] || fail "bundle target mismatch"
version=$(manifest_field version "$manifest")
case "$version" in
  ''|*[!0-9A-Za-z._+-]*) fail "bundle version is malformed" ;;
esac
if [ "$version_request" != latest ]; then
  requested=${version_request#v}
  [ "$version" = "$requested" ] || fail "requested $requested but bundle is $version"
fi

toomux_sha=$(manifest_field toomux_sha256 "$manifest")
tmux_sha=$(manifest_field tmux_sha256 "$manifest")
[ "$(hash_file "$bundle/bin/toomux")" = "$toomux_sha" ] || fail "toomux binary hash does not match manifest"
[ "$(hash_file "$bundle/bin/tmux")" = "$tmux_sha" ] || fail "tmux binary hash does not match manifest"
[ "$("$bundle/bin/toomux" --version | awk '{print $2}')" = "$version" ] || fail "toomux version check failed"
tmux_version=$(manifest_field tmux_version "$manifest")
TERMINFO_DIRS="$bundle/share/terminfo:" "$bundle/bin/tmux" -V | grep -qx "tmux $tmux_version" || fail "tmux version check failed"

short=$(printf '%s' "$got" | cut -c1-12)
id="$version-$short"
versions="$install_root/versions"
version_dir="$versions/$id"
mkdir -p "$versions" "$bin_dir"
printf '1\n' > "$install_root/.installer-format"
printf '%s\n' "$bin_dir" > "$install_root/.bin-dir"

stage="$install_root/.staging.$$"
rm -rf "$stage"
mkdir "$stage"
cp -R "$bundle/." "$stage/"
if [ -d "$version_dir" ]; then
  [ "$(hash_file "$version_dir/bin/toomux")" = "$toomux_sha" ] || fail "existing immutable version $id does not match"
  [ "$(hash_file "$version_dir/bin/tmux")" = "$tmux_sha" ] || fail "existing immutable runtime $id does not match"
  rm -rf "$stage"
  stage=
else
  mv "$stage" "$version_dir"
  stage=
fi

new_target="versions/$id"
if [ -L "$install_root/current" ]; then
  old_target=$(readlink "$install_root/current")
  case "$old_target" in
    versions/*)
      if [ "$old_target" != "$new_target" ] && [ -d "$install_root/$old_target" ]; then
        atomic_link "$old_target" "$install_root/previous"
      fi
      ;;
    *) fail "current link is not installer-owned: $old_target" ;;
  esac
fi
atomic_link "$new_target" "$install_root/current"

launcher="$bin_dir/toomux"
if [ -e "$launcher" ] || [ -L "$launcher" ]; then
  if owned_link "$launcher"; then
    :
  elif [ ! -L "$launcher" ] && "$launcher" --version 2>/dev/null | grep -q '^toomux '; then
    backup="$launcher.pre-toomux-bundle"
    if [ ! -e "$backup" ] && [ ! -L "$backup" ]; then
      mv "$launcher" "$backup"
      printf '%s\n' "$backup" > "$install_root/.previous-toomux-launcher"
    else
      rm -f "$launcher"
    fi
  else
    fail "refusing to replace unrelated $launcher"
  fi
fi
atomic_link "$install_root/current/bin/toomux" "$launcher"

managed_tmux=0
tmux_launcher="$bin_dir/tmux"
if owned_link "$tmux_launcher"; then
  atomic_link "$install_root/current/bin/tmux" "$tmux_launcher"
  managed_tmux=1
elif command -v tmux >/dev/null 2>&1; then
  echo "toomux: keeping existing $(command -v tmux) ($(tmux -V 2>/dev/null || echo unknown))"
elif [ ! -e "$tmux_launcher" ] && [ ! -L "$tmux_launcher" ]; then
  atomic_link "$install_root/current/bin/tmux" "$tmux_launcher"
  managed_tmux=1
  echo "toomux: installed private tmux $tmux_version"
else
  echo "toomux: $tmux_launcher already exists, so the private tmux stays internal"
fi
printf '%s\n' "$managed_tmux" > "$install_root/.managed-tmux"

installed=$("$install_root/current/bin/toomux" --version)
runtime=$(TERMINFO_DIRS="$install_root/current/share/terminfo:" "$install_root/current/bin/tmux" -V)
echo "toomux: installed $installed"
echo "toomux: runtime $runtime"
echo "toomux: $install_root/current"
echo "toomux: launcher $launcher"

case ":$PATH:" in
  *":$bin_dir:"*) command_name=toomux ;;
  *)
    command_name="$launcher"
    echo "toomux: note: $bin_dir is not on PATH in this shell"
    ;;
esac

if ! command -v claude >/dev/null 2>&1; then
  echo "toomux: Claude Code is still required and is not installed by toomux"
fi
echo
echo "next: $command_name init --apply"
echo "then: $command_name account setup   # optional, for multiple Claude Code accounts"
echo "rollback later: $command_name rollback"
