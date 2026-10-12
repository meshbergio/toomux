# Changelog

All notable user-visible changes are recorded here. toomux follows semantic
versioning once a compatibility promise is established; the 0.x series may
still change configuration and command details while they settle.

## [Unreleased]

### Added

- Choose whether the fresh session a handover starts has Remote Control and bypass permissions: `handover_remote_control` and `handover_skip_permissions` (`keep`, `on`, `off`), also under a new `ctrl-g` settings screen. `keep` carries Remote Control turned on with `/remote-control` too.
- Close Claude Code's usage-limit dialog a minute after the limit resets and carry on: a session the limit cut off mid-work gets a short "carry on" prompt, and voyages and pending handovers always continue. Set `resume_after_limit` to `"working"` (default), `"all"` or `"off"`; `auto_continue_at_limit = true` picks Claude's own "wait, then continue" option instead. Each step is logged in `limits/log.jsonl`, and it works with no tmux client attached.
- Add `alt-n` to start a new session from anywhere in the full-screen shell, and `toomux new [folder] [--account A] [--detach]` to do the same from a shell.
- Let `toomux reopen <id> [--account A]` open any conversation an account holds, by its id.
- Add `toomux voyage pause`, `resume` and `rebind`; `toomux voyage` lists a voyage whose conversation moved or is gone as "moved" or "orphaned", with how to rebind or clear it.
- List Claude processes that Claude Code's own registry doesn't know about, and flag a conversation open in two processes on both rows.
- Keep every TUI error in `errors.log` in the state folder (listed by `toomux where`) and show the last one under `?`.
- Add `docs/EVIDENCE.md`: what the 33,026-call comparison in the README measured, how both sides were chosen, its limits, and a script to reproduce it.

### Changed

- Rewrite the README around what toomux does, with a handover animation, a three-command quick start, a key table and an FAQ.
- Skip Claude's first-run screens the person has already been through: before any launch, toomux trusts a folder you picked with `alt-n` or `toomux new`, trusts a folder any of your accounts already trusts, and copies the "onboarding done" flag and theme from another of your accounts. Accounts that have never signed in are left to Claude's sign-in.
- Restructure the guide: install and first run come first, handover has its own section, and there are new sections on folder trust, configuration (every key with its default) and troubleshooting, plus a complete CLI reference.
- Adopt the Contributor Covenant 2.1, and add SUPPORT.md, issue templates that ask for toomux, Claude Code and tmux versions, and a getting-started guide and module map in CONTRIBUTING.md. Security reports go through GitHub's private vulnerability reporting.
- The installer prints the exact line to add the launcher to PATH for your shell, and ends by telling you to run `toomux`.
- Remote and phone access is out of the public docs until it can be set up without private infrastructure; the setup guides, the sample systemd unit and the browser-provider helper scripts are removed.
- Carry folder trust to a folder's real path, so a folder trusted under a symlinked path no longer stops every relaunch at the trust prompt.
- Moving a session to another account copies its conversation into that account's projects first, and the move only reports success once the session has started there; otherwise the session goes back where it was.
- Usage limits with no reset time (model or spend limits) now hold for five hours unless a real reply clears them sooner, instead of seven days.
- Voyages judge only real turn ends: a turn that ends while the session's agents, monitors or background commands are still running is let through and shown as "waits on background work". Pauses record why, and a pause toomux made because the session seemed to stop is lifted when background work reports back.
- A voyage follows its conversation through handovers, Claude's "continued in" records and the process moving to a new conversation.

### Fixed

- Follow a session that Claude Code has parked as a background job: its row, context, cost and state now come from the job that holds the conversation, rather than from where it was parked (which could show 120k context for a conversation at 517k). Such a session isn't handed over, since restarting its pane would leave the job running on its own.
- Memory search (`mem_search`) stays in the session's project and global notes unless it asks for `where: "all"`; it no longer fills up with other projects' matches when its own project has few.
- Handovers no longer stall silently: a handover waiting on text in the prompt shows "handing over · waiting: …", logs why, and after 30 minutes fails visibly. Retries run on their own schedule without an attached tmux client, and a fresh session that won't start puts the old conversation back and backs off (30 minutes, 2 hours, then a day after three attempts) instead of looping.
- Verify that typed prompts are sent and cleared prompts are empty, including multi-line drafts, rather than assuming it.
- Don't start a handover while Claude Code is counting down to continue after a usage limit, and recognise all of Claude Code 2.1.296's auto-continue screens so toomux never cancels that countdown.
- Never open a conversation that is already running in another process.
- Name the conversation each launch opens, so a handover successor no longer reports its predecessor's id.
- New sessions no longer copy another conversation's name or handover folder from the flags of running sessions.
- `/voyage` keeps a `--check` command's own flags (for example `cargo test -- --nocapture`) instead of dropping them.
- The full-screen shell no longer creates or follows a stray tmux server on the default socket.
- Keys that did nothing now say why ("busy: …", "nothing matches"), and footer hints do what they say.
- Wake-ups for a usage reset survive a reboot.
- Honour `XDG_STATE_HOME` on macOS.
- An older toomux still running after an upgrade no longer hides sessions by rewriting the shared pane cache.

