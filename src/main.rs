mod account_cli;

use toomux::setup::{BEGIN, END};
use toomux::{actions, paths, setup, archive, capture, config, handover, index, jobs, mcp, memory, queue, voyage, registry, snapshot, state, tmux, tokens, ui, upkeep, usage, scene, watch, hygiene};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use registry::State;

#[derive(Parser)]
#[command(name = "toomux", version, about = "A calm control center for every Claude Code session on this machine")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List running sessions
    List {
        #[arg(long)]
        json: bool,
    },
    /// One-line summary for the tmux status bar
    Status,
    /// Jump to a session (pid, session-id prefix, or name), or a pin
    Jump {
        #[arg(required_unless_present = "pin")]
        target: Option<String>,
        /// Pin number 1-9; reopens the conversation if it isn't running
        #[arg(long)]
        pin: Option<usize>,
    },
    /// Name a session (pid, session-id prefix, or name); an empty name clears toomux's
    Rename {
        target: String,
        name: Vec<String>,
        /// A name chosen for you to describe the work (by an agent): the
        /// next handover names the successor for the work as it is then.
        /// Without it the name is yours and stays.
        #[arg(long)]
        for_you: bool,
    },
    /// Resume a session's conversation under another account
    Switch {
        #[arg(required_unless_present = "limited")]
        target: Option<String>,
        /// Move every session that's stopped at its account's usage limit
        #[arg(long, conflicts_with_all = ["target", "wait", "force"])]
        limited: bool,
        /// Account name; defaults to the other account when there are two
        #[arg(long)]
        to: Option<String>,
        /// Wait for the session to go idle instead of refusing
        #[arg(long)]
        wait: bool,
        /// Report the outcome in the tmux status line
        #[arg(long)]
        notify: bool,
        /// Switch even though background tasks are running (they will stop)
        #[arg(long)]
        force: bool,
    },
    /// A narrow, always-on session list in a tmux pane (alt-b toggles it)
    Sidebar {
        /// Show it in the current window, or hide it if it's already here
        #[arg(long)]
        toggle: bool,
    },
    /// Move a session running in a plain terminal into tmux
    Adopt { target: String },
    /// Reopen conversations that aren't running (lost with their tmux
    /// server, from before a restart, or pinned), each in a tmux server of
    /// its own: session-id prefix or name
    Reopen {
        targets: Vec<String>,
        /// Everything from before the restart, once (the status tick runs
        /// this after a reboot)
        #[arg(long)]
        after_restart: bool,
    },
    /// Each account's 5-hour and weekly usage
    Usage {
        #[arg(long)]
        json: bool,
        /// Look up accounts with no recent report now (normally automatic)
        #[arg(long)]
        fetch: bool,
    },
    /// The full-screen toomux (what `toomux` and alt-s open)
    Shell {
        /// Running in alt-s's popup: alt-j closes it on a session
        #[arg(long, hide = true)]
        popup: bool,
    },
    /// Part of a long command output toomux kept whole
    Out {
        id: String,
        /// A line range, e.g. 40-120
        #[arg(long)]
        lines: Option<String>,
        /// Only lines containing this text
        #[arg(long)]
        grep: Option<String>,
        /// A character range, for output that is one long line
        #[arg(long)]
        chars: Option<String>,
    },
    /// The memory graph: projects, memory files, sessions, briefs and recalls
    Graph {
        /// Open it in the browser
        #[arg(long)]
        open: bool,
        /// Print it as JSON
        #[arg(long)]
        json: bool,
        /// Start on this node (its id, as --json shows it)
        #[arg(long)]
        at: Option<String>,
    },
    /// Search the memory every session shares
    Mem {
        query: Vec<String>,
        /// Every project, not just this one
        #[arg(long)]
        all: bool,
        /// Delete one memory for good (its id, as a search shows it)
        #[arg(long, value_name = "ID")]
        forget: Option<String>,
    },
    #[command(hide = true)]
    Cap {
        id: String,
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
        #[arg(long)]
        store: Option<std::path::PathBuf>,
    },
    #[command(hide = true)]
    Hook { event: String },
    #[command(hide = true)]
    Mcp,
    /// Start due handovers after each of these many seconds (used by the Stop hook)
    #[command(hide = true)]
    Tick { after: Vec<u64> },
    /// Hand a session over to a fresh one now (normally automatic)
    Handover { target: String },
    /// Background commands toomux is looking after
    Jobs,
    /// One background command: follow, log, stop (run and host are internal)
    Job {
        action: String,
        id: String,
        /// Only the last N lines first (follow, log)
        #[arg(long)]
        tail: Option<usize>,
        /// Follow on from where the last follower stopped, waiting a few
        /// minutes at most (follow)
        #[arg(long)]
        more: bool,
        /// Wait at most this many seconds, then leave it running
        #[arg(long, hide = true)]
        until: Option<u64>,
    },
    /// Where the tokens went: cost by kind, by who, cache writes by why, bash output held
    Tokens {
        /// How far back: 90m, 24h, 7d, or a local time like 2026-09-29 21:43
        #[arg(long, default_value = "24h")]
        since: String,
        /// Only this conversation (a session-id prefix) and its subagents
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Voyages: a session kept at an outcome until it's done (/voyage in Claude Code sets one)
    Voyage {
        #[command(subcommand)]
        what: Option<VoyageCmd>,
    },
    /// A day's token report (yesterday unless --day), kept once made
    Digest {
        /// A local date like 2026-09-29
        #[arg(long)]
        day: Option<String>,
        /// Make it again, and send its line as a notice (the daily tick does)
        #[arg(long)]
        announce: bool,
    },
    /// Index conversations into memory (normally automatic)
    #[command(hide = true)]
    Index,
    /// Look after what toomux keeps: archive transcripts, check memory against the disk (hourly by itself)
    Upkeep {
        /// Say what it would change, change nothing
        #[arg(long)]
        dry_run: bool,
        /// The hourly run: repositories only once a day
        #[arg(long, hide = true)]
        hourly: bool,
    },
    /// One conversation turn whole, every message and tool call, by its memory id
    Turn { id: String },
    /// Claude Code status line command: records usage, prints a quiet line
    #[command(hide = true)]
    Statusline,

    /// Write the default config, and with --apply add the tmux bindings
    Init {
        #[arg(long)]
        apply: bool,
    },
    /// Every place toomux reads or writes, and what `init --apply` changed
    Where,
    /// Claude Code accounts that share one history: list, add, share
    Account {
        #[command(subcommand)]
        what: Option<account_cli::AccountCmd>,
    },
    /// Take back what `init --apply` added; with --purge, remove what toomux keeps too
    Uninstall {
        /// Say what it would change, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Also delete toomux's config, state and transcript archive
        #[arg(long)]
        purge: bool,
    },
}

#[derive(Subcommand)]
enum VoyageCmd {
    /// Every open voyage, then the last ones that ended
    List,
    /// A voyage and every check it has had
    Show { id: String },
    /// Set a voyage on a running session (pid, session-id prefix, or name);
    /// an idle one starts on it at once
    Set {
        target: String,
        /// What done looks like; --check "<command>", --budget <dollars> and --persistence <light|steady|hard|relentless> may follow
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        outcome: Vec<String>,
    },
    /// End a voyage (its id, or its session)
    Clear { target: String },
    /// Draw a voyage's scene, for a look at it
    #[command(hide = true)]
    Scene {
        /// 0 to 1, how far along
        progress: f64,
        /// sailing, anchored or landed
        #[arg(long, default_value = "sailing")]
        sea: String,
        #[arg(long, default_value_t = 0)]
        frame: u64,
        #[arg(long, default_value_t = 80)]
        width: usize,
    },
}

