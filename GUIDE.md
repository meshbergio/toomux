# The toomux guide

Everything toomux does, in detail. The short version is in the [README](README.md).
toomux runs on Linux and macOS, and on Windows inside WSL 2.

`alt-s` from anywhere in tmux turns the whole terminal into toomux: every
session, what it's working on, what it's doing right now, which account it's on,
and the chosen one live beside the list. Type to filter, `enter` to open it,
`alt-j` to go there in tmux.

```
 needs you  2 ─────────────────────────────────────────────────────────
  ◆ Cloud cost review                                     home · 2d
    hit weekly limit · resets Oct 3, 5pm · ~/Desktop · infra#542
 working  3 ───────────────────────────────────────────────────────────
  ● Platform  Terraform module handover                  office · 7h
    working · 6m · work/platform
1 ● Engine                                                office · 3h
    working · 25m · code/engine · engine#723 · outside tmux
```

Sessions are titled by what they're about: a name you gave them, else Claude
Code's own title for the conversation, else the last prompt. A named session
also shows Claude's current topic beside the name.

## Contents

- [Install and first run](#install-and-first-run)
- [Full screen](#full-screen)
- [Keys](#keys)
- [States](#states)
- [Handover](#handover)
  - [Context policies: different limits per model or account](#context-policies-different-limits-per-model-or-account)
- [Long output and memory](#long-output-and-memory)
- [Voyages](#voyages)
- [The memory graph](#the-memory-graph)
- [Usage and limits](#usage-and-limits)
- [Accounts](#accounts)
- [Moving a session between accounts](#moving-a-session-between-accounts)
- [New sessions](#new-sessions)
- [Folder trust and first-run screens](#folder-trust-and-first-run-screens)
- [Sidebar, pins and notices](#sidebar-pins-and-notices)
- [A tmux server per session](#a-tmux-server-per-session)
- [After a restart](#after-a-restart)
- [Upkeep: nothing lost, nothing stale](#upkeep-nothing-lost-nothing-stale)
- [What toomux does by itself](#what-toomux-does-by-itself)
- [Configuration](#configuration)
- [CLI](#cli)
- [Troubleshooting](#troubleshooting)
- [Colours](#colours)
- [How it works](#how-it-works)
- [Remote access](#remote-access)
- [Licence](#licence)

## Install and first run

You need Linux or macOS (on Windows, WSL 2) and Claude Code. The installer
brings its own tmux, so a clean machine doesn't need Homebrew, npm, Rust or
tmux first.

```sh
curl -fsSL https://toomux.com/install | sh
toomux init --apply      # set up tmux and Claude Code; lists what toomux does by itself
toomux account setup     # optional: name your accounts, choose which share history
toomux                   # open it
```

Then start Claude Code sessions as you always have, or press `alt-n` in toomux.
Sessions running in plain terminal tabs are listed too; `ctrl-o` brings one
into tmux with its conversation intact.

**On macOS, turn on Option as Meta first.** Every global toomux key is `alt-*`,
and macOS terminals type symbols with Option instead. See
[Troubleshooting](#keys-dont-work-on-macos).

### What the installer does

It downloads a bundle with toomux, tmux 3.4 and tmux's terminfo, checks the
bundle's SHA-256 checksum and the hashes recorded inside it, and unpacks it
under `~/.local/lib/toomux/versions/<version>`. `~/.local/bin/toomux` points at
`current`.

- A tmux you already have is left alone. Outside tmux, toomux starts its own
  servers with the bundled tmux; inside an existing tmux server it uses that
  server's own tmux, so the two always speak the same protocol.
- Running the installer again puts the new bundle beside the old one, switches
  `current` in one step and keeps the old one as `previous`. `toomux rollback`
  swaps them back.
- `toomux uninstall` removes the bundle and the tmux launcher the installer
  added (see [What toomux does by itself](#taking-it-back)).

Other ways in:

- **Homebrew (macOS):** `brew install meshbergio/tap/toomux`. Installs tmux as a
  dependency.
- **npm:** `npm install -g toomux`. Installs the binary, not tmux.
- **From source:** `cargo install --git https://github.com/meshbergio/toomux`
  (Rust 1.88 or later), or `cargo install --path .` in a clone.

Without the installer you need tmux 3.2 or later; `toomux init` checks.

On Windows, toomux runs inside WSL 2, the same as tmux. It sees the Claude Code
you run in WSL, with its own `~/.claude`, not one installed on the Windows side.

### What `init --apply` changes

- **tmux:** a block in `~/.tmux.conf` between `# >>> toomux >>>` markers (backup
  at `~/.tmux.conf.pre-toomux`): `alt-s` and `prefix space` open toomux, `alt-b`
  toggles the sidebar, `alt-1..9` jump to pins, and the status bar refreshes every
  2 seconds with toomux's segment in it.
- **Each account's Claude Code `settings.json`** (backups beside it):
  - the status line becomes `toomux statusline`, refreshed every second (a status
    line of your own is left alone);
  - `CLAUDE_CODE_TMUX_TRUECOLOR` is set, without which Claude Code keeps to 256
    colours inside tmux;
  - three hooks: `toomux hook pre-tool` (PreToolUse, every tool), `toomux hook
    stop` (Stop) and `toomux hook prompt` (UserPromptSubmit).
- **Each account's `commands/voyage.md`:** the `/voyage` command.
- **The `toomux` MCP server**, registered at user scope: memory tools for every
  session.
- **The tmux server's environment:** Claude Code's runtime variables
  (`CLAUDE_CODE_CHILD_SESSION` and others) are cleared. They leak in when tmux is
  started from inside a Claude session, and every new pane would inherit them.

Adding an account later with `toomux account add` does the same for that account.

On macOS, Claude Code keeps each account's sign-in in the login Keychain, not in
a file. toomux reads it the way Claude Code does (through `security`), only to
show usage. Your usual `~/.claude` account launches without `CLAUDE_CONFIG_DIR`,
so it uses the same Keychain entry as a plain `claude`; every other account has
it set.

## Full screen

`toomux`, or `alt-s` from anywhere in tmux, takes the whole terminal: the
header with usage meters, the session list, and the chosen session live and
interactive beside it. In tmux it opens over everything, status bar included,
and closes the way it came: `alt-j` leaves you in the chosen session's own tmux
window, `ctrl-c` back where you were.

| key | |
|---|---|
| `alt-s` | between the session and the list |
| `alt-j` | opened with `alt-s`: close toomux on the chosen session, in tmux itself |
| `alt-b` | hide or show the list |
| `alt-n` | a [new session](#new-sessions) in a recent folder |
| `alt-u` | usage in place of the session |
| `alt-m` | the [memory graph](#the-memory-graph) in place of the list and session |
| `alt-a` | [accounts](#accounts): sign in, add, share, unshare or remove |
| `alt-r` | [remote access](#remote-access) (coming) |
| `alt-?` | every key |
| `alt-1..9` | a pin |
| `shift`-drag | select text in your terminal, as usual |
| in the list | everything under [Keys](#keys); `enter` opens here, `esc` goes back to the session, `ctrl-c` leaves (sessions keep running) |

These work wherever the keyboard is. With the keyboard in a session everything
else goes to the session, paste included (bracketed when the program asks): so
`ctrl-n` and `?` there are Claude's, and `alt-n` and `alt-?` are toomux's. A
toomux overlay (the new-session picker, help, a question) takes the keyboard
until it closes. The wheel scrolls toomux's copy of the history, or goes to the
program if it takes the mouse.

The live session is tmux control mode (`tmux -C`): tmux streams the pane's
output over a pipe, toomux parses it (with alacritty_terminal) and draws only
the pane, and your keys go back as tmux key names, so tmux encodes them for
whatever mode the program is in. There is no second tmux screen and nothing is
polled: the loop sleeps until a key, pane output or a change in Claude Code's
session registry arrives. Pane listings of the server you're looking at come
over the same connection; each other server is asked at most once every 10
seconds, and the status line shares those answers. Measured with eight servers:
48 small `tmux list-panes` a minute, about 0.5% CPU at rest, about 9 MB of
memory. The procedure is in [BENCHMARKS.md](BENCHMARKS.md).

## Keys

In the list (`alt-?` or `?` shows these too):

| key | |
|---|---|
| type | filter by title, folder, account or state |
| `enter` / click | open the session live; reopen it if it isn't running |
| `ctrl-a` | move it to another account: same conversation, same pane |
| `ctrl-o` | bring a session running in a plain terminal tab into tmux |
| `ctrl-x` | close it |
| `ctrl-r` | rename it (in toomux, its tmux window, and Claude's `/rename`) |
| `ctrl-p` | pin it to `alt-1..9`; again to unpin |
| `ctrl-n`, `alt-n` | start a new session in a recent folder |
| `ctrl-u` | usage: each account's 5-hour and weekly limits (clears a half-typed filter first) |
| `alt-a` | accounts |
| `alt-r` | remote access (coming) |
| `ctrl-s` | clean up sessions idle for more than `stale_days` |
| `ctrl-e` | reopen what was running before a restart |
| `tab` | group by attention, account or project |
| right-click / long-press | everything you can do with a session, with its key |
| `esc` | clear the filter, leave the usage view, then quit |

Anywhere in tmux:

| key | |
|---|---|
| `alt-s`, `prefix space` | open toomux |
| `alt-b` | show or hide the sidebar |
| `alt-1..9` | jump to a pinned session, reopening it if it has exited |

Every footer hint is clickable, so is each account in the header (its usage),
and the list scrolls with the mouse wheel.

Messages in the footer fade after a few seconds. Errors also stay under `?`
(the last one) and in `~/.local/state/toomux/errors.log`. A key that can't act
says why: `busy: …` while another operation runs, or why the folder list has
nothing to start in.

## States

| | |
|---|---|
| `◆` needs you | waiting on a permission prompt or dialog |
| `◆` limit | stopped by a usage limit, with when it's back |
| `●` working | mid-turn |
| `◐` tasks running | idle at the prompt while background shells or agents keep running (grouped under "background") |
| `●` finished | went idle within the last `finished_minutes` (15) |
| `○` idle | settled work fades back |
| handing over | writing its brief, waiting to start the fresh session, or brief written and going when its turn ends |
| handover failed | with the reason in a few words, for six hours or until a retry succeeds |
| not running | a pinned conversation that has exited, with when it was last active |
| before the restart | was running before a reboot or a lost tmux server, and is offered back (see [After a restart](#after-a-restart)) |
| open twice | the same conversation is open in two processes: close one |
| running · not in Claude's session registry | a Claude process in a tmux pane that Claude Code has no record of. It's listed so you can jump to it or close it, and toomux won't open it a second time |

## Handover

A long conversation gets slow and expensive: every call re-reads the whole
context. Instead of letting Claude Code compact it (a summary that loses
detail), toomux has the session write a complete brief and continues in a fresh
session that opens with it.

Measured on 120 real sessions (72,350 API calls, exact counts via
`count_tokens`): 87.5% of cache-weighted input is the context being re-read on
every call, and calls above 400k tokens of context are 78% of it. Tool output is
about 30%, but rewriting it losslessly saves only about 0.1%, so there is no
proxy in front of your sessions. Handover does the saving instead. Simulated on
the same traffic: about 59% less weighted input. The before-and-after numbers in
the README are explained in [docs/EVIDENCE.md](docs/EVIDENCE.md).

### When a session hands over

There are two limits for a main session, and one each for subagents and forks.
All are in tokens of context, and all can be changed in the
[configuration](#configuration) or per model with a
[context policy](#context-policies-different-limits-per-model-or-account).

| setting | default | what happens past it |
|---|---|---|
| `handover_turn_end_tokens` | 250,000 | at the **end of a turn** the session is asked to write its brief. Nothing is cut off mid-task |
| `handover_tokens` | 400,000 | the hard limit: the session is stopped **mid-turn** if it must, by refusing its next tool call until the brief is written |
| `subagent_handover_tokens` | 250,000 | a subagent hands over mid-task (it has no turns to wait for) |
| `fork_context_tokens` | 200,000 | a fork is refused, and a briefed general-purpose subagent is used instead |

Past the **turn-end limit**:

- When a turn ends, the Stop hook asks the session for its brief.
- A session that has sat idle for 10 minutes is asked too. You've read the
  answer by then, and the prompt cache (an hour) is still warm for writing the
  brief.
- If you send a prompt to a conversation that went past the limit without being
  asked (it crossed it before toomux was set up, say), the prompt hook has it
  write the brief with your request as its first next step, and the fresh
  session does it.

Past the **hard limit**, the PreToolUse hook (the "gate") refuses whatever tool
the session calls next and has it write the brief now. A session idle for 45
seconds past the hard limit with an empty prompt is asked for its brief. A
suggested prompt (the dim text) counts as empty.

A conversation that started with at least three quarters of the turn-end limit
already in it is never asked: its successor would start just as big, and hand
over again at once.

`handover_turn_end_tokens = 0` hands over only at the hard limit;
`handover_tokens = 0` turns handover off.

### What happens in a handover

1. **The brief.** The session writes the goal, the current state, decisions and
   why, what failed, exact references (files, commits, ids), what is still
   running, and the next steps, to `~/.local/state/toomux/handovers/`. Before
   that it brings its workspace's Claude Code memory up to date (see
   [Upkeep](#upkeep-nothing-lost-nothing-stale)); while it hands over, the gate
   lets it touch only that memory folder and the brief.
2. **The restart.** Once the brief is written and the turn is over, the session
   continues as a fresh conversation, with the same account, flags and name,
   that opens with the brief. It happens in the same pane, or, for a session in
   a plain terminal, in a tmux server of its own.
3. **What you didn't see.** The fresh session takes the pane moments after the
   old one's last reply, often before you've read it. So toomux copies that
   reply, and every file sent since your last message, from the old transcript
   into the brief, and the successor opens by giving them to you.
4. **Work in flight.** Each of the session's subagents hands over too, and the
   brief lists them for relaunch. Background Bash commands run as toomux jobs,
   which outlive the session; the successor re-attaches with `toomux job follow
   <id>`.

**Subagents** are measured by the same gate. Past their limit they write a brief
and end with a `HANDOVER:` message, and the parent launches a fresh subagent of
the same type from it. Read-only agents put the brief in their final message. A
subagent's limit counts from what it was born with: it hands over at
`subagent_handover_tokens`, or at its starting context plus half that, whichever
is higher. A fork starts with its parent's whole context, so its successor is a
fresh general-purpose subagent rather than another fork.

**Forks from big conversations.** A fork re-reads its caller's whole context on
every call. Past `fork_context_tokens` the caller is refused the fork and asked
to brief a general-purpose subagent instead. Measured over a day of real use,
forks started past 240k cost 2.6 times what the same calls would have cost
starting fresh; below 180k they were close to even.

### When it doesn't go through

- Handovers are started by the Stop hook as well as the status tick, so no
  terminal needs to be attached.
- Something typed in the prompt is never overwritten: toomux waits, and tries
  again a minute later. A handover that has waited on the prompt for 30 minutes
  gives up and says so.
- A failed handover is tried again after 30 minutes, or sooner once a new brief
  has been written. The list shows `handover failed` and the reason.
- If the fresh session won't start, the old conversation is put back in its
  pane. After 3 fresh sessions that wouldn't start, the conversation is left to
  carry on as it is for a day.
- Each conversation hands over once, even if an old copy of it is still open
  somewhere else.
- Every start and outcome is logged in `~/.local/state/toomux/handovers/log.jsonl`.

`toomux handover <target>` hands a session over now.

### Context policies: different limits per model or account

The global settings above apply to every session. If a model has a smaller
context window, or you want an account to keep Claude Code's own compaction, add
`[[context_policy]]` rules for just those cases:

```toml
handover_turn_end_tokens = 250000
handover_tokens = 400000
subagent_handover_tokens = 250000
fork_context_tokens = 200000

[[context_policy]]
model = "local-qwen-*"
context_window_tokens = 32768
handover_turn_end_tokens = 12000
handover_tokens = 20000
subagent_handover_tokens = 12000
fork_context_tokens = 10000
compaction = "toomux"

[[context_policy]]
account = "personal"
model = "claude-*"
compaction = "native"
```

- **Matching.** A rule can match `account`, `provider`, `model` and `session`,
  with `*` and `?` wildcards. It changes only the settings it names.
- **Order.** The global settings come first, then account rules, then
  provider and model rules, then rules that combine them, then session rules.
  Among equally specific rules, the later one wins.
- **Compaction.** `compaction = "native"` leaves Claude Code's automatic
  compaction alone. `"toomux"` turns it off for that session so handover does
  the job. `"auto"` lets toomux choose. toomux only turns native compaction off
  when it knows the model's context window and the hard limit, plus a safety
  margin, fits below it; otherwise it leaves native compaction on. It does this
  with a `--settings` flag on that one launch, not by editing the account's
  files, so `/compact` still works. If a manual `/compact` brings a session back
  under its limit, a pending handover is dropped.
- **Switching models.** If a session switches to a model whose policy is much
  smaller, toomux starts a fresh session so the new limits apply.
- **Carried along.** The resolved policy follows the session through resumes,
  handovers, subagents and restarts. Non-interactive `claude -p` and SDK calls
  can't be restarted, so the gate never stops them.

`toomux context-policy --account <name> --model <id>` (add `--json` for
structured output) shows the limits that apply, the context window, who owns
compaction, and which rules matched.

## Long output and memory

### Output capping

The PreToolUse hook runs Bash commands through toomux. Output up to 6,000 bytes
comes back byte for byte. Larger output is kept whole for 7 days, and the agent
sees its head, the lines mentioning errors and warnings, its tail, and how to
read the rest: `toomux out <id> --lines a-b`, `--grep text` or `--chars a-b`.
File views (`cat`, `sed -n`, `head`, `git diff`, ...) and background commands
are never capped: agents read those to edit them. About 2.4% saved, 2% of
results touched. `capture_bash = false` turns it off.

### Memory

Every exchange of every session on the machine is indexed as it happens ("you
asked" / "outcome", per project), as are handover briefs and kept outputs. So is
every project's Claude Code memory folder (`projects/<folder>/memory/*.md`), one
entry per file, kept as the file is. Search is full-text plus fuzzy matching
(SQLite FTS5 and trigrams, combined by reciprocal rank), and entries can be
superseded.

Sessions get the MCP tools `mem_search`, `mem_get`, `mem_save`, `mem_forget` and
`output`; you get `toomux mem <words>`.

**What a session can see.** There is one memory per machine, across all
accounts. `mem_search` looks in the session's own project and global notes
first. When that finds fewer results than asked for, it fills up with matches
from other projects, marked as coming from elsewhere. `where: "all"` searches
every project from the start. `toomux mem` searches the current project and global
notes unless you add `--all`. If you keep client work on this machine that other projects'
sessions must not see, run it under a separate user, or a separate `TOOMUX_HOME`
(see [Where things are](#where-things-are)).

Claude Code loads only a session's own `MEMORY.md` index, re-reading it on every
call, so keep it to standing rules and one-line pointers. Topic files beside it
are found through search.

## Voyages

`/voyage <outcome>` keeps a session working towards an outcome, turn after turn,
until it's done. `init --apply` adds the command to each account
(`commands/voyage.md`); toomux's prompt and Stop hooks do the work.

```
/voyage the auth tests pass                      set one (a new one replaces the old)
/voyage the docs build --check "make docs"       done only once the command exits 0
/voyage the migration is written --budget 40     pause once $40 is spent
/voyage the parser is done --persistence hard    how hard it pushes (light, steady, hard, relentless)
/voyage                                          how it's going
/voyage resume                                   carry on after a pause (lifts a spent budget)
/voyage clear                                    end it
```

- **After every turn** the Stop hook gives a small model (`voyage_judge_model`,
  default `haiku`) the outcome and the end of the conversation: messages, tool
  calls and their results. It answers *met*, *not yet*, *impossible* or *needs
  you*, with a reason. Not yet: the session is sent back with the reason. A
  check takes a few seconds and costs about a tenth of a cent. It runs on the
  session's own account.
- **`--check`** runs in the session's folder once the judge says met, and must
  exit 0. If it fails, the session goes back with the end of its output.
- **Handover.** A session on a voyage still hands over at the end of a turn.
  The brief gets a Voyage section and the fresh session's first prompt carries
  the voyage, so it picks up where the last one left off.
- **Limits.** A session that stops at its usage limit carries on by itself once
  the limit lifts and it has sat a minute at an empty prompt (see
  [After a limit resets](#after-a-limit-resets)). A voyage paused because its
  judge couldn't run at the limit carries on too.
- **Persistence** sets how hard it pushes and how hard *done* is to reach.
  `voyage_persistence` in the config sets the default (`steady`).

  | Level | Judge | Before done counts | Stuck, or stopping |
  |---|---|---|---|
  | light | `voyage_judge_model`; a clear claim of done is enough | the judge's word | pauses after 2 idle turns, 2 failed checks or 4 flat checks |
  | steady | `voyage_judge_model`; wants evidence shown | the judge's word | a nudge after 4 flat checks, a pause after 8; one push back; 4 idle turns; 3 failed checks |
  | hard | `voyage_hard_model` (`sonnet`); counts only evidence from these turns, and looks for skipped tests, stubs and hard-coded results | one proof lap: a clean re-run and a read over the changes | a nudge every 3 flat checks, a pause after 12; one push back; 6 idle turns; 4 failed checks |
  | relentless | `voyage_relentless_model` (`opus`), as strict as hard | two proof laps (the second tries to break it), then a sceptical review of `git diff` since the voyage began, committed or not | a nudge every 2 flat checks, a pause after 20; two push backs; 8 idle turns; 5 failed checks |

  A *flat check* is a *not yet* whose estimate doesn't beat the best so far. The
  first nudge asks the session to step back and try another way; later ones
  ask it to list what it has tried and why each failed, then pick something
  new.

  A *proof lap*: when the judge says met, the session is sent back to prove it,
  and the judge (told a lap was asked for) has to see it done and say met
  again. Any *not yet* starts the laps over. A *push back*: *needs you* or
  *impossible* sends the session back to settle it itself (a sensible,
  easy-to-undo choice) or find another way; only when the judge says it again
  does the voyage stop. `--check` runs first, on every met, so no lap is spent
  on a failing check; then the laps, then the review.
- **It stops for you** when the judge says it needs you, after a run of turns
  with no tool call or of flat checks, when its budget is spent, or when it
  can't be checked a few times running (the numbers are in the table above).
  You get a notice (and `notify_command` runs with `TOOMUX_EVENT=voyage`). Your
  next message carries it on, except after a spent budget, which takes
  `/voyage resume`.
- **Where it shows:** a `◎ steady voyage 1h 20m` chip in the session's status
  line and beside it in toomux, its persistence first. On a proof lap it reads
  `◎ hard voyage, proof lap 1 of 1`. It goes rose when the voyage is waiting on
  you, and green for ten minutes once it's done. toomux's detail pane adds the
  outcome, the judge's estimate, what it has spent and the last check. When the
  right side shows the live session instead, the chip leads the line above it.
- **The scene.** Above the chip, the status line draws the voyage as a ship
  sailing to an island, five lines tall. Where she is on the path is the judge's
  estimate of how much is done (it answers with a percent each turn), so she can
  lose ground as well as gain it. She drops anchor while the voyage waits on you
  or on a limit, and only lands when it's met; the landing stays up ten minutes.
  It's pixel art in text: each cell is a 2 by 3 grid of pixels (Unicode's
  sextant blocks) in the two colours that fit it best, so it needs a terminal
  font or renderer that draws them (VTE, kitty, WezTerm, ghostty, iTerm2 and
  Windows Terminal do). `toomux voyage show` draws it too, and so does toomux's
  detail pane. The sidebar has no room for it: the session's row shows the chip
  and the judge's estimate ("58% there") instead. `voyage_scene = false` keeps
  to the chip.
- **From outside:** `toomux voyage` lists open voyages and the last ones that
  ended; `toomux voyage show <id>` tells the whole story. `toomux voyage set
  <session> <outcome>` starts one (a session waiting at an empty prompt starts on
  it at once), and `pause`, `resume`, `clear` and `rebind` (move it onto another
  conversation) take a voyage id or its session.
- **Records:** `~/.local/state/toomux/voyages/<id>.json`, plus `<id>.log` with
  every check.

## The memory graph

`alt-m` in full-screen toomux shows everything in memory as a graph: every
project, its memory files, the sessions that ran in it, the briefs they wrote
when they handed over, and what each session looked up in memory.

It opens on a sky of projects. Each is a disc of what it holds, coloured by
kind: the bigger the disc, the more it holds, and the brighter the dots, the
more recently they were touched. A line joins two projects once two or more
links cross between them, such as a memory file naming one in the other
project, or a session reading the other project's memory.

Focusing something gathers its links around it, grouped by how they link: a
project's memory files and sessions, a file's links in and out, a session's
turns, kept outputs, the brief it wrote and the session that continued from it.
A session's turns and kept outputs appear only when you focus the session.
Each thing sits in one group, by its strongest link (a file a session both
found and read is under "read"). The side panel shows the selection, a preview
of its text, the same groups with the latest few in each, and its id,
`toomux jump` target or path.

A session is named as toomux's session list names it: the name you gave it,
then Claude's own title for it, then the heading of the brief it wrote, then
its first prompt. A name that three or more sessions share, such as one
carried along a chain of handovers, is passed over.

| key | |
|---|---|
| arrows, `hjkl` | move to the nearest thing in that direction |
| `tab` | the next one |
| `enter`, click | focus it (click a selected one) |
| `backspace` | back to the last focus |
| `/` | find a project, file, session or turn by name, path or text |
| `o` | open the same place in the browser view |
| `r` | read memory again |
| `?` | hide or show the legend |
| `esc` | back to the sky, then close |

The legend in the corner shows only the kinds and links on screen.

| mark | kind |
|---|---|
| ◉ | project |
| ◎ | memory index (`MEMORY.md`) |
| • | memory file |
| ▪ | note (saved with `mem_save`) |
| ● | session |
| ◆ | handover brief |
| · | turn |
| ▫ | kept output |

| line | link |
|---|---|
| grey | in a project; a session's turns and outputs |
| light grey | one memory file names another |
| amber | a session wrote a brief; a later session continued from it |
| green | a session read or found it in memory |

The browser view uses the same colours: memory files neutral, sessions and
their turns blue, briefs and the handover chains between them amber.

`toomux graph` prints how much there is, `--json` the whole graph, and `--open`
draws it in your browser: a page written to
`~/.local/state/toomux/memory-graph.html` (readable only by you) with the graph
inside it, so it needs no server or network. `--at <id>` opens it on one node.

Recalls come from each session's transcript as it is indexed: a `mem_search`
counts the entries it found, a `mem_get` or a Read of a memory file counts as
read. The first run goes back through every transcript once.

## Usage and limits

The header carries a hairline meter per account and window
(`office 5h ━━╾─────── 19%  wk ━──────── 10%`), rose only at a limit. `ctrl-u`
opens the full view: each window with its reset time, the pace ("at this pace the
limit comes in 38m", or "on pace to end near 60%"), the past day as a braille
line, and, for a session stopped by a limit, which account has room. The status
bar shows each account's tightest window (`office 42%·5h`), and a notice goes
out once when a window passes 90%.

Where the numbers come from, freshest first:

- **Live.** Claude Code gives its status line command the account's
  `rate_limits` after each response; `toomux init --apply` makes toomux that
  command (a calm `office · ctx 31% · 5h 20% · wk 10%` under the prompt). A
  session only counts if it has just talked to the API, since Claude Code keeps
  handing over its last numbers however old.
- **Looked up.** An account with no live report for 10 minutes is read from
  Anthropic's usage endpoint (the one behind `/usage`) with that account's
  sign-in, at most every 15 minutes, backing off on errors. The token is read
  from `.credentials.json` (the Keychain on macOS), passed to curl on stdin, and
  never refreshed or written. `toomux usage --fetch` looks up now.
- **A session stopped by a limit**, from its transcript.

### After a limit resets

When a limit stops a session, Claude Code asks "What do you want to do?", and
its first answer (stop and wait) doesn't carry on by itself. So a minute after
the reset, toomux closes that dialog (Esc, and checks it closed) and, for a
session that was mid-work, types `[toomux] The usage limit has reset. Carry on
from where you were cut off.` Each limit is acted on once (a marker per
session and limit, in `~/.local/state/toomux/limits/`, where `log.jsonl`
records every step). `resume_after_limit` in the config says which sessions:

| | carries on |
|---|---|
| `working` (default) | a turn the limit cut off after Claude had started on it, a voyage, a handover under way |
| `all` | also a prompt the limit refused before any work began |
| `off` | only voyages and handovers under way (they were asked for) |

Before the reset nothing is pressed in the dialog, unless
`auto_continue_at_limit = true`: then toomux picks Claude Code's own "Wait
here, then continue automatically" when the reset is less than a day away.
While Claude Code is counting down to continue by itself (its "Continue
automatically at usage limit" setting), toomux leaves the session alone: no
keys, no handover, no relaunch. It only steps in if the session still hasn't
carried on a minute after the reset.

No terminal needs to be attached. Whoever sees the limit first (Claude Code's
status line command, the tmux status tick, `toomux list`) leaves one detached
`toomux tick --at <reset>` sleeping until then, per session and reset. It
sleeps by the wall clock, so a machine that slept through the reset acts when
it wakes. A limit that names no reset (a model's own limit, a spend limit)
counts for five hours, or until a real reply follows it; toomux never types
into those by itself.

## Accounts

An account is one Claude Code sign-in: a config folder that Claude Code uses
when `CLAUDE_CONFIG_DIR` points at it (or `~/.claude`, the default). You can name
accounts whatever you like and keep their folders wherever you like. On first
run toomux finds every `~/.claude` and `~/.claude-*` that holds a sign-in; from
then on the config lists them. Typical use: separate work, personal and client
sign-ins, and moving a session to another account when one hits its limit.

**Standalone or sharing.** Each account can stand alone, with its own
conversations and settings, or join a group whose accounts share one history.
In a group, any account can resume any of the group's conversations. Accounts
can join and leave at any time without losing anything, and any mix works: all
separate, all in one group, two sharing and one alone, or several groups.
toomux's own memory (see [Memory](#memory)) is one per machine either way.

What a group shares, as links from each account into the group's folder
(`~/.claude-shared`, or `~/.claude-shared-<group>`): conversations and Claude
Code memory (`projects`), settings, `CLAUDE.md`, history, skills, agents,
commands, plugins, plans, todos, file history and Claude Code's caches. Never
shared: `.credentials.json` and `.claude.json` (the sign-in and per-account
state, such as folder trust). Only what toomux lists is shared, so anything
Claude Code adds later stays each account's own.

**In toomux**, `alt-a` opens Accounts: each account's folder, sign-in state,
group and number of running sessions.

- `a` adds an account step by step: name, folder, sharing, then an optional
  sign-in. An existing Claude config folder can be brought in as it is.
- `l` signs the selected account in (Claude Code's own sign-in flow). The page
  notices when the sign-in appears.
- `s` joins a group, `u` leaves one, and `x` removes the account from toomux,
  with a separate choice to delete its folder.
- `esc` goes back a step, then closes the page.

If a change would affect an account with sessions still running, toomux asks
first, then goes ahead the way the CLI's `--force` does.

**From the shell:**

```sh
toomux account                              # each account, sign-in, who it shares with
toomux account setup                        # a walk-through: names, folders, who shares
toomux account add <name>                   # a new one at ~/.claude-<name>
toomux account add <name> --dir ~/old-claude   # bring in a folder that exists
toomux account add <name> --share [group]   # ... and join a group straight away
toomux account rename <name> <new>          # the folder follows when it's ~/.claude-<name> (--dir to choose)
toomux account share a b                    # a and b share ~/.claude-shared (--all: every account)
toomux account share c --group lab          # c joins another group, ~/.claude-shared-lab
toomux account unshare b                    # b stands alone, with its own copy of what it saw
toomux account unshare b --fresh            # ... or starting with no history
toomux account remove <name>                # toomux stops using it; --delete deletes the folder
```

Joining moves files rather than copying them, so it wants Claude closed on that
account (`--force` goes ahead anyway); `--dry-run` shows the plan first. Folders
are merged; a file both have with different contents is kept as
`name.from-<account>.md`; history is joined; settings are combined, and where a
setting differs that account keeps its own and you're told. Leaving copies what
the account saw (settings always come along). Renaming moves the folder when it
was named `~/.claude-<name>`, and carries the new name into rules, pins and the
restart record.

## Moving a session between accounts

`ctrl-a` (or `toomux switch <session> --to <account>`):

1. waits for the session to be idle. A working session is queued, shown as
   "moves to X when idle", and moves as soon as it goes idle; `ctrl-a` again
   cancels. A session parked on a usage-limit dialog can move at once;
2. exits it with ctrl-c, the way you would;
3. relaunches `claude --resume <id>` in the same pane under the other account,
   keeping launch flags like `--model` and `--dangerously-skip-permissions`.

If the two accounts don't share a history, the conversation is copied to the
target account first. A different conversation already there under the same id
is never replaced.

When several sessions on an account are stopped at its limit, `ctrl-a` offers to
move them all (`toomux switch --limited` from the shell). `switch --wait` waits
for a working session instead of refusing; `--force` moves one whose background
tasks will stop.

## New sessions

`ctrl-n` in the list, or `alt-n` anywhere in full-screen toomux (including while
you're typing in a session, where `ctrl-n` belongs to Claude), lists folders by
how much and how recently you've used Claude in them (or type a path). Pick one
with `enter`, then the account with `enter` (the suggested one) or its number.

`toomux new [folder] [--account <name>]` does the same from a shell: the
current folder by default, then it takes you there (`--detach` only starts it).

- **The account** comes from the most specific `[[rules]]` entry for the
  folder, else the account sessions there already use, else one that isn't at
  its limit.
- **Launch flags** are the ones most of your running sessions use, unless
  `new_session_args` says otherwise. What belongs to one conversation alone is
  never copied: its `--name`, `--resume`/`--continue`/`--session-id`, its
  opening prompt, and the handovers folder toomux adds to a successor. Ties go
  to the longer set of flags, then the first in sorted order. The picker shows
  the flags it will use.

## Folder trust and first-run screens

Claude Code asks before working in a folder for the first time ("Is this a
project you created or one you trust?"), and a new account starts with a
first-run theme picker. A session toomux starts would sit at those screens
until someone answered. So before every launch (a new session, a handover, a
voyage step, a reopen, a move to another account) toomux prepares the account's
`.claude.json`:

- **A folder you picked** with `alt-n`, `ctrl-n` or `toomux new` is recorded as
  trusted on that account. Picking it is your decision.
- **A folder any of your accounts already trusts** is recorded as trusted on
  the account the session launches on. Trust given under a symlinked path is
  recorded for the real path too.
- **First-run onboarding** (the "done" flag, its version and your theme) is
  copied from another of your accounts that has finished it.

toomux never trusts a folder for a session you didn't start yourself unless
you've trusted it before. What's still left to you:

- signing in to an account that has never been signed in;
- accepting `--dangerously-skip-permissions` the first time on an account, if
  your sessions use it;
- trusting a folder none of your accounts has trusted, when a handover or
  reopen lands there;
- any first-run screen Claude Code adds in a later version.

When a launch stops at one of these, toomux says which, in the handover result,
in `toomux new`'s output, or as `needs you` in the list. Answer it in the pane.

## Sidebar, pins and notices

**Sidebar.** `alt-b` opens a narrow, always-on list at the left of the window. A
click (or `enter`) jumps to a session, and the sidebar comes along into that
window. `alt-b` in a window that has it hides it; anywhere else it moves it
there.

**Pins.** `ctrl-p` pins a conversation to the next free `alt-N`. Slots never
renumber. A pinned conversation that has exited stays listed under "pinned ·
not running", and `alt-N` reopens it with its last account and launch flags.

**Notices.** The tmux status bar shows per-account counts, and a quiet notice
when a session that worked for at least `notify_after_secs` (90) finishes, or
when any session starts needing you (`● Engine done +1`). A notice clears when
that pane is on screen or the session goes back to work. `notify_command` can
forward notices anywhere (for example a phone push), with `TOOMUX_MESSAGE`,
`TOOMUX_TITLE`, `TOOMUX_EVENT` and `TOOMUX_SESSION_ID` in its environment.

On macOS, a desktop notification:

```toml
notify_command = '''osascript -e 'on run argv' -e 'display notification (item 1 of argv) with title (item 2 of argv)' -e 'end run' "$TOOMUX_MESSAGE" "$TOOMUX_TITLE"'''
```

It shows as coming from Script Editor. If nothing appears, give Script Editor
notification permission in System Settings; macOS fails silently and doesn't ask.

In WSL, a Windows notification needs PowerShell on the Windows side. This is how
[JoliNotif](https://github.com/jolicode/JoliNotif) sends one: save it as
`~/.local/bin/toomux-toast`, `chmod +x` it, and set
`notify_command = 'toomux-toast'`.

```sh
#!/bin/sh
ps=$(command -v powershell.exe || echo /mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe)
body=$(printf %s "$TOOMUX_MESSAGE" | base64 -w0)
script="\$b = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('$body'))
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
\$t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastImageAndText01)
\$t.GetElementsByTagName('text').Item(0).AppendChild(\$t.CreateTextNode(\$b)) | Out-Null
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('toomux').Show([Windows.UI.Notifications.ToastNotification]::new(\$t))"
exec "$ps" -NoProfile -NonInteractive -EncodedCommand "$(printf %s "$script" | iconv -f UTF-8 -t UTF-16LE | base64 -w0)"
```

## A tmux server per session

Every session toomux starts, reopens or brings into tmux runs in a tmux server of
its own (`tmux -L toomux-<id>`), so nothing done to one server (a `kill-server`, a
crash) takes the others down with it. toomux lists panes across your default
server and all of its own, and carries each pane as `%N@<server>`. A session in
a server of your own making (`tmux -L work`, or `-S /some/socket`, carried as
`%N@/some/socket`) is found by the `TMUX` its process started with. Jumping to a
session on another server moves your terminal over (`detach-client -E` then
`attach`); the full-screen view reconnects its control client to that server.
It only ever attaches to a server that is running: when the session you're
watching ends, toomux says so with the last line it showed, and with no session
in tmux at all it attaches to nothing (it never starts a server or a shell
session of its own).

Its servers start from `~/.local/state/toomux/server.tmux.conf`: your
`~/.tmux.conf`, rewritten on each start without tmux-resurrect, tmux-continuum and
tpm (your other plugins are run directly). Those assume a single server, and in a
new one continuum would restore your whole saved layout into it.

Each job has one owner, so nothing saves, restores or kills the same thing twice:

| job | owner |
|---|---|
| Claude sessions: running, handing over, the record of what runs, reopening after a restart | toomux, a server per session |
| other tmux servers (services, `tmux -L work`) | whatever started them; toomux only lists their panes |
| your default server's plain-shell layout | you, with tmux-resurrect by hand (`prefix + ctrl-s` / `ctrl-r`) if you want it |

Don't run tmux-continuum alongside toomux. Its automatic restore brings Claude
panes back as empty shells (resurrect doesn't restart Claude), and it turns
itself off when it sees other tmux servers as your default server starts, which
toomux's always are.

## After a restart

toomux keeps a record of what's running. After a reboot it reopens those sessions
by itself, each in its own server, at the first status tick (as soon as any tmux
is up), and says so in one line; `reopen_after_restart = false` only offers them.
Sessions that vanish while the machine stays up (more than half at once, gone
for two minutes: a tmux server went down) are offered, not reopened.

Offered sessions show under "before the restart": `ctrl-e` reopens them all,
`enter` reopens one, `ctrl-x` stops offering one. `toomux reopen <id|name>...`
does the same from the shell, for these, pins, and anything last recorded
running. Given a session id it also opens any conversation an account keeps a
transcript of (a voyage's, say), in the folder it ran in, on `--account <name>`
or the account that folder would start on. A conversation still open anywhere
is never opened twice: it says where it runs instead.

## Upkeep: nothing lost, nothing stale

`toomux upkeep` runs by itself once an hour, from the status tick. It looks
after what toomux and Claude Code keep:

- **The archive.** Claude Code deletes a conversation's files after
  `cleanupPeriodDays` (30 by default), and memory holds each turn only in part
  (its prompt and the words that closed it). So every file under each
  account's `projects/` (transcripts, subagents', tool results, attachments;
  not `memory/`) is copied, zstd-compressed, to `~/.local/share/toomux/archive/`
  once it has been quiet a day, and again whenever it changes. Nothing there
  is deleted. `mem_get` with `whole: true` (or `toomux turn <id>`) reads any
  turn back complete, every message, tool call and result, from the
  transcript or the archive. Transcripts compress to about a third of their size.
- **Memory against the disk.** Claude Code names a workspace's memory folder
  after its path and deletes the transcripts that record it, so upkeep matches
  each folder to the folders under home (symlinks count as live).
  - *A workspace that moved* (old folder gone, and exactly one repository or
    folder Claude Code has run in shares its path's tail): its memory is
    brought to the new home, where sessions there load it. Same-named files
    with other text come in beside, as `name.from-<old>.md`; the old index
    comes as a topic file with one line for it in the new index. The old
    folder is left as it is.
  - *A cited path that's gone:* when its first folder under home moved and
    exactly one folder of that name holds the rest of the path, the path is
    corrected. A bare name (`~/go`) is no evidence, and nothing with a space
    in it is written. Other gone paths are listed: `mem_search` marks the
    file ("2 paths it cites are gone") and `mem_get` names them. Temporary
    places (`tmp*` worktrees, caches, build output) are expected to go.
  - Every file upkeep changes is kept in the archive first, as it was
    (`<file>.<ms>.zst`). `toomux upkeep --dry-run` says what it would do.
- **Repositories** (every repository under home, once a day). Tidied where
  nothing can be lost: worktrees whose folder is gone are pruned, and
  worktrees that are clean (nothing changed or untracked), already on the
  default branch, quiet a day and not any process's current folder are
  removed (`git worktree remove`, never forced). Branches are never deleted.
  Listed, never touched: uncommitted work and how long it has sat, worktrees
  holding unmerged work, merged branches. The summary is in
  `~/.local/state/toomux/upkeep.txt`, each repository in `repos.txt`.
  When the hourly run changes something by itself, one notice says so.
- **Memory written as work ends.** A conversation handing over is asked,
  before its brief, to bring its workspace's memory up to date: what it
  learned that lasts (decisions and why, gotchas, where things are,
  corrections) into topic files, `MEMORY.md` kept to standing rules and one
  line per topic. Its context is cached then, so it's the cheapest moment,
  and it knows best what it learned. The request carries upkeep's findings
  for that workspace: an index past 150 lines or 15 KB is slimmed (word for
  word into topic files), and files citing gone paths are named.

## What toomux does by itself

Everything is on from the start, and `init` lists it. Each is one line in the
config to turn off:

| What | Off |
|---|---|
| Handover: past a context limit, a conversation writes a complete brief and continues in a fresh session | `handover_tokens = 0` |
| Fork gate: past 200k of context, a fork is refused in favour of a briefed subagent | `fork_context_tokens = 0` |
| Bash capture: long output kept whole and shown short; background commands carried over a handover | `capture_bash = false` |
| Memory fixes (hourly): paths that moved are corrected in memory files, prior text kept | `fix_memory = false` |
| Worktrees (daily): clean, merged, idle git worktrees removed; never branches or changes | `tidy_worktrees = false` |
| Archive: every transcript kept compressed, past Claude Code's 30-day cleanup | `archive_transcripts = false` |
| After a reboot: what was running reopens by itself | `reopen_after_restart = false` |
| After a usage limit resets: sessions it cut off mid-work close Claude Code's limit dialog and carry on | `resume_after_limit = "off"` |
| Voyages: a session with a `/voyage` is checked after every turn and sent back until it's done | `/voyage clear` (only runs when you set one) |

Always on: every conversation is indexed into one memory that sessions search
(see [Memory](#memory)), and launches are prepared as described under
[Folder trust and first-run screens](#folder-trust-and-first-run-screens).

### Where things are

`toomux where` lists every place toomux reads or writes, with sizes, and what
`init --apply` changed:

- `~/.config/toomux/config.toml`: the config.
- `~/.local/state/toomux/`: memory, usage, handovers, kept outputs, logs.
  Everything here can be rebuilt or lost.
- `~/.local/share/toomux/`: the transcript archive, the one thing worth backing up.
- `$XDG_RUNTIME_DIR/toomux/`: sockets and queues, gone at reboot.

Set `TOOMUX_HOME` (in your shell profile, before tmux starts) to keep config,
state and data under one folder instead. Claude Code's own memory folders stay
where Claude Code reads them, in each account's `projects/<workspace>/memory/`.

### Taking it back

`toomux uninstall` takes back what `init --apply` added, found by its markers, so
anything you changed since stays as you left it: the tmux block and status
segment, the status line, the hooks, the MCP server and the `/voyage` command.
A backup of each file it edits is kept as `*.pre-toomux-uninstall`. With a
self-contained install it also removes the bundle and the tmux launcher the
installer added. `--dry-run` shows what it would do; `--purge` also deletes
toomux's config, state and archive, and init's backups. Your Claude Code memory
folders, and any status line or hooks of your own, are never touched.

## Configuration

`toomux init` writes `~/.config/toomux/config.toml` with every setting and a
comment on each. Changes apply the next time toomux opens.

| setting | default | |
|---|---|---|
| `claude_bin` | `~/.local/bin/claude` if it exists, else `claude` on your `PATH` | the Claude Code used to start and relaunch sessions |
| `finished_minutes` | `15` | an idle session shows as finished for this long, then settles to idle |
| `notify_after_secs` | `90` | a session that worked at least this long gets a notice when it finishes |
| `notify_command` | unset | a shell command run for each notice (see [Notices](#sidebar-pins-and-notices)) |
| `stale_days` | `3` | idle sessions older than this are offered for cleanup (`ctrl-s`) |
| `new_session_args` | unset | launch flags for new sessions, e.g. `["--model", "opus"]`; unset: the flags most running sessions use |
| `reopen_after_restart` | `true` | after a reboot, reopen what was running; `false`: only offer it |
| `handover_turn_end_tokens` | `250000` | hand over at the end of a turn past this; `0`: only at `handover_tokens` |
| `handover_tokens` | `400000` | the hard limit; `0` turns handover off |
| `subagent_handover_tokens` | `250000` | a subagent hands over past this; `0`: at `handover_tokens` |
| `fork_context_tokens` | `200000` | refuse forks from conversations bigger than this; `0`: always allow |
| `[[context_policy]]` | none | per-model, provider, account or session overrides (see [Context policies](#context-policies-different-limits-per-model-or-account)) |
| `voyage_judge_model` | `"haiku"` | the voyage judge at light and steady |
| `voyage_hard_model` | `"sonnet"` | the judge at hard |
| `voyage_relentless_model` | `"opus"` | the judge and reviewer at relentless |
| `voyage_persistence` | `"steady"` | a voyage's persistence when `/voyage` doesn't say |
| `voyage_scene` | `true` | draw the ship above a voyage's status line |
| `capture_bash` | `true` | cap long Bash output and carry background commands across a handover |
| `fix_memory` | `true` | upkeep corrects moved paths in memory files |
| `tidy_worktrees` | `true` | upkeep removes clean, merged, idle worktrees |
| `archive_transcripts` | `true` | upkeep keeps a compressed copy of every transcript |
| `resume_after_limit` | `"working"` | which sessions carry on after a limit resets: `working`, `all` or `off` |
| `auto_continue_at_limit` | `false` | choose Claude Code's "Wait here, then continue automatically" at the limit dialog |

Accounts, folder rules and colours are tables:

```toml
[[accounts]]
name = "work"
config_dir = "~/.claude-work"

# New sessions in a folder (or below it) start on this account.
[[rules]]
folder = "~/work"
account = "work"

[colors]
accent = "#57e6be"
```

## CLI

```
toomux                                  full screen (alt-s in tmux); same as `toomux shell`
toomux list [--json]                    running sessions
toomux status                           the tmux status-bar segment (also runs notices, upkeep and the restart record)
toomux jump [<target>] [--pin <n>]      go to a session (pid, session-id prefix or name), or a pin, reopening it if needed
toomux rename <target> [<name>...]      name a session; no name clears toomux's
           [--for-you]                  a name an agent chose for the work: the next handover renames the successor
toomux switch [<target>] [--to <account>] [--wait] [--force] [--notify]
                                        resume its conversation under another account
toomux switch --limited [--to <account>]   move every session stopped at its account's limit
toomux adopt <target>                   bring a session from a plain terminal into tmux
toomux new [<dir>] [--account <name>] [--detach]   a new session there (default: here), in its own server
toomux reopen <target>... [--account <name>]       conversations that aren't running, each in its own server
toomux reopen --after-restart           everything from before the restart, once (the status tick runs this)
toomux sidebar [--toggle]               the always-on list
toomux usage [--json] [--fetch]         each account's 5-hour and weekly usage
toomux handover <target>                hand a session over to a fresh one now
toomux context-policy [--account <a>] [--provider <p>] [--model <m>] [--session <id>] [--json]
                                        the context limits that apply, and why
toomux jobs                             background commands toomux is looking after
toomux job follow|log|stop <id> [--tail <n>] [--more]
                                        re-attach to, read or stop one (--more: follow on from the last follower)
toomux out <id> [--lines a-b] [--grep text] [--chars a-b]   a kept output
toomux mem [<words>...] [--all]         search memory (no words: how many entries)
toomux mem --forget <id>                delete one memory for good
toomux graph [--json] [--open] [--at <id>]   the memory graph: counts, JSON, or in the browser
toomux turn <id>                        one conversation turn whole, by its memory id
toomux tokens [--since 90m|24h|7d|"2026-09-29 21:43"] [--session <prefix>] [--json]
                                        where the tokens went, in dollars at API list prices (default: last 24h)
toomux digest [--day 2026-09-29] [--announce]
                                        a day's token report (yesterday by default); made each morning after 8
toomux voyage [list]                    open voyages, then the last ones that ended
toomux voyage show <id>                 a voyage and every check it has had
toomux voyage set <target> <outcome> [--check "<cmd>"] [--budget <dollars>] [--persistence <level>]
toomux voyage pause|resume|clear <id|session>
toomux voyage rebind <id|session> <session>   move a voyage onto another conversation
toomux account [list]                   accounts, sign-in state and sharing
toomux account setup|add|rename|share|unshare|remove ...   see Accounts
toomux upkeep [--dry-run]               archive transcripts, check memory against the disk (hourly by itself)
toomux where                            every place toomux reads or writes, and what init changed
toomux init [--apply]                   write the config; with --apply, set up tmux and each account
toomux uninstall [--dry-run] [--purge]  take back init; a self-contained install removes its runtime too
toomux rollback                         switch a self-contained install to its previous bundle
toomux statusline                       Claude Code's status line command (reads its JSON on stdin; set by init)
```

`toomux <command> --help` has the details of each.

## Troubleshooting

### Keys don't work on macOS

Every global toomux key is `alt-*`, and macOS terminals type symbols with Option
unless told otherwise:

- **Terminal.app:** Settings → Profiles → Keyboard → *Use Option as Meta key*.
- **iTerm2:** Settings → Profiles → Keys → Left Option key → *Esc+*.
- **Ghostty:** `macos-option-as-alt = true`.
- **kitty:** `macos_option_as_alt yes`.
- **Alacritty:** `option_as_alt = "Both"` under `[window]`.

Until then, `prefix space` opens toomux, and the list's own keys are `ctrl-*`.

### A key does something else

toomux binds `alt-s`, `alt-b` and `alt-1..9` in tmux's root table, so they work
in every pane, and `prefix space` in place of tmux's "next layout". Shells use
`alt-b` to move back a word, and some editors use `alt-<digit>`. To take a key
back, remove its line from the toomux block in `~/.tmux.conf` and reload tmux
(`toomux init --apply` will add it again). Inside full-screen toomux, `alt-n`,
`alt-u`, `alt-m`, `alt-a`, `alt-r`, `alt-j` and `alt-?` are toomux's even while
you type in a session.

### Handover, capping or voyages do nothing

These run from Claude Code hooks. Check:

- The session's account has them: `toomux where` lists what `init --apply`
  changed in each account's `settings.json`. An account added outside toomux
  needs `toomux account add <name> --dir <folder>` or `toomux init --apply`.
- The session started after the hooks were added. Claude Code reads its
  settings when it starts; a session from before needs restarting.
- The session runs on a known account. A Claude Code folder that isn't one of
  toomux's accounts (shown as `unknown`) gets nothing; `toomux account add
  <name> --dir <folder>` brings it in.
- The handover limit isn't off (`handover_tokens = 0`), and no context policy
  raises it (`toomux context-policy --model <id>`).

### A session sits at a Claude Code screen

A trust prompt, sign-in, bypass-permissions prompt or theme picker that toomux
couldn't answer for you: see
[Folder trust and first-run screens](#folder-trust-and-first-run-screens).
Answer it in the pane and the session carries on.

### A handover failed or keeps waiting

The list shows the reason. Common ones: text typed in its prompt (toomux won't
overwrite it; it tries again a minute later and gives up after 30 minutes), the
fresh session wouldn't start (the old conversation is put back), or a usage
limit (it hands over after the reset). Failed handovers are retried after 30
minutes; after 3 fresh sessions that wouldn't start, a conversation is left
alone for a day. Every step is in `~/.local/state/toomux/handovers/log.jsonl`.

### A session didn't carry on after a limit reset

Check `resume_after_limit` (a prompt refused before any work began only carries
on with `all`), and `~/.local/state/toomux/limits/log.jsonl`, which records every
step. A limit with no reset time (a spend limit, say) is never typed into.

### Where to look

| | |
|---|---|
| `toomux where` | every place toomux reads or writes, and what init changed |
| `~/.local/state/toomux/errors.log` | every error shown on screen, one line each, kept to about 256 KB (plus `errors.log.1`) |
| `~/.local/state/toomux/handovers/log.jsonl` | every handover's start, wait and outcome |
| `~/.local/state/toomux/limits/log.jsonl` | every step taken after a usage limit |
| `~/.local/state/toomux/voyages/<id>.log` | every voyage check |

tmux must be 3.2 or later; `toomux init` says if it isn't.

When you open an issue, include `toomux --version`, `claude --version`,
`tmux -V`, your OS, and the relevant lines from these logs (with anything private
removed).

## Colours

Three statuses carry meaning (attention rose, working amber, finished mint), plus
one accent (mint); everything else is a neutral. `text`, `dim` and `muted` step
down for less important words (idle sessions use `muted`), and `faint` is only
ever a hairline. toomux paints its own backgrounds, so it looks the same on any
terminal: `raised` for the header and footer bands, `base` for the list, `well`
for the live preview, `overlay` for pop-overs (which dim everything behind them),
plus `selection` and `hover`. All of them are in `[colors]`; the tests check text
contrast on every surface.

## How it works

No daemon. toomux reads Claude Code's live session registry
(`<config>/sessions/<pid>.json`), joins it with the process table (`/proc` on
Linux, libproc and sysctl on macOS; the account from `CLAUDE_CONFIG_DIR`, the
tty, liveness from the kernel's start time) and `tmux list-panes`, and watches
the registry for changes. Titles, PR links and usage-limit state come from the
tail of each transcript, read incrementally. Sessions outside tmux get a preview
from their transcript.

Notices and the restart record are kept by `toomux status`, which tmux already
runs every couple of seconds for the status bar. So are indexing, hourly
upkeep, handovers, voyages and the daily digest. A tmux client in control mode,
as full-screen toomux is, draws no status bar, so tmux never runs it there:
full-screen toomux runs `toomux status` itself every 2 seconds instead. State
lives in `~/.local/state/toomux` (pins, names, the restart record, usage) and
`$XDG_RUNTIME_DIR/toomux` (queued moves, notices).

## Remote access

Phone and tablet access to your sessions is coming. It isn't usable outside the
maintainer's own setup yet, so it isn't documented here.

## Licence

Either of [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
