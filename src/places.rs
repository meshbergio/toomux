//! Folders you work in, ranked by frecency from Claude Code's prompt history,
//! and which account a new session in one should use.

use crate::config::{Config, expand};
use crate::registry::Session;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Debug)]
pub struct Place {
    pub path: String,
    pub score: f64,
    pub last_ms: i64,
    pub prompts: u32,
    /// A few recent prompts there, newest first.
    pub recent: Vec<String>,
}

const TAIL: u64 = 4 * 1024 * 1024;

/// Each prompt counts for less the older it is: half as much per week.
fn weight(age_ms: i64) -> f64 {
    let days = age_ms.max(0) as f64 / 86_400_000.0;
    0.5f64.powf(days / 7.0)
}

/// Every distinct history.jsonl across accounts (they're often one shared file).
fn history_files(cfg: &Config) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for i in 0..cfg.accounts.len() {
        let p = cfg.account_dir(i).join("history.jsonl");
        if let Ok(c) = std::fs::canonicalize(&p)
            && !out.contains(&c)
        {
            out.push(c);
        }
    }
    out
}

pub fn frecent(cfg: &Config, now: i64) -> Vec<Place> {
    let mut by: HashMap<String, Place> = HashMap::new();
    let tmp = std::env::temp_dir().display().to_string();
    for path in history_files(cfg) {
        let Ok(mut f) = std::fs::File::open(&path) else {
            continue;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let from = len.saturating_sub(TAIL);
        if f.seek(SeekFrom::Start(from)).is_err() {
            continue;
        }
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes);
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines().skip(usize::from(from > 0)) {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let (Some(project), Some(ts)) = (
                v.get("project").and_then(Value::as_str),
                v.get("timestamp").and_then(Value::as_i64),
            ) else {
                continue;
            };
            if project.starts_with(&tmp)
                || project.starts_with("/tmp/")
                || project.starts_with("/private/tmp/")
            {
                continue;
            }
            let e = by.entry(project.to_string()).or_insert_with(|| Place {
                path: project.to_string(),
                score: 0.0,
                last_ms: 0,
                prompts: 0,
                recent: Vec::new(),
            });
            e.score += weight(now - ts);
            e.prompts += 1;
            e.last_ms = e.last_ms.max(ts);
            if let Some(d) = v.get("display").and_then(Value::as_str) {
                let d = d.trim();
                if !d.is_empty() && !d.starts_with('/') {
                    e.recent.push(d.to_string());
                }
            }
        }
    }
    let mut out: Vec<Place> = by
        .into_values()
        .filter(|p| std::path::Path::new(&p.path).is_dir())
        .collect();
    for p in out.iter_mut() {
        p.recent.reverse();
        p.recent.dedup();
        p.recent.truncate(5);
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.last_ms.cmp(&a.last_ms)));
    out
}

/// The account a new session in `folder` should start on, and why:
/// the most specific matching rule, else the account most of the sessions
/// running there use, else the first account that isn't at its limit.
pub fn account_for(cfg: &Config, folder: &str, running: &[Session]) -> (usize, &'static str) {
    let folder_path = expand(folder);
    let rule = cfg
        .rules
        .iter()
        .filter_map(|r| {
            let base = expand(&r.folder);
            folder_path
                .starts_with(&base)
                .then(|| (base.components().count(), r))
        })
        .max_by_key(|(depth, _)| *depth)
        .and_then(|(_, r)| cfg.account_by_name(&r.account));
    if let Some(i) = rule {
        return (i, "rule");
    }
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for s in running.iter().filter(|s| !s.dormant && s.cwd == folder) {
        if let Some(a) = s.account {
            *counts.entry(a).or_default() += 1;
        }
    }
    if let Some((&a, _)) = counts.iter().max_by_key(|(_, n)| **n) {
        return (a, "used here");
    }
    let limited: Vec<usize> = running
        .iter()
        .filter(|s| s.limit.is_some())
        .filter_map(|s| s.account)
        .collect();
    let free = (0..cfg.accounts.len()).find(|i| !limited.contains(i));
    (
        free.unwrap_or(0),
        if limited.is_empty() {
            "default"
        } else {
            "not at its limit"
        },
    )
}

/// Launch flags for a new session: the configured ones, else the flags most
/// of your running sessions were started with. Ties go to the longer set of
/// flags, then to the first in sorted order, so the answer never depends on
/// hash order.
pub fn new_args(cfg: &Config, running: &[Session]) -> Vec<String> {
    if let Some(a) = &cfg.new_session_args {
        return a.clone();
    }
    most_common_flags(
        running
            .iter()
            .filter(|s| !s.dormant)
            .map(|s| s.args.as_slice()),
    )
}

fn most_common_flags<'a>(argvs: impl Iterator<Item = &'a [String]>) -> Vec<String> {
    let mut counts: BTreeMap<Vec<String>, usize> = BTreeMap::new();
    for argv in argvs {
        *counts.entry(reusable_args(argv)).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|(a, n), (b, m)| {
            n.cmp(m)
                .then(a.len().cmp(&b.len()))
                // Equal counts and lengths: the first sorted one wins.
                .then(b.cmp(a))
        })
        .map(|(a, _)| a)
        .unwrap_or_default()
}

