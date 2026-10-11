# Getting help

- **Questions, ideas and show-and-tell:** start a thread in
  [Discussions](https://github.com/meshbergio/toomux/discussions).
- **Something broken:** open a
  [bug report](https://github.com/meshbergio/toomux/issues/new?template=bug_report.yml).
  Search [existing issues](https://github.com/meshbergio/toomux/issues?q=is%3Aissue)
  first.
- **A focused improvement:** open a
  [feature request](https://github.com/meshbergio/toomux/issues/new?template=feature_request.yml).
- **A security problem:** don't post it publicly. Report it privately as
  described in [SECURITY.md](SECURITY.md).

Before asking, [GUIDE.md](GUIDE.md) covers setup, every command and
troubleshooting.

## What to include

```sh
toomux --version
claude --version
tmux -V
uname -a        # or your macOS version / WSL distribution
toomux where    # where toomux reads and writes, and what setup changed
```

Also say which terminal you use. If something went wrong in the background,
look in the state folder `toomux where` shows: `errors.log` has what went
wrong on screen, `handovers/log.jsonl` each handover step, and
`limits/log.jsonl` what happened around usage limits.

Logs and transcripts can hold your prompts, file contents, folder paths and
account names. Read what you paste and trim it to the lines that matter.

toomux is maintained in spare time. Replies may take a few days.
