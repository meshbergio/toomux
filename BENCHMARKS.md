# Performance measurements

Performance numbers in the README and guide are observations from real toomux
use, not synthetic claims. This file makes the low-level resource measurement
repeatable so regressions can be compared on the same machine.

Run:

    scripts/bench-resource.sh

The script builds the locked release profile, launches an isolated toomux TUI
inside its own tmux server with a temporary TOOMUX_HOME, samples resident
memory and process CPU for ten seconds, and reports the release binary size.
It leaves the user's normal tmux servers and toomux state untouched.

Record the following with any published result:

- toomux commit;
- OS and kernel/macOS version;
- CPU architecture;
- tmux and Rust versions;
- terminal dimensions;
- whether sessions were present in the benchmark fixture.

The script's empty-fixture result is a baseline, not a substitute for the
README's multi-session measurements. For a session-loaded comparison, run the
same sampling method against a normal toomux instance and state the number of
visible/discovered tmux servers and sessions.
