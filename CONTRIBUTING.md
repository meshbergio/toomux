# Contributing

Thanks for improving toomux. Changes are easiest to review when they preserve
the project's core properties: one native binary, no daemon/proxy, explicit
ownership of user state, and behavior that is recoverable when a process or
machine disappears.

New here? Issues labelled
[good first issue](https://github.com/meshbergio/toomux/labels/good%20first%20issue)
are a good start, and questions are welcome in
[Discussions](https://github.com/meshbergio/toomux/discussions). For anything
larger than a fix, open an issue or discussion first, so we can agree on the
shape before you spend time on it.

## Getting started

You need Rust 1.88 or newer, tmux 3.2 or newer and Claude Code.

    git clone https://github.com/meshbergio/toomux
    cd toomux
    cargo build
    cargo test

To try a change, run the debug build with its own config and state:

    TOOMUX_HOME=$(mktemp -d) cargo run -- list
    TOOMUX_HOME=$(mktemp -d) cargo run -- where

`TOOMUX_HOME` keeps toomux's config, state and archive in one throwaway
folder. It doesn't hide anything else: the build still sees your real Claude
Code sessions and tmux servers, and it shares the runtime folder
(`$XDG_RUNTIME_DIR/toomux`) with any installed toomux. Commands that change
Claude Code or tmux settings (`init --apply`, `account add`, `uninstall`)
change them for real, so try those under a separate user or in a VM.

## Architecture

One binary (`src/main.rs` is the CLI; `src/lib.rs` exports the modules). No
daemon: work happens in the command you run, in the tmux status tick (`toomux
status`, which starts slower work in the background), and in Claude Code hooks
(`toomux hook ...`). Each module starts with a `//!` comment saying what it
owns. The main ones:

| Area | Modules |
| --- | --- |
| Sessions | `registry` (Claude Code's live session files), `running` (which conversation each process holds), `platform` (process facts per OS), `tmux`, `control` and `live` (panes and tmux control mode), `actions` (start, resume, move and relaunch Claude), `snapshot` (bring sessions back after a restart) |
| Handover and limits | `handover` (+ `handover/successor.rs`), `context_policy`, `limits` (carry on after a usage limit resets), `usage`, `queue` (moves that wait for a session to go idle) |
| Voyages | `voyage` (+ `voyage/evaluate.rs`, `voyage/lineage.rs`), `scene` |
| Memory | `memory`, `index`, `graph`, `mcp` (memory tools over MCP), `redact`, `capture` (long command output kept whole), `archive`, `upkeep`, `hygiene` |
| Accounts | `accounts`, `account_ops`, `account_cli`, `credentials` |
| Screens | `ui` (+ `ui/render.rs`, `ui/shell.rs`, `ui/memory.rs`), `watch` (notices) |
| Reports | `tokens`, `price`, `transcript` |
| Setup and storage | `setup` (hooks, status line, MCP registration), `install` (self-contained bundles), `config`, `paths`, `state`, `lock`, `places` (frecent folders), `jobs` (background commands) |
| Optional | `remote` (phone and Android remote) |

## Development

Before opening a pull request, run:

    cargo fmt --all -- --check
    cargo build --locked
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
    actionlint .github/workflows/*.yml

For dependency/security changes, also run:

    cargo audit --deny warnings
    cargo deny check

For performance-sensitive changes, run `scripts/bench-resource.sh` and include
the environment details described in `BENCHMARKS.md`.

CI separately proves the declared Rust 1.88 MSRV. macOS CI exercises the
libproc/sysctl and Keychain paths with a real, pinned Claude Code fixture.

## Tests and invariants

Prefer tests that state the invariant in their name and exercise the real
boundary. In particular:

- filesystem changes should cover interruption, symlinks and protected paths;
- persistent state must tolerate malformed/truncated files;
- read/modify/write state shared by several toomux processes must be
  serialised or otherwise demonstrably race-safe;
- PID-based state must also bind to process start identity where reuse matters;
- changes that preserve or move user data should prove that unrelated data is
  left untouched.

Tests that change process-wide environment variables must use
crate::TEST_ENV to avoid racing other tests.

## Pull requests

Keep behavior changes, mechanical formatting and generated assets separable
when practical. Explain:

1. the user-visible or engineering problem;
2. the invariant the change establishes;
3. the validation performed;
4. any compatibility, migration or security implications.

Do not include credentials, real transcripts or private machine paths in test
fixtures, screenshots, issues or commits.

Public pull requests execute only on GitHub-hosted runners. Never add a public
PR workflow that runs untrusted code on a private self-hosted runner.
