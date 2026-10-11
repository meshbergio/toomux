//! Last few exchanges of a session, read from the tail of its transcript.

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub enum Who {
    You,
    Claude,
}

pub struct Turn {
    pub who: Who,
    pub text: String,
}

const TAIL_BYTES: u64 = 768 * 1024;

pub fn tail(path: &Path, max_turns: usize) -> Vec<Turn> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(TAIL_BYTES);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return Vec::new();
    }
    let mut buf = String::new();
    if f.take(TAIL_BYTES).read_to_string(&mut buf).is_err() {
        let mut bytes = Vec::new();
        let _ = std::fs::File::open(path).and_then(|mut f| {
            f.seek(SeekFrom::Start(from))?;
            f.read_to_end(&mut bytes)
        });
        buf = String::from_utf8_lossy(&bytes).into_owned();
    }
    let lines = buf.lines().skip(usize::from(from > 0));

    let mut turns: Vec<Turn> = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("isMeta").and_then(Value::as_bool) == Some(true)
            || v.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let who = match v.get("type").and_then(Value::as_str) {
            Some("user") => Who::You,
            Some("assistant") => Who::Claude,
            _ => continue,
        };
        let Some(text) = text_of(v.pointer("/message/content")) else {
            continue;
        };
        let text = text.trim();
        // Skip slash-command plumbing and system-injected wrappers.
        if text.is_empty() || text.starts_with('<') || text.starts_with("Caveat:") {
            continue;
        }
        match (turns.last_mut(), &who) {
            (Some(last), Who::Claude) if matches!(last.who, Who::Claude) => {
                last.text = text.to_string();
            }
            _ => turns.push(Turn {
                who,
                text: text.to_string(),
            }),
        }
    }
    let skip = turns.len().saturating_sub(max_turns);
    turns.into_iter().skip(skip).collect()
}

/// What a transcript says about its session, beyond the conversation itself.
#[derive(Clone, Default)]
pub struct Meta {
    /// Claude Code's own summary of the conversation (`ai-title` entries).
    pub title: Option<String>,
    /// A name set with /rename (`custom-title` entries).
    pub custom: Option<String>,
    pub last_prompt: Option<String>,
    /// Most recent pull request the session opened: (repo, number).
    pub pr: Option<(String, u64)>,
    /// The latest reply, if it was a usage-limit error.
    limit_hit: Option<LimitHit>,
}

/// A reply that was a usage-limit error.
#[derive(Clone, Debug, PartialEq)]
pub struct LimitHit {
    /// "weekly limit · resets Oct 3, 5pm".
    pub text: String,
    /// When the error was written.
    pub at: Option<DateTime<Local>>,
}

/// How long a limit that names no reset ("You've reached your Fable limit",
/// a spend limit) is taken to last. Short, so a limit nobody can time doesn't
/// hold handovers and voyages back for days: a later real reply clears it
/// sooner, and hitting it again starts it over.
pub const UNTIMED_LIMIT: chrono::Duration = chrono::Duration::hours(5);

impl LimitHit {
    /// When it lifts, if it says.
    pub fn reset(&self) -> Option<DateTime<Local>> {
        reset_time(&self.text, self.at)
    }

    /// Whether it still holds at `now`.
    pub fn active_at(&self, now: DateTime<Local>) -> bool {
        match (self.at, self.reset()) {
            (_, Some(reset)) => now < reset,
            (Some(at), None) => now - at < UNTIMED_LIMIT,
            (None, None) => true,
        }
    }

    /// A main-chain assistant entry that is a usage-limit error.
    pub fn from_entry(v: &Value) -> Option<Self> {
        if v.get("error").and_then(Value::as_str) != Some("rate_limit") {
            return None;
        }
        let text = text_of(v.pointer("/message/content")).unwrap_or_else(|| "usage limit".into());
        let text = text
            .trim()
            .trim_start_matches("You've hit your ")
            .to_string();
        let text = text.split(" (").next().unwrap_or(&text).to_string();
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok());
        Some(LimitHit {
            text,
            at: at.map(|t| t.with_timezone(&Local)),
        })
    }
}

