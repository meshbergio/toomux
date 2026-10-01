//! Reads Claude Code's live session registry (`<config>/sessions/<pid>.json`)
//! and joins it with /proc and tmux so each session knows its account and pane.

use crate::config::Config;
use crate::tmux::{self, Pane};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    pid: i32,
    session_id: String,
    cwd: String,
    #[serde(default)]
    started_at: i64,
    #[serde(default)]
    proc_start: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    name_source: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    waiting_for: Option<String>,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    status_updated_at: i64,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum State {
    NeedsYou,
    Working,
    /// Idle at the prompt while background shells or agents keep running.
    Background,
    Finished,
    Idle,
}

impl State {
    pub fn section(self) -> &'static str {
        match self {
            State::NeedsYou => "needs you",
            State::Working => "working",
            State::Background => "background",
            State::Finished => "finished",
            State::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub pid: i32,
    pub proc_start: Option<String>,
    pub id: String,
    pub cwd: String,
    /// Claude Code's registry name: set with /rename, or derived ("desktop-93").
    pub name: String,
    /// What to call the session: a name you gave it, else Claude's own title
    /// for the conversation, else the last prompt, else the registry name.
    pub title: String,
    /// Claude's current title for the conversation, when `title` is a name you
    /// gave it and says something different.
    pub topic: Option<String>,
    pub pr: Option<(String, u64)>,
    /// Account a queued move will take it to once it goes idle.
    pub queued: Option<String>,
    /// Pin slot (0-based; alt-1 is slot 0).
    pub pin: Option<usize>,
    /// A pinned conversation that isn't running. Its pid is a placeholder.
    pub dormant: bool,
    /// Ran before the last restart: the tmux session and window it lived in.
    pub restore: Option<(Option<String>, Option<String>)>,
    pub state: State,
    pub waiting_for: Option<String>,
    /// Set when the account's usage limit stopped this session.
    pub limit: Option<String>,
    /// Handing over to a fresh session: under way, waiting on its brief, or
    /// failed.
    pub handover: Option<crate::handover::Phase>,
    /// When the current state began (ms since epoch).
    pub since_ms: i64,
    pub started_ms: i64,
    pub account: Option<usize>,
    pub config_dir: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub tty: Option<String>,
    pub pane: Option<Pane>,
}

impl Session {
    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle | State::Finished)
    }

    /// Safe to restart: idle, or parked on a usage-limit dialog.
    pub fn can_move(&self) -> bool {
        self.is_idle() || self.limit.is_some()
    }

    pub fn account_name<'a>(&self, cfg: &'a Config) -> &'a str {
        self.account
            .map(|i| cfg.accounts[i].name.as_str())
            .unwrap_or("unknown")
    }

    /// Short project label: path under ~/storage/projects or ~.
    pub fn place(&self) -> String {
        let t = crate::config::tilde(&self.cwd);
        t.strip_prefix("~/storage/projects/")
            .map(str::to_string)
            .unwrap_or(t)
    }

    pub fn pin_record(&self, cfg: &Config) -> crate::state::Pin {
        crate::state::Pin {
            id: self.id.clone(),
            cwd: self.cwd.clone(),
            account: self.account_name(cfg).to_string(),
            title: self.title.clone(),
            args: crate::actions::carried_args(&self.args),
        }
    }

    /// The state as parts, so only the word carries colour: ("limit",
    /// Some("back sat 5pm"), Some("2h")). The time is how long it has been in
    /// this state.
    pub fn state_parts(&self, now: i64) -> (String, Option<String>, Option<String>) {
        let since = (self.since_ms > 0).then(|| ago(now - self.since_ms));
        if self.restore.is_some() {
            return ("before the restart".into(), None, since);
        }
        if self.dormant {
            return ("not running".into(), None, since);
        }
        use crate::handover::Phase;
        match &self.handover {
            Some(Phase::Running) => return ("handing over".into(), None, since),
            Some(Phase::Asked { written: false }) => {
                return (
                    "handing over".into(),
                    Some("writing its brief".into()),
                    since,
                );
            }
            Some(Phase::Asked { written: true }) => {
                return (
                    "handing over".into(),
                    Some("brief written · goes when its turn ends".into()),
                    since,
                );
            }
            // Limited, it can't hand over until the reset, and is tried then:
            // the limit is the news.
            Some(Phase::Failed(why)) if self.limit.is_none() => {
                return ("handover failed".into(), Some(why.clone()), since);
            }
            _ => {}
        }
        if let Some(l) = &self.limit {
            let back = crate::transcript::reset_at(l).map(|t| back_at(t.timestamp_millis(), now));
            let then = if matches!(self.handover, Some(Phase::Failed(_))) {
                " · hands over then"
            } else {
                ""
            };
            return (
                "limit".into(),
                back.map(|b| format!("{b}{then}"))
                    .or_else(|| Some(l.clone())),
                since,
            );
        }
        let word = match self.state {
            State::NeedsYou => "needs you",
            State::Working => "working",
            State::Background => "tasks running",
            State::Finished => "finished",
            State::Idle => "idle",
        };
        let rest = match self.state {
            State::NeedsYou => self.waiting_for.clone(),
            _ => None,
        };
        (word.into(), rest, since)
    }

    pub fn status_text(&self, now: i64) -> String {
        if self.restore.is_some() {
            return format!(
                "was running {} ago, before the restart",
                ago(now - self.since_ms)
            );
        }
        if self.dormant {
            return match self.since_ms {
                0 => "not running".into(),
                t => format!("not running · last active {} ago", ago(now - t)),
            };
        }
        let since = ago(now - self.since_ms);
        let fresh = since == "now";
        if let Some(l) = &self.limit {
            return format!("hit {l}");
        }
        match self.state {
            State::NeedsYou => match &self.waiting_for {
                Some(w) => format!("needs you · {w}"),
                None => "needs you".into(),
            },
            State::Working if fresh => "working · just started".into(),
            State::Working => format!("working · {since}"),
            State::Background => format!("idle {since} · tasks running"),
            State::Finished if fresh => "finished just now".into(),
            State::Finished => format!("finished {since} ago"),
            State::Idle => format!("idle {since}"),
        }
    }

    pub fn transcript(&self, cfg: &Config) -> Option<PathBuf> {
        let dir: String = self
            .cwd
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let file = format!("{}.jsonl", self.id);
        let mut roots: Vec<PathBuf> = self
            .config_dir
            .iter()
            .map(|d| crate::config::expand(d))
            .collect();
        roots.extend((0..cfg.accounts.len()).map(|i| cfg.account_dir(i)));
        roots
            .into_iter()
            .map(|r| r.join("projects").join(&dir).join(&file))
            .find(|p| p.is_file())
    }
}

