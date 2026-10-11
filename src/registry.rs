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
    /// Running, but Claude's own registry has no record of it: found by its
    /// process in one of the tmux panes. Its state isn't known.
    pub unregistered: bool,
    /// Another live process has the same conversation open (its pid).
    pub twin: Option<i32>,
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
        if self.unregistered {
            return ("running".into(), Some(UNREGISTERED.into()), None);
        }
        if let Some(pid) = self.twin {
            return ("open twice".into(), Some(format!("also pid {pid}")), None);
        }
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
            Some(Phase::Waiting(note)) => {
                return ("handing over".into(), Some(note.clone()), since);
            }
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
        if self.unregistered {
            return format!("running · {UNREGISTERED}");
        }
        if let Some(pid) = self.twin {
            return format!("open twice · also pid {pid}: close one");
        }
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
        let dir = project_dir_name(&self.cwd);
        let file = format!("{}.jsonl", self.id);
        // The config folder it runs with first (Claude's default when it
        // runs with none); a placeholder only knows its account.
        let own = match &self.config_dir {
            Some(d) => Some(crate::config::expand(d)),
            None if !self.dormant => Some(crate::config::home().join(".claude")),
            None => self
                .account
                .filter(|a| *a < cfg.accounts.len())
                .map(|a| cfg.account_dir(a)),
        };
        let mut roots: Vec<PathBuf> = own.into_iter().collect();
        roots.extend((0..cfg.accounts.len()).map(|i| cfg.account_dir(i)));
        roots
            .into_iter()
            .map(|r| r.join("projects").join(&dir).join(&file))
            .find(|p| p.is_file())
    }
}

/// The folder under `projects/` Claude keeps a working folder's
/// conversations in.
pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
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
    let rows = tmux::pane_rows();
    let mut panes: std::collections::HashMap<String, Pane> = rows
        .iter()
        .map(|(tty, pane, _)| (tty.clone(), pane.clone()))
        .collect();
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
            let native_monitor = matches!(state, State::Finished | State::Idle)
                && !background_jobs.contains(&r.session_id)
                && (pane.as_ref().is_some_and(pane_has_native_monitor)
                    || native_background_shell(r.pid));
            let state = background_state(
                state,
                background_jobs.contains(&r.session_id) || native_monitor,
            );
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
                unregistered: false,
                twin: None,
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
    out.extend(unregistered(cfg, &st, &rows, &out));
    mark_twins(&mut out);
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

/// The same conversation open in two processes: both write its transcript.
/// Say so on each rather than list one and hide the other.
fn mark_twins(sessions: &mut [Session]) {
    let held: Vec<(String, i32)> = sessions.iter().map(|s| (s.id.clone(), s.pid)).collect();
    for s in sessions {
        s.twin = held
            .iter()
            .find(|(id, pid)| *id == s.id && *pid != s.pid)
            .map(|(_, pid)| *pid);
    }
}

const UNREGISTERED: &str = "not in Claude's session registry";

