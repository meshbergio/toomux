# The toomux guide

Everything toomux does, in detail. The short version is in the [README](README.md).
Runs on Linux and macOS, and on Windows inside WSL 2.

`alt-s` from anywhere in tmux turns the whole terminal into toomux: every
session, what it's working on, what it's doing right now, which account it's on,
and the chosen one live beside the list. Type to filter, enter to open it,
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

## Full screen

`toomux`, or `alt-s` from anywhere in tmux, takes the whole terminal: the
header with usage meters, the session list, and the chosen session live and
interactive beside it. In tmux it opens over everything, status bar included,
and closes the way it came: `alt-j` leaves you in the chosen session's own tmux
window, `ctrl-c` back where you were.

The live session is tmux control mode (`tmux -C`): tmux streams the pane's
output over a pipe, toomux parses it (alacritty_terminal) and draws only the
pane, and your keys go back as tmux key names, so tmux encodes them for
whatever mode the program is in. No second tmux screen, no status bar inside
the view, nothing polled: the loop sleeps until a key, pane output or a
registry change arrives. Pane listings of the server you're looking at come over
the same connection; each other server is asked at most once every 10s, and
the status line shares those answers. Measured with eight servers: 48 small
`tmux list-panes` a minute, ~0.5% CPU at rest, about 9MB of memory.
The repeatable resource-measurement procedure is in [BENCHMARKS.md](BENCHMARKS.md).

