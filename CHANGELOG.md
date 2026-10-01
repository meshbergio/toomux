# Changelog

All notable user-visible changes are recorded here. toomux follows semantic
versioning once a compatibility promise is established; the 0.x series may
still change configuration and command details while they settle.

## Unreleased

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