/// Claude processes in tmux panes that Claude's registry has no record of
/// (the file was lost, or never written), so they're still listed, can be
/// jumped to and closed, and aren't opened a second time. Only the panes
/// already listed are looked at: the pane's own process, and in toomux's own
/// servers (one session each) the processes under it.
fn unregistered(
    cfg: &Config,
    st: &crate::state::State,
    rows: &[(String, Pane, Option<i32>)],
    known: &[Session],
) -> Vec<Session> {
    let mut out: Vec<Session> = Vec::new();
    for (tty, pane, pid) in rows {
        let Some(pid) = *pid else { continue };
        if known.iter().any(|s| s.tty.as_deref() == Some(tty.as_str())) {
            continue;
        }
        let ours = tmux::server_of(&pane.id).is_some_and(|sv| sv.starts_with(tmux::OWN));
        let mut candidates = vec![pid];
        if ours {
            candidates.extend(crate::platform::descendants(pid));
        }
        let Some(h) = candidates
            .into_iter()
            .filter(|p| !known.iter().chain(&out).any(|s| s.pid == *p))
            .find_map(crate::running::holder_at)
            .filter(|h| !crate::running::has_record(cfg, h.pid))
        else {
            continue;
        };
        let Some(proc_start) = proc_start(h.pid) else {
            continue;
        };
        let Some(cwd) = crate::platform::cwd(h.pid).map(|c| c.display().to_string()) else {
            continue;
        };
        let environ = crate::platform::environ(h.pid);
        let config_dir = environ
            .iter()
            .find_map(|v| v.strip_prefix("CLAUDE_CONFIG_DIR="))
            .map(str::to_string);
        let name = cwd.rsplit('/').next().unwrap_or("session").to_string();
        let mut s = Session {
            pid: h.pid,
            proc_start: Some(proc_start),
            id: h.id,
            cwd,
            title: name.clone(),
            name,
            topic: None,
            pr: None,
            queued: None,
            pin: None,
            dormant: false,
            restore: None,
            // Nothing says what it's doing: never treat it as safe to restart.
            state: State::Working,
            waiting_for: None,
            limit: None,
            handover: None,
            since_ms: now_ms(),
            started_ms: 0,
            account: cfg.account_for(config_dir.as_deref()),
            config_dir,
            args: crate::platform::cmdline(h.pid),
            env: environ
                .iter()
                .filter_map(|v| v.split_once('='))
                .filter(|(k, _)| carry_env(k))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            tty: Some(tty.clone()),
            pane: Some(pane.clone()),
            unregistered: true,
            twin: None,
        };
        let meta = s
            .transcript(cfg)
            .map(|p| crate::transcript::meta(&p))
            .unwrap_or_default();
        if let Some(n) = st.names.get(&s.id) {
            s.title = n.clone();
        } else if let Some(t) = meta.custom.clone().or(meta.title.clone()) {
            s.title = t;
        } else if let Some(p) = &meta.last_prompt {
            s.title = prompt_title(p);
        }
        s.pr = meta.pr.clone();
        s.pin = st.slot_of(&s.id);
        out.push(s);
    }
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
                unregistered: false,
                twin: None,
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
                env: e.context_env.clone(),
                tty: None,
                pane: None,
                unregistered: false,
                twin: None,
            };
            if let Some(t) = s.transcript(cfg) {
                s.pr = crate::transcript::meta(&t).pr;
            }
            s
        })
        .collect()
}

/// Any conversation an account's `projects/` holds, by session-id prefix,
/// as a placeholder to reopen: one that was never pinned or recorded (a
/// voyage's conversation, say). On `account` if given, else the account
/// its folder's rules or recent use pick; the folder is the one the
/// transcript says it ran in.
pub fn conversation(
    cfg: &Config,
    prefix: &str,
    account: Option<usize>,
    live: &[Session],
) -> anyhow::Result<Session> {
    use anyhow::{Context, bail};
    if prefix.len() < 4 || !prefix.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        bail!("no session matches '{prefix}'");
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for i in 0..cfg.accounts.len() {
        let projects = cfg.account_dir(i).join("projects");
        // Accounts that share one projects folder: look once.
        let real = std::fs::canonicalize(&projects).unwrap_or(projects.clone());
        if seen.contains(&real) {
            continue;
        }
        seen.push(real);
        for dir in std::fs::read_dir(&projects).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(dir.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let name = f.file_name().to_string_lossy().into_owned();
                if name.starts_with(prefix) && name.ends_with(".jsonl") {
                    found.push(f.path());
                }
            }
        }
    }
    let path = match found.as_slice() {
        [] => bail!("no session matches '{prefix}'"),
        [one] => one.clone(),
        _ => bail!(
            "'{prefix}' matches {} conversations · give more of its id",
            found.len()
        ),
    };
    let id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .context("its transcript has no name")?;
    let cwd = transcript_cwd(&path).context("its transcript doesn't say which folder it ran in")?;
    let account = match account {
        Some(a) => a,
        None => crate::places::account_for(cfg, &cwd, live).0,
    };
    let meta = crate::transcript::meta(&path);
    let st = crate::state::State::load();
    let title = st
        .names
        .get(&id)
        .cloned()
        .or(meta.custom.clone())
        .or(meta.title.clone())
        .unwrap_or_else(|| format!("conversation {}", &id[..8.min(id.len())]));
    Ok(Session {
        pid: -10_000,
        proc_start: None,
        id,
        cwd,
        name: title.clone(),
        title,
        topic: None,
        pr: meta.pr.clone(),
        queued: None,
        pin: None,
        dormant: true,
        restore: None,
        state: State::Idle,
        waiting_for: None,
        limit: None,
        handover: None,
        since_ms: 0,
        started_ms: 0,
        account: Some(account),
        config_dir: None,
        args: vec!["claude".to_string()],
        env: vec![],
        tty: None,
        pane: None,
        unregistered: false,
        twin: None,
    })
}