fn main() -> Result<()> {
    // Behave like a normal CLI when piped into head/grep.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    // What runs inside every session (hooks, the status line, output capture,
    // the MCP server, background jobs) must keep working while the config is
    // mid-edit: it runs on defaults, with nothing handed over. Commands you
    // run yourself say what's wrong.
    let unattended = matches!(
        cli.cmd,
        Some(Cmd::Hook { .. } | Cmd::Cap { .. } | Cmd::Statusline | Cmd::Mcp | Cmd::Job { .. } | Cmd::Tick { .. } | Cmd::Index | Cmd::Out { .. })
    );
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(_) if unattended => Config { handover_tokens: 0, ..Config::default() },
        Err(e) => return Err(e),
    };
    match cli.cmd {
        // In a terminal, inside tmux or not, toomux is the whole screen.
        None if std::io::IsTerminal::is_terminal(&std::io::stdout()) => ui::run_shell(cfg, false),
        None => ui::run(cfg, false),
        Some(Cmd::Shell { popup }) => ui::run_shell(cfg, popup),
        Some(Cmd::Sidebar { toggle: true }) => actions::toggle_sidebar(),
        Some(Cmd::Sidebar { toggle: false }) => {
            if let Ok(me) = std::env::var("TMUX_PANE") {
                let _ = tmux::run(&["set-option", "-p", "-t", &me, "@toomux_sidebar", "1"]);
                let _ = tmux::run(&["select-pane", "-t", &me, "-T", "toomux"]);
            }
            ui::run(cfg, true)
        }
        Some(Cmd::List { json }) => list(&cfg, json),
        Some(Cmd::Status) => {
            print!("{}", status_line(&cfg));
            Ok(())
        }
        Some(Cmd::Jump { target, pin }) => {
            let all = registry::load(&cfg);
            match (pin, target) {
                (Some(n), _) => {
                    let r = jump_pin(&cfg, &all, n);
                    if let Err(e) = &r {
                        actions::notify(&e.to_string());
                    }
                    r
                }
                (None, Some(t)) => actions::jump(registry::find(&all, &t)?),
                (None, None) => unreachable!(),
            }
        }
        Some(Cmd::Switch { limited: true, to, notify, .. }) => {
            let all = registry::load(&cfg);
            let stuck: Vec<&registry::Session> = all.iter().filter(|s| s.limit.is_some()).collect();
            if stuck.is_empty() {
                println!("no session is at its usage limit");
                return Ok(());
            }
            let mut failed = 0;
            for s in stuck {
                let account = match &to {
                    Some(name) => cfg.account_by_name(name).with_context(|| format!("no account named '{name}'"))?,
                    None => match (0..cfg.accounts.len()).filter(|&i| Some(i) != s.account).collect::<Vec<_>>().as_slice() {
                        [one] => *one,
                        _ => bail!("say which account with --to ({})", names(&cfg)),
                    },
                };
                let r = actions::switch(&cfg, s, account, false, false);
                let msg = match &r {
                    Ok(m) => m.clone(),
                    Err(e) => {
                        failed += 1;
                        format!("couldn't move {}: {e}", s.title)
                    }
                };
                if notify {
                    actions::notify(&msg);
                }
                println!("{msg}");
            }
            if failed > 0 {
                bail!("{failed} could not be moved");
            }
            Ok(())
        }
        Some(Cmd::Switch { target, to, wait, notify, force, .. }) => {
            let all = registry::load(&cfg);
            let s = registry::find(&all, target.as_deref().unwrap_or_default())?;
            let account = match to {
                Some(name) => cfg.account_by_name(&name).with_context(|| format!("no account named '{name}'"))?,
                None => {
                    let others: Vec<usize> = (0..cfg.accounts.len()).filter(|&i| Some(i) != s.account).collect();
                    match others.as_slice() {
                        [one] => *one,
                        _ => bail!("say which account with --to ({})", names(&cfg)),
                    }
                }
            };
            if wait {
                queue::put(s.pid, s.proc_start.as_deref(), &cfg.accounts[account].name);
            }
            let r = actions::switch(&cfg, s, account, wait, force);
            if wait {
                queue::done(s.pid);
            }
            if notify {
                actions::notify(&match &r {
                    Ok(m) => m.clone(),
                    Err(e) => format!("couldn't move {}: {e}", s.title),
                });
            }
            println!("{}", r?);
            Ok(())
        }
        Some(Cmd::Rename { target, name, for_you }) => {
            let all = registry::load(&cfg);
            let s = registry::find(&all, &target)?;
            println!("{}", actions::rename(&cfg, s, &name.join(" "), for_you)?);
            Ok(())
        }
        Some(Cmd::Adopt { target }) => {
            let all = registry::load(&cfg);
            println!("{}", actions::adopt(&cfg, registry::find(&all, &target)?)?);
            Ok(())
        }
        Some(Cmd::Reopen { after_restart: true, .. }) => {
            if !snapshot::claim_reopen() {
                return Ok(());
            }
            let live = registry::load(&cfg);
            let mut ok = Vec::new();
            let mut failed = Vec::new();
            for s in registry::restorable(&cfg, &live) {
                match actions::revive(&cfg, &s) {
                    Ok(_) => ok.push(s.id.clone()),
                    Err(e) => failed.push(format!("{}: {e}", s.title)),
                }
            }
            snapshot::forget(&ok);
            if !ok.is_empty() || !failed.is_empty() {
                let mut msg = format!("reopened {} {} from before the restart", ok.len(), if ok.len() == 1 { "session" } else { "sessions" });
                for f in &failed {
                    msg.push_str(&format!(" · {f}"));
                }
                println!("{msg}");
                watch::announce_text(&cfg, &msg, "restart", "", "");
            }
            Ok(())
        }
        Some(Cmd::Reopen { targets, .. }) => {
            let live = registry::load(&cfg);
            let gone = registry::reopenable(&cfg, &live);
            let mut failed = 0;
            for t in &targets {
                let r = registry::find(&gone, t).and_then(|s| {
                    let pane = actions::revive(&cfg, s)?;
                    snapshot::forget(std::slice::from_ref(&s.id));
                    Ok(format!("{} reopened ({pane})", s.title))
                });
                match r {
                    Ok(m) => println!("{m}"),
                    Err(e) => {
                        failed += 1;
                        eprintln!("{t}: {e}");
                    }
                }
            }
            if failed > 0 {
                bail!("{failed} could not be reopened");
            }
            Ok(())
        }
        Some(Cmd::Init { apply }) => init(apply),
        Some(Cmd::Where) => where_(),
        Some(Cmd::Account { what }) => account_cli::run(what.unwrap_or(account_cli::AccountCmd::List)),
        Some(Cmd::Uninstall { dry_run, purge }) => uninstall(dry_run, purge),
        Some(Cmd::Statusline) => {
            print!("{}", usage::statusline(&cfg));
            Ok(())
        }
        Some(Cmd::Usage { json, fetch }) => usage_cmd(&cfg, json, fetch),
        Some(Cmd::Out { id, lines, grep, chars }) => {
            print!("{}", capture::out(&id, lines.as_deref(), grep.as_deref(), chars.as_deref())?);
            Ok(())
        }
        Some(Cmd::Cap { id, dir, store }) => capture::cap(&id, dir, store),
        // A voyage's judge is a `claude -p` of its own: its hooks do nothing.
        Some(Cmd::Hook { .. }) if std::env::var_os(voyage::JUDGE_ENV).is_some() => Ok(()),
        Some(Cmd::Hook { event }) => match event.as_str() {
            "pre-tool" => capture::pre_tool_hook(&cfg),
            "stop" => {
                let mut raw = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw)?;
                // A handover first; else a voyage judges the turn.
                if let Some(out) = serde_json::from_str::<serde_json::Value>(&raw).ok().and_then(|v| handover::stop_hook(&cfg, &v).or_else(|| voyage::stop_hook(&cfg, &v))) {
                    print!("{out}");
                }
                Ok(())
            }
            "prompt" => {
                let mut raw = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw)?;
                if let Some(out) = serde_json::from_str::<serde_json::Value>(&raw).ok().and_then(|v| voyage::prompt_hook(&cfg, &v).or_else(|| handover::prompt_hook(&cfg, &v))) {
                    print!("{out}");
                }
                Ok(())
            }
            _ => Ok(()),
        },
        Some(Cmd::Mcp) => mcp::serve(),
        Some(Cmd::Tick { after }) => {
            handover::tick(&cfg, &after);
            Ok(())
        }
        Some(Cmd::Handover { target }) => {
            let all = registry::load(&cfg);
            let pid = registry::find(&all, &target)?.pid;
            println!("{}", handover::run(&cfg, pid)?);
            Ok(())
        }
        Some(Cmd::Jobs) => {
            let now = registry::now_ms();
            for j in jobs::all().into_iter().take(40) {
                println!(
                    "{:8} {:8} {:>6} ago  {}  {}",
                    j.id,
                    j.status,
                    registry::ago(now - j.started_ms),
                    &j.session[..8.min(j.session.len())],
                    j.command.lines().next().unwrap_or("")
                );
            }
            Ok(())
        }
        Some(Cmd::Job { action, id, tail, more, until }) => match action.as_str() {
            "run" => std::process::exit(jobs::run(&id, until.map(std::time::Duration::from_secs))?),
            "host" => jobs::host(&id),
            "follow" => std::process::exit(jobs::follow(&id, tail, until.map(std::time::Duration::from_secs), more)?),
            "stop" => {
                println!("{}", jobs::stop(&id)?);
                Ok(())
            }
            "log" => {
                print!("{}", jobs::log(&id, tail)?);
                Ok(())
            }
            other => bail!("no job action {other} (follow, log, stop)"),
        },
        Some(Cmd::Tokens { since, session, json }) => {
            let since = tokens::parse_since(&since, chrono::Utc::now().timestamp_millis())?;
            let r = tokens::report(&cfg, since, session.as_deref());
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                let colour = std::io::IsTerminal::is_terminal(&std::io::stdout()) && std::env::var_os("NO_COLOR").is_none();
                print!("{}", tokens::render(&r, chrono::Utc::now().timestamp_millis(), colour.then_some(&cfg.colors)));
            }
            Ok(())
        }
        Some(Cmd::Voyage { what }) => {
            let now = registry::now_ms();
            match what.unwrap_or(VoyageCmd::List) {
                VoyageCmd::List => print!("{}", voyage::list(now)),
                VoyageCmd::Show { id } => {
                    use std::io::IsTerminal;
                    let width = std::io::stdout().is_terminal().then(|| crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(80));
                    print!("{}", voyage::show(&id, now, width)?)
                }
                VoyageCmd::Set { target, outcome } => {
                    // Words as typed; a --check command keeps its spaces.
                    let words: Vec<String> = outcome.iter().enumerate().map(|(i, w)| if i > 0 && outcome[i - 1] == "--check" { format!("\"{w}\"") } else { w.clone() }).collect();
                    println!("{}", voyage::set_from_outside(&cfg, &target, &words.join(" "))?)
                }
                VoyageCmd::Clear { target } => println!("{}", voyage::clear(&target)?),
                VoyageCmd::Scene { progress, sea, frame, width } => {
                    let sea = match sea.as_str() {
                        "anchored" => scene::Sea::Anchored,
                        "landed" => scene::Sea::Landed,
                        _ => scene::Sea::Sailing,
                    };
                    let at = scene::Scene { progress, sea, frame };
                    for l in scene::render(width, &at) {
                        println!("{l}");
                    }
                }
            }
            Ok(())
        }
        Some(Cmd::Digest { day, announce }) => {
            let day = match day {
                Some(d) => chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").with_context(|| format!("'{d}' isn't a date like 2026-09-29"))?,
                None => chrono::Local::now().date_naive().pred_opt().context("no yesterday")?,
            };
            match (announce, tokens::kept(day)) {
                (false, Some(text)) => print!("{text}"),
                _ => {
                    let (text, line) = tokens::digest(&cfg, day)?;
                    if announce {
                        if let Some(l) = line {
                            watch::announce_text(&cfg, &l, "digest", "", "");
                        }
                    } else {
                        print!("{text}");
                    }
                }
            }
            Ok(())
        }
        Some(Cmd::Index) => {
            println!("{} added", index::run(&cfg)?);
            Ok(())
        }
        Some(Cmd::Upkeep { dry_run, hourly }) => {
            let repos = !hourly || due_every("hygiene.stamp", 24 * 3_600_000, registry::now_ms());
            let text = upkeep(&cfg, !dry_run, repos)?;
            if !dry_run {
                let _ = std::fs::write(memory::path().with_file_name("upkeep.txt"), &text);
                // What it changed by itself, in one notice; findings wait to be asked for.
                let changed: Vec<&str> = text
                    .lines()
                    .filter(|l| !l.contains("would be") && (l.contains(" corrected in ") || l.contains(" brought from ") || l.contains(" removed and ")))
                    .collect();
                if hourly && !changed.is_empty() {
                    let msg = format!("upkeep · {} changes to memory and worktrees, each kept · toomux upkeep --dry-run for the rest", changed.len());
                    watch::announce_text(&cfg, &msg, "upkeep", "", "");
                }
            }
            print!("{text}");
            Ok(())
        }
        Some(Cmd::Turn { id }) => {
            let source = if id.starts_with("session:") { id } else { memory::Memory::open()?.get(&id)?.ok_or_else(|| anyhow::anyhow!("no memory {id}"))?.source };
            println!("{}", index::whole_turn(&cfg, &source)?);
            Ok(())
        }
        Some(Cmd::Graph { open, json, at }) => {
            let g = toomux::graph::build(&memory::Memory::open()?)?;
            if json {
                println!("{}", serde_json::to_string(&g)?);
            } else if open {
                println!("{}", toomux::graph::open_page(&g, at.as_deref())?.display());
            } else {
                use toomux::graph::Kind;
                let n = |k| g.count(k);
                println!(
                    "{} projects · {} memory files ({} indexes) · {} notes · {} sessions · {} handover briefs · {} turns · {} kept outputs\n{} links · toomux graph --open draws it",
                    n(Kind::Project), n(Kind::File) + n(Kind::Index), n(Kind::Index), n(Kind::Note), n(Kind::Session), n(Kind::Handover), n(Kind::Turn), n(Kind::Output), g.edges.len()
                );
            }
            Ok(())
        }
        Some(Cmd::Mem { query, all, forget }) => mem_cmd(&query.join(" "), all, forget.as_deref()),
    }
}

