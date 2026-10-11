//! Turn evidence, judge/reviewer calls and proof checks for a voyage.

use super::{JUDGE_ENV, Verdict, Voyage, short};
use crate::config::Config;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

// ---- reading the turn --------------------------------------------------------

pub(super) struct TurnInfo {
    /// Index (among the tail's entries) where the turn began.
    pub start: usize,
    pub tools: usize,
}

/// The transcript's tail as entries, oldest first.
fn tail_entries(transcript: &Path) -> Vec<Value> {
    const TAIL: u64 = 1024 * 1024;
    let Ok(mut f) = std::fs::File::open(transcript) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(TAIL);
    let mut bytes = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .skip(usize::from(from > 0))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            matches!(
                v.get("type").and_then(Value::as_str),
                Some("user" | "assistant")
            )
        })
        .filter(|v| v.get("isSidechain").and_then(Value::as_bool) != Some(true))
        .collect()
}

/// Text Claude Code adds to the conversation mid-turn, as a user entry, that
/// nobody sent: reminders and the like.
fn mid_turn_text(t: &str) -> bool {
    let t = t.trim_start();
    t.is_empty() || t.starts_with("<system-reminder>") || t.starts_with("Caveat:")
}

/// A user entry that starts a turn: your prompt, a background task's report,
/// or a Stop hook sending the session back. Not a tool result, and not what
/// Claude Code slips in mid-turn as a user entry: a skill's body or other
/// `isMeta` entry, a system reminder.
fn starts_turn(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let meta = v.get("isMeta").and_then(Value::as_bool) == Some(true);
    match v.pointer("/message/content") {
        // The Stop hook's feedback is isMeta too, and does start a turn.
        Some(Value::String(s)) if meta => s.starts_with("Stop hook feedback"),
        Some(Value::String(s)) => !mid_turn_text(s),
        Some(Value::Array(_)) if meta => false,
        Some(Value::Array(a)) => {
            a.iter()
                .all(|b| b.get("type").and_then(Value::as_str) != Some("tool_result"))
                && a.iter()
                    .any(|b| match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            !mid_turn_text(b.get("text").and_then(Value::as_str).unwrap_or(""))
                        }
                        Some("image" | "document") => true,
                        _ => false,
                    })
        }
        _ => false,
    }
}

pub(super) fn last_turn(transcript: &Path) -> TurnInfo {
    let e = tail_entries(transcript);
    let start = e.iter().rposition(starts_turn).unwrap_or(0);
    let tools = e[start..]
        .iter()
        .filter_map(|v| v.pointer("/message/content").and_then(Value::as_array))
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .count();
    TurnInfo { start, tools }
}

fn clip(s: &str, head: usize, tail: usize) -> String {
    let n = s.chars().count();
    if n <= head + tail + 20 {
        return s.to_string();
    }
    let a: String = s.chars().take(head).collect();
    let b: String = s.chars().skip(n - tail).collect();
    format!("{a} … [{} chars] … {b}", n - head - tail)
}