/// A running session's launch flags that make sense for a different, new
/// conversation: `carried_args` (no conversation selector, no prompt) less
/// what belongs to that one conversation alone: its name, and the handover
/// briefs folder toomux gave it.
pub fn reusable_args(argv: &[String]) -> Vec<String> {
    let carried = crate::actions::carried_args(argv);
    let mut out = Vec::new();
    let mut it = carried.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" | "-n" => {
                it.next();
            }
            _ if a.starts_with("--name=") || a.starts_with("-n=") => {}
            "--add-dir" => {
                let mut dirs = Vec::new();
                while let Some(d) = it.next_if(|n| !n.starts_with('-')) {
                    if !is_handover_dir(&d) {
                        dirs.push(d);
                    }
                }
                if !dirs.is_empty() {
                    out.push(a);
                    out.extend(dirs);
                }
            }
            _ if a.strip_prefix("--add-dir=").is_some_and(is_handover_dir) => {}
            _ => out.push(a),
        }
    }
    out
}

/// The folder handover adds to a successor so it can read its brief: toomux's
/// own, under this or any other state home.
fn is_handover_dir(dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    let ours = crate::handover::dir();
    std::path::Path::new(dir) == ours || expand(dir) == ours || dir.ends_with("/toomux/handovers")
}

#[cfg(test)]
mod tests {
    use super::{most_common_flags, reusable_args, weight};

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn handovers() -> String {
        crate::handover::dir().display().to_string()
    }

    #[test]
    fn reusable_args_drop_name_resume_prompt_and_handover_dir() {
        let h = handovers();
        let argv = v(&[
            "claude",
            "--dangerously-skip-permissions",
            "--model",
            "opus",
            "--add-dir",
            &h,
            "--name",
            "Widgets",
            "--resume",
            "0b6c6a1e-1111-2222-3333-444455556666",
        ]);
        assert_eq!(
            reusable_args(&argv),
            v(&["--dangerously-skip-permissions", "--model", "opus"])
        );
        let add_eq = format!("--add-dir={h}");
        let argv = v(&[
            "claude",
            "-n=Fix it",
            "--name=Other",
            &add_eq,
            "-c",
            "do the thing",
        ]);
        assert!(
            reusable_args(&argv).is_empty(),
            "{:?}",
            reusable_args(&argv)
        );
        // Another HOME's handovers folder is still toomux's, not yours.
        let argv = v(&[
            "claude",
            "--add-dir",
            "/home/someone/.local/state/toomux/handovers/",
        ]);
        assert!(reusable_args(&argv).is_empty());
    }

    #[test]
    fn reusable_args_keep_your_own_add_dirs() {
        let h = handovers();
        let argv = v(&[
            "claude",
            "--add-dir",
            "/work/a",
            &h,
            "/work/b",
            "--model",
            "x",
        ]);
        assert_eq!(
            reusable_args(&argv),
            v(&["--add-dir", "/work/a", "/work/b", "--model", "x"])
        );
    }

    #[test]
    fn handover_successors_never_name_a_new_session() {
        let h = handovers();
        // Every handover successor carries its own --name; none may become
        // the new session's flags, whatever order the sessions come in.
        let mut argvs = [
            v(&["claude", "--add-dir", &h, "--name", "Widgets"]),
            v(&["claude", "--add-dir", &h, "--name", "A much longer name"]),
            v(&[
                "claude",
                "--add-dir",
                &h,
                "--name",
                "Third",
                "--resume",
                "x",
            ]),
            v(&["claude", "--model", "opus"]),
        ];
        let first = most_common_flags(argvs.iter().map(Vec::as_slice));
        assert!(first.is_empty(), "{first:?}");
        argvs.reverse();
        assert_eq!(most_common_flags(argvs.iter().map(Vec::as_slice)), first);
    }

    #[test]
    fn new_session_flags_tie_break_is_deterministic() {
        let a = v(&["claude", "--model", "opus"]);
        let b = v(&["claude", "--model", "haiku"]);
        let forward = most_common_flags([a.as_slice(), b.as_slice()].into_iter());
        let backward = most_common_flags([b.as_slice(), a.as_slice()].into_iter());
        assert_eq!(forward, backward);
        assert_eq!(forward, v(&["--model", "haiku"]));
        // More sessions outvote longer flags.
        let c = v(&["claude"]);
        let picked = most_common_flags([a.as_slice(), c.as_slice(), c.as_slice()].into_iter());
        assert!(picked.is_empty());
    }

    #[test]
    fn recent_use_outranks_old_volume() {
        let day = 86_400_000;
        let old: f64 = (0..2590).map(|_| weight(109 * day)).sum();
        let recent: f64 = (0..40).map(|_| weight(day)).sum();
        assert!(recent > old * 10.0, "recent={recent} old={old}");
    }
}