/// The working folder a transcript's entries were written in.
fn transcript_cwd(path: &std::path::Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    std::io::BufReader::new(f)
        .lines()
        .take(200)
        .map_while(Result::ok)
        .find_map(|l| {
            serde_json::from_str::<serde_json::Value>(&l)
                .ok()?
                .get("cwd")?
                .as_str()
                .map(str::to_string)
        })
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
    let relevant = k.starts_with("ANTHROPIC_")
        || k.starts_with("CLAUDE_CODE_")
        || k.starts_with("OHI_")
        || k == "TOOMUX_PROVIDER_SESSION_ID"
        || k.starts_with("TOOMUX_CONTEXT_");
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

/// Claude's own native Monitor tool does not appear in Toomux's job registry:
/// Claude leaves the session registry at `idle`, while its terminal footer
/// carries the live monitor count. Inspect only the current footer line (the
/// `⏵⏵` mode line), never historical prose in scrollback.
fn pane_has_native_monitor(pane: &Pane) -> bool {
    tmux::capture(&pane.id, 8)
        .and_then(|raw| String::from_utf8(raw).ok())
        .is_some_and(|screen| native_monitor_footer(&screen))
}

/// Claude's newer native background Bash path can keep work alive in a
/// daemon/bg-spare process tree even when the session registry says idle and
/// the visible footer no longer contains a monitor count. A claimed Bash tool
/// is distinguishable from the daemon's long-lived spare/MCP helpers by the
/// shell-snapshot wrapper Claude injects into the command line.
///
/// Inspect descendants only after the foreground state has already settled to
/// finished/idle, so ordinary foreground Bash tools never affect this path.
fn native_background_shell(pid: i32) -> bool {
    crate::platform::descendants(pid)
        .into_iter()
        .map(crate::platform::cmdline)
        .any(|args| native_background_argv(&args))
}

fn native_background_argv(args: &[String]) -> bool {
    let shell = args.first().is_some_and(|arg| {
        std::path::Path::new(arg)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "bash" | "sh" | "zsh"))
    });
    shell
        && args
            .iter()
            .any(|arg| arg.contains("/shell-snapshots/snapshot-"))
}

