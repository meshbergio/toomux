# Contributing

Thanks for improving toomux. Changes are easiest to review when they preserve
the project's core properties: one native binary, no daemon/proxy, explicit
ownership of user state, and behavior that is recoverable when a process or
machine disappears.

## Development

toomux requires Rust 1.88 or newer and tmux 3.2 or newer.

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