fn jump_pin(cfg: &Config, all: &[registry::Session], n: usize) -> Result<()> {
    if !(1..=state::SLOTS).contains(&n) {
        bail!("pins are numbered 1 to {}", state::SLOTS);
    }
    let st = state::State::load();
    let Some(pin) = st.pins[n - 1].clone() else { bail!("nothing is pinned to alt-{n} · ctrl-p in toomux pins a session") };
    if let Some(s) = all.iter().find(|s| s.id == pin.id) {
        return actions::jump(s);
    }
    let dormant = registry::dormant(cfg, all);
    let s = dormant.iter().find(|s| s.id == pin.id).context("pin vanished")?;
    let pane = actions::revive(cfg, s)?;
    actions::notify(&format!("reopening {}", s.title));
    actions::jump_pane(&pane)
}

/// Start `toomux index` about once a minute.
fn index_in_background(now: i64) {
    if !due_every("index.stamp", 60_000, now) {
        return;
    }
    jobs::prune();
    handover::prune();
    in_background(&["index"]);
}

/// Runs toomux with `args` detached from this process.
/// True once per `every_ms`, by the modified time of a stamp in the state dir.
fn due_every(stamp: &str, every_ms: i64, now: i64) -> bool {
    let stamp = memory::path().with_file_name(stamp);
    let last = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    if now - last < every_ms {
        return false;
    }
    let _ = std::fs::create_dir_all(stamp.parent().unwrap());
    std::fs::write(&stamp, b"").is_ok()
}

