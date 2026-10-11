<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/logo/svg/toomux-logo-reversed.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/brand/logo/svg/toomux-logo-primary.svg">
    <img src="assets/brand/logo/svg/toomux-logo-primary.svg" alt="toomux" width="340">
  </picture>
</p>

<h3 align="center">Claude Code, minus the babysitting.</h3>

<p align="center">
  Every Claude Code session you run, on one screen in tmux.<br>
  Long conversations hand over to a fresh session <em>before</em> context gets slow and expensive,<br>
  and nothing stops just because you looked away.
</p>

<p align="center">
  <a href="https://github.com/meshbergio/toomux/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/meshbergio/toomux/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/meshbergio/toomux/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/meshbergio/toomux?color=4fd1b5&label=release"></a>
  <img alt="One Rust binary" src="https://img.shields.io/badge/one%20binary-Rust%201.88%2B-e8a33d">
  <img alt="Linux, macOS, WSL 2" src="https://img.shields.io/badge/linux%20%C2%B7%20macOS%20%C2%B7%20WSL%202-supported-7c8590">
  <a href="#license"><img alt="MIT or Apache 2.0" src="https://img.shields.io/badge/license-MIT%20%2F%20Apache%202.0-7c8590"></a>
</p>

<p align="center">
  <a href="#quick-start"><b>Quick start</b></a> ·
  <a href="#handover-instead-of-compaction">Handover</a> ·
  <a href="#keep-work-moving">Limits &amp; restarts</a> ·
  <a href="#voyages">Voyages</a> ·
  <a href="#memory-that-survives-the-session">Memory</a> ·
  <a href="#faq">FAQ</a> ·
  <a href="GUIDE.md">Guide</a>
</p>

<p align="center">
  <img src="assets/handover.gif" alt="A Claude Code session passes 250k tokens of context. At the end of its turn toomux asks it to write a handover brief; it writes the goal, what is done, what is still running and the next steps. A fresh session starts in the same pane, reads the brief, and carries on with the next step at about 20k tokens of context." width="100%">
</p>

<p align="center"><sub>A real handover, start to finish: the session writes its own brief, and a fresh one picks the work up in the same pane.</sub></p>

<picture>
  <source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="assets/readme/headline-mobile-dark.svg">
  <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="assets/readme/headline-mobile-light.svg">
  <source media="(prefers-color-scheme: dark)" srcset="assets/readme/headline-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/readme/headline-light.svg">
  <img src="assets/readme/headline-light.svg" alt="Measured on a matched 33,026-call workload: cost per 1,000 calls down 54% ($131.59 to $60.09), context per call down 52% (309,783 to 147,188 tokens), calls above 400k tokens down from 27.84% to 0.15%, output tokens per dollar up 82%" width="100%">
</picture>

<p align="center"><sub>
  33,026 real calls before toomux against 33,026 with it, same workload, both priced at the same rates.
  <a href="docs/EVIDENCE.md">How it was measured</a> · <a href="#measure-your-own">measure your own</a>
</sub></p>

## Quick start

```sh
curl -fsSL https://toomux.com/install | sh   # one binary, its own tmux; checksums verified
toomux init --apply                          # tmux keys, Claude Code hooks, status line (backed up)
toomux                                       # every session, one screen
```