/// When something comes back, briefly: "back 5pm", "back sat 5pm",
/// "back sat 4:30pm".
pub fn back_at(at_ms: i64, now: i64) -> String {
    use chrono::TimeZone;
    // Resets land a hair either side of the hour: read it to the nearest minute.
    let at_ms = (at_ms + 30_000) / 60_000 * 60_000;
    let Some(t) = chrono::Local.timestamp_millis_opt(at_ms).single() else {
        return String::new();
    };
    let clock = if t.format("%M").to_string() == "00" {
        t.format("%-I%P")
    } else {
        t.format("%-I:%M%P")
    }
    .to_string();
    if at_ms - now < 20 * 3_600_000 {
        format!("back {clock}")
    } else {
        format!("back {} {clock}", t.format("%a").to_string().to_lowercase())
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=9 => "now".into(),
        10..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// A length of time for prose: "40s", "12m", "2h 5m".
pub fn duration(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 if (s % 3600) / 60 == 0 => format!("{}h", s / 3600),
        3600..=86399 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        _ if (s % 86400) / 3600 == 0 => format!("{}d", s / 86400),
        _ => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
    }
}

/// Kernel start time of a live pid, used to reject reused pids.
pub fn proc_start(pid: i32) -> Option<String> {
    crate::platform::start_time(pid)
}

pub fn alive(pid: i32, start: Option<&str>) -> bool {
    match (proc_start(pid), start) {
        (Some(now), Some(then)) => now == then,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

pub fn load(cfg: &Config) -> Vec<Session> {
    let st = crate::state::State::load();
    let background_jobs = crate::jobs::running_sessions();
    let mut panes = tmux::panes_by_tty();
    // Servers of other people's making (`tmux -L work`) are only asked when a
    // session is found in one, by the TMUX its process was started with.
    let mut asked: Vec<String> = Vec::new();
    let now = now_ms();
    let finished_window = cfg.finished_minutes as i64 * 60_000;
    let mut out: Vec<Session> = Vec::new();

    for dir in cfg.session_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(r) = serde_json::from_str::<Raw>(&raw) else {
                continue;
            };
            if r.kind.as_deref().is_some_and(|k| k != "interactive") {
                continue;
            }
            if !alive(r.pid, r.proc_start.as_deref()) || out.iter().any(|s| s.pid == r.pid) {
                continue;
            }

            let environ = crate::platform::environ(r.pid);
            let config_dir = environ
                .iter()
                .find_map(|v| v.strip_prefix("CLAUDE_CONFIG_DIR="))
                .map(str::to_string);
            let env = environ
                .iter()
                .filter_map(|v| v.split_once('='))
                .filter(|(k, _)| carry_env(k))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let tty = crate::platform::stdin(r.pid)
                .map(|t| t.display().to_string())
                .filter(|t| t.starts_with("/dev/pts/") || t.starts_with("/dev/tty"));
            if let Some(t) = &tty {
                let server = environ
                    .iter()
                    .find_map(|v| v.strip_prefix("TMUX="))
                    .and_then(tmux::server_in);
                if let Some(server) =
                    server.filter(|sv| !panes.contains_key(t) && !asked.contains(sv))
                {
                    // A new session, or a server of someone else's. At most
                    // every 2s: a process can carry a TMUX it has left.
                    panes.extend(tmux::panes_on(&server, 2_000));
                    asked.push(server);
                }
            }
            let pane = tty.as_ref().and_then(|t| panes.get(t)).cloned();

            let since = if r.status_updated_at > 0 {
                r.status_updated_at
            } else {
                r.updated_at
            };
            let state = match r.status.as_deref() {
                Some("waiting") => State::NeedsYou,
                Some("busy") => State::Working,
                Some("shell") => State::Background,
                _ if now - since < finished_window => State::Finished,
                _ => State::Idle,
            };
            // Claude's foreground turn can return to the prompt while a
            // command captured by Toomux is still running. Claude then
            // reports finished/idle even though the session still owns live
            // work. Fold Toomux's own job registry into the session state so
            // the list cannot hide those monitors as plain idle sessions.
            let state = background_state(state, background_jobs.contains(&r.session_id));
            let named = r.name_source.as_deref() == Some("user")
                && r.name.as_deref().is_some_and(|n| !n.is_empty());
            let name = match &r.name {
                Some(n) if !n.is_empty() => n.clone(),
                _ => r.cwd.rsplit('/').next().unwrap_or("session").to_string(),
            };

            let mut s = Session {
                pid: r.pid,
                proc_start: r.proc_start,
                id: r.session_id,
                cwd: r.cwd,
                title: name.clone(),
                topic: None,
                pr: None,
                queued: None,
                pin: None,
                dormant: false,
                restore: None,
                name,
                state,
                waiting_for: r.waiting_for,
                limit: None,
                handover: None,
                since_ms: since,
                started_ms: r.started_at,
                account: cfg.account_for(config_dir.as_deref()),
                config_dir,
                args: crate::platform::cmdline(r.pid),
                env,
                tty,
                pane,
            };
            let meta = s
                .transcript(cfg)
                .map(|p| crate::transcript::meta(&p))
                .unwrap_or_default();
            // A name chosen for you gives way to one you've since given it in Claude.
            let yours_since =
                |n: &String| st.chosen.contains(&s.id) && named && !s.name.eq_ignore_ascii_case(n);
            if let Some(n) = st.names.get(&s.id).filter(|n| !yours_since(n)) {
                s.title = n.clone();
                s.topic = meta.title.clone().filter(|t| !t.eq_ignore_ascii_case(n));
            } else if named {
                s.topic = meta
                    .title
                    .clone()
                    .filter(|t| !t.eq_ignore_ascii_case(&s.name));
            } else if let Some(t) = meta.custom.clone().or(meta.title.clone()) {
                s.title = t;
            } else if let Some(p) = &meta.last_prompt {
                s.title = prompt_title(p);
            }
            s.pr = meta.pr.clone();
            s.queued = crate::queue::get(s.pid, s.proc_start.as_deref()).map(|q| q.to);
            s.pin = st.slot_of(&s.id);
            s.handover = crate::handover::phase(&s.id, s.pid);
            if s.state != State::Working {
                s.limit = meta.limit();
                if s.limit.is_some() {
                    s.state = State::NeedsYou;
                }
            }
            out.push(s);
        }
    }
    // What a continued session is on, from the brief it continued from
    // (Claude's own summary of it only says "Handover continuation").
    let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
    let topics = crate::handover::topics(&ids);
    for s in &mut out {
        if let Some(t) = topics.get(&s.id) {
            s.topic = Some(t.clone());
        }
        if s.topic
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(&s.title) || crate::handover::generic(t))
        {
            s.topic = None;
        }
    }
    out.sort_by(|a, b| a.state.cmp(&b.state).then(b.since_ms.cmp(&a.since_ms)));
    out
}

