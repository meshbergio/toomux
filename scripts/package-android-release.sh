#!/bin/sh
# Build the signed Android release APK from the maintainer-held signing key.
# Required environment:
#   TOOMUX_ANDROID_KEYSTORE
#   TOOMUX_ANDROID_STORE_PASSWORD
#   TOOMUX_ANDROID_KEY_ALIAS
#   TOOMUX_ANDROID_KEY_PASSWORD
# Usage: scripts/package-android-release.sh [output-directory]
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
out=${1:-"$root/dist"}

: "${TOOMUX_ANDROID_KEYSTORE:?set TOOMUX_ANDROID_KEYSTORE}"
: "${TOOMUX_ANDROID_STORE_PASSWORD:?set TOOMUX_ANDROID_STORE_PASSWORD}"
: "${TOOMUX_ANDROID_KEY_ALIAS:?set TOOMUX_ANDROID_KEY_ALIAS}"
: "${TOOMUX_ANDROID_KEY_PASSWORD:?set TOOMUX_ANDROID_KEY_PASSWORD}"

cargo_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -1)
android_version=$(sed -n 's/.*versionName = "\([^"]*\)"/\1/p' "$root/android/app/build.gradle.kts" | head -1)
[ -n "$cargo_version" ]
[ "$cargo_version" = "$android_version" ] || {
    echo "Rust version $cargo_version != Android version $android_version" >&2
    exit 1
}

sdk=${ANDROID_HOME:-}
if [ -z "$sdk" ] && [ -f "$root/android/local.properties" ]; then
    sdk=$(sed -n 's/^sdk.dir=//p' "$root/android/local.properties" | head -1)
fi
[ -n "$sdk" ] || {
    echo "Android SDK not found; set ANDROID_HOME or android/local.properties." >&2
    exit 1
}

apksigner=$(find "$sdk/build-tools" -type f -name apksigner -perm -111 2>/dev/null | sort -V | tail -1)
[ -x "$apksigner" ] || {
    echo "apksigner not found under $sdk/build-tools" >&2
    exit 1
}

(cd "$root/android" && ./gradlew --no-daemon clean testDebugUnitTest lintDebug assembleRelease)

mkdir -p "$out"
apk="$root/android/app/build/outputs/apk/release/app-release.apk"
dest="$out/toomux-android.apk"
cp "$apk" "$dest"

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$out" && sha256sum toomux-android.apk > toomux-android.apk.sha256)
else
    (cd "$out" && shasum -a 256 toomux-android.apk > toomux-android.apk.sha256)
fi

"$apksigner" verify --verbose --print-certs "$dest"
echo "built toomux Android $android_version: $dest"