/// One upkeep pass, one at a time. Returns what it did, a line each.
fn upkeep(cfg: &Config, apply: bool, repos: bool) -> Result<String> {
    use std::os::fd::AsRawFd;
    let lock = std::fs::OpenOptions::new().create(true).append(true).open(memory::path().with_file_name("upkeep.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok("upkeep is already running\n".into());
    }
    let mb = |b: u64| format!("{:.0} MB", b as f64 / 1e6);
    let mut out = String::new();
    let places = upkeep::places(cfg);
    let m = upkeep::run(cfg, &places, apply)?;
    // Recalls from before they were kept, once.
    if apply {
        match toomux::graph::backfill(cfg) {
            Ok(0) => {}
            Ok(n) => out.push_str(&format!("memory · {n} recalls read from past transcripts, for the memory graph\n")),
            Err(e) => out.push_str(&format!("memory · recalls from past transcripts failed: {e}\n")),
        }
    }
    let will = if apply && cfg.fix_memory { "" } else { "would be " };
    for (from, to, n) in &m.moved {
        out.push_str(&format!("memory · {n} files {will}brought from {from} to {to}, where that workspace is now\n"));
    }
    for (file, old, new) in &m.fixed {
        out.push_str(&format!("memory · {old} → {new} {will}corrected in {file}\n"));
    }
    let gone: usize = m.gone.values().map(Vec::len).sum();
    out.push_str(&format!(
        "memory · {} files cite {} paths; {gone} gone in {} files, {} temporary ones gone as expected\n",
        m.files,
        m.cited,
        m.gone.len(),
        m.temporary
    ));
    if !m.archives.is_empty() {
        out.push_str(&format!("memory · {} folders kept as archives, left where other memory points: {}\n", m.archives.len(), m.archives.join(", ")));
    }
    if !m.homeless.is_empty() {
        out.push_str(&format!("memory · {} folders belong to a workspace that's gone: {}\n", m.homeless.len(), m.homeless.join(", ")));
    }
    if repos {
        let home = config::home();
        let short = |p: &std::path::Path| p.strip_prefix(&home).map_or_else(|_| p.display().to_string(), |r| format!("~/{}", r.display()));
        let all = hygiene::run(&places, apply && cfg.tidy_worktrees);
        let will = if apply && cfg.tidy_worktrees { "" } else { "would be " };
        let mut detail = String::new();
        for r in &all {
            let mut said = Vec::new();
            if !r.removed.is_empty() {
                said.push(format!("{} merged, clean worktrees {will}removed", r.removed.len()));
            }
            if r.pruned > 0 {
                said.push(format!("{} whose folder is gone {will}pruned", r.pruned));
            }
            if r.kept > 0 {
                said.push(format!("{} worktrees kept (work not on the default branch, or changes)", r.kept));
            }
            if r.uncommitted > 0 {
                let age = if r.oldest_days > 0 { format!(", the oldest {} days", r.oldest_days) } else { String::new() };
                said.push(format!("{} uncommitted changes{age}", r.uncommitted));
            }
            if r.merged_branches > 0 {
                said.push(format!("{} branches already merged", r.merged_branches));
            }
            if !said.is_empty() {
                detail.push_str(&format!("{}: {}\n", short(&r.path), said.join(", ")));
            }
        }
        let tidied: Vec<_> = all.iter().filter(|r| !r.removed.is_empty() || r.pruned > 0).collect();
        if !tidied.is_empty() {
            let removed: usize = tidied.iter().map(|r| r.removed.len()).sum();
            let pruned: usize = tidied.iter().map(|r| r.pruned).sum();
            let names: Vec<String> = tidied.iter().map(|r| short(&r.path)).collect();
            out.push_str(&format!(
                "repos · {removed} merged, clean worktrees {will}removed and {pruned} whose folder is gone {will}pruned, in {}\n",
                names.join(", ")
            ));
        }
        let mut open: Vec<_> = all.iter().filter(|r| r.uncommitted > 0).collect();
        open.sort_by_key(|r| std::cmp::Reverse(r.uncommitted));
        if !open.is_empty() {
            let top: Vec<String> = open.iter().take(4).map(|r| format!("{} ({})", short(&r.path), r.uncommitted)).collect();
            out.push_str(&format!("repos · {} of {} hold uncommitted work, most in {}\n", open.len(), all.len(), top.join(", ")));
        }
        let list = memory::path().with_file_name("repos.txt");
        if std::fs::write(&list, &detail).is_ok() {
            out.push_str(&format!("repos · each one in {}\n", list.display()));
        }
    }
    if !apply || !cfg.archive_transcripts {
        return Ok(out);
    }
    let a = archive::run(cfg)?;
    let (n, bytes) = archive::size();
    if a.copied > 0 {
        out.push_str(&format!("archive · kept {} files, {} as {}\n", a.copied, mb(a.bytes_in), mb(a.bytes_out)));
    }
    out.push_str(&format!(
        "archive · holds {n} files in {}{}\n",
        mb(bytes),
        if a.waiting > 0 { format!(" · {} still changing or waiting their turn", a.waiting) } else { String::new() }
    ));
    Ok(out)
}

fn in_background(args: &[&str]) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

fn mem_cmd(query: &str, all: bool, forget: Option<&str>) -> Result<()> {
    let m = memory::Memory::open()?;
    if let Some(id) = forget {
        let e = m.forget(id)?;
        println!("deleted {}  {}", &e.id[..12], e.source);
        return Ok(());
    }
    if query.trim().is_empty() {
        println!("{} memories", m.count()?);
        return Ok(());
    }
    let here = std::env::current_dir()?.display().to_string();
    let scopes = if all { vec![] } else { vec![memory::project_scope(&here), "global".into()] };
    let now = registry::now_ms();
    let hits = m.search(&scopes, None, query, 12)?;
    for h in &hits {
        println!("{}  {}  {} ago\n    {}\n", &h.id[..12], h.source, registry::ago(now - h.created_ms), h.snippet.replace('\n', " "));
    }
    if hits.is_empty() {
        println!("nothing matches{}", if all { "" } else { " here · --all searches every project" });
    }
    Ok(())
}

fn usage_cmd(cfg: &Config, json: bool, fetch: bool) -> Result<()> {
    if fetch {
        for line in usage::fetch_due(cfg) {
            println!("{line}");
        }
    }
    let all = registry::load(cfg);
    let now = registry::now_ms();
    let u = usage::summary(cfg, &all, now);
    let window = |m: &Option<usage::Meter>| {
        m.as_ref().map(|m| serde_json::json!({"used": m.used, "resets_ms": m.resets_ms, "limited": m.limited}))
    };
    if json {
        let v: Vec<_> = cfg
            .accounts
            .iter()
            .zip(&u)
            .enumerate()
            .map(|(i, (a, u))| {
                serde_json::json!({
                    "account": a.name, "plan": usage::plan(cfg, i), "five_hour": window(&u.five), "seven_day": window(&u.week),
                    "reported_ms": u.at_ms, "source": u.source.map(|s| format!("{s:?}").to_lowercase()), "problem": u.problem,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for (a, u) in cfg.accounts.iter().zip(&u) {
        let fmt = |label: &str, m: &Option<usage::Meter>| match m {
            Some(m) if m.limited => format!("{label} at limit, resets in {}", registry::duration(m.resets_ms - now)),
            Some(m) if m.resets_ms > 0 => format!("{label} {:.0}%, resets in {}", m.used, registry::duration(m.resets_ms - now)),
            Some(_) => format!("{label} 0%"),
            None => format!("{label} unknown"),
        };
        let when = match u.at_ms {
            0 => "no report yet".to_string(),
            t if now - t < 60_000 => "just now".to_string(),
            t => format!("as of {} ago", registry::ago(now - t)),
        };
        println!("{:<10} {} · {} · {when}", a.name, fmt("5h", &u.five), fmt("week", &u.week));
        if let Some(p) = &u.problem {
            println!("{:<10} {p}", "");
        }
    }
    Ok(())
}

fn names(cfg: &Config) -> String {
    cfg.accounts.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
}

fn list(cfg: &Config, json: bool) -> Result<()> {
    let all = registry::load(cfg);
    let now = registry::now_ms();
    if json {
        let v: Vec<_> = all
            .iter()
            .map(|s| {
                serde_json::json!({
                    "pid": s.pid, "session_id": s.id, "name": s.name, "title": s.title, "topic": s.topic, "cwd": s.cwd,
                    "account": s.account_name(cfg), "state": s.state.section(),
                    "waiting_for": s.waiting_for, "pane": s.pane.as_ref().map(|p| &p.id), "tty": s.tty,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for s in &all {
        println!(
            "{:>8}  {:<10} {:<28} {:<34} {}",
            s.pid,
            s.account_name(cfg),
            s.status_text(now),
            s.title,
            s.pane.as_ref().map(|p| p.id.clone()).unwrap_or_else(|| "outside tmux".into())
        );
    }
    Ok(())
}

fn status_line(cfg: &Config) -> String {
    let all = registry::load(cfg);
    registry::sync_pins(cfg, &all);
    snapshot::record(cfg, &all, registry::now_ms());
    if snapshot::reopen_due() {
        in_background(&["reopen", "--after-restart"]);
    }
    for n in watch::update(cfg, &all, registry::now_ms()) {
        watch::announce(cfg, &n);
    }
    let notices = watch::notices();
    let now = registry::now_ms();
    let usage = usage::summary(cfg, &all, now);
    for msg in usage::crossed(cfg, &usage) {
        watch::announce_text(cfg, &msg, "usage", "", "");
    }
    usage::fetch_in_background(cfg, now);
    // Handovers, the conversation index and successors' names, all from the
    // tick tmux already runs.
    handover::start_due(cfg, &all, now);
    voyage::tick(&all, now);
    index_in_background(now);
    if due_every("upkeep.stamp", 3_600_000, now) {
        in_background(&["upkeep", "--hourly"]);
    }
    if let Some(day) = tokens::digest_due(now) {
        in_background(&["digest", "--announce", "--day", &day.to_string()]);
    }
    let c = &cfg.colors;
    let mut parts = Vec::new();
    for (i, a) in cfg.accounts.iter().enumerate() {
        let mine: Vec<_> = all.iter().filter(|s| s.account == Some(i)).collect();
        let u = &usage[i];
        if mine.is_empty() && u.tightest().is_none_or(|(_, m)| m.used < 50.0) {
            continue;
        }
        let n = |st: &[State]| mine.iter().filter(|s| st.contains(&s.state)).count();
        let mut seg = format!("#[fg={}]{}", c.dim, a.name);
        match u.tightest() {
            // News in its first minutes, then a quiet fact with one rose dot.
            Some((_, m)) if m.limited => {
                let fresh = mine.iter().any(|s| s.limit.is_some() && s.since_ms > 0 && now - s.since_ms < 10 * 60_000);
                let back = registry::back_at(m.resets_ms, now);
                if fresh {
                    seg.push_str(&format!(" #[fg={}]limit #[fg={}]{back}", c.attention, c.muted));
                } else {
                    seg.push_str(&format!(" #[fg={}]at limit #[fg={}]{back} #[fg={}]•", c.dim, c.muted, c.attention));
                }
            }
            Some((label, m)) if m.resets_ms > 0 => {
                let tone = if u.stale(now) {
                    &c.muted
                } else if m.used >= 95.0 {
                    &c.attention
                } else if m.used >= 80.0 {
                    &c.working
                } else {
                    &c.dim
                };
                seg.push_str(&format!(" #[fg={tone}]{:.0}%#[fg={}]·{label}", m.used, c.muted));
            }
            _ if mine.iter().any(|s| s.limit.is_some()) => seg.push_str(&format!(" #[fg={}]limit", c.attention)),
            _ => {}
        }
        for (count, glyph, color) in [
            (n(&[State::NeedsYou]), "◆", &c.attention),
            (n(&[State::Working]), "●", &c.working),
            (n(&[State::Background]), "◐", &c.dim),
            (n(&[State::Finished]), "●", &c.finished),
            (n(&[State::Idle]), "○", &c.muted),
        ] {
            if count > 0 {
                seg.push_str(&format!(" #[fg={color}]{glyph}{count}"));
            }
        }
        parts.push(seg);
    }
    // The newest notice by name; needing you outranks finishing.
    let top = notices.iter().rev().find(|n| n.kind == watch::Kind::NeedsYou).or(notices.last());
    if let Some(n) = top {
        let (glyph, color) = match n.kind {
            watch::Kind::NeedsYou => ("◆", &c.attention),
            watch::Kind::Finished => ("●", &c.finished),
        };
        let more = if notices.len() > 1 { format!(" #[fg={}]+{}", c.muted, notices.len() - 1) } else { String::new() };
        let what = match n.kind {
            watch::Kind::NeedsYou => "needs you",
            watch::Kind::Finished => "done",
        };
        parts.insert(
            0,
            format!("#[fg={color}]{glyph} #[fg={}]{} #[fg={}]{what}{more}", c.text, actions::window_name(&n.title), c.muted),
        );
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{}#[default]", parts.join(&format!("#[fg={}]  │  ", c.muted)))
    }
}


/// What toomux needs, checked before it changes anything: tmux 3.2 or later
/// (popups) and Claude Code where the config says it is.
/// tmux's version as (major, minor), and how it names itself.
fn tmux_version() -> Option<((u32, u32), String)> {
    let o = std::process::Command::new("tmux").arg("-V").output().ok()?;
    let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let n: Vec<u32> = v.trim_start_matches("tmux ").split(|c: char| !c.is_ascii_digit()).filter_map(|x| x.parse().ok()).collect();
    Some(((n.first().copied().unwrap_or(0), n.get(1).copied().unwrap_or(0)), v))
}

fn prerequisites() -> Vec<String> {
    let mut missing = Vec::new();
    match tmux_version() {
        Some((n, v)) if n.0 > 0 && n < (3, 2) => missing.push(format!("tmux 3.2 or later (this is {v})")),
        Some(_) => {}
        None => missing.push("tmux 3.2 or later (not found)".into()),
    }
    let cfg = Config::load().unwrap_or_default();
    let claude = config::expand(&cfg.claude_bin);
    let on_path = || std::process::Command::new("claude").arg("--version").output().is_ok_and(|o| o.status.success());
    if !claude.is_file() && !on_path() {
        missing.push(format!("Claude Code at {} (set claude_bin in the config)", config::tilde(&claude.display().to_string())));
    }
    missing
}

fn init(apply: bool) -> Result<()> {
    let missing = prerequisites();
    if !missing.is_empty() {
        println!("toomux needs: {}", missing.join("; "));
        if apply {
            bail!("nothing changed");
        }
    }
    let cfg_path = Config::path();
    if cfg_path.exists() {
        println!("config: {} (kept)", cfg_path.display());
    } else {
        std::fs::create_dir_all(cfg_path.parent().unwrap())?;
        std::fs::write(&cfg_path, Config::render_default())?;
        println!("config: wrote {}", cfg_path.display());
    }

    let bin = std::env::current_exe()?.display().to_string();
    // The whole terminal, status bar included: the same full screen as
    // outside tmux, drawn over whatever you were looking at. tmux 3.2 has
    // no borderless popups, so there it keeps a thin border.
    let borderless = tmux_version().is_none_or(|(n, _)| n >= (3, 3));
    let popup = format!("display-popup -E{} -w 100% -h 100% '{bin}' shell --popup", if borderless { " -B" } else { "" });
    let pins: String = (1..=state::SLOTS).map(|n| format!("bind -n M-{n} run-shell -b '{bin} jump --pin {n}'\n")).collect();
    let block = format!(
        "{BEGIN}\n# toomux: alt-s (or prefix + space) opens toomux full screen,\n# alt-b toggles the sidebar, alt-1..9 jump to pinned sessions\nbind -n M-s {popup}\nbind Space {popup}\nbind -n M-b run-shell -b '{bin} sidebar --toggle'\n{pins}set -g status-interval 2\nset -g status-right-length 160\n{END}\n"
    );
    if !apply {
        println!("\nadd to ~/.tmux.conf (or run `toomux init --apply`):\n\n{block}");
        println!("and put #({bin} status) at the start of your status-right.");
        print!("{}", what_acts(&Config::load().unwrap_or_default()));
        return Ok(());
    }

    let conf = config::home().join(".tmux.conf");
    let old = std::fs::read_to_string(&conf).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut skipping = false;
    for l in old.lines() {
        if l.trim() == BEGIN {
            skipping = true;
            continue;
        }
        if l.trim() == END {
            skipping = false;
            continue;
        }
        if !skipping {
            lines.push(l.to_string());
        }
    }
    // status-right: prepend our segment once.
    let seg = format!("#({bin} status)  ");
    let mut have_status = false;
    for l in lines.iter_mut() {
        let t = l.trim_start();
        if t.starts_with("set -g status-right ") || t.starts_with("set-option -g status-right ") {
            have_status = true;
            if !l.contains("toomux status")
                && let Some(q) = l.find('"') {
                    l.insert_str(q + 1, &seg);
                }
        }
    }
    let bindings = block.clone();
    let mut block = block;
    if !have_status {
        block = block.replace(END, &format!("set -g status-right \"{seg}%H:%M \"\n{END}"));
    }
    // Plugins (tpm) must stay last, so insert before them when present.
    let at = lines
        .iter()
        .position(|l| l.starts_with("#### Plugins") || l.contains("set -g @plugin") || l.contains("tpm/tpm"))
        .unwrap_or(lines.len());
    lines.insert(at, block.trim_end().to_string());
    if at < lines.len() - 1 {
        lines.insert(at + 1, String::new());
    }
    let backup = conf.with_extension("conf.pre-toomux");
    if !backup.exists() && !old.is_empty() {
        std::fs::write(&backup, &old)?;
    }
    std::fs::write(&conf, lines.join("\n") + "\n")?;
    println!("tmux: updated {} (backup at {})", conf.display(), backup.display());
    // A tmux server started from inside a Claude session holds that session's
    // runtime markers in its global environment; every new pane inherits them.
    let mut cleared = 0;
    for v in registry::RUNTIME_VARS {
        if tmux::run(&["set-environment", "-gu", v]).is_ok() {
            cleared += 1;
        }
    }
    if cleared > 0 {
        println!("tmux: cleared inherited claude session variables from the server environment");
    }
    if tmux::run(&["source-file", &conf.display().to_string()]).is_ok() {
        println!("tmux: reloaded. alt-s opens toomux.");
    }
    // Each session's own server read the config when it started: give the
    // running ones the new keys too.
    let here = tmux::current_server().unwrap_or_else(|| "default".into());
    let others: Vec<String> = tmux::servers().into_iter().filter(|s| *s != here).collect();
    if !others.is_empty() {
        let tmp = std::env::temp_dir().join(format!("toomux-keys-{}.conf", std::process::id()));
        std::fs::write(&tmp, &bindings)?;
        let n = others.iter().filter(|s| tmux::run_on(s, &["source-file", &tmp.display().to_string()]).is_ok()).count();
        let _ = std::fs::remove_file(&tmp);
        println!("tmux: the keys also reached {n} running session server{}", if n == 1 { "" } else { "s" });
    }
    let cfg = Config::load()?;
    for i in 0..cfg.accounts.len() {
        let dir = cfg.account_dir(i);
        println!("{}: {}", cfg.accounts[i].name, install_statusline(&dir, &bin)?);
        println!("{}: {}", cfg.accounts[i].name, install_hook(&dir, &bin)?);
        println!("{}: {}", cfg.accounts[i].name, register_mcp(&cfg, &dir, &bin));
        println!("{}: {}", cfg.accounts[i].name, install_voyage_command(&dir)?);
    }
    print!("{}", what_acts(&cfg));
    Ok(())
}

/// What toomux does by itself, said plainly at install: each on unless the
/// config says otherwise, and the line that turns it off.
fn what_acts(cfg: &Config) -> String {
    let k = |n: u64| n / 1000;
    let on = |b: bool| if b { "on " } else { "off" };
    let rows = [
        (
            cfg.handover_tokens > 0,
            format!(
                "handover: past {}k tokens a conversation writes a complete brief and continues in a fresh session (subagents past {}k)",
                k(cfg.turn_end_limit()),
                k(cfg.subagent_limit())
            ),
            "handover_tokens = 0",
        ),
        (
            cfg.fork_context_tokens > 0,
            format!("fork gate: past {}k of context, a fork is refused in favour of a briefed subagent", k(cfg.fork_context_tokens)),
            "fork_context_tokens = 0",
        ),
        (cfg.capture_bash, "bash capture: long output kept whole and shown short; background commands carried over a handover".into(), "capture_bash = false"),
        (cfg.fix_memory, "memory fixes: hourly, paths that moved are corrected in memory files (prior text kept)".into(), "fix_memory = false"),
        (cfg.tidy_worktrees, "worktrees: daily, clean, merged, idle git worktrees are removed (never branches or changes)".into(), "tidy_worktrees = false"),
        (cfg.archive_transcripts, "archive: every transcript kept compressed, past Claude Code's 30-day cleanup".into(), "archive_transcripts = false"),
        (cfg.reopen_after_restart, "after a reboot: what was running reopens by itself".into(), "reopen_after_restart = false"),
    ];
    let mut out = format!("\nwhat toomux does by itself (each a line in {}):\n", config::tilde(&Config::path().display().to_string()));
    for (b, what, off) in rows {
        out.push_str(&format!("  {} {what}\n      {}\n", on(b), if b { format!("off: {off}") } else { "turned off".into() }));
    }
    out.push_str("  always: every conversation is indexed into one memory all sessions search (the toomux MCP tools)\n");
    out.push_str("  /voyage <outcome> in any session keeps it at that outcome until it's done, across handovers\n");
    out.push_str("\ntoomux account setup names your Claude Code accounts and chooses which share one history.\n");
    out.push_str("toomux where shows every place it keeps things; toomux uninstall takes it all back.\n");
    out
}

/// Route Bash through toomux, so large outputs are kept whole but shown short.
fn install_hook(dir: &std::path::Path, bin: &str) -> Result<String> {
    let path = std::fs::canonicalize(dir.join("settings.json")).unwrap_or_else(|_| dir.join("settings.json"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut v: serde_json::Value = serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    // Every tool: the handover gate measures whoever calls one; Bash output
    // capping and background jobs ride on the same hook. Every turn's end: a
    // due handover starts without waiting for tmux's status tick. Every
    // prompt: past the turn-end limit, the conversation hands over there.
    let wanted = [
        ("PreToolUse", "hook pre-tool", serde_json::json!({"matcher": "*", "hooks": [{"type": "command", "command": format!("{bin} hook pre-tool"), "timeout": 10}]})),
        // A voyage's check runs here too: its judge, and its own command.
        ("Stop", "hook stop", serde_json::json!({"hooks": [{"type": "command", "command": format!("{bin} hook stop"), "timeout": 600}]})),
        ("UserPromptSubmit", "hook prompt", serde_json::json!({"hooks": [{"type": "command", "command": format!("{bin} hook prompt"), "timeout": 10}]})),
    ];
    let mut changed = false;
    for (event, mark, entry) in wanted {
        let mut list = v.pointer(&format!("/hooks/{event}")).and_then(|l| l.as_array()).cloned().unwrap_or_default();
        let ours = |e: &serde_json::Value| e.to_string().contains(mark);
        if list.iter().any(|e| ours(e) && *e == entry) {
            continue;
        }
        list.retain(|e| !ours(e));
        list.push(entry);
        let obj = v.as_object_mut().context("settings.json isn't an object")?;
        let hooks = obj.entry("hooks").or_insert(serde_json::json!({}));
        hooks.as_object_mut().context("hooks isn't an object")?.insert(event.into(), serde_json::Value::Array(list));
        changed = true;
    }
    if !changed {
        return Ok("handover gate, handover trigger and output capping already on".into());
    }
    let backup = path.with_extension("json.pre-toomux-hooks");
    if !backup.exists() {
        std::fs::write(&backup, &raw)?;
    }
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&v)? + "\n")?;
    std::fs::rename(tmp, &path)?;
    Ok(format!("handover gate, handover trigger and output capping on in {} (backup {})", config::tilde(&path.display().to_string()), config::tilde(&backup.display().to_string())))
}

/// `/voyage` in Claude Code: the hooks do the work, this file puts it in the
/// command menu and tells the session what it's for.
fn install_voyage_command(dir: &std::path::Path) -> Result<String> {
    remove_quest_command(dir)?;
    let path = dir.join("commands").join("voyage.md");
    let now = std::fs::read_to_string(&path).ok();
    if now.as_deref() == Some(setup::VOYAGE_COMMAND) {
        return Ok("/voyage already there".into());
    }
    if now.as_deref().is_some_and(|t| !t.contains(setup::VOYAGE_MARK)) {
        return Ok(format!("/voyage left alone: {} is someone else's", config::tilde(&path.display().to_string())));
    }
    std::fs::create_dir_all(path.parent().context("no commands folder")?)?;
    std::fs::write(&path, setup::VOYAGE_COMMAND)?;
    Ok(format!("/voyage added ({})", config::tilde(&path.display().to_string())))
}

/// `/voyage` was `/quest` once: toomux's old command file goes (yours stays).
fn remove_quest_command(dir: &std::path::Path) -> Result<bool> {
    let old = dir.join("commands").join("quest.md");
    if !std::fs::read_to_string(&old).is_ok_and(|t| t.contains(setup::QUEST_MARK)) {
        return Ok(false);
    }
    std::fs::remove_file(&old)?;
    Ok(true)
}

/// Memory and kept outputs as tools in every session of this account.
fn register_mcp(cfg: &Config, dir: &std::path::Path, bin: &str) -> String {
    let claude = config::expand(&cfg.claude_bin);
    let listed = toomux::credentials::with_config_dir(std::process::Command::new(&claude).args(["mcp", "get", "toomux"]), dir).output();
    if listed.as_ref().is_ok_and(|o| o.status.success()) {
        return "memory tools already registered".into();
    }
    let added = toomux::credentials::with_config_dir(std::process::Command::new(&claude).args(["mcp", "add", "--scope", "user", "toomux", "--", bin, "mcp"]), dir).output();
    match added {
        Ok(o) if o.status.success() => "memory tools registered (user scope)".into(),
        Ok(o) => format!("couldn't register memory tools: {}", String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => format!("couldn't register memory tools: {e}"),
    }
}

/// Make toomux the account's Claude Code status line, which is how it learns
/// the account's usage. An existing status line of your own is left alone.
fn install_statusline(dir: &std::path::Path, bin: &str) -> Result<String> {
    let path = dir.join("settings.json");
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut v: serde_json::Value = serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let want = format!("{bin} statusline");
    // Once a second: a voyage's scene moves, and lands the moment it's met.
    let every = v.pointer("/statusLine/refreshInterval").and_then(|r| r.as_u64());
    let colour = v.pointer(&format!("/env/{}", setup::TRUECOLOR_ENV)).is_some();
    match v.pointer("/statusLine/command").and_then(|c| c.as_str()) {
        Some(c) if c == want && every == Some(1) && colour => return Ok("status line already reports usage".into()),
        Some(c) if !c.contains("toomux") => {
            return Ok(format!("has its own status line ({c}); usage for it will come from the usage lookup"));
        }
        _ => {}
    }
    let obj = v.as_object_mut().context("settings.json isn't an object")?;
    obj.insert("statusLine".into(), serde_json::json!({"type": "command", "command": want, "padding": 0, "refreshInterval": 1}));
    if !colour {
        let env = obj.entry("env").or_insert_with(|| serde_json::json!({}));
        if let Some(env) = env.as_object_mut() {
            env.insert(setup::TRUECOLOR_ENV.into(), "1".into());
        }
    }
    let backup = path.with_extension("json.pre-toomux");
    if !backup.exists() {
        std::fs::write(&backup, &raw)?;
    }
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&v)? + "\n")?;
    std::fs::rename(tmp, &path)?;
    Ok(format!("status line set to toomux in {} (backup {})", config::tilde(&path.display().to_string()), config::tilde(&backup.display().to_string())))
}

/// Bytes under a folder, not following links.
fn du(p: &std::path::Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else { return 0 };
    if !m.is_dir() {
        return m.len();
    }
    std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| du(&e.path())).sum()
}

fn human(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{} MB", b >> 20),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b => format!("{b} B"),
    }
}

fn where_() -> Result<()> {
    let t = |p: &std::path::Path| config::tilde(&p.display().to_string());
    let one = std::env::var_os("TOOMUX_HOME").is_some_and(|v| !v.is_empty());
    println!("toomux keeps{}", if one { " (under TOOMUX_HOME)" } else { "" });
    println!("  config   {}", t(&Config::path()));
    println!("  state    {}  {}  memory, usage, handovers, kept outputs, reports", t(&paths::state()), human(du(&paths::state())));
    println!("  data     {}  {}  the transcript archive", t(&paths::data()), human(du(&paths::data())));
    println!("  runtime  {}  sockets and queues, gone at reboot", t(&paths::runtime()));
    let cfg = Config::load().unwrap_or_default();
    println!("\naccounts (Claude Code keeps each workspace's memory in projects/<workspace>/memory; toomux indexes and tidies it there)");
    for i in 0..cfg.accounts.len() {
        let dir = cfg.account_dir(i);
        let projects = dir.join("projects");
        let shared = std::fs::symlink_metadata(&projects).is_ok_and(|m| m.file_type().is_symlink());
        let real = std::fs::canonicalize(&projects).unwrap_or(projects.clone());
        let n = std::fs::read_dir(&real).into_iter().flatten().flatten().filter(|e| e.path().join("memory").is_dir()).count();
        let whose = if shared { format!(", shared from {}", t(&real)) } else { String::new() };
        println!("  {:<10} {}  memory for {n} workspaces{whose}", cfg.accounts[i].name, t(&dir));
    }
    println!("\ninit --apply changed");
    println!("  {}  between {BEGIN} and {END}, and #(… toomux status) in status-right", t(&config::home().join(".tmux.conf")));
    println!("  each account's settings.json  statusLine and 3 hooks (toomux hook pre-tool, stop, prompt)");
    println!("  each account's MCP servers  \"toomux\" (user scope)");
    println!("  each account's commands/voyage.md  the /voyage command");
    println!("\ntoomux uninstall takes these back; --purge also deletes config, state and data.");
    Ok(())
}

fn uninstall(dry_run: bool, purge: bool) -> Result<()> {
    let will = if dry_run { "would be " } else { "" };
    let t = |p: &std::path::Path| config::tilde(&p.display().to_string());

    let conf = config::home().join(".tmux.conf");
    match std::fs::read_to_string(&conf).ok().as_deref().and_then(setup::strip_tmux) {
        Some(text) => {
            if !dry_run {
                let backup = conf.with_extension("conf.pre-toomux-uninstall");
                std::fs::copy(&conf, &backup)?;
                std::fs::write(&conf, text)?;
            }
            println!("tmux: toomux's bindings and status segment {will}removed from {}", t(&conf));
        }
        None => println!("tmux: nothing of toomux's in {}", t(&conf)),
    }
    if !dry_run && tmux::run(&["show", "-gv", "status-right"]).is_ok() {
        // This server forgets the bindings now; toomux's own per-session
        // servers do when they next start.
        let keys = ["M-s", "M-b", "M-1", "M-2", "M-3", "M-4", "M-5", "M-6", "M-7", "M-8", "M-9"];
        for k in keys {
            let _ = tmux::run(&["unbind", "-n", k]);
        }
        let _ = tmux::run(&["bind", "Space", "next-layout"]);
        if let Ok(right) = tmux::run(&["show", "-gv", "status-right"])
            && right.contains("toomux status") {
                let _ = tmux::run(&["set", "-g", "status-right", &setup::strip_segment(right.trim_end_matches('\n'))]);
            }
    }

    let cfg = Config::load().unwrap_or_default();
    let claude = config::expand(&cfg.claude_bin);
    // Accounts can share one settings.json: each file once, named by all of them.
    let mut files: Vec<(std::path::PathBuf, Vec<String>)> = Vec::new();
    for i in 0..cfg.accounts.len() {
        let dir = cfg.account_dir(i);
        let path = std::fs::canonicalize(dir.join("settings.json")).unwrap_or_else(|_| dir.join("settings.json"));
        match files.iter_mut().find(|(p, _)| *p == path) {
            Some((_, names)) => names.push(cfg.accounts[i].name.clone()),
            None => files.push((path, vec![cfg.accounts[i].name.clone()])),
        }
    }
    for (path, names) in &files {
        let names = names.join(", ");
        let Ok(raw) = std::fs::read_to_string(path) else { continue };
        let mut v: serde_json::Value = serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        if setup::strip_settings(&mut v) {
            if !dry_run {
                std::fs::write(path.with_extension("json.pre-toomux-uninstall"), &raw)?;
                let tmp = path.with_extension(format!("json.{}", std::process::id()));
                std::fs::write(&tmp, serde_json::to_string_pretty(&v)? + "\n")?;
                std::fs::rename(tmp, path)?;
            }
            println!("{names}: status line and hooks {will}removed from {}", t(path));
        } else {
            println!("{names}: nothing of toomux's in {}", t(path));
        }
    }
    for i in 0..cfg.accounts.len() {
        let dir = cfg.account_dir(i);
        let listed = toomux::credentials::with_config_dir(std::process::Command::new(&claude).args(["mcp", "get", "toomux"]), &dir).output();
        if listed.is_ok_and(|o| o.status.success()) {
            if !dry_run {
                let _ = toomux::credentials::with_config_dir(std::process::Command::new(&claude).args(["mcp", "remove", "--scope", "user", "toomux"]), &dir).output();
            }
            println!("{}: memory tools {will}unregistered", cfg.accounts[i].name);
        }
        let cmd = dir.join("commands").join("voyage.md");
        if std::fs::read_to_string(&cmd).is_ok_and(|t| t.contains(setup::VOYAGE_MARK)) {
            if !dry_run {
                std::fs::remove_file(&cmd)?;
            }
            println!("{}: /voyage {will}removed", cfg.accounts[i].name);
        }
        if !dry_run {
            remove_quest_command(&dir)?;
        }
    }

    if purge {
        // init's backups; uninstall's own stay, the one way back if it misjudged.
        let mut backups = vec![conf.with_extension("conf.pre-toomux")];
        // Beside each settings file as it is now, and as it was when init ran
        // (an account that has since joined a group kept its backups).
        let mut beside: Vec<std::path::PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
        beside.extend((0..cfg.accounts.len()).map(|i| cfg.account_dir(i).join("settings.json")));
        for path in beside {
            for b in [path.with_extension("json.pre-toomux"), path.with_extension("json.pre-toomux-hooks")] {
                if !backups.contains(&b) {
                    backups.push(b);
                }
            }
        }
        for b in backups.iter().filter(|b| b.exists()) {
            if !dry_run {
                std::fs::remove_file(b)?;
            }
            println!("backup: {} {will}deleted", t(b));
        }
        for (what, p) in [("config", paths::config()), ("state", paths::state()), ("data", paths::data()), ("runtime", paths::runtime())] {
            if p.exists() {
                let size = human(du(&p));
                if !dry_run {
                    std::fs::remove_dir_all(&p).with_context(|| format!("removing {}", p.display()))?;
                }
                println!("{what}: {} ({size}) {will}deleted", t(&p));
            }
        }
    } else {
        println!("kept: config, state and the transcript archive (toomux where; --purge deletes them)");
    }
    println!("left as they are: Claude Code's memory folders, and any status line or hooks of your own.");
    if toomux::accounts::shared_dir(&config::home()).is_dir() {
        println!("accounts stay sharing {}: Claude Code works the same through the links.", t(&toomux::accounts::shared_dir(&config::home())));
    }
    let exe = std::env::current_exe()?;
    let how = match exe.to_string_lossy() {
        p if p.contains("/node_modules/") => "npm uninstall -g toomux",
        p if p.contains("/Cellar/toomux/") => "brew uninstall toomux",
        p if p.contains("/.cargo/bin/") => "cargo uninstall toomux",
        _ => "delete it when you like",
    };
    println!("the binary stays at {}: {how}", t(&exe));
    Ok(())
}