/// Keep pin records current with running sessions (account moves, renames,
/// launch flags), so a pin can reopen its conversation after it exits.
pub fn sync_pins(cfg: &Config, sessions: &[Session]) {
    let st = crate::state::State::load();
    let fresh: Vec<crate::state::Pin> = sessions
        .iter()
        .filter(|s| !s.dormant && s.pin.is_some())
        .map(|s| s.pin_record(cfg))
        .filter(|p| st.pins.iter().flatten().any(|q| q.id == p.id && q != p))
        .collect();
    if !fresh.is_empty() {
        let _ = crate::state::State::update(|st| {
            for p in fresh {
                st.refresh_pin(p);
            }
        });
    }
}

/// Pinned conversations that aren't running, as placeholder sessions.
pub fn dormant(cfg: &Config, live: &[Session]) -> Vec<Session> {
    let st = crate::state::State::load();
    st.pins
        .iter()
        .enumerate()
        .filter_map(|(n, p)| Some((n, p.as_ref()?)))
        .filter(|(_, p)| !live.iter().any(|s| s.id == p.id))
        .map(|(n, p)| {
            let mut s = Session {
                pid: -(n as i32) - 1,
                proc_start: None,
                id: p.id.clone(),
                cwd: p.cwd.clone(),
                name: p.title.clone(),
                title: st
                    .names
                    .get(&p.id)
                    .cloned()
                    .unwrap_or_else(|| p.title.clone()),
                topic: None,
                pr: None,
                queued: None,
                pin: Some(n),
                dormant: true,
                restore: None,
                state: State::Idle,
                waiting_for: None,
                limit: None,
                handover: None,
                since_ms: 0,
                started_ms: 0,
                account: cfg.account_by_name(&p.account),
                config_dir: None,
                args: std::iter::once("claude".to_string())
                    .chain(p.args.iter().cloned())
                    .collect(),
                env: vec![],
                tty: None,
                pane: None,
            };
            if let Some(t) = s.transcript(cfg) {
                let modified = std::fs::metadata(&t).and_then(|m| m.modified()).ok();
                s.since_ms = modified
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                s.pr = crate::transcript::meta(&t).pr;
            }
            s.started_ms = s.since_ms;
            s
        })
        .collect()
}