fn result_text(b: &Value) -> String {
    match b.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The conversation's end as the judge reads it: this turn, and the one
/// before when there's room, newest kept when it must be cut.
pub(super) fn evidence(transcript: &Path, turn_start: usize, max: usize) -> String {
    let e = tail_entries(transcript);
    let from = e[..turn_start.min(e.len())]
        .iter()
        .rposition(starts_turn)
        .unwrap_or(turn_start.min(e.len()));
    let mut lines: Vec<String> = Vec::new();
    for v in &e[from..] {
        let user = v.get("type").and_then(Value::as_str) == Some("user");
        match v.pointer("/message/content") {
            Some(Value::String(s)) => {
                let who = if s.starts_with("Stop hook feedback") {
                    "toomux"
                } else {
                    "user"
                };
                lines.push(format!("[{who}] {}", clip(s.trim(), 1200, 300)));
            }
            Some(Value::Array(a)) => {
                for b in a {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = b.get("text").and_then(Value::as_str).unwrap_or("").trim();
                            if !t.is_empty() {
                                lines.push(format!(
                                    "[{}] {}",
                                    if user { "user" } else { "claude" },
                                    clip(t, 2500, 800)
                                ));
                            }
                        }
                        Some("tool_use") => {
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let input = b.get("input").cloned().unwrap_or(Value::Null);
                            let what = input
                                .get("command")
                                .or_else(|| input.get("file_path"))
                                .or_else(|| input.get("prompt"))
                                .or_else(|| input.get("pattern"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| input.to_string());
                            lines.push(format!("[tool {name}] {}", clip(&what, 300, 100)));
                        }
                        Some("tool_result") => {
                            let err = b.get("is_error").and_then(Value::as_bool) == Some(true);
                            lines.push(format!(
                                "[result{}] {}",
                                if err { ", error" } else { "" },
                                clip(result_text(b).trim(), 400, 600)
                            ));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // Newest first into the budget, then back in order.
    let mut kept = Vec::new();
    let mut used = 0;
    for l in lines.iter().rev() {
        if used + l.len() > max && !kept.is_empty() {
            kept.push("[… earlier in the conversation, cut]".to_string());
            break;
        }
        used += l.len() + 1;
        kept.push(l.clone());
    }
    kept.reverse();
    kept.join("\n")
}

// ---- the judge ---------------------------------------------------------------

const JUDGE: &str = "You check whether a coding agent has reached an outcome it was given. You see the outcome and the end of \
its conversation: its messages, the tools it called, and their results. Judge from evidence shown, not from claims alone: \
\"tests pass\" with no output showing it is not enough, and neither is work that was planned but not done. Reply with one line \
of JSON and nothing else: {\"verdict\": \"met\" | \"not_yet\" | \"impossible\" | \"needs_you\", \"reason\": \"...\", \
\"progress\": 0-100}. progress is your rough estimate of how much of the outcome is done, from the evidence. met: the \
evidence shows the outcome is reached. not_yet: it isn't yet; the reason says in a sentence or two what is missing or what to \
do next. impossible: it can't be reached at all (it contradicts itself, or needs something the agent can never have). \
needs_you: the agent has stopped at a decision, access or information only the user can give; the reason says what, in a \
sentence. Write reasons plainly, to the agent. Two more fields: \"milestones\": a list of short phrases, each a part of the \
outcome this evidence shows done (only what the evidence itself shows; empty if nothing new). \"other_work\": for needs_you, \
true if there is still work toward the outcome the agent could do without the user's answer, false if it is blocked entirely \
on the user. You may also be told what earlier checks confirmed and what earlier conversations' handover briefs say: the \
conversation you see is only the latest part of a longer one, so don't call something missing just because it was done \
before this part; judge what's left.";

pub(super) fn parse_verdict(text: &str) -> Result<(Verdict, String)> {
    let (a, b) = (text.find('{'), text.rfind('}'));
    let (Some(a), Some(b)) = (a, b) else {
        bail!("the judge didn't answer in JSON")
    };
    let v: Value = serde_json::from_str(&text[a..=b]).context("the judge's JSON didn't parse")?;
    let reason = v
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let verdict = match v
        .get("verdict")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase()
        .replace([' ', '-'], "_")
        .as_str()
    {
        "met" | "done" => Verdict::Met,
        "not_yet" | "not_met" => Verdict::NotYet,
        "impossible" => Verdict::Impossible,
        "needs_you" | "needs_user" => Verdict::NeedsYou,
        other => bail!("the judge said {other:?}"),
    };
    Ok((
        verdict,
        if reason.is_empty() {
            "no reason given".into()
        } else {
            reason
        },
    ))
}

/// The judge's estimate of how much is done, in percent, if it gave one.
pub(super) fn parse_progress(text: &str) -> Option<u8> {
    let (a, b) = (text.find('{')?, text.rfind('}')?);
    let v: Value = serde_json::from_str(text.get(a..=b)?).ok()?;
    let p = v.get("progress")?;
    let n = p
        .as_f64()
        .or_else(|| p.as_str()?.trim().trim_end_matches('%').parse().ok())?;
    Some(n.clamp(0.0, 100.0).round() as u8)
}

/// The extra fields of the judge's answer: the milestones it saw shown done,
/// and whether there's work that doesn't wait on the user.
pub(super) fn parse_extras(text: &str) -> (Vec<String>, bool) {
    let v: Option<Value> = text
        .find('{')
        .zip(text.rfind('}'))
        .and_then(|(a, b)| text.get(a..=b))
        .and_then(|j| serde_json::from_str(j).ok());
    let Some(v) = v else {
        return (Vec::new(), false);
    };
    let milestones = v
        .get("milestones")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .take(10)
                .collect()
        })
        .unwrap_or_default();
    let other = v.get("other_work").is_some_and(|o| {
        o.as_bool()
            .unwrap_or_else(|| o.as_str().is_some_and(|s| s.eq_ignore_ascii_case("true")))
    });
    (milestones, other)
}

/// What the judge said of a turn.
pub(super) struct Said {
    pub verdict: Verdict,
    pub reason: String,
    pub progress: Option<u8>,
    pub usd: f64,
    pub milestones: Vec<String>,
    pub other_work: bool,
}

/// A small model's verdict on the turn, with its estimate of how much is
/// done, what it saw done, and what it cost. `verified` is what earlier
/// checks and conversations settled, for the judge to build on.
pub(super) fn judge(
    cfg: &Config,
    q: &Voyage,
    verified: &str,
    evidence: &str,
    limit: Duration,
) -> Result<Said> {
    let mut ask = format!("The outcome:\n{}\n", q.outcome);
    if let Some(c) = &q.check {
        ask.push_str(&format!(
            "(Once you say met, toomux also runs `{c}`, which must pass.)\n"
        ));
    }
    ask.push_str(&format!("\nThis is turn {} of the voyage", q.turns));
    if q.handovers() > 0 {
        ask.push_str(&format!(
            ", in its {} conversation (the earlier ones handed their work on)",
            ordinal(q.sessions.len())
        ));
    }
    ask.push_str(".\n");
    if let Some(l) = &q.last {
        ask.push_str(&format!("Your last verdict's reason: {l}\n"));
    }
    let rules = q.persistence.rules();
    if q.laps > 0 {
        ask.push_str(&format!(
            "You said met last time, so the agent was sent on proof lap {} of {}. Say met again only if this turn did the lap \
             (fresh output shown, not the earlier run) and it holds up; anything it found and didn't fix is not_yet.\n",
            q.laps, rules.proof_laps
        ));
    }
    if !verified.is_empty() {
        ask.push_str(&format!("\n{verified}\n"));
    }
    ask.push_str(&format!("\nThe end of the conversation:\n{evidence}\n"));
    let (text, usd) = ask_model(
        cfg,
        q,
        q.persistence.model(cfg),
        &format!("{JUDGE}{}", rules.judge_rules),
        &ask,
        limit,
    )?;
    let (verdict, reason) = parse_verdict(&text)?;
    let (milestones, other_work) = parse_extras(&text);
    Ok(Said {
        verdict,
        reason,
        progress: parse_progress(&text),
        usd,
        milestones,
        other_work,
    })
}

const REVIEW: &str = "You are a sceptical reviewer. A coding agent says it has reached an outcome, and a judge agreed. Your \
job is to find any reason it isn't done. You see the outcome, the end of the conversation, and every change in the folder \
since the work began. Look for parts of the outcome not addressed; tests skipped, disabled, weakened or deleted; stubs, \
TODOs and placeholders; hard-coded or special-cased results; errors or warnings passed over; and claims the output doesn't \
back. Reply with one line of JSON and nothing else: {\"verdict\": \"met\" | \"not_yet\", \"reason\": \"...\"}. met only if \
you find nothing that matters; otherwise not_yet, and the reason says plainly, to the agent, what to fix.";

/// Relentless: a second model reads every change since the voyage began,
/// looking for why it isn't done. (passed, reason, cost)
pub(super) fn review(
    cfg: &Config,
    q: &Voyage,
    evidence: &str,
    limit: Duration,
) -> Result<(bool, String, f64)> {
    let mut ask = format!(
        "The outcome:\n{}\n\nThe end of the conversation:\n{evidence}\n\n",
        q.outcome
    );
    ask.push_str(&changes(&q.cwd, q.base.as_deref()));
    let (text, usd) = ask_model(cfg, q, q.persistence.model(cfg), REVIEW, &ask, limit)?;
    let (verdict, reason) = parse_verdict(&text)?;
    Ok((verdict == Verdict::Met, reason, usd))
}

/// The commit a folder is at, if it's a git repository.
pub(super) fn head_commit(cwd: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "HEAD"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Every change in the folder since `base`, committed or not, for the review.
fn changes(cwd: &str, base: Option<&str>) -> String {
    const MAX: usize = 40_000;
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(["-C", cwd])
            .args(args)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let Some(status) = git(&["status", "--short"]) else {
        return "Changes: none to show (the folder isn't a git repository). Judge from the conversation.\n".into();
    };
    let diff = git(&["diff", base.unwrap_or("HEAD")]).unwrap_or_default();
    let diff = if diff.chars().count() > MAX {
        format!(
            "{}\n[… the rest of the diff, cut]",
            diff.chars().take(MAX).collect::<String>()
        )
    } else {
        diff
    };
    format!(
        "Files changed or new (git status):\n{status}\nThe diff since the voyage began:\n{diff}\n"
    )
}

/// One `claude -p` answer from a model with no tools, within `limit`: its
/// text and cost.
fn ask_model(
    cfg: &Config,
    q: &Voyage,
    model: &str,
    system: &str,
    ask: &str,
    limit: Duration,
) -> Result<(String, f64)> {
    let mut cmd = std::process::Command::new(crate::config::expand(&cfg.claude_bin));
    cmd.args([
        "-p",
        "--model",
        model,
        "--tools",
        "",
        "--setting-sources",
        "",
        "--strict-mcp-config",
    ])
    .args([
        "--no-session-persistence",
        "--output-format",
        "json",
        "--system-prompt",
        system,
    ])
    .current_dir(if Path::new(&q.cwd).is_dir() {
        q.cwd.as_str()
    } else {
        "/"
    })
    .env(JUDGE_ENV, "1")
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null());
    let Some((out, _)) = run_with_timeout(cmd, Some(ask.as_bytes()), limit)? else {
        bail!("it took longer than {} seconds", limit.as_secs());
    };
    let v: Value = serde_json::from_str(out.trim()).context("the judge's run gave no result")?;
    if v.get("is_error").and_then(Value::as_bool) == Some(true) {
        bail!(
            "{}",
            short(
                v.get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("the judge's run failed"),
                120
            )
        );
    }
    let usd = v
        .get("total_cost_usd")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    Ok((
        v.get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        usd,
    ))
}

/// Whether the judge's run was refused for a usage limit ("You've hit your
/// limit · resets 5pm"), not a failure of the check itself.
pub(super) fn limit_error(e: &str) -> bool {
    let e = e.to_lowercase();
    e.contains("limit")
        && (e.contains("hit your")
            || e.contains("resets")
            || e.contains("usage limit")
            || e.contains("rate limit")
            || e.contains("rate_limit"))
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, x) if x != 11 => "st",
        (2, x) if x != 12 => "nd",
        (3, x) if x != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Run a command to its end or the limit: stdout, and whether it exited 0.
/// None when it ran out of time (it's killed, its process group with it).
fn run_with_timeout(
    mut cmd: std::process::Command,
    input: Option<&[u8]>,
    limit: Duration,
) -> Result<Option<(String, bool)>> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let mut child = cmd.spawn().context("couldn't start it")?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = stdout.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let out = reader.join().unwrap_or_default();
            return Ok(Some((out, status.success())));
        }
        if start.elapsed() > limit {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Why a voyage's check didn't pass.
#[derive(Debug)]
pub(super) enum CheckFail {
    /// It exited non-zero (the end of its output), or couldn't start.
    Failed(String),
    /// It was still running when its time ran out.
    TimedOut(Duration),
}

impl std::fmt::Display for CheckFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckFail::Failed(s) => f.write_str(s),
            CheckFail::TimedOut(d) => write!(f, "it took longer than {} seconds", d.as_secs()),
        }
    }
}

/// The voyage's own check, within `limit`: Ok when it exits 0.
pub(super) fn run_check(check: &str, cwd: &str, limit: Duration) -> Result<(), CheckFail> {
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-c", &format!("{check} 2>&1")])
        .current_dir(cwd)
        .env(JUDGE_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    match run_with_timeout(cmd, None, limit) {
        Ok(Some((_, true))) => Ok(()),
        Ok(Some((out, false))) => {
            let tail: Vec<&str> = out
                .lines()
                .rev()
                .filter(|l| !l.trim().is_empty())
                .take(12)
                .collect();
            Err(CheckFail::Failed(clip(
                &tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
                0,
                1200,
            )))
        }
        Ok(None) => Err(CheckFail::TimedOut(limit)),
        Err(e) => Err(CheckFail::Failed(e.to_string())),
    }
}

// ---- background work -----------------------------------------------------------

/// How long a background task with no report back still counts as running:
/// past this, it's taken as a server left up or a report that was lost.
const BASH_WAIT_MS: i64 = 2 * 3_600_000;
const AGENT_WAIT_MS: i64 = 6 * 3_600_000;

/// Background tasks the session started and that haven't reported back:
/// background commands (`backgroundTaskId`), background agents (`agentId`
/// of an async launch) and monitors (`taskId`, until they expire), less
/// those a `<task-notification>` reported finished or a stop tool stopped.
pub(super) fn background_tasks(transcript: &Path, now_ms: i64) -> usize {
    const TAIL: u64 = 1024 * 1024;
    let Ok(mut f) = std::fs::File::open(transcript) else {
        return 0;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(TAIL);
    let mut bytes = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.read_to_end(&mut bytes).is_err() {
        return 0;
    }
    let text = String::from_utf8_lossy(&bytes);
    // Task id -> until when it counts as running.
    let mut running: std::collections::HashMap<String, i64> = Default::default();
    let mut done: std::collections::HashSet<String> = Default::default();
    for l in text.lines().skip(usize::from(from > 0)) {
        if l.contains("<task-notification>") {
            // One report per notification; a monitor's events say running.
            for n in l.split("<task-notification>").skip(1) {
                let field = |k: &str| {
                    n.split(&format!("<{k}>"))
                        .nth(1)
                        .and_then(|r| r.split(&format!("</{k}>")).next())
                        .map(str::trim)
                };
                if let Some(id) = field("task-id")
                    && field("status").is_some_and(|s| s != "running")
                {
                    done.insert(id.to_string());
                }
            }
            continue;
        }
        let launch = ["backgroundTaskId", "async_launched", "\"taskId\""]
            .iter()
            .any(|k| l.contains(k));
        let stop = ["\"TaskStop\"", "\"KillShell\"", "\"KillBash\""]
            .iter()
            .any(|k| l.contains(k));
        if !launch && !stop {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map_or(now_ms, |t| t.timestamp_millis());
        if let Some(r) = v.get("toolUseResult") {
            let s = |k: &str| r.get(k).and_then(Value::as_str).map(str::to_string);
            if let Some(id) = s("backgroundTaskId") {
                running.insert(id, at + BASH_WAIT_MS);
            } else if let Some(id) = s("agentId").filter(|_| {
                r.get("isAsync").and_then(Value::as_bool) == Some(true)
                    || s("status").as_deref() == Some("async_launched")
            }) {
                running.insert(id, at + AGENT_WAIT_MS);
            } else if let Some(id) = s("taskId") {
                let persistent = r.get("persistent").and_then(Value::as_bool) == Some(true);
                let until = match r.get("timeoutMs").and_then(Value::as_i64) {
                    Some(ms) if !persistent => at + ms + 60_000,
                    _ => at + AGENT_WAIT_MS,
                };
                running.insert(id, until);
            }
        }
        for b in v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if b.get("type").and_then(Value::as_str) == Some("tool_use")
                && matches!(
                    b.get("name").and_then(Value::as_str),
                    Some("TaskStop" | "KillShell" | "KillBash")
                )
            {
                let input = b.get("input").cloned().unwrap_or(Value::Null);
                for k in ["task_id", "shell_id", "bash_id", "id"] {
                    if let Some(id) = input.get(k).and_then(Value::as_str) {
                        done.insert(id.to_string());
                    }
                }
            }
        }
    }
    running
        .into_iter()
        .filter(|(id, until)| !done.contains(id) && now_ms < *until)
        .count()
}