/// How the conversation's latest turn ended, if a usage limit ended it:
/// `Some(true)` when the limit cut it off mid-work (Claude had already
/// replied or used a tool since the prompt, or an earlier turn was cut off
/// and every prompt since has only met the limit), `Some(false)` when the
/// limit was the only answer a prompt got. `None`: the latest reply isn't a
/// limit error.
pub fn cut_off_by_limit(path: &Path) -> Option<bool> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return None;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    f.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    cut_off_in(text.lines().skip(usize::from(from > 0)))
}

fn cut_off_in<'a>(lines: impl Iterator<Item = &'a str>) -> Option<bool> {
    let mut worked = false;
    let mut ended_at_limit: Option<bool> = None;
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => match v.get("error") {
                None | Some(Value::Null) => {
                    worked = true;
                    ended_at_limit = None;
                }
                Some(_) if LimitHit::from_entry(&v).is_some() => ended_at_limit = Some(worked),
                Some(_) => {}
            },
            Some("user") if v.get("isMeta").and_then(Value::as_bool) != Some(true) => {
                let content = v.pointer("/message/content");
                let tool_result = content.and_then(Value::as_array).is_some_and(|a| {
                    a.iter()
                        .any(|p| p.get("type").and_then(Value::as_str) == Some("tool_result"))
                });
                if tool_result {
                    worked = true;
                } else if text_of(content).is_some() {
                    // A new prompt: its turn starts fresh, unless the turn
                    // before it was cut off and never got going again.
                    worked = ended_at_limit == Some(true);
                }
            }
            _ => {}
        }
    }
    ended_at_limit
}

impl Meta {
    /// If the conversation's latest reply is a usage-limit error, its message
    /// ("weekly limit · resets Oct 3, 5pm"). A later real reply clears it.
    pub fn limit(&self) -> Option<String> {
        let hit = self.limit_hit.as_ref()?;
        hit.active_at(Local::now()).then(|| hit.text.clone())
    }

    /// The latest reply's usage-limit error, whether or not it has lifted.
    pub fn limit_hit(&self) -> Option<&LimitHit> {
        self.limit_hit.as_ref()
    }

    fn absorb(&mut self, line: &str) {
        // Cheap prefilter: most lines are tool traffic we never need to parse.
        let kind = [
            "\"type\":\"assistant\"",
            "\"type\":\"ai-title\"",
            "\"type\":\"custom-title\"",
            "\"type\":\"last-prompt\"",
            "\"type\":\"pr-link\"",
        ]
        .into_iter()
        .position(|k| line.contains(k));
        let Some(kind) = kind else { return };
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let str_at = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        match (kind, v.get("type").and_then(Value::as_str)) {
            (0, Some("assistant")) => {
                if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                    return;
                }
                // A limit error is the news until a real reply follows it.
                // Another kind of error (overloaded, a dropped connection)
                // says nothing about the limit either way.
                match v.get("error") {
                    None | Some(Value::Null) => self.limit_hit = None,
                    Some(_) => {
                        if let Some(hit) = LimitHit::from_entry(&v) {
                            self.limit_hit = Some(hit);
                        }
                    }
                }
            }
            (1, Some("ai-title")) => {
                self.title = str_at("aiTitle").filter(|t| !t.trim().is_empty())
            }
            (2, Some("custom-title")) => {
                self.custom = str_at("customTitle").filter(|t| !t.trim().is_empty())
            }
            (3, Some("last-prompt")) => {
                self.last_prompt = str_at("lastPrompt").filter(|t| !t.trim().is_empty())
            }
            (4, Some("pr-link")) => {
                if let (Some(repo), Some(n)) = (
                    str_at("prRepository"),
                    v.get("prNumber").and_then(Value::as_u64),
                ) {
                    self.pr = Some((repo, n));
                }
            }
            _ => {}
        }
    }
}