/// Sessions from before the last restart that aren't running (and aren't
/// already listed as pinned), as placeholder sessions.
pub fn restorable(cfg: &Config, listed: &[Session]) -> Vec<Session> {
    let r = crate::snapshot::restorable(listed);
    placeholders(cfg, listed, r.entries, r.at_ms)
}

/// Every conversation `toomux reopen` can bring back: from before a restart,
/// pinned, or last recorded running and now gone.
pub fn reopenable(cfg: &Config, live: &[Session]) -> Vec<Session> {
    let mut out = restorable(cfg, live);
    out.extend(dormant(cfg, live));
    let (at_ms, recorded) = crate::snapshot::recorded();
    let known: Vec<Session> = live.iter().chain(out.iter()).cloned().collect();
    let lost = placeholders(cfg, &known, recorded, at_ms);
    out.extend(lost.into_iter().map(|mut s| {
        s.pid -= 1000;
        s
    }));
    out
}

fn placeholders(
    cfg: &Config,
    listed: &[Session],
    entries: Vec<crate::snapshot::Entry>,
    at_ms: i64,
) -> Vec<Session> {
    entries
        .into_iter()
        .filter(|e| !listed.iter().any(|s| s.id == e.id))
        .enumerate()
        .map(|(i, e)| {
            let mut s = Session {
                pid: -100 - i as i32,
                proc_start: None,
                id: e.id.clone(),
                cwd: e.cwd.clone(),
                name: e.title.clone(),
                title: e.title.clone(),
                topic: None,
                pr: None,
                queued: None,
                pin: None,
                dormant: true,
                restore: Some((e.tmux_session.clone(), e.window.clone())),
                state: State::Idle,
                waiting_for: None,
                limit: None,
                handover: None,
                since_ms: at_ms,
                started_ms: at_ms,
                account: cfg.account_by_name(&e.account),
                config_dir: None,
                args: std::iter::once("claude".to_string())
                    .chain(e.args.iter().cloned())
                    .collect(),
                env: vec![],
                tty: None,
                pane: None,
            };
            if let Some(t) = s.transcript(cfg) {
                s.pr = crate::transcript::meta(&t).pr;
            }
            s
        })
        .collect()
}

