# toomux for Android

Native Android Toomux, reached through ByteTraverse.

The main screen is not a mobile reinterpretation of Toomux. The host starts an
isolated `toomux shell` for each paired device, captures its ANSI cell grid, and
the Android app renders that grid natively. Input goes back into the same shell.
The result is the desktop product by construction: the same session ordering,
state colours, account meters, grouping, filters, help, right-click session
menu, live Claude pane, usage view and `alt-m` memory graph.

Touch maps onto that grammar rather than replacing it:

- tap = terminal mouse click;
- long-press a session = the real Toomux right-click menu;
- double-tap = open the Android keyboard;
- pinch = change terminal cell size and therefore the host TUI geometry;
- long-press the Toomux header = the real `?` help overlay;
- in the TUI memory graph, `o` (or the header gesture) opens the immersive GUI
  memory explorer full-screen.

The GUI explorer is Toomux's existing self-contained memory page, loaded into a
locked-down WebView with file/content access disabled and all HTTP(S)
subrequests blocked. The ordinary TUI remains a native Canvas renderer.

The app is deliberately split from the VPN. ByteTraverse owns the encrypted
device-to-host path and its Android `VpnService`; Toomux owns the authenticated
application protocol on top. No ByteTraverse Rust/JNI code is linked into the
Toomux APK.

## Network and authority

The default topology is:

```text
Android tablet                      workstation
10.30.0.3                           10.30.0.1
┌──────────────┐                    ┌──────────────────────┐
│ toomux app   │ -- HTTP :7462 --> │ toomux remote serve │
└──────┬───────┘                    └──────────┬───────────┘
       │                                       │
       └──── encrypted ByteTraverse path ──────┘
```

The HTTP hop is cleartext *inside* the encrypted ByteTraverse overlay. The
server refuses wildcard, LAN and public binds; production binds must be
loopback or `10.30.0.0/16`, and a mesh listener rejects peers outside the same
range. The Android client independently refuses endpoints outside
`10.30.0.0/16` (loopback is accepted only for local/ADB development).

Network reachability is not authentication. Pairing creates a separate
256-bit per-device bearer token:

1. `toomux remote pair` creates an unbiased eight-digit one-time code, valid
   for ten minutes. Only its SHA-256 digest is stored on the host.
2. The app exchanges the code over the ByteTraverse path.
3. The host stores only the new bearer token's SHA-256 digest.
4. Android encrypts the bearer token at rest with a non-exportable AES-GCM key
   in Android Keystore.
5. **Forget device** revokes the host grant when reachable and removes the
   local key/token. `toomux remote revoke <device-id>` and
   `toomux remote revoke all` are the host-side fallback.

The mobile API is intentionally a Toomux protocol, not a general remote shell.
Its primary surface is an authenticated TUI frame/input pair backed by a
per-device tmux server. Text is sent literally, keys are validated, terminal
mouse events are bounded to the negotiated grid, and revoking the device kills
its display shell. The API also exposes the self-contained memory page.

## Run the host

The ByteTraverse peer must already give the Android device a route to
`10.30.0.1`.

```sh
# foreground, good for first setup
toomux remote serve

# in another terminal, when the app asks to pair
toomux remote pair
```

The app defaults to `http://10.30.0.1:7462`. Enter the one-time code and pair.

For development only, loopback plus `adb reverse` can replace the ByteTraverse
route while the UI is being worked on. That does not count as a transport
qualification; production use is the `10.30.0.1:7462` path.

On Linux, once the path has been tested, an opt-in user service can keep the
endpoint available without changing the ordinary toomux install:

```ini
# ~/.config/systemd/user/toomux-remote.service
[Unit]
Description=toomux Android endpoint over ByteTraverse
After=network-online.target

[Service]
ExecStart=%h/.local/bin/toomux remote serve
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
```

Then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now toomux-remote.service
```

Use the actual `toomux` path in `ExecStart` if it is not
`~/.local/bin/toomux`.

## Build

The app has no third-party runtime dependencies. It needs Android API 34 and a
JDK capable of targeting Java 17.

```sh
cd android
printf 'sdk.dir=%s\n' "$HOME/Android/Sdk" > local.properties
./gradlew testDebugUnitTest lintDebug assembleDebug
```

The debug APK is:

```text
android/app/build/outputs/apk/debug/app-debug.apk
```

For a connected development device:

```sh
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n io.github.meshbergio.toomux/.MainActivity
```

For initial UI testing before the ByteTraverse peer is up, run the host on
loopback and use ADB reverse:

```sh
toomux remote serve --bind 127.0.0.1:7462
adb reverse tcp:7462 tcp:7462
```

Set the app endpoint to `http://127.0.0.1:7462`. This is a development path
only; normal use goes through ByteTraverse at `10.30.0.1:7462`.