## [0.4.4] - 2026-10-11

### Fixed

- Prevent inherited `NO_COLOR` from removing Claude Code's dim prompt styling, which caused suggested prompts to be mistaken for user drafts and blocked handover after automation or workspace migration.
- Remove `NO_COLOR` for each Claude launch and for new tmux servers, including resumes and handovers on existing servers, while retaining protection for genuine typed drafts.

### Reliability

- Add regression coverage for launch environment filtering and suggestion detection in an isolated tmux server.

## [0.4.3] - 2026-10-04

### Fixed

- Keep a paused Voyage paused when Claude Code delivers synthetic user-channel events such as background task completions, system reminders or teammate notifications; only a genuine user prompt (or explicit `/voyage resume`) can carry a `needs you` Voyage forward.
- Apply the same human-input classification to turn-end handover so background notifications cannot be mistaken for the user's next request and spuriously trigger lifecycle transitions.

### Reliability

- Add regression coverage for the exact `<task-notification>` false-resume incident and for synthetic notifications arriving at a handover boundary.

## [0.4.2] - 2026-10-03

### Added

- Add delta-aware rich per-session terminal frames for phone clients, with ANSI styling, content hashes, pane geometry and cursor metadata while preserving the plain-screen Android contract.
- Add browser-native history and keyboard-aware visual viewport behavior to the phone web remote, so Back/Forward stays inside semantic Toomux surfaces and the command deck remains visible as the mobile keyboard changes the viewport.

### Changed

- Persist Toomux-owned Claude hooks, status-line commands, tmux bindings and the remote service against the stable `~/.local/bin/toomux` launcher when running from an installer-managed bundle, rather than stamping an immutable version payload path.
- Make unchanged phone terminal polls return metadata-only frames, avoiding repeated ANSI capture, parsing and DOM repaint work.

### Reliability

- Preserve compatibility for already-running sessions during launcher migrations and add regression coverage for stable-launcher resolution, rich-frame compatibility, browser Back/Forward, delta polling and mobile visual-viewport sizing.

## [0.4.1] - 2026-10-03

### Added

- Add authenticated per-session literal text and terminal-key input so phone clients can control the selected session directly instead of steering the desktop TUI through coordinate hit targets.
- Add a native phone browser shell with semantic session cards, attention and active filters, dedicated session terminals, a visible composer, terminal key rail, session switcher, connection sheet, Memory and a separate Full Console power surface.

### Changed

- Make the browser remote session-first on phones. The full Toomux TUI remains available under Console, but it is no longer the navigation substrate for ordinary session selection and control.
- Make the browser shell safe-area aware with first-class phone touch targets, mobile web-app metadata and explicit portrait/landscape layouts.

### Reliability

- Add real-Chrome regression coverage at iPhone 15 Pro, iPhone SE and landscape phone sizes, including repeated list-to-session transitions, filtering, direct input, terminal keys, session switching, Console, Memory and settings.

## [0.4.0] - 2026-10-03

### Added

- Add provider-agnostic context lifecycle policies so each provider/model can own a different safe context window, handover threshold and compaction posture without hard-coding Claude's limits.
- Add an experimental browser-based provider path for running sessions on a non-Claude provider, with concurrent worker capacity and provider reconciliation.
- Add self-contained Linux and macOS release bundles with a pinned private tmux 3.4 runtime and terminfo database, verified by archive and manifest hashes.
- Add atomic versioned curl installs with `current` / `previous` bundles and `toomux rollback`.

### Changed

- Make automatic handover ownership explicit per provider/model and fail safe to the provider's native behavior when context capacity is unknown or unsafe.
- Make `https://toomux.com/install` the canonical installer path; Homebrew and npm remain supported package-manager alternatives.
- Use the exact tmux server executable when toomux is already inside an existing tmux server, and the bundled runtime for new standalone servers, avoiding client/server version mismatches.
- Make self-contained uninstall remove only installer-owned launchers/runtime and restore a migrated pre-bundle toomux launcher.

## [0.3.0] - 2026-10-02

### Added

- Add the production phone web remote at `toomux.com/remote/`, rendering the real isolated host-side toomux shell through a Leptos/WASM client over a purpose-bound ByteTraverse WebRTC capability.
- Add one-use browser pairing with an independent 256-bit toomux application invite, per-browser durable device grants, reconnect, self-revoke and host revoke semantics.
- Add in-TUI `remote` access plus `alt-r`, opening a scan-safe phone pairing code directly from toomux.

