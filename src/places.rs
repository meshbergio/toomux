//! Folders you work in, ranked by frecency from Claude Code's prompt history,
//! and which account a new session in one should use.

use crate::config::{Config, expand};
use crate::registry::Session;
use serde_json::Value;
use std::collections::HashMap;
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
/// of your running sessions were started with.
pub fn new_args(cfg: &Config, running: &[Session]) -> Vec<String> {
    if let Some(a) = &cfg.new_session_args {
        return a.clone();
    }
    let mut counts: HashMap<Vec<String>, usize> = HashMap::new();
    for s in running.iter().filter(|s| !s.dormant) {
        *counts
            .entry(crate::actions::carried_args(&s.args))
            .or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(a, n)| (*n, a.len()))
        .map(|(a, _)| a)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::weight;

    #[test]
    fn recent_use_outranks_old_volume() {
        let day = 86_400_000;
        let old: f64 = (0..2590).map(|_| weight(109 * day)).sum();
        let recent: f64 = (0..40).map(|_| weight(day)).sum();
        assert!(recent > old * 10.0, "recent={recent} old={old}");
    }
}
