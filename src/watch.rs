//! Notices for sessions that finished a long run or started needing you.
//!
//! There is no daemon: `toomux status`, which tmux already runs every couple
//! of seconds for the status bar, compares each session with what it saw last
//! time. A notice stays until you look at the session (its pane is on screen
//! in an attached client) or it goes back to work.

use crate::config::Config;
use crate::registry::{Session, State};
use crate::tmux;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Debug)]
pub enum Kind {
    Finished,
    NeedsYou,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Notice {
    pub pid: i32,
    pub id: String,
    pub title: String,
    pub kind: Kind,
    /// How long it had been working, in ms.
    pub took_ms: i64,
    pub at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Seen {
    state: State,
    /// When the current stretch of work began.
    busy_from: Option<i64>,
}

#[derive(Default, Serialize, Deserialize)]
struct Book {
    seen: HashMap<i32, Seen>,
    notices: Vec<Notice>,
}

pub fn dir() -> PathBuf {
    crate::paths::runtime()
}

fn load() -> Book {
    std::fs::read_to_string(dir().join("watch.json"))
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_default()
}

/// Current notices, for the UI.
pub fn notices() -> Vec<Notice> {
    load().notices
}

/// Panes currently on screen in some attached client.
fn visible_panes() -> Vec<String> {
    tmux::servers()
        .iter()
        .flat_map(|sv| {
            tmux::run_on(sv, &["list-clients", "-F", "#{pane_id}"])
                .map(|o| o.lines().map(|p| tmux::qualify(sv, p)).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect()
}

/// Compare with the last look, record new notices, drop settled ones.
/// Returns notices that are new this time.
pub fn update(cfg: &Config, sessions: &[Session], now: i64) -> Vec<Notice> {
    let _ = std::fs::create_dir_all(dir());
    // Several status lines can run at once (one per tmux client); serialise.
    let Ok(_guard) = crate::lock::exclusive(&dir().join("watch.lock")) else {
        return Vec::new();
    };

    let mut book = load();
    let first_run = book.seen.is_empty();
    let visible = visible_panes();
    let threshold = cfg.notify_after_secs as i64 * 1000;
    let mut fresh = Vec::new();

    for s in sessions.iter().filter(|s| !s.dormant) {
        let prev = book.seen.get(&s.pid).cloned();
        let busy_from = match (s.state, &prev) {
            (State::Working, Some(p)) if p.state == State::Working => p.busy_from,
            (State::Working, _) => Some(s.since_ms),
            _ => None,
        };
        if let Some(p) = &prev {
            let took = p.busy_from.map(|b| now - b).unwrap_or(0);
            let kind = match (p.state, s.state) {
                (a, State::NeedsYou) if a != State::NeedsYou => Some(Kind::NeedsYou),
                (State::Working, State::Finished | State::Idle | State::Background)
                    if took >= threshold =>
                {
                    Some(Kind::Finished)
                }
                _ => None,
            };
            let on_screen = s.pane.as_ref().is_some_and(|p| visible.contains(&p.id));
            if let Some(kind) = kind.filter(|_| !on_screen && !first_run) {
                book.notices.retain(|n| n.pid != s.pid);
                let n = Notice {
                    pid: s.pid,
                    id: s.id.clone(),
                    title: s.title.clone(),
                    kind,
                    took_ms: took,
                    at_ms: now,
                };
                book.notices.push(n.clone());
                fresh.push(n);
            }
        }
        book.seen.insert(
            s.pid,
            Seen {
                state: s.state,
                busy_from,
            },
        );
    }

    book.seen
        .retain(|pid, _| sessions.iter().any(|s| s.pid == *pid));
    book.notices.retain(|n| {
        let Some(s) = sessions.iter().find(|s| s.pid == n.pid) else {
            return false;
        };
        let on_screen = s.pane.as_ref().is_some_and(|p| visible.contains(&p.id));
        let back_at_work = s.state == State::Working;
        let resolved = n.kind == Kind::NeedsYou && s.state != State::NeedsYou;
        !(on_screen || back_at_work || resolved)
    });

    if let Ok(json) = serde_json::to_string(&book) {
        let tmp = dir().join(format!(".watch.{}", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, dir().join("watch.json"));
        }
    }
    fresh
}

pub fn message(n: &Notice) -> String {
    let took = crate::registry::duration(n.took_ms);
    match n.kind {
        Kind::NeedsYou => format!("{} needs you", n.title),
        Kind::Finished => format!("{} finished after {took}", n.title),
    }
}

/// Tell the person: a brief message in every attached tmux client, and the
/// optional notify_command (e.g. a phone push) with the details in its env.
pub fn announce(cfg: &Config, n: &Notice) {
    let kind = match n.kind {
        Kind::NeedsYou => "needs-you",
        Kind::Finished => "finished",
    };
    announce_text(cfg, &message(n), kind, &n.title, &n.id);
}

/// The same, for any message (usage warnings have no session).
pub fn announce_text(cfg: &Config, msg: &str, kind: &str, title: &str, id: &str) {
    for sv in tmux::servers() {
        if let Ok(clients) = tmux::run_on(&sv, &["list-clients", "-F", "#{client_name}"]) {
            for c in clients.lines() {
                let _ = tmux::run_on(
                    &sv,
                    &[
                        "display-message",
                        "-c",
                        c,
                        "-d",
                        "4000",
                        &format!("toomux · {msg}"),
                    ],
                );
            }
        }
    }
    if let Some(cmd) = cfg
        .notify_command
        .as_deref()
        .filter(|c| !c.trim().is_empty())
    {
        let spawned = std::process::Command::new("sh")
            .args(["-c", cmd])
            .env("TOOMUX_MESSAGE", msg)
            .env("TOOMUX_TITLE", title)
            .env("TOOMUX_EVENT", kind)
            .env("TOOMUX_SESSION_ID", id)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Err(e) = spawned {
            let _ = writeln!(std::io::stderr(), "notify_command: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_notice_state_is_ignored() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-watch-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &tmp) };
        std::fs::create_dir_all(dir()).unwrap();
        std::fs::write(dir().join("watch.json"), "{broken").unwrap();
        assert!(notices().is_empty());
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn notice_messages_keep_the_session_title() {
        let n = Notice {
            pid: 1,
            id: "s".into(),
            title: "Deploy".into(),
            kind: Kind::NeedsYou,
            took_ms: 0,
            at_ms: 0,
        };
        assert_eq!(message(&n), "Deploy needs you");
        let mut finished = n;
        finished.kind = Kind::Finished;
        finished.took_ms = 90_000;
        assert_eq!(message(&finished), "Deploy finished after 1m");
    }
}