/// Metadata entries are re-appended every turn, so the tail holds the latest.
const META_TAIL: u64 = 128 * 1024;

struct Cached {
    /// Bytes consumed so far (always ends on a line boundary).
    upto: u64,
    meta: Meta,
}

static CACHE: std::sync::Mutex<Option<std::collections::HashMap<std::path::PathBuf, Cached>>> =
    std::sync::Mutex::new(None);

/// Read a transcript's metadata. Within one process this is incremental: only
/// bytes appended since the last call are read.
pub fn meta(path: &Path) -> Meta {
    let Ok(len) = std::fs::metadata(path).map(|m| m.len()) else {
        return Meta::default();
    };
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(Default::default);
    let (from, mut meta, fresh) = match cache.remove(path) {
        Some(c) if c.upto == len => {
            let m = c.meta.clone();
            cache.insert(path.to_path_buf(), c);
            return m;
        }
        Some(c) if c.upto < len && len - c.upto <= 4 * META_TAIL => (c.upto, c.meta, false),
        _ => (len.saturating_sub(META_TAIL), Meta::default(), true),
    };
    let mut bytes = Vec::new();
    if let Ok(mut f) = std::fs::File::open(path)
        && f.seek(SeekFrom::Start(from)).is_ok()
    {
        let _ = f.take(len - from).read_to_end(&mut bytes);
    }
    // Only whole lines: a writer may be mid-line at the end, and a tail read
    // starts mid-line at the front.
    let end = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let start = if fresh && from > 0 {
        bytes
            .iter()
            .position(|&b| b == b'\n')
            .map_or(end, |i| i + 1)
            .min(end)
    } else {
        0
    };
    for line in String::from_utf8_lossy(&bytes[start..end]).lines() {
        meta.absorb(line);
    }
    cache.insert(
        path.to_path_buf(),
        Cached {
            upto: from + end as u64,
            meta: meta.clone(),
        },
    );
    meta
}

/// When a limit message's reset time falls, read relative to now.
pub fn reset_at(text: &str) -> Option<DateTime<Local>> {
    reset_time(text, None)
}

/// "weekly limit · resets Oct 3, 5pm" / "resets 11:30pm" -> the local reset instant.
fn reset_time(text: &str, hit_at: Option<DateTime<Local>>) -> Option<DateTime<Local>> {
    let spec = text.split("resets ").nth(1)?.trim().to_lowercase();
    let (date, time) = match spec.rsplit_once(", ") {
        Some((d, t)) => (Some(d.to_string()), t.to_string()),
        None => (None, spec),
    };
    let pm = time.ends_with("pm");
    let clock = time.trim_end_matches("am").trim_end_matches("pm");
    let (h, m) = match clock.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (clock.parse::<u32>().ok()?, 0),
    };
    let h = match (h, pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, true) => h + 12,
        (h, false) => h,
    };
    let base = hit_at.unwrap_or_else(Local::now);
    let at_time = |d: NaiveDate| {
        Local
            .from_local_datetime(&d.and_hms_opt(h, m, 0)?)
            .earliest()
    };
    match date {
        Some(d) => {
            let mut parts = d.split_whitespace();
            let name = parts.next()?;
            let month = [
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ]
            .iter()
            .position(|mo| name.starts_with(mo))? as u32
                + 1;
            let day: u32 = parts.next()?.parse().ok()?;
            let mut reset = at_time(NaiveDate::from_ymd_opt(base.year(), month, day)?)?;
            if reset < base - chrono::Duration::days(1) {
                reset = at_time(NaiveDate::from_ymd_opt(base.year() + 1, month, day)?)?;
            }
            Some(reset)
        }
        None => {
            let today = at_time(base.date_naive())?;
            Some(if today > base {
                today
            } else {
                at_time(base.date_naive().succ_opt()?)?
            })
        }
    }
}

