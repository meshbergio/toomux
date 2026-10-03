# Changelog

All notable user-visible changes are recorded here. toomux follows semantic
versioning once a compatibility promise is established; the 0.x series may
still change configuration and command details while they settle.

## 0.4.2 - 2026-10-03

### Added

- Add delta-aware rich per-session terminal frames for phone clients, with ANSI styling, content hashes, pane geometry and cursor metadata while preserving the plain-screen Android contract.
- Add browser-native history and keyboard-aware visual viewport behavior to the phone web remote, so Back/Forward stays inside semantic Toomux surfaces and the command deck remains visible as the mobile keyboard changes the viewport.

### Changed

- Persist Toomux-owned Claude hooks, status-line commands, tmux bindings and the remote service against the stable `~/.local/bin/toomux` launcher when running from an installer-managed bundle, rather than stamping an immutable version payload path.
- Make unchanged phone terminal polls return metadata-only frames, avoiding repeated ANSI capture, parsing and DOM repaint work.

### Reliability

- Preserve compatibility for already-running sessions during launcher migrations and add regression coverage for stable-launcher resolution, rich-frame compatibility, browser Back/Forward, delta polling and mobile visual-viewport sizing.

## 0.4.1 - 2026-10-03

### Added

- Add authenticated per-session literal text and terminal-key input so phone
  clients can control the selected session directly instead of steering the
  desktop TUI through coordinate hit targets.
- Add a native phone browser shell with semantic session cards, attention and
  active filters, dedicated session terminals, a visible composer, terminal
  key rail, session switcher, connection sheet, Memory and a separate Full
  Console power surface.

### Changed

- Make the browser remote session-first on phones. The full Toomux TUI remains
  available under Console, but it is no longer the navigation substrate for
  ordinary session selection and control.
- Make the browser shell safe-area aware with first-class phone touch targets,
  mobile web-app metadata and explicit portrait/landscape layouts.

### Reliability

- Add real-Chrome regression coverage at iPhone 15 Pro, iPhone SE and
  landscape phone sizes, including repeated list-to-session transitions,
  filtering, direct input, terminal keys, session switching, Console, Memory
  and settings.

## 0.4.0 - 2026-10-03

### Added

- Add provider-agnostic context lifecycle policies so each provider/model can
  own a different safe context window, handover threshold and compaction
  posture without hard-coding Claude's limits.
- Add the standalone ChatGPT browser-provider path, including Fable 5.1 model
  mapping, concurrent browser-worker capacity and provider reconciliation.
- Add self-contained Linux and macOS release bundles with a pinned private tmux 3.4 runtime and terminfo database, verified by archive and manifest hashes.
- Add atomic versioned curl installs with `current` / `previous` bundles and `toomux rollback`.

### Changed

- Make automatic handover ownership explicit per provider/model and fail safe
  to the provider's native behavior when context capacity is unknown or unsafe.
- Make `https://toomux.com/install` the canonical installer path; Homebrew and npm remain supported package-manager alternatives.
- Use the exact tmux server executable when toomux is already inside an existing tmux server, and the bundled runtime for new standalone servers, avoiding client/server version mismatches.
- Make self-contained uninstall remove only installer-owned launchers/runtime and restore a migrated pre-bundle toomux launcher.

## 0.3.0 - 2026-10-02

### Added

- Add the production phone web remote at `toomux.com/remote/`, rendering the
  real isolated host-side toomux shell through a Leptos/WASM client over a
  purpose-bound ByteTraverse WebRTC capability.
- Add one-use browser pairing with an independent 256-bit toomux application
  invite, per-browser durable device grants, reconnect, self-revoke and host
  revoke semantics.
- Add in-TUI `remote` access plus `alt-r`, opening a scan-safe phone pairing
  code directly from toomux.

### Changed

- Render phone pairing codes as owner-only square SVG geometry in a local
  browser page instead of relying on terminal character-cell aspect ratios.
- Generalize the remote API from Android-only use to native Android and browser
  clients while keeping the host TUI as the single authoritative product
  surface.
