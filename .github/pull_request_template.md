## What changed

<!-- The problem this solves, and the invariant the change establishes. Link the issue if there is one. -->

## Validation

<!-- Commands and tests run, including platform-specific checks where relevant. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo build --locked`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] `cargo test --locked`
- [ ] `actionlint .github/workflows/*.yml` (if workflows changed)
- [ ] `cargo audit --deny warnings` and `cargo deny check` (if dependencies changed)
- [ ] No credentials, real transcripts or private machine paths in code, fixtures or screenshots
- [ ] CHANGELOG.md and docs updated if user-visible behaviour changed

## Risk / compatibility

<!-- Config, filesystem, process, credential, release or migration implications. -->
