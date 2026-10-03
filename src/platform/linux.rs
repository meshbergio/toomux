//! Linux (and Windows through WSL 2, which is Linux): /proc.

use std::collections::HashSet;
use std::path::PathBuf;

/// The executable backing a live process.
pub fn exe(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// A process's state letter and the rest of its stat line after the name.
fn stat(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(stat[stat.rfind(')')? + 1..].trim_start().to_string())
}

/// Kernel start time of a live pid, used to reject reused pids. Zombies count
/// as gone: tmux keeps a dead pane's process unreaped.
pub fn start_time(pid: i32) -> Option<String> {
    let after = stat(pid)?;
    let mut fields = after.split_whitespace();
    if matches!(fields.next(), Some("Z" | "X")) {
        return None;
    }
    fields.nth(18).map(str::to_string)
}

/// A process that has exited but not been reaped.
pub fn is_zombie(pid: i32) -> bool {
    stat(pid).is_some_and(|s| s.starts_with('Z'))
}

fn nul_split(path: String) -> Vec<String> {
    std::fs::read(path)
        .map(|b| {
            b.split(|c| *c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A process's environment, as `KEY=value` strings.
pub fn environ(pid: i32) -> Vec<String> {
    nul_split(format!("/proc/{pid}/environ"))
}

/// A process's command line.
pub fn cmdline(pid: i32) -> Vec<String> {
    nul_split(format!("/proc/{pid}/cmdline"))
}

/// What a process's standard input is (a terminal's device path, say).
pub fn stdin(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/fd/0")).ok()
}

/// All currently-live descendants of a process, nearest children first.
///
/// Linux exposes each task's direct children without scanning every process.
/// Keep a seen set because the tree can change while it is being walked and a
/// rapidly reused pid must never turn a transient kernel view into a loop.
pub fn descendants(pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut queue = vec![pid];
    let mut seen = HashSet::from([pid]);
    let mut at = 0;
    while at < queue.len() {
        let parent = queue[at];
        at += 1;
        let path = format!("/proc/{parent}/task/{parent}/children");
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for child in text
            .split_whitespace()
            .filter_map(|s| s.parse::<i32>().ok())
        {
            if child > 0 && seen.insert(child) {
                out.push(child);
                queue.push(child);
            }
        }
    }
    out
}

/// Every process's current folder.
pub fn folders_in_use() -> Vec<PathBuf> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|p| std::fs::read_link(p.path().join("cwd")).ok())
        .collect()
}

/// Changes at every boot: how toomux tells a restart from a lost tmux server.
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}