/// A last prompt standing in for a title: its first line, cut at a word
/// boundary. It's a hint for sessions Claude hasn't titled yet, not a label
/// anyone needs to read in full (the preview shows the whole prompt).
pub fn prompt_title(p: &str) -> String {
    let first = p
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let first: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if first.chars().count() <= 64 {
        return first;
    }
    let mut out = String::new();
    for w in first.split(' ') {
        if out.chars().count() + w.chars().count() + 1 > 60 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    if out.is_empty() {
        out = first.chars().take(60).collect();
    }
    out + "…"
}

/// Variables a running Claude Code session sets for its own children. They
/// describe *that* session (its id, messaging socket, "child session" marker),
/// so a process that inherits them misbehaves; e.g. the child marker turns off
/// transcript saving. They must never reach a relaunched session.
pub const RUNTIME_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SSE_PORT",
];

/// Environment that shapes a session (model routing, context limits) and must
/// survive a relaunch. Credentials stay behind: the target account's own
/// login is what should apply.
pub fn carry_env(k: &str) -> bool {
    let secret = k.ends_with("_KEY")
        || k.ends_with("_TOKEN")
        || k.contains("SECRET")
        || k.contains("PASSWORD");
    let relevant =
        k.starts_with("ANTHROPIC_") || k.starts_with("CLAUDE_CODE_") || k.starts_with("OHI_");
    relevant && !secret && k != "CLAUDE_CONFIG_DIR" && !RUNTIME_VARS.contains(&k)
}

