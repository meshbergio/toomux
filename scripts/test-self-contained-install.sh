#!/bin/sh
# End-to-end self-contained installer lifecycle test.
# Usage: test-self-contained-install.sh <bundle.tar.gz>
set -eu

archive=${1:?bundle archive}
case "$archive" in
  /*) ;;
  *) archive="$(pwd)/$archive" ;;
esac
[ -f "$archive" ] || { echo "missing bundle: $archive" >&2; exit 1; }
installer="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)/install.sh"

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT INT TERM
home="$t/home"
root="$t/runtime"
bin="$t/bin"
tools="$t/tools"
release1="$t/release-1"
release2="$t/release-2"
mkdir -p "$home" "$bin" "$tools" "$release1" "$release2"

# Give the installer ordinary POSIX tools but deliberately no tmux or claude,
# so it must expose its private tmux runtime on a clean machine.
for name in awk cp curl cut grep gzip ln mkdir mktemp mv readlink rm sed sh shasum sha256sum sysctl tar uname; do
  path=$(command -v "$name" 2>/dev/null || true)
  [ -n "$path" ] || continue
  ln -s "$path" "$tools/$name"
done

asset=$(basename "$archive")
cp "$archive" "$release1/$asset"
printf '%s  %s\n' "$(hash_file "$release1/$asset")" "$asset" > "$release1/$asset.sha256"

# An old curl/source-style launcher is backed up and restored on uninstall.
cat > "$bin/toomux" <<'EOF'
#!/bin/sh
echo "toomux 0.2.9"
EOF
chmod 755 "$bin/toomux"

HOME="$home" PATH="$tools" \
  TOOMUX_INSTALL_ROOT="$root" TOOMUX_BIN_DIR="$bin" TOOMUX_RELEASE_BASE="$release1" \
  sh "$installer" > "$t/install-1.log"

[ -L "$root/current" ]
[ ! -L "$root/previous" ]
[ -L "$bin/toomux" ]
[ -L "$bin/tmux" ]
[ "$(cat "$root/.managed-tmux")" = 1 ]
[ -f "$bin/toomux.pre-toomux-bundle" ]
"$bin/toomux" --version
"$bin/tmux" -V | grep -qx 'tmux 3.4'
first=$(readlink "$root/current")

# Make a byte-distinct but internally identical verified archive. This models
# reinstall/repair of one semantic version and proves immutable bundle ids,
# previous retention and rollback without requiring a fake product binary.
mkdir "$release2/unpack"
tar xzf "$archive" -C "$release2/unpack"
bundle_dir=$(find "$release2/unpack" -mindepth 1 -maxdepth 1 -type d | head -1)
printf '\ninstaller lifecycle variant\n' >> "$bundle_dir/README.md"
tar czf "$release2/$asset" -C "$release2/unpack" "$(basename "$bundle_dir")"
printf '%s  %s\n' "$(hash_file "$release2/$asset")" "$asset" > "$release2/$asset.sha256"

HOME="$home" PATH="$tools" \
  TOOMUX_INSTALL_ROOT="$root" TOOMUX_BIN_DIR="$bin" TOOMUX_RELEASE_BASE="$release2" \
  sh "$installer" > "$t/install-2.log"

second=$(readlink "$root/current")
previous=$(readlink "$root/previous")
[ "$second" != "$first" ]
[ "$previous" = "$first" ]
[ -d "$root/$first" ]
[ -d "$root/$second" ]

HOME="$home" PATH="$bin:$tools" "$bin/toomux" rollback > "$t/rollback-1.log"
[ "$(readlink "$root/current")" = "$first" ]
[ "$(readlink "$root/previous")" = "$second" ]

HOME="$home" PATH="$bin:$tools" "$bin/toomux" rollback > "$t/rollback-2.log"
[ "$(readlink "$root/current")" = "$second" ]
[ "$(readlink "$root/previous")" = "$first" ]

HOME="$home" PATH="$bin:$tools" "$bin/toomux" uninstall --dry-run > "$t/uninstall-dry.log"
[ -d "$root" ]
[ -L "$bin/toomux" ]

HOME="$home" PATH="$bin:$tools" "$bin/toomux" uninstall > "$t/uninstall.log"
[ ! -e "$root" ]
[ ! -L "$bin/tmux" ]
[ -f "$bin/toomux" ]
[ ! -L "$bin/toomux" ]
[ "$("$bin/toomux" --version)" = "toomux 0.2.9" ]

echo "self-contained installer lifecycle PASS"
