//! Moves waiting for a session to go idle. Each is a detached
//! `toomux switch --wait` process; a small file per session records it so the
//! UI can show it and cancel it. Files live in the runtime dir, so a reboot
//! clears them along with the processes they describe.

use crate::registry;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub struct Queued {
    pub pid: i32,
    pub proc_start: Option<String>,
    /// Target account name.
    pub to: String,
    /// The waiting `toomux switch --wait` process.
    pub waiter: i32,
    pub waiter_start: Option<String>,
}

fn dir() -> PathBuf {
    crate::paths::runtime().join("queue")
}

fn file(pid: i32) -> PathBuf {
    dir().join(format!("{pid}.json"))
}

/// The queued move for a live session, if one is still waiting. Stale records
/// (either process gone or its pid reused) are cleaned up on sight.
pub fn get(pid: i32, proc_start: Option<&str>) -> Option<Queued> {
    let path = file(pid);
    let raw = std::fs::read_to_string(&path).ok()?;
    let q: Queued = serde_json::from_str(&raw).ok()?;
    let valid = q.proc_start.as_deref() == proc_start && registry::alive(q.waiter, q.waiter_start.as_deref());
    if !valid {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(q)
}

/// Record that this process is waiting to move `pid` to `to`.
pub fn put(pid: i32, proc_start: Option<&str>, to: &str) {
    let me = std::process::id() as i32;
    let q = Queued {
        pid,
        proc_start: proc_start.map(str::to_string),
        to: to.to_string(),
        waiter: me,
        waiter_start: registry::proc_start(me),
    };
    let _ = std::fs::create_dir_all(dir());
    if let Ok(json) = serde_json::to_string(&q) {
        let tmp = dir().join(format!(".{pid}.{me}"));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, file(pid));
        }
    }
}

/// Drop the record, but only if this process is the one that wrote it.
pub fn done(pid: i32) {
    let me = std::process::id() as i32;
    let path = file(pid);
    let mine = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Queued>(&raw).ok())
        .is_some_and(|q| q.waiter == me);
    if mine {
        let _ = std::fs::remove_file(path);
    }
}

/// Stop a waiting move.
pub fn cancel(pid: i32, proc_start: Option<&str>) -> bool {
    let Some(q) = get(pid, proc_start) else { return false };
    unsafe { libc::kill(q.waiter, libc::SIGTERM) };
    let _ = std::fs::remove_file(file(pid));
    true
}
