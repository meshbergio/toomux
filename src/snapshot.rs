//! What was running, so it can be brought back after a restart.
//!
//! `toomux status` keeps ~/.local/state/toomux/running.json current. When it
//! finds that file was written during an earlier boot, it moves it aside to
//! restore.json, which the UI offers as "before the restart".

use crate::config::Config;
use crate::registry::Session;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Entry {
    pub id: String,
    pub cwd: String,
    pub account: String,
    pub title: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// tmux session and window name it lived in, if any.
    pub tmux_session: Option<String>,
    pub window: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Snap {
    boot: String,
    at_ms: i64,
    entries: Vec<Entry>,
    /// Set while a sudden mass exit is being doubted (see `record`).
    shrink_since: Option<i64>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Restore {
    pub at_ms: i64,
    pub entries: Vec<Entry>,
}

fn dir() -> PathBuf {
    crate::paths::state()
}

fn boot_id() -> String {
    crate::platform::boot_id()
}

fn read<T: for<'de> Deserialize<'de> + Default>(name: &str) -> T {
    std::fs::read_to_string(dir().join(name)).ok().and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default()
}

fn write<T: Serialize>(name: &str, v: &T) {
    let _ = std::fs::create_dir_all(dir());
    if let Ok(json) = serde_json::to_string_pretty(v) {
        let tmp = dir().join(format!(".{name}.{}", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, dir().join(name));
        }
    }
}

fn entry(cfg: &Config, s: &Session) -> Entry {
    let window = s.pane.as_ref().and_then(|p| {
        // Only a name someone chose; tmux's automatic one is just the command.
        crate::tmux::run(&["display-message", "-p", "-t", &p.id, "#{?automatic-rename,,#{window_name}}"])
            .ok()
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty())
    });
    Entry {
        id: s.id.clone(),
        cwd: s.cwd.clone(),
        account: s.account_name(cfg).to_string(),
        title: s.title.clone(),
        args: crate::actions::carried_args(&s.args),
        tmux_session: s.pane.as_ref().map(|p| p.session.clone()),
        window,
    }
}

/// Bring running.json up to date. Cheap when nothing changed: it compares
/// session ids and places before doing any tmux lookups or writes.
pub fn record(cfg: &Config, sessions: &[Session], now: i64) {
    let Ok(_guard) = crate::lock::exclusive(&dir().join("snapshot.lock")) else {
        return;
    };
    let boot = boot_id();
    let mut snap: Snap = read("running.json");
    if snap.boot != boot {
        if !snap.entries.is_empty() {
            write("restore.json", &Restore { at_ms: snap.at_ms, entries: std::mem::take(&mut snap.entries) });
            if cfg.reopen_after_restart {
                let _ = std::fs::write(dir().join(REOPEN_DUE), "");
            }
        }
        snap = Snap { boot: boot.clone(), ..Default::default() };
    }
    let live: Vec<&Session> = sessions.iter().filter(|s| !s.dormant).collect();
    let same = live.len() == snap.entries.len()
        && live.iter().all(|s| {
            snap.entries.iter().any(|e| {
                e.id == s.id && e.title == s.title && e.tmux_session == s.pane.as_ref().map(|p| p.session.clone())
            })
        });
    if same {
        if snap.shrink_since.take().is_some() {
            write("running.json", &snap);
        }
        return;
    }
    // A shutdown ends every session at once, usually before tmux itself
    // goes. Don't let that wipe the record: doubt a sudden loss of more than
    // half for two minutes before believing it.
    let lost = snap.entries.iter().filter(|e| !live.iter().any(|s| s.id == e.id)).count();
    if snap.entries.len() >= 3 && lost * 2 > snap.entries.len() {
        match snap.shrink_since {
            None => {
                snap.shrink_since = Some(now);
                write("running.json", &snap);
                return;
            }
            Some(t) if now - t < 120_000 => return,
            Some(_) => {
                // Real: a tmux server went down with them (a reboot is
                // caught above). Offer them back, as after a restart.
                let lost: Vec<Entry> = snap.entries.iter().filter(|e| !live.iter().any(|s| s.id == e.id)).cloned().collect();
                let mut r: Restore = read("restore.json");
                r.entries.retain(|e| !lost.iter().any(|l| l.id == e.id));
                r.entries.extend(lost);
                r.at_ms = now;
                write("restore.json", &r);
            }
        }
    }
    snap.entries = live.iter().map(|s| entry(cfg, s)).collect();
    snap.at_ms = now;
    snap.shrink_since = None;
    write("running.json", &snap);
}

/// Left by the first record after a reboot: what was running is to be
/// reopened (`toomux reopen --after-restart`).
const REOPEN_DUE: &str = "reopen-due";

pub fn reopen_due() -> bool {
    dir().join(REOPEN_DUE).exists()
}

/// Take the reopen on: true for one caller only.
pub fn claim_reopen() -> bool {
    std::fs::remove_file(dir().join(REOPEN_DUE)).is_ok()
}

/// What was last recorded running, and when.
pub fn recorded() -> (i64, Vec<Entry>) {
    let snap: Snap = read("running.json");
    (snap.at_ms, snap.entries)
}

/// Sessions from before the last restart that aren't running now.
pub fn restorable(running: &[Session]) -> Restore {
    let mut r: Restore = read("restore.json");
    // Don't offer things from long ago forever.
    if crate::registry::now_ms() - r.at_ms > 7 * 86_400_000 {
        return Restore::default();
    }
    r.entries.retain(|e| !running.iter().any(|s| !s.dormant && s.id == e.id));
    r
}

/// Stop offering these (restored, or dismissed).
pub fn forget(ids: &[String]) {
    let Ok(_guard) = crate::lock::exclusive(&dir().join("snapshot.lock")) else {
        return;
    };
    let mut r: Restore = read("restore.json");
    let before = r.entries.len();
    r.entries.retain(|e| !ids.contains(&e.id));
    if r.entries.len() != before {
        if r.entries.is_empty() {
            let _ = std::fs::remove_file(dir().join("restore.json"));
        } else {
            write("restore.json", &r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn after_a_reboot_what_ran_is_reopened_once() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-reboot-{}", std::process::id()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp) };
        let earlier = |cfg: &Config| {
            let e = Entry { id: "a".into(), cwd: "/".into(), account: "x".into(), title: "work".into(), args: vec![], tmux_session: None, window: None };
            write("running.json", &Snap { boot: "an earlier boot".into(), at_ms: 1, entries: vec![e], shrink_since: None });
            record(cfg, &[], crate::registry::now_ms());
        };
        let mut cfg = Config::default();
        earlier(&cfg);
        let r: Restore = read("restore.json");
        assert_eq!(r.entries.len(), 1, "offered under before the restart");
        assert!(reopen_due());
        assert!(claim_reopen(), "one taker");
        assert!(!claim_reopen() && !reopen_due(), "and only one");
        cfg.reopen_after_restart = false;
        earlier(&cfg);
        assert!(!reopen_due(), "off: only offered");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn truncated_snapshot_files_are_treated_as_empty() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-snapshot-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp) };
        std::fs::create_dir_all(dir()).unwrap();
        std::fs::write(dir().join("running.json"), "{").unwrap();
        std::fs::write(dir().join("restore.json"), "not-json").unwrap();
        assert_eq!(recorded(), (0, vec![]));
        assert!(restorable(&[]).entries.is_empty());
        let _ = std::fs::remove_dir_all(tmp);
    }
}