That's it. Start Claude Code sessions as you always have, or press `alt-n` in toomux.
Handover, restart recovery and recovery after usage limits are on by default.
Homebrew, npm, upgrades and uninstalling are [below](#install).

> [!TIP]
> Already have sessions running in plain terminal tabs? toomux lists them too, and
> `ctrl-o` brings one into tmux with its conversation intact.

## Why this exists

I run about ten Claude Code sessions at once. At any moment one is coding, one is
waiting for me, one has hit a usage limit, one finished an hour ago and I never
noticed, and one is dragging 600k tokens of context behind it.

That last one is the expensive one. Every call re-reads the whole conversation:
at 800k tokens that's $0.16 a call from cache, and $6.40 if the cache has gone cold
and the conversation has to be written back into a 1-hour cache (Opus 5.5 list
prices). On a Max plan you pay in limits instead. And the model gets worse at
using what's in front of it as context grows;
[Anthropic say so themselves](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents).

tmux kept the terminals alive. It didn't coordinate the work. toomux does, and
everything in it came from running this way for real, every day.

## Every session, one screen

`toomux` (or `alt-s` from anywhere in tmux) shows every Claude Code session on the
machine, grouped by what needs you: waiting on you, working, running in the
background, just finished, idle. The selected session runs live beside the list, so
you can type into it without leaving.

<p align="center">
  <img src="assets/hero.png" alt="toomux full screen: sessions grouped by attention state (needs you, working, background, finished, idle) on the left, the selected Claude Code session live on the right, and each account's 5-hour and weekly usage across the top" width="100%">
</p>

| Key | Does |
|---|---|
| `enter` | open the session live (reopens it if it had exited) |
| `alt-n` · `ctrl-n` | start a new session in a recent folder (`alt-n` works even while you type in a session) |
| `ctrl-a` | move the conversation to another account, same pane |
| `ctrl-o` | bring a session from a plain terminal tab into tmux |
| `ctrl-p` · `alt-1..9` | pin a session, then jump to it from anywhere |
| `ctrl-u` | each account's 5-hour and weekly usage |
| `tab` | group by attention, account or project |
| `alt-b` | the same list as a sidebar in your normal tmux layout |
| `alt-?` | every key |

Every session toomux starts or reopens gets a tmux server of its own, so one
crashing never takes the others with it. toomux sits above Claude Code; it doesn't replace it, wrap it or
proxy its traffic.

<p align="center">
  <img src="assets/sidebar.png" alt="The toomux session list as a sidebar beside an ordinary tmux window" width="100%">
</p>

## Handover instead of compaction

Compaction squeezes a long conversation into a summary you never see, at a moment
you didn't choose. toomux does the opposite.

1. **At a natural break.** Once a session passes **250k tokens**, toomux waits for
   the end of a turn. Nothing is cut off mid-edit. **400k** is the hard limit.
2. **The session writes its own brief.** Goal, decisions and why, what's done, what's
   running, what was tried and failed, exact paths and commands, next steps. It's a
   file you can read, at `~/.local/state/toomux/handovers/`.
3. **A fresh session picks it up in the same pane,** with the brief as its starting
   point. Subagents and background commands carry over. The old conversation is kept
   whole and searchable; nothing is summarised away.

Thresholds are per model, provider, account or session: a 32k local model can hand
over far earlier than a 1M Opus session. See [context policies](GUIDE.md#context-policies).

<picture>
  <source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="assets/evidence/matched-cohort-mobile-dark.svg">
  <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="assets/evidence/matched-cohort-mobile-light.svg">
  <source media="(prefers-color-scheme: dark)" srcset="assets/evidence/matched-cohort-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/evidence/matched-cohort-light.svg">
  <img src="assets/evidence/matched-cohort-light.svg" alt="Matched comparison of exactly 33,026 Claude calls before and with toomux: context per call down 52.5%, normalized cost per 1,000 calls down 54.3%, calls above 400k down from 27.84% to 0.154%, output tokens per dollar up 81.8%" width="100%">
</picture>

> [!NOTE]
> **Matched, not scaled.** Exactly 33,026 calls on each side, main sessions and every
> subagent, both repriced at the same Opus 5.5 list rates so a change of model gets no
> credit. One person's real workload, not a controlled benchmark of every Claude Code
> user. [The full method and its limits](docs/EVIDENCE.md).

<picture>
  <source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="assets/evidence/context-mechanism-mobile-dark.svg">
  <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="assets/evidence/context-mechanism-mobile-light.svg">
  <source media="(prefers-color-scheme: dark)" srcset="assets/evidence/context-mechanism-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/evidence/context-mechanism-light.svg">
  <img src="assets/evidence/context-mechanism-light.svg" alt="Where the difference came from: fresh input stayed nearly flat while cache reads fell 51.8% and cache rewrites after expiry fell 77 to 91%" width="100%">
</picture>

Fresh input barely moved; old context did. That's where the mechanism says the change
should show up, and it's where it did.

<details>
<summary><strong>The workload behind these numbers</strong></summary>

<picture>
  <source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="assets/readme/proof-mobile-dark.svg">
  <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="assets/readme/proof-mobile-light.svg">
  <source media="(prefers-color-scheme: dark)" srcset="assets/readme/proof-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/readme/proof-light.svg">
  <img src="assets/readme/proof-light.svg" alt="205 days of real use: 196.0 billion tokens, a $141.7 thousand API list-price equivalent" width="100%">
</picture>

205 days of daily use: everything toomux does was built against this workload first.
The list-price figure is a translation into API prices, not an invoice. Public usage
profiles: [CCGather](https://ccgather.com/@meshbergio) ·
[VibeRank](https://www.viberank.app/profile/meshbergio).

</details>

### Measure your own

```sh
toomux tokens --since 7d
```

Where your tokens went, from your own transcripts: fresh input, cache reads, cache
writes and *why* they happened (a cold start, a cache that expired while you were
away, normal growth), by session and subagent, priced at list rates. What toomux
estimates it avoided is reported separately and never added in.

<details>
<summary><strong>See a full token report</strong></summary>

<p align="center">
  <img src="assets/tokens.png" alt="The toomux seven-day token report: observed list-price usage, estimated avoided usage, model and subagent breakdowns, cache-write reasons and old command-output composition" width="100%">
</p>

</details>

## Keep work moving

The unit of work should outlive the terminal, the conversation and the account that
happened to start it.

**Usage limits.** toomux tracks each account's 5-hour and weekly windows, reset times
and pace. When a limit cuts a session off mid-work, it carries on by itself once the
limit resets, even overnight with no terminal attached. It closes Claude Code's limit
dialog for you and leaves Claude's own "continue automatically" countdown alone.
Or, if you have more than one account, `ctrl-a` moves the same conversation to
another one in the same pane.

<picture>
  <source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="assets/readme/account-capacity-mobile-dark.svg">
  <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="assets/readme/account-capacity-mobile-light.svg">
  <source media="(prefers-color-scheme: dark)" srcset="assets/readme/account-capacity-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/readme/account-capacity-light.svg">
  <img src="assets/readme/account-capacity-light.svg" alt="How toomux treats account capacity: five-hour and weekly windows, reset time and pace inform whether a conversation stays on its account or moves, without changing the conversation or pane" width="100%">
</picture>

**Restarts.** After a reboot, the sessions that were running reopen by themselves,
each in its own tmux server. A conversation that is still running somewhere is never
opened a second time; toomux says where it is instead.

**Background work.** Background shells and subagents count as work in progress:
the list shows them, and a handover carries them over instead of waiting.

## Voyages

Sometimes you don't want to manage the next turn. You want to describe the outcome
and come back when it's actually true.

```text
/voyage the auth tests pass --check "cargo test auth" --budget 40
```

After each turn a separate judge asks whether the outcome is met, and if not, sends
the session back with what's still missing. `--check` makes "done" a command that has
to pass. A voyage follows its conversation across handovers, waits out usage limits,
and doesn't count waiting on background work as stalling.

<p align="center">
  <img src="assets/voyage.gif" alt="A voyage progressing through work, waiting at a usage limit, running a proof lap and landing only after the outcome is accepted" width="100%">
</p>

How hard "done" is to reach is up to you: **light** · **steady** (default) · **hard**
adds a proof lap · **relentless** adds two and a skeptical review of the whole diff.
A proof lap sends the session back to try to *falsify* "done": re-run the evidence,
reread its own changes, look for the skipped test or the TODO.

<details>
<summary><strong>See a proof lap catch a false "done"</strong></summary>

<p align="center">
  <img src="assets/proof-lap.png" alt="A relentless voyage proof lap finding a TODO and an ignored test after the session had claimed the parser work was complete, then going back to fix the missed case" width="100%">
</p>

</details>

## Memory that survives the session

The answer doesn't have to be in the current conversation. Every conversation,
handover brief and project memory file is indexed as it happens, and every session
can search all of it through toomux's MCP tool. A session in one project can find the
decision another session made in another project last week, and reuse it.

<p align="center">
  <img src="assets/memory.png" alt="A billing-service session searching toomux memory for how atlas-api implemented rate limiting earlier, finding the decision and reusing it" width="100%">
</p>

Before leaving, a handing-over session updates the project's durable memory. Old
transcripts are archived compressed and kept past Claude Code's default cleanup, so a
complete old turn can still be recovered. `alt-m` shows it all as a graph: which
session wrote which brief, who continued from it, what each one read.

<p align="center">
  <img src="assets/graph-chain.png" alt="One body of work continued across 22 sessions and 22 linked handover briefs in the memory graph" width="100%">
</p>

## FAQ

<details>
<summary><strong>How is this different from just using tmux?</strong></summary>

tmux keeps terminals alive. It doesn't know a pane is a Claude Code conversation,
that it's at 300k tokens, waiting on you, or stopped by a usage limit that reset an
hour ago. toomux reads Claude Code's own session records and transcripts to know
that, and acts on it. You keep your tmux setup; toomux adds one layer above it.

</details>

<details>
<summary><strong>Claude Code already compacts and can continue after a limit. Why toomux?</strong></summary>

Compaction summarises the conversation, in place, when it's nearly full; you don't
choose what's kept. A toomux handover happens far earlier, at a turn boundary, from a
brief the session writes itself, and the full original stays searchable. The
[evidence](docs/EVIDENCE.md) is about exactly that difference.

Claude Code's own "continue automatically" only lasts while that one session stays
open and waits. toomux doesn't interfere with it, and it covers the cases it doesn't:
a session that chose to stop at the limit, a reset that comes overnight, a session
that was restarted, a voyage or handover that was under way.

</details>

<details>
<summary><strong>Does anything leave my machine?</strong></summary>

Nothing of yours goes anywhere new. toomux has no telemetry and no server of its
own; it reads and writes local files, and Claude Code talks to Anthropic exactly as it
always does. The one request toomux makes itself is to ask Anthropic for each
account's usage limits, with that account's own sign-in, so it can show them. Remote
access is off unless you set it up, and goes over your own
[ByteTraverse](#the-same-toomux-remotely) connection.

</details>

<details>
<summary><strong>What does it change on my machine?</strong></summary>

`toomux init --apply` adds tmux key bindings and a status segment (between marked
lines in `~/.tmux.conf`), three Claude Code hooks, a status line and an MCP server in
each account's settings, and a `/voyage` command, keeping backups of every file it
edits. `toomux where` lists every place toomux reads or writes. `toomux uninstall`
takes it all back; `--purge` also deletes toomux's own state.

</details>

<details>
<summary><strong>I use more than one Claude account. Does it handle that?</strong></summary>

Yes: work and personal, or one per client. `toomux account setup` adds each account
you've signed into with Claude Code. Every account's limits show side by side,
history is shared across them, and a conversation can move from one to another
without losing anything. toomux never creates accounts or shares credentials between
them; how you use your accounts is between you and
[Anthropic's terms](https://www.anthropic.com/legal/consumer-terms).

</details>

<details>
<summary><strong>Which Claude Code versions does it work with?</strong></summary>

It's tested against current Claude Code releases (2.1.x) on Linux and macOS, and on
Windows through WSL 2. toomux relies on Claude Code's hooks, session records and
transcripts, and on a handful of on-screen strings (the usage-limit dialog, first-run
screens) that are kept together in the source, so a Claude Code wording change is a
small, obvious fix.

</details>

<details>
<summary><strong>Is it safe to let it type into my sessions?</strong></summary>

toomux only types into a prompt it has just read back as empty, checks afterwards
that its text was sent, and gives up visibly (with a logged reason) rather than
guessing. It never types over your unsent draft: if a handover is due while you're
mid-sentence, it waits and tells you so; if it has to go ahead, your draft is saved
into the brief so the fresh session has it.

</details>

## The same toomux, remotely

Pair a phone, tablet or laptop over [ByteTraverse](https://toomux.com/remote/) and open
the full toomux TUI in a browser at [toomux.com/remote](https://toomux.com/remote/), or
in the native Android app. The workstation stays authoritative; each paired device
gets its own isolated `toomux shell` on the host. It's opt-in, and local use never
needs it.

<details>
<summary><strong>How remote access works</strong></summary>

<p align="center">
  <img src="assets/readme/android-native-qualified.png" alt="The native Android toomux client rendering the same host-side session system over ByteTraverse" width="100%">
</p>

For each paired device, the host starts an isolated `toomux shell`, captures its cell
grid, and the client renders it and sends input back. The public site is the
UI/bootstrap only; it is not a relay for session data. Browser and Android both need
a ByteTraverse connection to the host, and toomux adds its own per-device
authentication on top.

```sh
toomux remote serve
toomux remote phone   # browser: scan the one-use pairing code at toomux.com/remote
toomux remote pair    # Android
```

The exact boundaries: [web remote design](docs/MOBILE-WEB-REMOTE.md) ·
[Android guide](android/README.md).

<picture>
  <source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="assets/readme/architecture-mobile-dark.svg">
  <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="assets/readme/architecture-mobile-light.svg">
  <source media="(prefers-color-scheme: dark)" srcset="assets/readme/architecture-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/readme/architecture-light.svg">
  <img src="assets/readme/architecture-light.svg" alt="toomux product model: one host-side coordination system used locally through the full TUI, sidebar, normal tmux and automation, or remotely through ByteTraverse from a browser or the Android app" width="100%">
</picture>

</details>

## Install

**Self-contained** (Linux, macOS, Windows through WSL 2), the canonical install:

```sh
curl -fsSL https://toomux.com/install | sh
```

It verifies the bundle's checksum and every hash in its manifest, then installs an
immutable version under `~/.local/lib/toomux` with its own tested tmux and terminfo.
If you already have tmux it's left alone. Upgrades install beside the old version and
switch atomically; `toomux rollback` goes back. It doesn't install Claude Code, touch
your shell config or enable remote access.

**Homebrew** (macOS), which also installs tmux:

```sh
brew install meshbergio/tap/toomux
```

**npm** (Linux, macOS, or Windows with WSL 2), which needs tmux 3.2+ already installed:

```sh
npm install -g toomux
```

Then:

```sh
toomux init --apply      # tmux keys, Claude Code hooks, status line, MCP server
toomux account setup     # only if you use more than one Claude account
toomux                   # or alt-s from anywhere in tmux
```

Defaults are meant to be right out of the box: handover, restart recovery, recovery
after limits, transcript archiving and memory upkeep are all on. Every setting is in
the [Guide](GUIDE.md#setup).

**Upgrade:** rerun the installer, `brew upgrade toomux`, or
`npm install -g toomux@latest`.
**Uninstall:** `toomux uninstall` (`--dry-run` to see what it would remove first,
`--purge` to also delete toomux's own state and archive; your Claude Code memory is
never touched).

---

<p align="center">
  <a href="GUIDE.md">Guide</a> ·
  <a href="docs/EVIDENCE.md">Evidence</a> ·
  <a href="SECURITY.md">Security</a> ·
  <a href="BENCHMARKS.md">Benchmarks</a> ·
  <a href="CHANGELOG.md">Changelog</a> ·
  <a href="CONTRIBUTING.md">Contributing</a>
</p>

<p align="center" id="license">
  <sub>MIT or Apache 2.0, your pick: <a href="LICENSE-MIT">MIT</a> · <a href="LICENSE-APACHE">Apache 2.0</a>.</sub>
</p>
