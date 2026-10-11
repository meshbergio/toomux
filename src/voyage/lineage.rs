//! Where a voyage's conversation went. Claude Code gives a conversation a
//! new id on /clear, a resume into a new session or a branch, and leaves a
//! `continued-in` record in the old transcript; a handover's successor names
//! its predecessor in its first prompt; and the Claude Code process a hook
//! runs for has its own session file. A voyage follows all three, so it isn't
//! left behind on an id nobody uses any more.

use crate::config::Config;
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// How a handover's successor is started (handover.rs writes it): its first
/// prompt names the conversation it took over from.
pub(super) const HANDOVER_PROMPT: &str = "Continue from a handover. The previous session here (id ";

fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Conversation `id`'s transcript, begun in `cwd`: beside `near` (another
/// transcript of the same folder) first, then in every account.
pub(super) fn transcript_of(
    cfg: &Config,
    cwd: &str,
    id: &str,
    near: Option<&Path>,
) -> Option<PathBuf> {
    if !plain_id(id) {
        return None;
    }
    let file = format!("{id}.jsonl");
    if let Some(p) = near
        .and_then(Path::parent)
        .map(|d| d.join(&file))
        .filter(|p| p.is_file())
    {
        return Some(p);
    }
    let dir: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    crate::index::roots(cfg)
        .into_iter()
        .map(|r| r.join(&dir).join(&file))
        .find(|p| p.is_file())
}

/// Up to `n` bytes of a file, from its start or its end, as whole lines.
fn read_part(path: &Path, n: u64, from_end: bool) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = if from_end { len.saturating_sub(n) } else { 0 };
    let mut bytes = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.take(n).read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let start = if from > 0 {
        text.find('\n').map_or(text.len(), |i| i + 1)
    } else {
        0
    };
    let end = if from_end || from + n >= len {
        text.len()
    } else {
        text.rfind('\n').map_or(start, |i| i + 1).max(start)
    };
    text[start..end].to_string()
}

/// The conversation this one continued in, if Claude Code recorded one. The
/// record lands as the old conversation ends, so its tail holds it.
pub(super) fn continued_in(transcript: &Path) -> Option<String> {
    read_part(transcript, 256 * 1024, true)
        .lines()
        .rev()
        .filter(|l| l.contains("\"continued-in\""))
        .find_map(|l| {
            let v: Value = serde_json::from_str(l).ok()?;
            if v.get("type").and_then(Value::as_str) != Some("continued-in") {
                return None;
            }
            v.get("continuedInSessionId")
                .and_then(Value::as_str)
                .filter(|s| plain_id(s))
                .map(str::to_string)
        })
}

/// Every conversation `id` went on in, in order: `continued-in` records
/// followed a few hops, never round a loop.
pub(super) fn successors(cfg: &Config, cwd: &str, id: &str, near: Option<&Path>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = id.to_string();
    for _ in 0..8 {
        let Some(next) = transcript_of(cfg, cwd, &cur, near).and_then(|p| continued_in(&p)) else {
            break;
        };
        if next == id || out.contains(&next) {
            break;
        }
        out.push(next.clone());
        cur = next;
    }
    out
}

/// The conversation a handover's successor took over from, by its prompt.
pub(super) fn handed_over_from(prompt: &str) -> Option<String> {
    let rest = prompt.trim_start().strip_prefix(HANDOVER_PROMPT)?;
    let id = rest.split(')').next()?.trim();
    plain_id(id).then(|| id.to_string())
}

/// The same, from the successor's transcript: its first prompt.
pub(super) fn handed_over_from_transcript(transcript: &Path) -> Option<String> {
    read_part(transcript, 256 * 1024, false)
        .lines()
        .filter(|l| l.contains("Continue from a handover."))
        .find_map(|l| {
            let v: Value = serde_json::from_str(l).ok()?;
            if v.get("type").and_then(Value::as_str) != Some("user") {
                return None;
            }
            let text = match v.pointer("/message/content")? {
                Value::String(s) => s.clone(),
                Value::Array(a) => a
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => return None,
            };
            handed_over_from(&text)
        })
}

/// When a transcript began: its first entry's timestamp, in ms.
pub(super) fn began_ms(transcript: &Path) -> Option<i64> {
    read_part(transcript, 64 * 1024, false)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| {
            let t = v.get("timestamp")?.as_str()?;
            chrono::DateTime::parse_from_rfc3339(t)
                .ok()
                .map(|t| t.timestamp_millis())
        })
}

pub(super) fn modified_ms(path: &Path) -> Option<i64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64)
}

/// The Claude Code process a hook runs for: the nearest ancestor with a
/// session file of its own. (pid, its start, the session id it records)
pub(super) fn hook_process(cfg: &Config) -> Option<(i32, Option<String>, Option<String>)> {
    let dirs = cfg.session_dirs();
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..8 {
        if pid <= 1 {
            return None;
        }
        if let Some(file) = dirs
            .iter()
            .map(|d| d.join(format!("{pid}.json")))
            .find(|f| f.is_file())
        {
            let id = std::fs::read_to_string(file)
                .ok()
                .and_then(|r| serde_json::from_str::<Value>(&r).ok())
                .and_then(|v| v.get("sessionId")?.as_str().map(str::to_string));
            return Some((pid, crate::registry::proc_start(pid), id));
        }
        pid = parent_of(pid)?;
    }
    None
}

#[cfg(target_os = "linux")]
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid …": comm may hold spaces and parentheses.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn parent_of(pid: i32) -> Option<i32> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conversation_is_followed_through_its_continued_in_records() {
        let dir = std::env::temp_dir().join(format!("toomux-lineage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("aaaa-1.jsonl");
        std::fs::write(
            &a,
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n\
             {\"type\":\"continued-in\",\"sessionId\":\"aaaa-1\",\"continuedInSessionId\":\"bbbb-2\"}\n\
             {\"type\":\"custom-title\",\"customTitle\":\"x\"}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("bbbb-2.jsonl"),
            "{\"type\":\"continued-in\",\"continuedInSessionId\":\"cccc-3\"}\n",
        )
        .unwrap();
        // A loop back to the start ends the chain.
        std::fs::write(
            dir.join("cccc-3.jsonl"),
            "{\"type\":\"continued-in\",\"continuedInSessionId\":\"aaaa-1\"}\n",
        )
        .unwrap();
        assert_eq!(continued_in(&a).as_deref(), Some("bbbb-2"));
        let cfg = Config::default();
        assert_eq!(
            successors(&cfg, "/nowhere", "aaaa-1", Some(&a)),
            vec!["bbbb-2".to_string(), "cccc-3".to_string()]
        );
        assert!(successors(&cfg, "/nowhere", "../etc", Some(&a)).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_handover_successor_names_its_predecessor_in_its_first_prompt() {
        assert_eq!(
            handed_over_from(
                "Continue from a handover. The previous session here (id 0e09-77ab) reached 300k tokens"
            )
            .as_deref(),
            Some("0e09-77ab")
        );
        assert!(handed_over_from("Continue from a handover, please").is_none());
        let dir = std::env::temp_dir().join(format!("toomux-lineage-h-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("t.jsonl");
        std::fs::write(
            &t,
            serde_json::json!({"type": "user", "timestamp": "2026-10-04T15:16:19.508Z", "message": {"content": "Continue from a handover. The previous session here (id old-1) reached 1k tokens"}})
                .to_string()
                + "\n",
        )
        .unwrap();
        assert_eq!(handed_over_from_transcript(&t).as_deref(), Some("old-1"));
        assert_eq!(began_ms(&t), Some(1_791_126_979_508));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
