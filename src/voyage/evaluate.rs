//! Turn evidence, judge/reviewer calls and proof checks for a voyage.

use super::{CHECK_TIMEOUT, JUDGE_ENV, JUDGE_TIMEOUT, Verdict, Voyage, short};
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

/// A user entry that starts a turn: your prompt, or a Stop hook sending the
/// session back (not a tool result).
fn starts_turn(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match v.pointer("/message/content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(a)) => a
            .iter()
            .all(|b| b.get("type").and_then(Value::as_str) != Some("tool_result")),
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
sentence. Write reasons plainly, to the agent.";

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

/// A small model's verdict on the turn, with its estimate of how much is
/// done and what it cost.
pub(super) fn judge(
    cfg: &Config,
    q: &Voyage,
    evidence: &str,
) -> Result<(Verdict, String, Option<u8>, f64)> {
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
    ask.push_str(&format!("\nThe end of the conversation:\n{evidence}\n"));
    let (text, usd) = ask_model(
        cfg,
        q,
        q.persistence.model(cfg),
        &format!("{JUDGE}{}", rules.judge_rules),
        &ask,
    )?;
    let (verdict, reason) = parse_verdict(&text)?;
    Ok((verdict, reason, parse_progress(&text), usd))
}

const REVIEW: &str = "You are a sceptical reviewer. A coding agent says it has reached an outcome, and a judge agreed. Your \
job is to find any reason it isn't done. You see the outcome, the end of the conversation, and every change in the folder \
since the work began. Look for parts of the outcome not addressed; tests skipped, disabled, weakened or deleted; stubs, \
TODOs and placeholders; hard-coded or special-cased results; errors or warnings passed over; and claims the output doesn't \
back. Reply with one line of JSON and nothing else: {\"verdict\": \"met\" | \"not_yet\", \"reason\": \"...\"}. met only if \
you find nothing that matters; otherwise not_yet, and the reason says plainly, to the agent, what to fix.";

/// Relentless: a second model reads every change since the voyage began,
/// looking for why it isn't done. (passed, reason, cost)
pub(super) fn review(cfg: &Config, q: &Voyage, evidence: &str) -> Result<(bool, String, f64)> {
    let mut ask = format!(
        "The outcome:\n{}\n\nThe end of the conversation:\n{evidence}\n\n",
        q.outcome
    );
    ask.push_str(&changes(&q.cwd, q.base.as_deref()));
    let (text, usd) = ask_model(cfg, q, q.persistence.model(cfg), REVIEW, &ask)?;
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

/// One `claude -p` answer from a model with no tools: its text and cost.
fn ask_model(
    cfg: &Config,
    q: &Voyage,
    model: &str,
    system: &str,
    ask: &str,
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
    let (out, _) = run_with_timeout(cmd, Some(ask.as_bytes()), JUDGE_TIMEOUT)?;
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

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, x) if x != 11 => "st",
        (2, x) if x != 12 => "nd",
        (3, x) if x != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Run a command to its end or the timeout: stdout, and whether it exited 0.
fn run_with_timeout(
    mut cmd: std::process::Command,
    input: Option<&[u8]>,
    limit: Duration,
) -> Result<(String, bool)> {
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
            return Ok((out, status.success()));
        }
        if start.elapsed() > limit {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            bail!("it took longer than {} seconds", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The voyage's own check: Ok when it exits 0, else the end of its output.
pub(super) fn run_check(check: &str, cwd: &str) -> std::result::Result<(), String> {
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-c", &format!("{check} 2>&1")])
        .current_dir(cwd)
        .env(JUDGE_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    match run_with_timeout(cmd, None, CHECK_TIMEOUT) {
        Ok((_, true)) => Ok(()),
        Ok((out, false)) => {
            let tail: Vec<&str> = out
                .lines()
                .rev()
                .filter(|l| !l.trim().is_empty())
                .take(12)
                .collect();
            Err(clip(
                &tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
                0,
                1200,
            ))
        }
        Err(e) => Err(e.to_string()),
    }
}