- Use a source-controlled user-systemd unit for the durable toomux remote
  listener.

### Security

- Keep ByteTraverse enrollment and toomux application authority independent;
  the `toomux` ByteTraverse capability exposes only the explicit remote API
  allowlist and cannot inherit console or Bonnie authority.
- Scrub QR authority from browser history before the WASM application loads,
  and wrap the browser bearer with AES-GCM under a non-extractable WebCrypto
  key before IndexedDB storage.
- Bind the site transport bundle to a committed, qualified ByteTraverse source
  SHA with a restrictive CSP and exact import-map hash.

### Reliability

- Reject QR replay without minting another device, restore paired browsers
  across reloads without a new QR, and surface both self-revocation and
  workstation-side revocation immediately.
- Add scan/geometry regression coverage plus end-to-end validation of pairing,
  TUI input, memory access, reconnect and revocation.


## 0.2.3 - 2026-10-02

### Fixed

- Use Claude Code's first-class `claude auth login` flow for account sign-in
  instead of launching the full interactive client and injecting `/login`.
  New account folders can stop at Claude's first-run theme screen, which meant
  the old prompt injector never ran and no browser opened.

## 0.2.2 - 2026-10-01

### Fixed

- Preserve unsent Claude prompt text into a completed handover brief before
  clearing the old input and continuing in a fresh session, instead of
  retrying indefinitely with `something is typed in its prompt`.
- Detect the newest visible prompt rather than an older submitted prompt still
  in scrollback, preserve multi-line drafts, and refuse to clear input if it
  changes between inspection and handover.

## 0.2.1 - 2026-10-01

### Added

- Add an `alt-a` Accounts page to the TUI for account status, login, sharing,
  unsharing and removal.
- Add an in-TUI account wizard for creating or adopting a Claude Code account,
  choosing its shared-history group and optionally signing in immediately.
- Add a Windows npm entry point that runs the matching toomux release inside
  WSL 2.
- Publish a persistently signed Android APK with tagged GitHub releases.

### Changed

- Route CLI and TUI account mutations through one account operation layer so
  both surfaces use the same path safety, busy-session guards and
  data-preservation semantics.
- Keep the add-account wizard card stable across steps, wrap long paths and
  tighten its copy and spacing.
- Allow `install.sh` to pin an exact release with `TOOMUX_VERSION`, used by
  the Windows/WSL 2 npm launcher.

## 0.2.0 - 2026-10-01

### Added

- Add the native Android toomux client over ByteTraverse, keeping the real
  host-side toomux TUI and session model rather than introducing a separate
  mobile dashboard.
- Add the ByteTraverse-bound remote API with device pairing, per-device bearer
  credentials and Android Keystore-backed token storage.
- Add touch-native terminal controls and hardware-keyboard parity for the
  Android client.

### Changed

- Move the Android build to Gradle 9.8 and Android Gradle Plugin 9.4.1 with
  AGP 9 built-in Kotlin support.
- Update the public dependency and release-action set while preserving the
  Rust 1.88 minimum supported version.

### Fixed

- Preserve usage high-water marks within the same reset window instead of
  allowing temporary lower readings to move usage backwards.
- Keep toomux jobs and native Claude background work visible as active after a
  foreground turn settles, preventing live sessions from falling back to plain
  idle.
- Correct Android TUI cell alignment, font selection, touch interactions and
  hardware-meta handling.

### Security

- Harden account-root validation and recursive account deletion against broad,
  protected, symlink-disguised and unrelated populated paths.
- Move public pull-request CI entirely to GitHub-hosted runners.
- Add dependency advisory/license/source policy and release provenance/SBOM
  attestations.

### Reliability

- Serialise shared state, job metadata and reboot-snapshot read/modify/write
  transactions.
- Add adversarial tests for corrupt state, concurrent writers and stale PID
  records.

## 0.1.0

Initial public release: tmux session control, handovers, shared memory,
multi-account usage/switching, transcript archives, background jobs and
voyages.