| key | |
|---|---|
| `alt-s` | between the session and the list |
| `alt-j` | opened with `alt-s`: close toomux on the chosen session, in tmux itself |
| `alt-b` | hide or show the list |
| `alt-u` | usage in place of the session |
| `alt-m` | the [memory graph](#the-memory-graph) in place of the list and session |
| `alt-1..9` | a pin |
| in the list | everything under [Keys](#keys); `enter` opens here, `esc` goes back to the session, `ctrl-c` leaves (sessions keep running) |

Everything else goes to the session, paste included (bracketed when the program
asks). The wheel scrolls toomux's copy of the history, or goes to the program
if it takes the mouse; shift-drag selects text in your terminal as usual.

## Tokens: handover, capping and memory

Measured on 120 real sessions (72,350 API calls, exact counts via
`count_tokens`): 87.5% of cache-weighted input is the context being re-read on
every call, and calls above 400k tokens of context are 78% of it. Tool output is
~30%, but completely lossless rewriting of it saves only ~0.1%, so there is no
proxy in front of your sessions. Instead:

- **Handover** (`handover_tokens`, default 400k). Every agent follows it, main
  sessions and subagents alike:
  - *The gate.* The PreToolUse hook measures whoever calls a tool. Past the
    limit it refuses the call and has that agent write a complete brief (goal,
    state, decisions, what failed, exact references, next steps) to
    `~/.local/state/toomux/handovers/`. A subagent then ends with a `HANDOVER:`
    message, and its parent launches a fresh subagent of the same type from the
    brief. Read-only agents put the brief in their final message. A subagent's
    limit (`subagent_handover_tokens`, 250k) counts from what it was born with:
    a fork starts with its parent's whole context, so it gets half the limit
    of its own work first, and its successor is a fresh general-purpose
    subagent rather than another fork.
  - *Forks from big conversations.* A fork re-reads its caller's whole context
    on every call. Past `fork_context_tokens` (200k) the caller is refused the
    fork and asked to launch a general-purpose subagent with a complete brief
    instead. Measured over a day of real use, forks started past 240k cost 2.6
    times what the same calls would have starting fresh; below 180k they were
    close to even.
  - *The restart.* Once a main session's brief is written and its turn is over,
    it continues as a fresh conversation (same account, flags and name) that
    opens with the brief: in its own pane, or, for a session running in a
    plain terminal, in a tmux server of its own. A session idle
    45s past the limit with an empty prompt is asked for its brief. A
    suggested prompt (the dim text) counts as empty.
  - *Work in flight.* While a main session hands over, each of its subagents
    hands over too, and the brief lists them for relaunch. Background Bash
    commands run as toomux jobs, which outlive the session. The successor
    re-attaches with `toomux job follow <id>`.
  - *What you didn't see.* The fresh session takes the pane moments after
    the old one's last reply, often before you've read it. So toomux copies
    that reply, and every file sent since your last message, from the old
    transcript into the brief. The successor opens by giving them to you.
  - It's triggered by the Stop hook as well as the status tick, so it doesn't
    need a terminal attached. Starts and outcomes are logged to
    `handovers/log.jsonl`. A handover that met a typed prompt is retried a
    minute later. Each conversation hands over once, even if an old copy of
    it is still open somewhere else.

  Simulated on the same traffic: ~59% less weighted input. `toomux handover
  <target>` does it now.
- **Output capping.** A PreToolUse hook routes Bash commands through `toomux
  cap`. Output up to 4k characters comes back byte for byte; larger output is
  kept whole and the agent sees its head, the lines mentioning errors, its tail,
  and `toomux out <id> --lines/--grep/--chars` for the rest. File views (`cat`,
  `sed -n`, `head`, `git diff`, ...) and background commands are never touched:
  agents read those to edit them. ~2.4% saved, 2% of results touched.
- **Memory** (ported from AgentOS: FTS5 plus trigram fuzzy recall, fused by
  reciprocal rank; entries can be superseded). Every exchange of every session is
  indexed as it happens ("you asked" / "outcome", per project), as are handover
  briefs and kept outputs. So is every project's Claude Code memory folder
  (`projects/<folder>/memory/*.md`), one entry per file, kept as the file is:
  Claude Code loads only a session's own `MEMORY.md` index, and this lets any
  session search every project's topic files. Keep `MEMORY.md` itself to
  standing rules and one-line pointers; it is re-read on every call. Sessions
  get MCP tools `mem_search`, `mem_get`, `mem_save`, `mem_forget` and
  `output`; you get `toomux mem <words> [--all]`.

`init --apply` installs the hooks (PreToolUse for every tool, Stop) in each account's settings (backup
`settings.json.pre-toomux-hooks`) and registers the MCP server at user scope. To
undo: restore the backup and `claude mcp remove -s user toomux` per account.

## Voyages

`/voyage <outcome>` keeps a session at an outcome, turn after turn, until it's
done. `init --apply` adds the command to each account (`commands/voyage.md`);
toomux's prompt and Stop hooks do the work.

```
/voyage the auth tests pass                      set one (a new one replaces the old)
/voyage the docs build --check "make docs"       done only once the command exits 0
/voyage the migration is written --budget 40     pause once $40 is spent
/voyage                                          how it's going
/voyage clear                                    end it
/voyage resume                                   carry on after a pause (lifts a spent budget)
/voyage the parser is done --persistence hard    how hard it pushes (light, steady, hard, relentless)
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
  the limit lifts and it has sat a minute at an empty prompt.
- **Persistence** sets how hard it pushes and how hard done is to reach. Each
  level keeps everything of the one below. `voyage_persistence` in the config
  sets the default (`steady`).

  | Level | Judge | Before done counts | Stuck, or stopping |
  |---|---|---|---|
  | light | `voyage_judge_model`; a clear claim of done is enough | the judge's word | pauses after 2 idle turns, 2 failed checks or 4 flat checks |
  | steady | `voyage_judge_model`; wants evidence shown | the judge's word | a nudge after 4 flat checks, a pause after 8; 4 idle turns; 3 failed checks |
  | hard | `voyage_hard_model` (sonnet); counts only evidence from these turns, and looks for skipped tests, stubs and hard-coded results | one proof lap: a clean re-run and a read over the changes | a nudge every 3 flat checks, a pause after 12; one push back before it stops for you or gives up; 6 idle turns |
  | relentless | `voyage_relentless_model` (opus), as strict as hard | two proof laps (the second tries to break it), then a sceptical review of `git diff` since the voyage began, committed or not | a nudge every 2 flat checks, a pause after 20; two push backs; 8 idle turns |

  A flat check is a *not yet* whose estimate doesn't beat the best so far. The
  first nudge asks the session to step back and try another way; later ones
  ask it to list what it has tried and why each failed, then pick something
  new.

  A proof lap: when the judge says met, the session is sent back to prove it,
  and the judge (told a lap was asked for) has to see it done and say met
  again. Any *not yet* starts the laps over. A
  push back: *needs you* or *impossible* sends the session back to settle it
  itself (a sensible, easy-to-undo choice) or find another way; only when the
  judge says it again does the voyage stop. `--check` runs first, on every
  met, so no lap is spent on a failing check; then the laps, then the review.
- **It stops for you** when the judge says it needs you, after a run of turns
  with no tool call or of flat checks, when its budget is spent, or when it can't be checked a few
  times running (the numbers are in the table above). You get a notice (and `notify_command`, with `TOOMUX_EVENT=voyage`).
  Your next message carries it on, except after a spent budget, which takes
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
  sextant blocks) in the two colours that fit it best, so it wants a terminal
  font or renderer that draws them (VTE, kitty, WezTerm, ghostty, iTerm2 and
  Windows Terminal do). `toomux voyage show` draws it too, and so does toomux's
  detail pane. The sidebar has no room for it: the session's row shows the chip
  and the judge's estimate ("58% there") instead.
  `voyage_scene = false` keeps to the chip.
- **Records:** `~/.local/state/toomux/voyages/<id>.json`, plus `<id>.log` with
  every check. `toomux voyage` lists them, `toomux voyage show <id>` tells the
  whole story, and `toomux voyage set <session> <outcome>` starts one from
  outside (a session waiting at an empty prompt starts on it at once).

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
  corrections) into topic files, MEMORY.md kept to standing rules and one
  line per topic. Its context is cached then, so it's the cheapest moment,
  and it knows best what it learned. The request carries upkeep's findings
  for that workspace: an index past 150 lines or 15KB is slimmed (word for
  word into topic files), and files citing gone paths are named. While it
  hands over, the gate lets it read and write that memory folder and
  nothing else beside the brief.

## Keys

In toomux (`?` shows these too):

| key | |
|---|---|
| type | filter by title, folder, account or state |
| `enter` / click | open the session live; reopen it if it isn't running |
| `ctrl-a` | move it to another account, same conversation, same pane |
| `ctrl-o` | bring a session running in a plain terminal tab into tmux |
| `ctrl-x` | close it |
| `ctrl-r` | rename it (in toomux, its tmux window, and Claude's `/rename`) |
| `ctrl-p` | pin it to `alt-1..9`; again to unpin |
| `ctrl-n` | start a new session in a recent folder |
| `ctrl-s` | clean up sessions idle for days |
| `ctrl-e` | reopen what was running before a restart |
| `ctrl-u` | usage: each account's 5-hour and weekly limits (clears a half-typed filter first) |
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

## Usage

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
  from `.credentials.json`, passed to curl on stdin, and never refreshed or
  written.
- **A session stopped by a limit**, from its transcript.

## States

| | |
|---|---|
| `◆` needs you | waiting on a permission prompt or dialog, or stopped by a usage limit (with its reset time) |
| `●` working | mid-turn |
| `◐` background | idle at the prompt while background shells or agents keep running |
| `●` finished | went idle within the last `finished_minutes` |
| `○` idle | settled work fades back |

Handing over shows in place of the state word: `handing over` (writing its
brief, or brief written and going when its turn ends), or `handover failed` with
the reason in a few words (for six hours, or until a retry succeeds).

## Sidebar

`alt-b` opens a narrow, always-on list at the left of the window. A click (or
enter) jumps to a session, and the sidebar comes along into that window. `alt-b`
in a window that has it hides it; anywhere else it moves it there.

## Pins

`ctrl-p` pins a conversation to the next free `alt-N`. Slots never renumber. A
pinned conversation that has exited stays listed under "pinned · not running",
and `alt-N` reopens it with its last account and launch flags.

## Notices

The tmux status bar shows per-account counts, and a quiet notice when a session
that worked for `notify_after_secs` finishes, or when any session starts needing
you (`● Engine done +1`). A notice clears when that pane is on screen or the
session goes back to work. `notify_command` can forward notices anywhere (for
example a phone push), with `TOOMUX_MESSAGE`, `TOOMUX_TITLE`, `TOOMUX_EVENT` and
`TOOMUX_SESSION_ID` in its environment.

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

## Accounts

Each Claude Code login is an account: a folder chosen with `CLAUDE_CONFIG_DIR`,
named whatever you like and kept wherever you like. toomux finds every `~/.claude`
and `~/.claude-*` that holds a login; the config lists them from then on.

Any account can stand alone, or join a group of accounts that share one history.
In a group, a conversation resumes under any of its accounts and they all read the
same memory. Accounts join and leave at any time, and nothing is lost either way.
Any mix works: all separate, all in one group, two sharing and one alone, or
several groups.

```sh
toomux account                   # each account, its folder, and who it shares with
toomux account setup             # a walk-through: names, folders, who shares
toomux account add <name>        # a new one at ~/.claude-<name> (--dir to choose)
toomux account add <name> --dir ~/old-claude   # bring in a folder that exists
toomux account rename <name> <new>             # the folder follows (--dir to choose)
toomux account share a b         # a and b share ~/.claude-shared
toomux account share c --group lab             # c joins another group, ~/.claude-shared-lab
toomux account unshare b         # b stands alone, with its own copy of what it saw
toomux account unshare b --fresh # ... or starting with no history
toomux account remove <name>     # toomux stops using it; --delete deletes the folder
```

What a group shares, as links from each account into the group's folder:
conversations and memory (`projects`), settings, `CLAUDE.md`, history, skills,
agents, commands, plugins, plans, todos, file history and Claude Code's caches.
Never shared: `.credentials.json` and `.claude.json`, the login itself. Only what
toomux lists is shared, so anything Claude Code adds later stays each account's own
until it is listed.

Joining moves rather than copies, so it wants Claude closed on that account
(`--force` goes ahead anyway), and `--dry-run` shows the plan first. Folders are
merged; a file both have with other contents is kept as `name.from-<account>.md`;
history is joined; settings are combined unless a setting differs, in which case
that account keeps its own and you're told. Leaving copies what the account saw
(settings always come along). Renaming moves the folder when it was named
`~/.claude-<name>` and carries the new name into rules, pins and the restart record.

## Moving a session between accounts

All accounts share one data dir, so a conversation can be resumed under any of
them. `ctrl-a` (or `toomux switch <name> --to <account>`):

1. waits for the session to be idle (a working session is queued, shown as
   "moves to X when idle", and moves as soon as it goes idle; `ctrl-a` again
   cancels). A session parked on a usage-limit dialog can move at once,
2. exits it with ctrl-c, the way you would,
3. relaunches `claude --resume <id>` in the same pane under the other account,
   keeping launch flags like `--model` and `--dangerously-skip-permissions`.

When several sessions on an account are stopped at its limit, `ctrl-a` offers to
move them all (`toomux switch --limited` from the CLI).

Folder trust is recorded per account. If the current account already trusts the
folder, the same decision is recorded for the target account so the resumed session
doesn't stop at the trust prompt. toomux never grants trust you haven't given.

## New sessions

`ctrl-n` lists folders by how much and how recently you've used Claude in them
(or type a path). The account comes from the most specific `[[rules]]` entry, else
the account sessions there already use, else one that isn't at its limit. Launch
flags are the ones most of your running sessions use, unless `new_session_args`
says otherwise.

## A tmux server per session

Every session toomux starts, reopens or brings into tmux runs in a tmux server of
its own (`tmux -L toomux-<id>`), so nothing done to one server (a `kill-server`, a
crash) takes the others down with it. toomux lists panes across your default
server and all of its own, and carries each pane as `%N@<server>`. A session in
a server of your own making (`tmux -L work`, or `-S /some/socket`, carried as
`%N@/some/socket`) is found by the `TMUX` its process started with. Jumping to a
session on another server moves your terminal over (`detach-client -E` then
`attach`); the full-screen view reconnects its control client to that server.

Its servers start from `~/.local/state/toomux/server.tmux.conf`: your
`~/.tmux.conf`, rewritten on each start without tmux-resurrect, tmux-continuum and
tpm (your other plugins are run directly). Those assume a single server, and in a
new one continuum would restore your whole saved layout into it.

### Who looks after what

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
is up), and says so in one line; `reopen_after_restart = false` in the config only
offers them. Sessions that vanish while the machine stays up (more than half at
once, gone for two minutes: a tmux server went down) are offered, not reopened.
Offered sessions show under "before the restart"; `ctrl-e` reopens them all, enter
reopens one, `ctrl-x` stops offering one. `toomux reopen <id|name>...` does the
same from the command line, for these, pins, and anything last recorded running.

## Setup

Needs Linux or macOS (on Windows, inside WSL 2), tmux 3.2 or later, and Claude Code.
On macOS, `brew install meshbergio/tap/toomux` installs toomux and tmux together,
and `brew upgrade` keeps it current. Or use the installer below, with
`brew install tmux` if you haven't got it.

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/meshbergio/toomux/master/install.sh | sh
toomux init --apply      # tmux and Claude Code set up; lists what toomux does by itself
toomux account setup     # optional: name your accounts, choose which share history
```

The installer puts a binary for your machine (Linux or macOS, x86_64 or arm64) in
`~/.local/bin`, checked against its published checksum, and changes nothing else.
On Linux it's static, so any distro will do.
With npm instead: `npm install -g toomux` installs the same binary for your machine
(from `@toomux/<os>-<cpu>`); it doesn't bring tmux.
From source instead: `cargo install --git https://github.com/meshbergio/toomux`
(Rust 1.88 or later), or `cargo install --path .` in a clone.

On Windows, toomux runs inside WSL 2, the same as tmux. It sees the Claude Code
you run in WSL, with its own `~/.claude`, not one installed on the Windows side.

On macOS, Claude Code keeps each account's sign-in in the login Keychain, not in a
file. toomux reads it the way Claude Code does (through `security`), only to show
usage. Your usual `~/.claude` account launches without `CLAUDE_CONFIG_DIR`, so it
uses the same Keychain entry as a plain `claude`; every other account has it set.

`init --apply` edits `~/.tmux.conf` between `# >>> toomux >>>` markers (a backup is
kept at `~/.tmux.conf.pre-toomux`). In each account's Claude Code `settings.json` it
sets the `statusLine` to `toomux statusline`, refreshed every second (a status
line of your own is left alone), and sets `CLAUDE_CODE_TMUX_TRUECOLOR`, without
which Claude Code keeps to 256 colours inside tmux. It adds three hooks (`toomux hook pre-tool`, `stop`, `prompt`), with backups
beside it; and it registers the `toomux` MCP server at user scope. It also clears
Claude Code runtime variables (`CLAUDE_CODE_CHILD_SESSION` and friends) from the
tmux server's environment. They leak in when the tmux server is started from inside
a Claude session, and every new pane would inherit them.

### What toomux does by itself

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
| Voyages: a session with a `/voyage` is checked after every turn and sent back until it's done | `/voyage clear` (only runs when you set one) |

Always on: every conversation is indexed into one memory that all sessions search.

`toomux where` lists every place toomux reads or writes, with sizes:

- `~/.config/toomux/`: the config.
- `~/.local/state/toomux/`: memory, usage, handovers, kept outputs, reports.
  Everything here can be rebuilt or lost.
- `~/.local/share/toomux/`: the transcript archive, the one thing worth backing up.
- `$XDG_RUNTIME_DIR/toomux/`: sockets and queues, gone at reboot.

Set `TOOMUX_HOME` (in your shell profile, before tmux starts) to keep config, state
and data under one folder instead. Claude Code's own memory folders stay where
Claude Code reads them, in each account's `projects/<workspace>/memory/`.

`toomux uninstall` takes back what `init --apply` added, found by its markers, so
anything you changed since stays as you left it: the tmux block and status
segment, the status line, the hooks, the MCP server, the `/voyage` command. A backup of each file it edits
is kept as `*.pre-toomux-uninstall`. `--dry-run` shows what it would do; `--purge`
also deletes toomux's config, state and archive, and init's backups. Your memory
folders, and any status line or hooks of your own, are never touched.

## CLI

```
toomux                          full screen (alt-s in tmux)
toomux sidebar [--toggle]       the always-on list
toomux list [--json]            running sessions
toomux status                   tmux status-bar segment (also runs notices and the restart record)
toomux jump <target>            pid, session-id prefix, or name
toomux jump --pin <n>           a pin, reopening it if needed
toomux switch <target> [--to <account>] [--wait] [--force]
toomux switch --limited [--to <account>]
toomux adopt <target>
toomux reopen <target>...       conversations that aren't running, each in its own server
toomux usage [--json] [--fetch] each account's 5-hour and weekly usage
toomux statusline               Claude Code status line command (reads its JSON on stdin)
toomux handover <target>        hand a session over to a fresh one now
toomux jobs                     background commands toomux is looking after
toomux job follow|log|stop <id> [--tail N]   re-attach to, read or stop one
toomux out <id> [--lines a-b] [--grep text] [--chars a-b]   a kept output
toomux mem [words] [--all]      search memory (no words: how many entries)
toomux graph [--json] [--open] [--at <id>]   the memory graph: counts, JSON, or in the browser
toomux turn <id>                one conversation turn whole, by its memory id
toomux upkeep [--dry-run]       archive transcripts, check memory against the disk (hourly)
toomux where                    every place toomux reads or writes, and what init changed
toomux init [--apply]           write the config; with --apply, set up tmux and each account
toomux uninstall [--dry-run] [--purge]   take back what init added
toomux tokens [--since 24h|7d|"2026-09-29 21:43"] [--session <prefix>] [--json]
                                where the tokens go, in dollars at API list prices
toomux digest [--day 2026-09-29]  a day's report; made each morning after 8, with a one-line notice
toomux voyage [list]             open voyages, then the last ones that ended
toomux voyage show <id>          a voyage and every check it has had
toomux voyage set <target> <outcome> [--check "<cmd>"] [--budget <dollars>]
toomux voyage clear <id|session>
```

## Colours

Four statuses (attention rose, working amber, finished emerald, idle slate) and
one accent (sky) carry meaning; everything else is a neutral. `text`, `dim` and
`muted` step down for less important words and `faint` is only ever a hairline.
toomux paints its own grounds, so it looks the same on any terminal: `raised`
for the header and footer bands, `base` for the list, `well` for the live
preview, `overlay` for pop-overs (which dim everything behind them), plus
`selection` and `hover`. All of them are in `[colors]`; the tests check text
contrast on every surface.

## How it works

No daemon. toomux reads Claude Code's live session registry
(`<config>/sessions/<pid>.json`), joins it with the process table (`/proc` on Linux, libproc and sysctl on macOS; account from
`CLAUDE_CONFIG_DIR`, tty, liveness via kernel start time) and `tmux list-panes`,
and watches the registry for changes. Titles, PR links and usage-limit state come
from the tail of each transcript, read incrementally. Sessions outside tmux get a
preview from their transcript.

Notices and the restart record are kept by `toomux status`, which tmux already
runs every couple of seconds for the status bar. So are indexing, hourly
upkeep, handovers, voyages and the daily digest. A tmux client in control mode,
as full-screen toomux is, draws no status bar, so tmux never runs it there:
full-screen toomux runs `toomux status` itself every 2 seconds instead. State lives in
`~/.local/state/toomux` (pins, names, the restart record, usage) and
`$XDG_RUNTIME_DIR/toomux` (queued moves, notices).

## Licence

Either of [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
