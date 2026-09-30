# Changelog

All notable user-visible changes are recorded here. toomux follows semantic
versioning once a compatibility promise is established; the 0.x series may
still change configuration and command details while they settle.

## Unreleased

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
