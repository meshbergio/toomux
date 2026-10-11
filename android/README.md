# toomux for Android

The Android client for toomux's remote access, which is not available yet.
The app renders a paired host's toomux TUI natively and sends touch and
keyboard input back to it. It needs a network transport that isn't public
yet, so there is no supported way to use it today. This folder is here so the
code is built and tested alongside the rest of toomux.

## Build

The app has no third-party runtime dependencies. It needs Android API 34 and a
JDK that can target Java 17.

```sh
cd android
printf 'sdk.dir=%s\n' "$HOME/Android/Sdk" > local.properties
./gradlew testDebugUnitTest lintDebug assembleDebug
```

The debug APK is written to
`android/app/build/outputs/apk/debug/app-debug.apk`.

For a signed release build, keep the release key outside the repository, set
the four `TOOMUX_ANDROID_*` signing variables, and run from the repository
root:

```sh
scripts/package-android-release.sh dist
```

That reruns the unit and lint checks, builds the release variant, verifies its
signature, and writes `dist/toomux-android.apk` with its SHA-256 file.
