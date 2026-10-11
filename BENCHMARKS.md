# Benchmarks

toomux publishes two kinds of numbers, measured in different ways.

## Token use and cost

The README's headline comparison (33,026 Claude Code calls before toomux and
33,026 with it) is a measurement of one person's real workload, not a
synthetic benchmark. [docs/EVIDENCE.md](docs/EVIDENCE.md) has the full
results, how the two periods were chosen, what the comparison does and
doesn't show, and a script that recomputes both sides from Claude Code's own
transcripts. To measure your own workload, follow "Measure your own" on that
page.

## toomux's own footprint

`scripts/bench-resource.sh` measures what the toomux TUI itself costs to run,
so regressions can be compared on the same machine:

    scripts/bench-resource.sh

It builds the locked release profile, starts the TUI in a tmux server of its
own, samples resident memory and process CPU once a second for ten seconds,
and reports the release binary's size. The script kills its tmux server when
it finishes. It points `TOOMUX_HOME` at a temporary folder, so toomux's own
configuration and state are left alone; the TUI still sees the Claude Code
sessions running on the machine.

No reference results are published yet. When you publish one, record:

- the toomux commit;
- OS and kernel or macOS version;
- CPU architecture;
- tmux and Rust versions;
- terminal size;
- how many tmux servers and Claude sessions toomux could see.