/// A process's whole environment, less what belongs to that one process (its
/// Claude runtime markers, its tmux pane, its config dir, which launches set).
pub fn full_env(pid: i32) -> Vec<(String, String)> {
    const OWN: &[&str] = &[
        "TMUX",
        "TMUX_PANE",
        "CLAUDE_CONFIG_DIR",
        "_",
        "SHLVL",
        "PWD",
        "OLDPWD",
    ];
    crate::platform::environ(pid)
        .iter()
        .filter_map(|v| v.split_once('='))
        .filter(|(k, _)| !OWN.contains(k) && !RUNTIME_VARS.contains(k))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A live interactive Claude session holds this conversation (not `claude -p`
/// or an SDK run, which toomux can't restart).
pub fn is_interactive(cfg: &Config, session: &str) -> bool {
    cfg.session_dirs().iter().any(|dir| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                let Ok(raw) = std::fs::read_to_string(e.path()) else {
                    return false;
                };
                let Ok(r) = serde_json::from_str::<Raw>(&raw) else {
                    return false;
                };
                r.session_id == session
                    && r.kind.as_deref().is_none_or(|k| k == "interactive")
                    && alive(r.pid, r.proc_start.as_deref())
            })
    })
}

/// Resolve a user-supplied target: pid, session-id prefix, or name.
pub fn find<'a>(sessions: &'a [Session], target: &str) -> anyhow::Result<&'a Session> {
    if let Ok(pid) = target.parse::<i32>()
        && let Some(s) = sessions.iter().find(|s| s.pid == pid)
    {
        return Ok(s);
    }
    if let Some(s) = sessions.iter().find(|s| s.id.starts_with(target)) {
        return Ok(s);
    }
    let t = target.to_lowercase();
    let hits: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.name.to_lowercase() == t || s.title.to_lowercase() == t)
        .collect();
    let hits = if hits.is_empty() {
        sessions
            .iter()
            .filter(|s| s.name.to_lowercase().contains(&t) || s.title.to_lowercase().contains(&t))
            .collect()
    } else {
        hits
    };
    match hits.as_slice() {
        [one] => Ok(one),
        [] => anyhow::bail!("no session matches '{target}'"),
        many => anyhow::bail!(
            "'{target}' matches {} sessions: {}",
            many.len(),
            many.iter()
                .map(|s| format!("{} ({})", s.title, s.pid))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn background_state(state: State, has_running_job: bool) -> State {
    if has_running_job && matches!(state, State::Finished | State::Idle) {
        State::Background
    } else {
        state
    }
}

#[cfg(test)]
mod tests {
    use super::State;

    #[test]
    fn live_background_jobs_outlive_the_foreground_turn() {
        assert_eq!(
            super::background_state(State::Idle, true),
            State::Background
        );
        assert_eq!(
            super::background_state(State::Finished, true),
            State::Background
        );
        // Foreground attention/work remains more important than the fact a
        // monitor also happens to be running.
        assert_eq!(
            super::background_state(State::Working, true),
            State::Working
        );
        assert_eq!(
            super::background_state(State::NeedsYou, true),
            State::NeedsYou
        );
        assert_eq!(super::background_state(State::Idle, false), State::Idle);
    }

    #[test]
    fn durations_read_naturally() {
        let d = super::duration;
        assert_eq!(d(42_000), "42s");
        assert_eq!(d(12 * 60_000), "12m");
        assert_eq!(d(2 * 3_600_000), "2h");
        assert_eq!(d(2 * 3_600_000 + 5 * 60_000), "2h 5m");
        assert_eq!(d(96 * 3_600_000 + 9 * 60_000), "4d");
        assert_eq!(d(3 * 86_400_000 + 23 * 3_600_000), "3d 23h");
    }
}