fn native_monitor_footer(screen: &str) -> bool {
    let footer = screen
        .lines()
        .rev()
        .map(strip_ansi)
        .find(|line| line.contains("⏵⏵"));
    let Some(footer) = footer else {
        return false;
    };
    let words: Vec<&str> = footer.split_whitespace().collect();
    words.windows(2).any(|w| {
        w[0].parse::<u32>().is_ok_and(|n| n > 0)
            && matches!(
                w[1].trim_matches(|c: char| !c.is_alphabetic()),
                "monitor" | "monitors"
            )
    })
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::State;

    fn session(pid: i32, id: &str) -> super::Session {
        super::Session {
            pid,
            proc_start: None,
            id: id.into(),
            cwd: "/nonexistent".into(),
            name: "s".into(),
            title: "s".into(),
            topic: None,
            pr: None,
            queued: None,
            pin: None,
            dormant: false,
            restore: None,
            state: State::Idle,
            waiting_for: None,
            limit: None,
            handover: None,
            since_ms: 0,
            started_ms: 0,
            account: None,
            config_dir: None,
            args: vec![],
            env: vec![],
            tty: None,
            pane: None,
            unregistered: false,
            twin: None,
        }
    }

    #[test]
    fn any_conversation_an_account_holds_can_be_found_to_reopen() {
        let root = std::env::temp_dir().join(format!("toomux-conv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shared = root.join("shared").join("projects").join("-work-app");
        std::fs::create_dir_all(&shared).unwrap();
        let line = |cwd: &str| format!("{{\"type\":\"user\",\"cwd\":\"{cwd}\"}}\n");
        let id = "abcd0123-0000-4000-8000-000000000001";
        std::fs::write(shared.join(format!("{id}.jsonl")), line("/work/app")).unwrap();
        // Two accounts on one projects folder, a third of its own.
        let mut cfg = crate::config::Config::default();
        cfg.accounts.clear();
        for name in ["a", "b", "c"] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            if name != "c" {
                std::os::unix::fs::symlink(
                    root.join("shared").join("projects"),
                    dir.join("projects"),
                )
                .unwrap();
            }
            cfg.accounts.push(crate::config::Account {
                name: name.into(),
                config_dir: dir.display().to_string(),
            });
        }
        let s = super::conversation(&cfg, "abcd0123", Some(2), &[]).unwrap();
        assert_eq!(
            (s.id.as_str(), s.cwd.as_str(), s.account, s.dormant),
            (id, "/work/app", Some(2), true),
            "found once though two accounts share it, on the account asked for"
        );
        assert!(super::conversation(&cfg, "ffff", None, &[]).is_err());
        assert!(
            super::conversation(&cfg, "../x", None, &[]).is_err(),
            "only ids"
        );
        let other = root.join("c").join("projects").join("-work-app");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("abcd0123-0000-4000-8000-000000000002.jsonl"),
            line("/work/app"),
        )
        .unwrap();
        let e = super::conversation(&cfg, "abcd0123", None, &[]).unwrap_err();
        assert!(e.to_string().contains("matches 2"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_claude_in_a_pane_without_a_registry_record_is_still_listed() {
        use std::os::unix::process::CommandExt;
        let id = "0f0f0f0f-1234-4abc-8def-000000000002";
        let dir = std::env::temp_dir().join(format!("toomux-unreg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut claude = std::process::Command::new("sh")
            .arg0("claude")
            .args(["-c", "sleep 30; :", "--resume", id])
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pane = |n: u32| crate::tmux::Pane {
            id: format!("%{n}@toomux-test"),
            session: "t".into(),
            window_index: "0".into(),
        };
        let rows = vec![
            (
                "/dev/pts/9001".to_string(),
                pane(1),
                Some(claude.id() as i32),
            ),
            (
                "/dev/pts/9002".to_string(),
                pane(2),
                Some(other.id() as i32),
            ),
            ("/dev/pts/9003".to_string(), pane(3), None),
        ];
        let mut cfg = crate::config::Config::default();
        cfg.accounts.clear();
        let st = crate::state::State::default();
        let found = super::unregistered(&cfg, &st, &rows, &[]);
        assert_eq!(found.len(), 1, "only the claude process: {found:?}");
        let s = &found[0];
        assert!(s.unregistered && s.id == id && s.pid == claude.id() as i32);
        assert_eq!(
            s.pane.as_ref().map(|p| p.id.as_str()),
            Some("%1@toomux-test")
        );
        assert_eq!(
            std::path::Path::new(&s.cwd),
            std::fs::canonicalize(&dir).unwrap()
        );
        assert!(
            !s.can_move(),
            "its state isn't known: never restarted as idle"
        );
        assert_eq!(s.state_parts(0).0, "running");

        // A registry record for that pane's terminal wins.
        let mut known = session(1, "other");
        known.tty = Some("/dev/pts/9001".into());
        assert!(super::unregistered(&cfg, &st, &rows, &[known]).is_empty());

        for c in [&mut claude, &mut other] {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_conversation_in_two_processes_is_flagged_on_both() {
        let mut all = vec![session(10, "a"), session(11, "a"), session(12, "b")];
        super::mark_twins(&mut all);
        assert_eq!(all[0].twin, Some(11));
        assert_eq!(all[1].twin, Some(10));
        assert_eq!(all[2].twin, None);
        assert_eq!(all[0].state_parts(0).0, "open twice");
    }

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
    fn native_monitor_count_is_read_only_from_the_live_footer() {
        assert!(super::native_monitor_footer(
            "✻ done · 1 monitor still running\n❯\n⏵⏵ bypass permissions on · ← 2 agents · 1 monitor\n"
        ));
        assert!(super::native_monitor_footer(
            "❯\n\u{1b}[2m⏵⏵ bypass permissions on · 2 monitors\u{1b}[0m\n"
        ));
        assert!(!super::native_monitor_footer(
            "✻ done · 1 monitor still running\n❯\n⏵⏵ bypass permissions on · ← 2 agents\n"
        ));
        assert!(!super::native_monitor_footer(
            "❯\n⏵⏵ bypass permissions on · 0 monitors\n"
        ));
    }

    #[test]
    fn native_background_shell_requires_claudes_snapshot_wrapper() {
        let wrapped = vec![
            "/bin/bash".to_string(),
            "-c".to_string(),
            "source /home/x/.claude-cingulum/shell-snapshots/snapshot-bash-123.sh 2>/dev/null || true && eval 'sleep 60'".to_string(),
        ];
        assert!(super::native_background_argv(&wrapped));

        let mcp = vec![
            "npm".to_string(),
            "exec".to_string(),
            "@playwright/mcp@latest".to_string(),
        ];
        assert!(!super::native_background_argv(&mcp));

        let ordinary_shell = vec![
            "/bin/bash".to_string(),
            "-c".to_string(),
            "sleep 60".to_string(),
        ];
        assert!(!super::native_background_argv(&ordinary_shell));
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