### Changed

- Render phone pairing codes as owner-only square SVG geometry in a local browser page instead of relying on terminal character-cell aspect ratios.
- Generalize the remote API from Android-only use to native Android and browser clients while keeping the host TUI as the single authoritative product surface.
- Use a source-controlled user-systemd unit for the durable toomux remote listener.

### Security

- Keep ByteTraverse enrollment and toomux application authority independent; the `toomux` ByteTraverse capability exposes only the explicit remote API allowlist and cannot inherit any other authority on the host.
- Scrub QR authority from browser history before the WASM application loads, and wrap the browser bearer with AES-GCM under a non-extractable WebCrypto key before IndexedDB storage.
- Bind the site transport bundle to a committed, qualified ByteTraverse source SHA with a restrictive CSP and exact import-map hash.

### Reliability

- Reject QR replay without minting another device, restore paired browsers across reloads without a new QR, and surface both self-revocation and workstation-side revocation immediately.
- Add scan/geometry regression coverage plus end-to-end validation of pairing, TUI input, memory access, reconnect and revocation.

## [0.2.3] - 2026-10-02

### Fixed

- Use Claude Code's first-class `claude auth login` flow for account sign-in instead of launching the full interactive client and injecting `/login`. New account folders can stop at Claude's first-run theme screen, which meant the old prompt injector never ran and no browser opened.

## [0.2.2] - 2026-10-01

### Fixed

- Preserve unsent Claude prompt text into a completed handover brief before clearing the old input and continuing in a fresh session, instead of retrying indefinitely with `something is typed in its prompt`.
- Detect the newest visible prompt rather than an older submitted prompt still in scrollback, preserve multi-line drafts, and refuse to clear input if it changes between inspection and handover.

## [0.2.1] - 2026-10-01

### Added

- Add an `alt-a` Accounts page to the TUI for account status, login, sharing, unsharing and removal.
- Add an in-TUI account wizard for creating or adopting a Claude Code account, choosing its shared-history group and optionally signing in immediately.
- Add a Windows npm entry point that runs the matching toomux release inside WSL 2.
- Publish a persistently signed Android APK with tagged GitHub releases.

### Changed

- Route CLI and TUI account mutations through one account operation layer so both surfaces use the same path safety, busy-session guards and data-preservation semantics.
- Keep the add-account wizard card stable across steps, wrap long paths and tighten its copy and spacing.
- Allow `install.sh` to pin an exact release with `TOOMUX_VERSION`, used by the Windows/WSL 2 npm launcher.

## [0.2.0] - 2026-10-01

### Added

- Add the native Android toomux client over ByteTraverse, keeping the real host-side toomux TUI and session model rather than introducing a separate mobile dashboard.
- Add the ByteTraverse-bound remote API with device pairing, per-device bearer credentials and Android Keystore-backed token storage.
- Add touch-native terminal controls and hardware-keyboard parity for the Android client.

### Changed

- Move the Android build to Gradle 9.8 and Android Gradle Plugin 9.4.1 with AGP 9 built-in Kotlin support.
- Update the public dependency and release-action set while preserving the Rust 1.88 minimum supported version.

### Fixed

- Preserve usage high-water marks within the same reset window instead of allowing temporary lower readings to move usage backwards.
- Keep toomux jobs and native Claude background work visible as active after a foreground turn settles, preventing live sessions from falling back to plain idle.
- Correct Android TUI cell alignment, font selection, touch interactions and hardware-meta handling.

### Security

- Harden account-root validation and recursive account deletion against broad, protected, symlink-disguised and unrelated populated paths.
- Move public pull-request CI entirely to GitHub-hosted runners.
- Add dependency advisory/license/source policy and release provenance/SBOM attestations.

### Reliability

- Serialise shared state, job metadata and reboot-snapshot read/modify/write transactions.
- Add adversarial tests for corrupt state, concurrent writers and stale PID records.

## [0.1.0] - 2026-10-01

Initial public release: tmux session control, handovers, shared memory, multi-account usage/switching, transcript archives, background jobs and voyages.

[Unreleased]: https://github.com/meshbergio/toomux/compare/v0.4.4...HEAD
[0.4.4]: https://github.com/meshbergio/toomux/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/meshbergio/toomux/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/meshbergio/toomux/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/meshbergio/toomux/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/meshbergio/toomux/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/meshbergio/toomux/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/meshbergio/toomux/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/meshbergio/toomux/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/meshbergio/toomux/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/meshbergio/toomux/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/meshbergio/toomux/releases/tag/v0.1.0