fn text_of(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Meta, reset_time};
    use chrono::{Local, TimeZone, Timelike};

    #[test]
    fn parses_dated_and_daily_resets() {
        let hit = Local.with_ymd_and_hms(2026, 9, 29, 15, 53, 0).unwrap();
        let weekly = reset_time("weekly limit · resets Oct 3, 5pm", Some(hit)).unwrap();
        assert_eq!(
            weekly,
            Local.with_ymd_and_hms(2026, 10, 3, 17, 0, 0).unwrap()
        );
        let later_today = reset_time("5-hour limit · resets 5pm", Some(hit)).unwrap();
        assert_eq!(
            later_today,
            Local.with_ymd_and_hms(2026, 9, 29, 17, 0, 0).unwrap()
        );
        let tomorrow = reset_time("limit · resets 11:30am", Some(hit)).unwrap();
        assert_eq!((tomorrow.hour(), tomorrow.minute()), (11, 30));
        assert!(tomorrow > hit);
    }

    fn limit_line(text: &str, at: &str) -> String {
        serde_json::json!({"type": "assistant", "error": "rate_limit", "timestamp": at,
            "message": {"content": [{"type": "text", "text": text}]}})
        .to_string()
    }

    fn reply(error: Option<&str>) -> String {
        let mut v = serde_json::json!({"type": "assistant", "timestamp": "2026-10-01T10:00:00Z",
            "message": {"content": [{"type": "text", "text": "done"}]}});
        if let Some(e) = error {
            v["error"] = e.into();
        }
        v.to_string()
    }

    fn prompt(text: &str) -> String {
        serde_json::json!({"type": "user", "message": {"content": text}}).to_string()
    }

    fn tool_result() -> String {
        serde_json::json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "ok"}]}})
            .to_string()
    }

    #[test]
    fn a_limit_holds_until_a_real_reply_and_other_errors_dont_clear_it() {
        let mut m = Meta::default();
        m.absorb(&limit_line(
            "You've hit your limit · resets 8pm (Europe/London)",
            "2026-10-01T09:00:00Z",
        ));
        assert_eq!(m.limit_hit().unwrap().text, "limit · resets 8pm");
        m.absorb(&reply(Some("overloaded")));
        assert!(m.limit_hit().is_some(), "an unrelated error keeps it");
        m.absorb(&reply(None));
        assert!(m.limit_hit().is_none(), "a real reply clears it");
    }

    #[test]
    fn a_limit_with_no_reset_lifts_after_hours_not_days() {
        let mut m = Meta::default();
        m.absorb(&limit_line(
            "You've reached your Fable limit",
            "2026-10-01T09:00:00Z",
        ));
        let hit = m.limit_hit().unwrap().clone();
        assert_eq!(hit.reset(), None);
        let at = hit.at.unwrap();
        assert!(hit.active_at(at + chrono::Duration::hours(1)));
        assert!(!hit.active_at(at + super::UNTIMED_LIMIT));
    }

    #[test]
    fn a_turn_cut_off_mid_work_is_told_from_a_prompt_the_limit_refused() {
        let at = "2026-10-01T09:00:00Z";
        let lim = limit_line("limit · resets 8pm", at);
        let run = |lines: &[String]| super::cut_off_in(lines.iter().map(String::as_str));
        // Worked, then the limit: cut off.
        assert_eq!(
            run(&[prompt("fix it"), reply(None), tool_result(), lim.clone()]),
            Some(true)
        );
        // The limit was the prompt's only answer.
        assert_eq!(run(&[prompt("hi"), lim.clone()]), Some(false));
        // Poking a cut-off session while limited keeps it cut off.
        assert_eq!(
            run(&[
                prompt("fix it"),
                tool_result(),
                lim.clone(),
                prompt("still there?"),
                lim.clone()
            ]),
            Some(true)
        );
        // A real reply since: not at a limit.
        assert_eq!(run(&[prompt("fix it"), lim, reply(None)]), None);
    }
}
