//! Handover: an agent whose context has grown past its resolved context policy
//! writes a complete brief and continues as a fresh one that starts from it.
//! Main sessions and subagents follow the same rule.
//!
//! Why: every call re-reads the whole context, so long contexts are where the
//! tokens go. Measured on 72k real calls, 78% of cache-weighted input came from
//! calls above 400k tokens; handing over there cuts it by about 59%.
//!
//! Nothing is thrown away: briefs and every exchange of the old conversation
//! are indexed, so the successor can search for anything the brief left out.
//!
//! How it happens:
//! - The PreToolUse hook measures whichever agent is calling a tool. Past the
//!   threshold it refuses the call and tells that agent to write its brief;
//!   only writing the brief (and, for a main session, waiting on its
//!   subagents) is allowed from then on.
//! - A subagent then ends with a HANDOVER message telling its parent to launch
//!   a fresh subagent of the same type from the brief.
//! - A main session ends its turn; `toomux status` sees the brief and restarts
//!   the pane as a fresh session (same account and flags) that opens with it.
//! - A main session that is simply idle past the threshold is asked to write
//!   its brief by toomux, the same way.
//! - Before that, past the lower turn-end limit (250k), a conversation hands
//!   over at a natural break: when you send your next prompt (the
//!   UserPromptSubmit hook asks it to write its brief with your request as
//!   the first next step, and the fresh session does it), or once idle ten
//!   minutes. Subagents have no turns: theirs is a plain limit (250k), or
//!   half that past what they were born with (a fork starts with its
//!   parent's context), and a fork's successor is a fresh general-purpose one.
//! - While a main session hands over, every one of its subagents is asked to
//!   hand over too; toomux waits for them and lists their briefs for the
//!   successor. Background commands started through toomux keep running and
//!   the successor re-attaches to them.

use crate::actions;
use crate::config::Config;
use crate::context_policy::{self, ResolvedContextPolicy};
use crate::registry::{self, Session, State};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Idle this long before toomux asks an idle session to hand over.
const QUIET_MS: i64 = 45_000;
/// Past only the turn-end limit, idle this long: you've read the answer,
/// and the cache (an hour) is still warm for writing the brief.
const TURN_END_QUIET_MS: i64 = 10 * 60_000;
/// A failed handover isn't retried for this long...
const RETRY_MS: i64 = 30 * 60_000;
/// ...unless it only waited on someone typing in the prompt.
const RETRY_TYPED_MS: i64 = 60_000;
const TYPED: &str = "something is typed in its prompt";
const WRITE_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const SUBAGENT_TIMEOUT: Duration = Duration::from_secs(12 * 60);

pub fn dir() -> PathBuf {
    crate::paths::state().join("handovers")
}

/// Where an agent's brief goes.
pub fn brief_path(session: &str, agent: Option<&str>) -> PathBuf {
    match agent {
        Some(a) => dir().join(format!("{session}-agent-{a}.md")),
        None => dir().join(format!("{session}.md")),
    }
}

fn written(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.len() > 200)
}

/// A handover has been asked of this agent (by the hook or by toomux).
fn requested_path(session: &str, agent: Option<&str>) -> PathBuf {
    match agent {
        Some(a) => dir().join(format!("req-{session}-{a}")),
        None => dir().join(format!("req-{session}")),
    }
}

/// Every subagent of this session must hand over at its next tool call.
fn drain_path(session: &str) -> PathBuf {
    dir().join(format!("drain-{session}"))
}

fn touch(p: &Path, what: &str) {
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(p, what);
}

// ---- measuring an agent's context -------------------------------------------

/// Tokens in an agent's context at its latest call: its transcript's last
/// recorded usage.
pub fn context_tokens(transcript: &Path) -> Option<u64> {
    let mut f = std::fs::File::open(transcript).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(512 * 1024);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    f.take(len - from).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        // Compacted since its last call: the context is what the compaction left.
        if line.contains("\"compact_boundary\"") {
            let v = serde_json::from_str::<Value>(line).ok();
            if let Some(post) = v
                .as_ref()
                .filter(|v| v.get("subtype").and_then(Value::as_str) == Some("compact_boundary"))
                .and_then(|v| v.pointer("/compactMetadata/postTokens"))
                .and_then(Value::as_u64)
            {
                return Some(post);
            }
            continue;
        }
        if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(u) = v.pointer("/message/usage") else {
            continue;
        };
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let total =
            n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
        if total > 0 {
            return Some(total);
        }
    }
    None
}

/// Tokens in an agent's context at its first call.
fn first_context(transcript: &Path) -> Option<u64> {
    let f = std::fs::File::open(transcript).ok()?;
    let mut head = Vec::new();
    f.take(512 * 1024).read_to_end(&mut head).ok()?;
    String::from_utf8_lossy(&head).lines().find_map(|line| {
        if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
            return None;
        }
        let u = serde_json::from_str::<Value>(line)
            .ok()?
            .pointer("/message/usage")
            .cloned()?;
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        Some(n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"))
            .filter(|t| *t > 0)
    })
}

/// A conversation that began near the limit: handing it over would only
/// start another one just as big, and again (a limit set below what a fresh
/// session starts with).
fn born_big(limit: u64, transcript: &Path) -> bool {
    first_context(transcript).is_some_and(|t| t >= limit / 4 * 3)
}

/// A subagent's limit, counted from what it was born with: a fork starts
/// with its parent's whole context, and handing it over at once would only
/// start another just as big. Every subagent gets half the limit for its own
/// work before it goes.
fn own_limit(limit: u64, transcript: &Path) -> u64 {
    first_context(transcript).map_or(limit, |born| limit.max(born + limit / 2))
}

/// The context it had when it was asked to hand over.
fn asked_at(session: &str, agent: Option<&str>) -> Option<u64> {
    std::fs::read_to_string(requested_path(session, agent))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Asked, not yet written, and its context has shrunk well below where it
/// was asked (a /compact): no need any more.
fn shrank(session: &str, agent: Option<&str>, tokens: u64, limit: u64) -> bool {
    tokens < asked_at(session, agent).unwrap_or(limit) / 10 * 9
}

/// A subagent's transcript, beside its session's.
fn subagent_transcript(main: &Path, session: &str, agent: &str) -> PathBuf {
    main.with_file_name(session)
        .join("subagents")
        .join(format!("agent-{agent}.jsonl"))
}

// ---- the gate (PreToolUse) ------------------------------------------------------

/// Called by the PreToolUse hook for every tool call. Returns the hook's
/// answer when the call must wait for a handover.
pub fn gate(cfg: &Config, v: &Value) -> Option<String> {
    let policy = context_policy::for_hook(cfg, v);
    if let Some(no) = fork_steer(v, &policy) {
        return Some(no);
    }
    let session = v.get("session_id").and_then(Value::as_str)?;
    let transcript = PathBuf::from(v.get("transcript_path").and_then(Value::as_str)?);
    let agent = v.get("agent_id").and_then(Value::as_str);
    let agent_type = v
        .get("agent_type")
        .and_then(Value::as_str)
        .unwrap_or("general-purpose");
    let tool = v.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let brief = brief_path(session, agent);
    let asked_here = requested_path(session, agent).exists();
    let asked = asked_here || (agent.is_some() && drain_path(session).exists());
    let policy_boundary = agent.is_none() && context_policy::needs_policy_boundary(&policy);
    if policy.handover_tokens == 0 && !policy_boundary {
        return None;
    }
    let own = match agent {
        Some(a) => subagent_transcript(&transcript, session, a),
        None => transcript.clone(),
    };
    let tokens = if asked {
        context_tokens(&own).unwrap_or(0)
    } else {
        context_tokens(&own)?
    };
    // Mid-turn a conversation goes only at the hard limit; a subagent has no
    // turns, so its own limit applies here.
    let limit = if agent.is_some() {
        own_limit(policy.subagent_limit(), &own)
    } else {
        policy.handover_tokens
    };
    if !asked && !policy_boundary && tokens < limit {
        return None;
    }
    if agent.is_none() {
        let release = || {
            let _ = std::fs::remove_file(requested_path(session, None));
            let _ = std::fs::remove_file(drain_path(session));
        };
        // This conversation already handed over, and someone reopened it on
        // purpose: it carries on as it is.
        if load_lineage().iter().any(|l| l.old_id == session) {
            release();
            return None;
        }
        // Only sessions toomux can restart are held: not `claude -p` or SDK runs.
        if !registry::is_interactive(cfg, session) {
            return None;
        }
        if !asked && !policy_boundary && born_big(limit, &transcript) {
            return None;
        }
        // Its context shrank (/compact) before it wrote a brief: no need any more.
        if asked_here
            && !written(&brief)
            && shrank(session, None, tokens, limit)
            && !in_progress_for(session)
        {
            release();
            return None;
        }
    }
    if !asked {
        touch(&requested_path(session, agent), &tokens.to_string());
        if agent.is_none() {
            // The main session is handing over: so do its subagents.
            touch(&drain_path(session), "");
        }
    }
    // Writing the brief is the one thing to do now.
    let target = v
        .pointer("/tool_input/file_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    if matches!(tool, "Write" | "Edit") && target.as_deref() == Some(brief.as_path()) {
        // Allowed outright: a permission prompt here would stall an unattended
        // session (the brief lives outside the project).
        return Some(
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "permissionDecisionReason": "toomux: writing the handover brief"}})
                .to_string(),
        );
    }
    // A main session brings its workspace memory up to date meanwhile.
    if agent.is_none() && in_memory_dir(tool, v, &transcript) {
        return None;
    }
    // A main session may wait on, and hear from, its subagents meanwhile.
    if agent.is_none() && (tool.contains("Output") || tool == "SendMessage" || tool == "ToolSearch")
    {
        return None;
    }
    let k = tokens / 1000;
    let turn_end = asked_at(session, None).is_some_and(|t| t < policy.handover_tokens);
    let reason = match (agent, written(&brief)) {
        (None, false) if policy_boundary => {
            policy_boundary_instruction(k, &policy, &brief, Some(&transcript))
        }
        (None, false) => main_instruction(
            k,
            if turn_end {
                policy.turn_end_limit()
            } else {
                limit
            } / 1000,
            &brief,
            turn_end,
            Some(&transcript),
        ),
        (None, true) => format!(
            "toomux: your handover brief is written. To add to it, use Edit or Write on {}; otherwise stop here and end your turn \
             with: handover written",
            brief.display()
        ),
        (Some(_), done) => {
            let own_limit = tokens >= limit;
            // A fork's successor starts fresh: another fork would carry the
            // parent's whole context again, and the brief has what it needs.
            let next = if agent_type == "fork" {
                "general-purpose"
            } else {
                agent_type
            };
            let why = if own_limit {
                format!(
                    "this {agent_type} subagent is at {k}k tokens of context (the limit is {}k)",
                    limit / 1000
                )
            } else {
                "the session this subagent belongs to is handing over to a fresh one".to_string()
            };
            let signoff = if own_limit {
                format!(
                    "HANDOVER: this {agent_type} subagent stopped at {k}k tokens. Its complete brief is at {path} (or, if it \
                     couldn't write files, below). To continue its work, launch a new {next} subagent with the prompt: \
                     \"Continue from the handover brief at {path}: read it first, then finish the task it describes.\" (If the \
                     file doesn't exist, pass the brief below in that prompt instead.)",
                    path = brief.display()
                )
            } else {
                format!(
                    "HANDOVER: this {agent_type} subagent stopped because its session is handing over. Its complete brief is at \
                     {path} (or, if it couldn't write files, below). Don't relaunch it now: toomux lists it for the next session, \
                     which continues it.",
                    path = brief.display()
                )
            };
            if done {
                format!(
                    "STOP. toomux: your brief is written. Call no more tools; your final message must be:\n\n{signoff}"
                )
            } else {
                format!(
                    "STOP. toomux: {why}, so it hands over to a fresh subagent instead of growing further. Do not call any more \
                     tools for the task. Write a complete, standalone brief: the task you were given (in full), what you have \
                     found or done so far with exact references (paths, commands, ids, URLs), what didn't work, and what \
                     remains. If you have a Write tool, write it to {path}; if you don't (a read-only agent), don't try, put it \
                     in your final message instead. Then stop, and make your final message:\n\n{signoff}\n\n<the brief, if \
                     it isn't in the file>",
                    path = brief.display()
                )
            }
        }
    };
    Some(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}}).to_string())
}

/// A fork asked for by a caller past `fork_context_tokens`: it would carry
/// the caller's whole context into every call it makes, so the caller briefs
/// a fresh general-purpose subagent instead. Measured on 09-30, forks born
/// past 240k cost 2.6 times what the same work cost starting fresh.
fn fork_steer(v: &Value, policy: &ResolvedContextPolicy) -> Option<String> {
    let tool = v.get("tool_name").and_then(Value::as_str)?;
    let kind = v
        .pointer("/tool_input/subagent_type")
        .and_then(Value::as_str)?;
    if !matches!(tool, "Agent" | "Task") || kind != "fork" || policy.fork_context_tokens == 0 {
        return None;
    }
    let session = v.get("session_id").and_then(Value::as_str)?;
    let transcript = PathBuf::from(v.get("transcript_path").and_then(Value::as_str)?);
    let own = match v.get("agent_id").and_then(Value::as_str) {
        Some(a) => subagent_transcript(&transcript, session, a),
        None => transcript,
    };
    let tokens = context_tokens(&own)?;
    if tokens < policy.fork_context_tokens {
        return None;
    }
    let reason = format!(
        "toomux: a fork would copy this conversation's {}k tokens of context and re-read them on every call it makes (forks \
         are allowed below {}k). Launch a general-purpose subagent instead, and make its prompt a complete brief: the task \
         in full, what you already know that it needs (exact paths, decisions, constraints, what didn't work), and what to \
         report back.",
        tokens / 1000,
        policy.fork_context_tokens / 1000
    );
    Some(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}}).to_string())
}

/// What a main session is told when it has to hand over. `turn_end`: asked
/// at a turn's end (past the turn-end limit, not the hard one), when a new
/// request may be waiting that the fresh session will do.
fn main_instruction(
    k: u64,
    limit: u64,
    brief: &Path,
    turn_end: bool,
    transcript: Option<&Path>,
) -> String {
    let (why, pending) = if turn_end {
        (
            format!(
                "this conversation is at {k}k tokens of context (past {limit}k, where it continues fresh at a turn's end), so \
                 any further work happens in a fresh session"
            ),
            " If the user has just asked for something: when it needs no tools (an answer from what is already in this \
             conversation), give that answer first, then write the brief; otherwise don't start it, and put the request first \
             among the next steps, in their words, so the fresh session does it.",
        )
    } else {
        (
            format!(
                "this conversation is at {k}k tokens of context (the limit is {limit}k), so it continues in a fresh session instead of growing further. Don't continue the task"
            ),
            "",
        )
    };
    format!(
        "STOP. toomux: {why}. Write a complete, standalone handover brief to {path} with the \
         Write tool: the next session will have nothing else, except that it can search this conversation. Begin it with one \
         heading that names the work as it stands now in a few words, \"# Handover: <the work>\" (it becomes the next \
         session's name; not \"Handover brief\"). Include the goal \
         and what the user wants (in their words where it matters); the current state and what is done; decisions made and \
         why; where recent additions came from (what prompted each) and why anything in progress is being done; what was tried and didn't work; exact references (file paths, commands, branches, commits, PRs, URLs, ids); \
         work in progress: what each running subagent and background command is for (they carry over: toomux lists their \
         briefs and job ids for the next session, so don't wait for them); open questions and anything the user is waiting \
         on; and the precise next steps.{pending} Be thorough rather than short.{memory} Then end your turn with: handover \
         written",
        path = brief.display(),
        memory = transcript.map(memory_ask).unwrap_or_default()
    )
}

/// A running client can change model/account policy without restarting its
/// process. If that changes who owns compaction or lowers the hard threshold,
/// the old launch settings are no longer safe. Cross a fresh-session boundary
/// before doing more work, regardless of current token count.
fn policy_boundary_instruction(
    k: u64,
    policy: &ResolvedContextPolicy,
    brief: &Path,
    transcript: Option<&Path>,
) -> String {
    let model = policy.model.as_deref().unwrap_or("unknown model");
    let owner = match policy.compaction_owner {
        context_policy::CompactionOwner::Native => "native client",
        context_policy::CompactionOwner::Toomux => "Toomux",
    };
    format!(
        "STOP. toomux: this conversation is at {k}k tokens, but its resolved context lifecycle changed while it was running \
         (model {model}, hard handover {}k, compaction owner {owner}). The current process was launched under a different \
         lifecycle policy, so further work must continue in a fresh session that applies the new policy. Write a complete, \
         standalone handover brief to {path} with the Write tool: the next session will have nothing else, except that it can \
         search this conversation. Begin it with one heading that names the work as it stands now in a few words, \
         \"# Handover: <the work>\" (it becomes the next session's name; not \"Handover brief\"). Include the goal and what \
         the user wants (in their words where it matters); the current state and what is done; decisions made and why; where \
         recent additions came from and why anything in progress is being done; what was tried and didn't work; exact \
         references (file paths, commands, branches, commits, PRs, URLs, ids); work in progress: what each running subagent \
         and background command is for (they carry over: toomux lists their briefs and job ids for the next session, so don't \
         wait for them); open questions and anything the user is waiting on; and the precise next steps. If the user has just \
         asked for something: when it needs no tools, answer it first and then write the brief; otherwise don't start it, and \
         put the request first among the next steps, in their words, so the fresh session does it. Be thorough rather than \
         short.{memory} Then end your turn with: handover written",
        policy.handover_tokens / 1000,
        path = brief.display(),
        memory = transcript.map(memory_ask).unwrap_or_default(),
    )
}

/// A MEMORY.md past this is slimmed at a handover (Claude Code loads at most
/// 200 lines, and every line of it is re-read on every call).
const INDEX_LINES: usize = 150;
const INDEX_BYTES: usize = 15_000;

/// The workspace memory folder beside a conversation's transcript.
fn memory_dir(transcript: &Path) -> Option<PathBuf> {
    Some(transcript.parent()?.join("memory"))
}

/// What a session handing over is asked to do for its workspace's memory:
/// it knows best what it learned, and its context is still cached.
fn memory_ask(transcript: &Path) -> String {
    let Some(dir) = memory_dir(transcript) else {
        return String::new();
    };
    let index = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap_or_default();
    let (lines, bytes) = (index.lines().count(), index.len());
    let over = if lines > INDEX_LINES || bytes > INDEX_BYTES {
        format!(
            " Its MEMORY.md is {lines} lines, {}KB, past the budget of {INDEX_LINES} lines and {}KB: move what isn't a standing \
             rule into topic files, word for word, leaving a one-line pointer, so nothing is lost.",
            bytes / 1000,
            INDEX_BYTES / 1000
        )
    } else {
        String::new()
    };
    let prefix = std::fs::canonicalize(&dir)
        .unwrap_or(dir.clone())
        .display()
        .to_string();
    let stale = stale_list(crate::upkeep::gone_files(&prefix));
    let stale = if stale.is_empty() {
        String::new()
    } else {
        format!(
            " These memory files cite paths that are gone: {stale}. Correct any you know the new place of; leave the rest."
        )
    };
    format!(
        " Before the brief, bring this workspace's memory up to date in {}: what this session learned that will matter beyond \
         this task (decisions and why, gotchas, where things are, and corrections to anything there that proved wrong) goes \
         into topic files, one subject per file with frontmatter (name, description, metadata.type), and MEMORY.md keeps only \
         standing rules and one line per topic file. Keep it to what lasts (the brief carries the task itself), and skip this \
         if nothing did.{over}{stale}",
        dir.display()
    )
}

/// The gone paths each memory file cites, named in full: the session can't
/// run a shell while handing over, so this is all it has to go on.
fn stale_list(files: Vec<(String, Vec<String>)>) -> String {
    const PATHS: usize = 20;
    let mut left = PATHS;
    let mut parts = Vec::new();
    let mut unnamed = 0;
    for (f, paths) in files {
        let name = Path::new(&f)
            .file_name()
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or(f);
        if left == 0 {
            unnamed += paths.len();
            continue;
        }
        let shown: Vec<&str> = paths.iter().take(left).map(String::as_str).collect();
        left -= shown.len();
        unnamed += paths.len() - shown.len();
        parts.push(format!("{name} cites {}", shown.join(", ")));
    }
    let mut out = parts.join("; ");
    if unnamed > 0 {
        out.push_str(&format!("; and {unnamed} more, for a later handover"));
    }
    out
}

/// A tool call that only touches this conversation's workspace memory.
fn in_memory_dir(tool: &str, v: &Value, transcript: &Path) -> bool {
    if !matches!(
        tool,
        "Read" | "Write" | "Edit" | "MultiEdit" | "Glob" | "Grep"
    ) {
        return false;
    }
    let Some(dir) = memory_dir(transcript) else {
        return false;
    };
    let target = v
        .pointer("/tool_input/file_path")
        .or_else(|| v.pointer("/tool_input/path"))
        .and_then(Value::as_str);
    let Some(target) = target.map(Path::new) else {
        return false;
    };
    if target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    let Some(dir) = canon(&dir) else { return false };
    let at = canon(target).or_else(|| Some(canon(target.parent()?)?.join(target.file_name()?)));
    at.is_some_and(|t| t.starts_with(&dir))
}

// ---- at a turn's end (UserPromptSubmit) ------------------------------------------

/// Past the turn-end limit and not yet asked: ask it now (the gate holds it
/// to the brief from here). Returns what to tell it.
fn ask_at_turn_end(cfg: &Config, v: &Value, how: &str) -> Option<String> {
    let policy = context_policy::for_hook(cfg, v);
    let soft = policy.turn_end_limit();
    let policy_boundary = context_policy::needs_policy_boundary(&policy);
    if !policy_boundary && (soft == 0 || soft >= policy.handover_tokens) {
        return None;
    }
    let session = v.get("session_id").and_then(Value::as_str)?;
    let transcript = PathBuf::from(v.get("transcript_path").and_then(Value::as_str)?);
    if requested(session) {
        return None;
    }
    let mut tokens = context_tokens(&transcript)?;
    // At a stop the turn's last call lands in the transcript a moment after
    // the hook starts: within reach of the limit, give it a second.
    let start = Instant::now();
    while !policy_boundary
        && how == "stop"
        && tokens < soft
        && tokens >= soft / 2
        && start.elapsed() < Duration::from_secs(1)
    {
        std::thread::sleep(Duration::from_millis(100));
        tokens = context_tokens(&transcript)?;
    }
    if (!policy_boundary && tokens < soft)
        || (!policy_boundary && born_big(soft, &transcript))
        || load_lineage().iter().any(|l| l.old_id == session)
        || !registry::is_interactive(cfg, session)
    {
        return None;
    }
    touch(&requested_path(session, None), &tokens.to_string());
    touch(&drain_path(session), "");
    log(
        json!({"session": session, "event": format!("asked at turn end ({how})"), "tokens": tokens}),
    );
    let brief = brief_path(session, None);
    Some(if policy_boundary {
        policy_boundary_instruction(tokens / 1000, &policy, &brief, Some(&transcript))
    } else {
        main_instruction(tokens / 1000, soft / 1000, &brief, true, Some(&transcript))
    })
}

/// `toomux hook prompt`: you've sent a prompt to a conversation past the
/// turn-end limit that wasn't asked when its last turn ended (it crossed the
/// limit before this was on, say). It writes its brief with your request as
/// the first next step, and the fresh session does it.
pub fn prompt_hook(cfg: &Config, v: &Value) -> Option<String> {
    let prompt = v.get("prompt").and_then(Value::as_str).unwrap_or("");
    // Commands, and toomux's own requests, aren't work to move.
    if prompt.trim_start().starts_with('/') || prompt.starts_with("[toomux") {
        return None;
    }
    let said = ask_at_turn_end(cfg, v, "prompt")?;
    Some(json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": said}}).to_string())
}

// ---- when to hand a main session over -------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Marker {
    pub pid: i32,
    pub id: String,
    pub pane: String,
    pub state: String,
    pub at_ms: i64,
    pub tokens: u64,
    /// A failed handover may be tried again from then (0: the default wait).
    #[serde(default)]
    pub retry_at_ms: i64,
    /// Why it failed.
    #[serde(default)]
    pub why: String,
    /// The process doing the handover (0 while it starts).
    #[serde(default)]
    pub owner: i32,
}

/// A handover is under way for this marker: its process is alive and it
/// hasn't run past every timeout (a killed handover must not hold a session
/// forever).
fn live(m: &Marker, now: i64) -> bool {
    let age = now - m.at_ms;
    m.state == "writing"
        && age < 40 * 60_000
        && if m.owner > 0 {
            registry::alive(m.owner, None)
        } else {
            age < 60_000
        }
}

/// Every marker about this conversation (they're per process, and a
/// conversation can move between processes).
fn markers_for(session: &str) -> Vec<Marker> {
    std::fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name().to_str().is_some_and(|n| {
                n.ends_with(".json") && n[..n.len() - 5].chars().all(|c| c.is_ascii_digit())
            })
        })
        .filter_map(|e| {
            std::fs::read_to_string(e.path())
                .ok()
                .and_then(|r| serde_json::from_str::<Marker>(&r).ok())
        })
        .filter(|m| m.id == session)
        .collect()
}

/// A handover of this conversation is under way.
fn in_progress_for(session: &str) -> bool {
    let now = registry::now_ms();
    markers_for(session).iter().any(|m| live(m, now))
}

fn marker_path(pid: i32) -> PathBuf {
    dir().join(format!("{pid}.json"))
}

pub fn marker(pid: i32) -> Option<Marker> {
    std::fs::read_to_string(marker_path(pid))
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
}

/// Where a session is in handing over, for the list.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// toomux is handing it over now.
    Running,
    /// The gate asked for its brief; it goes once that's written and it's idle.
    Asked { written: bool },
    /// The last try failed, and why.
    Failed(String),
}

pub fn phase(session: &str, pid: i32) -> Option<Phase> {
    let now = registry::now_ms();
    match marker(pid) {
        Some(m) if live(&m, now) => return Some(Phase::Running),
        Some(m)
            if m.state == "failed"
                && m.id == session
                && registry::now_ms() - m.at_ms < 6 * 3_600_000 =>
        {
            return Some(Phase::Failed(if m.why.is_empty() {
                "see handovers/log.jsonl".into()
            } else {
                m.why
            }));
        }
        _ => {}
    }
    requested(session).then(|| Phase::Asked {
        written: written(&brief_path(session, None)),
    })
}

/// A session asked by the gate to hand over (its brief may be on the way).
pub fn requested(session: &str) -> bool {
    requested_path(session, None).exists()
}

/// Clear out requests and markers no handover will look at again: three days
/// on, their sessions have handed over, ended or been asked anew.
pub fn prune() {
    let now = registry::now_ms();
    for e in std::fs::read_dir(dir()).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let marker = name
            .strip_suffix(".json")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        let stray = name.contains(".tmp");
        if !(name.starts_with("req-")
            || name.starts_with("drain-")
            || name.starts_with("tick-")
            || marker
            || stray)
        {
            continue;
        }
        let Some(at) = mtime_ms(&e.path()) else {
            continue;
        };
        let keep = if stray { 3_600_000 } else { 3 * 86_400_000 };
        let running = marker
            && std::fs::read_to_string(e.path())
                .ok()
                .and_then(|r| serde_json::from_str::<Marker>(&r).ok())
                .is_some_and(|m| live(&m, now));
        if now - at > keep && !running {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Written whole or not at all: a reader never sees half a marker.
fn write_atomic(p: &Path, text: &str) {
    let _ = std::fs::create_dir_all(dir());
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, p);
    }
}

fn write_marker(m: &Marker) {
    write_atomic(
        &marker_path(m.pid),
        &serde_json::to_string(m).unwrap_or_default(),
    );
}

/// Main sessions ready to hand over now: idle past the threshold, or asked by
/// the gate and done writing their brief.
pub fn due(cfg: &Config, sessions: &[Session], now: i64) -> Vec<i32> {
    let info = crate::usage::sessions();
    let lineage = load_lineage();
    sessions
        .iter()
        .filter(|s| !s.dormant && s.limit.is_none() && s.queued.is_none())
        // A conversation hands over once: another process still holding it
        // afterwards is an old copy, not a second one to continue.
        .filter(|s| !lineage.iter().any(|l| l.old_id == s.id))
        // One conversation open in two processes (an old copy left behind in
        // a pane, say): only the one in use hands over.
        .filter(|s| {
            !sessions
                .iter()
                .any(|o| o.pid != s.pid && o.id == s.id && !o.dormant && o.since_ms > s.since_ms)
        })
        // Background work no longer holds a handover back: it carries over.
        .filter(|s| matches!(s.state, State::Idle | State::Finished | State::Background))
        .filter(|s| {
            let policy = context_policy::for_session(cfg, s);
            let policy_boundary = context_policy::needs_policy_boundary_for_session(s, &policy);
            if policy.handover_tokens == 0 && !policy_boundary {
                return false;
            }
            let asked = requested(&s.id);
            let idle = now - s.since_ms;
            let quiet = idle >= if asked { 5_000 } else { QUIET_MS };
            // Outside tmux toomux can't type the request, so only a session
            // whose brief is already written can go.
            if s.pane.is_none() {
                return quiet && asked && written(&brief_path(&s.id, None));
            }
            if !quiet {
                return false;
            }
            let transcript = s.transcript(cfg);
            let tokens = || transcript.as_deref().and_then(context_tokens);
            if asked {
                // Compacted instead of writing a brief: nothing to hand over.
                if !policy_boundary
                    && !written(&brief_path(&s.id, None))
                    && tokens().is_some_and(|t| shrank(&s.id, None, t, policy.handover_tokens))
                {
                    let _ = std::fs::remove_file(requested_path(&s.id, None));
                    let _ = std::fs::remove_file(drain_path(&s.id));
                    return false;
                }
                return true;
            }
            if policy_boundary {
                return true;
            }
            // Past the hard limit after a short quiet; past only the turn-end
            // limit after a longer one.
            let limit = idle_limit(&policy, idle);
            // The status line's figure first (cheap), then the transcript's,
            // which a /compact since has changed.
            info.get(&s.id)
                .is_some_and(|i| i.tokens.unwrap_or(0) >= limit && now - i.at_ms < 6 * 3_600_000)
                && tokens().is_none_or(|t| t >= limit)
                && !transcript.as_deref().is_some_and(|t| born_big(limit, t))
        })
        .filter(|s| {
            if marker(s.pid).is_some_and(|m| live(&m, now)) {
                return false;
            }
            // Failures count per conversation: a relaunch that failed brings
            // the old conversation back under a new pid, which mustn't retry
            // at once (and again, and again).
            let ms = markers_for(&s.id);
            !ms.iter().any(|m| live(m, now))
                && !ms.iter().any(|m| {
                    let wait = if m.retry_at_ms > 0 {
                        m.retry_at_ms
                    } else {
                        m.at_ms + RETRY_MS
                    };
                    // A brief written since the failure is worth another try now.
                    m.state == "failed" && now < wait && !brief_since(&s.id, m.at_ms)
                })
        })
        .map(|s| s.pid)
        .collect()
}

/// The context past which a session idle this long is asked to hand over.
fn idle_limit(policy: &ResolvedContextPolicy, idle_ms: i64) -> u64 {
    if idle_ms >= TURN_END_QUIET_MS {
        policy.turn_end_limit()
    } else {
        policy.handover_tokens
    }
}

/// A failure in a few words, for the list (the log keeps the whole message).
fn short_why(e: &str) -> String {
    let short = if e.starts_with(TYPED) {
        "unsent prompt text"
    } else if e.contains("wouldn't start") {
        "the fresh session didn't start"
    } else if e.contains("before the brief was written") {
        "waiting for the limit to reset"
    } else if e.contains("no handover was written") {
        "no brief after 20 min"
    } else if e.contains("ended while writing") {
        "it ended mid-brief"
    } else if e.contains("outside tmux") {
        "outside tmux, no brief yet"
    } else {
        e
    };
    short.to_string()
}

/// This session's brief was written after `at_ms`.
fn brief_since(session: &str, at_ms: i64) -> bool {
    let p = brief_path(session, None);
    written(&p)
        && std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| d.as_millis() as i64 > at_ms)
}

/// Every handover start and outcome, one JSON line each, so a failure can be
/// looked into after its message has left the screen.
fn log(what: Value) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir().join("log.jsonl"))
    {
        let mut v = what;
        v["at"] = json!(chrono_now());
        let _ = writeln!(f, "{v}");
    }
}

fn chrono_now() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%z")
        .to_string()
}

/// Start every handover that is due. Called from the tmux status tick and,
/// so that nothing depends on a terminal being attached, after a turn ends.
pub fn start_due(cfg: &Config, sessions: &[Session], now: i64) {
    use std::os::fd::AsRawFd;
    let _ = std::fs::create_dir_all(dir());
    let Ok(lock) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir().join("due.lock"))
    else {
        return;
    };
    // Two ticks at once would start the same handover twice.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    for pid in due(cfg, sessions, now) {
        // Someone is typing in it: wait, quietly, rather than fail each minute.
        let typing = sessions.iter().find(|s| s.pid == pid).is_some_and(|s| {
            !written(&brief_path(&s.id, None))
                && s.pane
                    .as_ref()
                    .is_some_and(|p| !actions::prompt_empty(&p.id))
        });
        if !typing {
            spawn(pid);
        }
    }
    carry(cfg, sessions);
}

/// Stop hook: a turn has ended. Past the turn-end limit, this is the break
/// to hand over at: the session is told to write its brief now, while its
/// cache is warm and nothing is waiting (the returned output blocks the stop
/// with that instruction; Claude Code doesn't run the prompt hook for a
/// prompt queued mid-turn, so this is the dependable one). Otherwise, if it
/// has to hand over (or has grown past the limit), look again shortly, when
/// it has settled.
pub fn stop_hook(cfg: &Config, v: &Value) -> Option<String> {
    let policy = context_policy::for_hook(cfg, v);
    let policy_boundary = context_policy::needs_policy_boundary(&policy);
    if policy.handover_tokens == 0 && !policy_boundary {
        return None;
    }
    // Not twice in a row: a stop that follows our own instruction is the
    // brief being done. A voyage's loop keeps the Stop hook active turn after
    // turn, so there it's whether the brief was asked for.
    let voyaging = v
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(crate::voyage::open_for)
        .is_some();
    if (v.get("stop_hook_active").and_then(Value::as_bool) != Some(true) || voyaging)
        && let Some(said) = ask_at_turn_end(cfg, v, "stop")
    {
        return Some(json!({"decision": "block", "reason": said}).to_string());
    }
    schedule_tick(v, &policy);
    None
}

fn schedule_tick(v: &Value, policy: &ResolvedContextPolicy) {
    let Some(session) = v.get("session_id").and_then(Value::as_str) else {
        return;
    };
    let transcript = v
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let asked = requested(session);
    // Half the limit, not the limit: the turn's last call may not be in the
    // transcript yet when this runs (it lands a few ms later), and one turn can
    // add tens of thousands of tokens. The tick decides on settled numbers.
    if !asked
        && !context_policy::needs_policy_boundary(policy)
        && transcript.as_deref().and_then(context_tokens).unwrap_or(0) < policy.turn_end_limit() / 2
    {
        return;
    }
    // Asked: its brief should be ready once it settles. Only big: toomux asks
    // it after it has been quiet a while (QUIET_MS, or TURN_END_QUIET_MS past
    // only the turn-end limit).
    let late = (TURN_END_QUIET_MS / 1000 + 15) as u64;
    let after: &[u64] = if asked { &[8, 30] } else { &[50, 120, late] };
    // One tick waiting per session is enough (a busy session stops often).
    let stamp = dir().join(format!("tick-{session}"));
    if !asked && mtime_ms(&stamp).is_some_and(|t| registry::now_ms() - t < 60_000) {
        return;
    }
    touch(&stamp, "");
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("tick")
        .args(after.iter().map(|s| s.to_string()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
    if let Ok(mut c) = cmd.spawn() {
        let _ = c.wait();
    }
}

/// `toomux tick <secs>...`: at each of these many seconds from now, start
/// whatever handovers are due.
pub fn tick(cfg: &Config, after: &[u64]) {
    let start = Instant::now();
    for s in after {
        let at = Duration::from_secs(*s);
        if let Some(wait) = at.checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
        let all = registry::load(cfg);
        start_due(cfg, &all, registry::now_ms());
    }
}

/// Start `toomux handover <pid>` in the background.
pub fn spawn(pid: i32) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["handover", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    // Claim it before the child starts, so the next status tick can't too.
    let mut claim = Marker {
        pid,
        id: String::new(),
        pane: String::new(),
        state: "writing".into(),
        at_ms: registry::now_ms(),
        tokens: 0,
        retry_at_ms: 0,
        why: String::new(),
        owner: 0,
    };
    write_marker(&claim);
    match cmd.spawn() {
        Ok(child) => {
            // Unless it has already written its own.
            if marker(pid).is_some_and(|m| m.owner == 0 && m.state == "writing") {
                claim.owner = child.id() as i32;
                write_marker(&claim);
            }
        }
        Err(_) => {
            let _ = std::fs::remove_file(marker_path(pid));
        }
    }
}

fn find(cfg: &Config, pid: i32) -> Option<Session> {
    registry::load(cfg).into_iter().find(|s| s.pid == pid)
}

/// The whole handover of a main session, start to finish.
pub fn run(cfg: &Config, pid: i32) -> Result<String> {
    let s = find(cfg, pid).context("that session has gone")?;
    if load_lineage().iter().any(|l| l.old_id == s.id) {
        let _ = std::fs::remove_file(marker_path(pid));
        bail!(
            "{} has already handed over (this process holds an old copy of it)",
            s.title
        );
    }
    let transcript = s.transcript(cfg);
    let tokens = transcript
        .as_deref()
        .and_then(context_tokens)
        .or_else(|| crate::usage::sessions().get(&s.id).and_then(|i| i.tokens))
        .unwrap_or(0);
    let pane = s.pane.as_ref().map(|p| p.id.clone());
    let mut m = Marker {
        pid,
        id: s.id.clone(),
        pane: pane.clone().unwrap_or_default(),
        state: "writing".into(),
        at_ms: registry::now_ms(),
        tokens,
        retry_at_ms: 0,
        why: String::new(),
        owner: std::process::id() as i32,
    };
    write_marker(&m);
    log(
        json!({"event": "start", "session": s.id, "pid": pid, "title": s.title, "pane": pane, "tokens": tokens}),
    );
    let result = hand_over(cfg, &s, pane.as_deref(), tokens, transcript.as_deref());
    let now = registry::now_ms();
    m.state = if result.is_ok() {
        "done".into()
    } else {
        "failed".into()
    };
    m.at_ms = now;
    if let Err(e) = &result {
        m.retry_at_ms = now
            + if e.to_string().starts_with(TYPED) {
                RETRY_TYPED_MS
            } else {
                RETRY_MS
            };
        m.why = short_why(&e.to_string());
        // Its background commands go back to it (they'd otherwise run on,
        // unowned).
        crate::jobs::release(&s.id);
    }
    write_marker(&m);
    // A request stays while the session still has to go: the gate keeps it to
    // its brief, and the next try goes ahead as soon as it's ready.
    if result.is_ok() {
        for p in [requested_path(&s.id, None), drain_path(&s.id)] {
            let _ = std::fs::remove_file(p);
        }
    }
    let msg = match &result {
        Ok(where_) => format!(
            "{} handed over at {}k · continuing in a fresh session{where_}",
            s.title,
            tokens / 1000
        ),
        Err(e) => format!("couldn't hand over {}: {e}", s.title),
    };
    log(
        json!({"event": if result.is_ok() { "done" } else { "failed" }, "session": s.id, "pid": pid, "message": msg}),
    );
    crate::watch::announce_text(cfg, &msg, "handover", &s.title, &s.id);
    result.map(|_| msg)
}

/// Hands the session over; on success, says where the fresh one is when
/// that isn't where the old one was.
fn hand_over(
    cfg: &Config,
    s: &Session,
    pane: Option<&str>,
    tokens: u64,
    transcript: Option<&Path>,
) -> Result<String> {
    let policy = context_policy::for_session(cfg, s);
    let policy_boundary = context_policy::needs_policy_boundary_for_session(s, &policy);
    let brief = brief_path(&s.id, None);
    if !written(&brief) {
        let Some(pane) = pane else {
            bail!("it runs outside tmux, so toomux can't ask it for a brief")
        };
        if !actions::prompt_empty(pane) {
            bail!("{TYPED}");
        }
    }
    // Subagent briefs from before this session was asked belong to earlier
    // rounds, already relaunched by it.
    let asked_at = mtime_ms(&drain_path(&s.id))
        .or_else(|| mtime_ms(&requested_path(&s.id, None)))
        .unwrap_or_else(registry::now_ms)
        - 120_000;
    // From here the gate holds the session (and its subagents) to the brief.
    touch(&requested_path(&s.id, None), &tokens.to_string());
    touch(&drain_path(&s.id), "");
    if let (false, Some(pane)) = (written(&brief), pane) {
        let turn_end = tokens < policy.handover_tokens;
        let ask = if policy_boundary {
            policy_boundary_instruction(tokens / 1000, &policy, &brief, transcript)
        } else {
            let limit = if turn_end {
                policy.turn_end_limit()
            } else {
                policy.handover_tokens
            };
            main_instruction(tokens / 1000, limit / 1000, &brief, turn_end, transcript)
        };
        let ask = format!("[toomux handover] {}", ask);
        actions::type_prompt(pane, &ask)?;
    }
    // The brief, and the session settled after writing it.
    let start = Instant::now();
    loop {
        let Some(now) = find(cfg, s.pid) else {
            bail!("the session ended while writing its handover")
        };
        if now.id != s.id {
            bail!("the session moved to another conversation");
        }
        // It can't write anything until the limit resets; it's asked again then.
        if let Some(l) = &now.limit {
            bail!(
                "the account hit its usage limit ({l}) before the brief was written; it hands over once that resets"
            );
        }
        let settled = matches!(now.state, State::Idle | State::Finished | State::Background);
        // A session that a Stop hook keeps waking (a goal loop) never
        // settles: once its brief is written and it has signed off, it goes.
        let signed_off = || {
            mtime_ms(&brief).is_some_and(|t| registry::now_ms() - t > 10_000)
                && transcript.and_then(last_words).is_some_and(|w| {
                    w.trim()
                        .trim_end_matches('.')
                        .eq_ignore_ascii_case("handover written")
                })
        };
        if written(&brief) && start.elapsed() > Duration::from_secs(3) && (settled || signed_off())
        {
            break;
        }
        if start.elapsed() > WRITE_TIMEOUT {
            bail!(
                "no handover was written within {} minutes",
                WRITE_TIMEOUT.as_secs() / 60
            );
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    // Its subagents are handing over too: wait until they have all stopped.
    let subagents = transcript.map(|t| t.with_file_name(&s.id).join("subagents"));
    if let Some(d) = &subagents {
        let start = Instant::now();
        while any_active(d) && start.elapsed() < SUBAGENT_TIMEOUT {
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    // What the successor has to pick up besides the brief.
    let handed: Vec<(String, String, PathBuf)> =
        subagent_briefs(&s.id, subagents.as_deref(), asked_at);
    let jobs = crate::jobs::running_for(&s.id);
    let mut own = std::fs::read_to_string(&brief)?;
    if let Some(i) = own.find(UNSEEN) {
        own.truncate(i);
    }
    let mut text = with_carried(&own, &handed, &jobs);
    if let Some(i) = text.find(crate::voyage::BRIEF_HEADING) {
        text.truncate(i);
    }
    if let Some(q) = crate::voyage::brief_section(&s.id) {
        text.push_str(&q);
    }
    let unseen = transcript.and_then(unseen);
    if let Some(u) = &unseen {
        text.push_str(&unseen_section(u));
    }

    // Once the brief is complete, an old unsent draft must not strand the
    // handover forever. Preserve it first, then clear only that exact draft.
    // If the user types something new between those two checks, clearing is
    // refused and the handover stays put.
    if let Some(p) = pane
        && !actions::prompt_empty(p)
    {
        let Some(cur) = find(cfg, s.pid) else {
            bail!("the session ended before its prompt draft could be preserved")
        };
        if cur.id != s.id
            || cur.proc_start != s.proc_start
            || cur.pane.as_ref().map(|pane| pane.id.as_str()) != pane
        {
            bail!("the session changed while preserving its prompt draft");
        }
        let draft = actions::prompt_draft(p).context("can't read the unsent prompt draft")?;
        if draft.trim().is_empty() {
            bail!("{TYPED}");
        }
        text = with_unsent_draft(&text, &draft);
        // Persistence comes before deletion: a crash after this point can lose
        // neither the brief nor the user's unsent text.
        write_atomic(&brief, &text);
        actions::clear_prompt_if(p, &draft)?;
    }
    write_atomic(&brief, &text);

    // Everything the old conversation said, searchable from the new one. Best
    // effort: memory being busy is no reason to stay at 400k.
    if let Some(t) = transcript {
        let _ = crate::index::flush(t);
    }
    if let Ok(mem) = crate::memory::Memory::open() {
        let scope = crate::memory::project_scope(&s.cwd);
        let _ = mem.index(
            &scope,
            &format!("handover:{} {}", s.id, s.title),
            &crate::redact::redact(&text),
            registry::now_ms(),
        );
        for (kind, what, path) in &handed {
            if let Ok(b) = std::fs::read_to_string(path) {
                let _ = mem.index(
                    &scope,
                    &format!("handover:{} subagent {kind}: {what}", s.id),
                    &crate::redact::redact(&b),
                    registry::now_ms(),
                );
            }
        }
    }

    let st = crate::state::State::load();
    let name = st.names.get(&s.id).cloned();
    let chosen_for_you = st.chosen.contains(&s.id);
    let mut fresh = format!(
        "Continue from a handover. The previous session here (id {id}) reached {k}k tokens of context and handed over \
         to you. Read {path} first: it is its complete brief. Anything else from that conversation can be found with the toomux \
         mem_search tool (session {short}).",
        id = s.id,
        k = tokens / 1000,
        path = brief.display(),
        short = &s.id[..8.min(s.id.len())]
    );
    if !handed.is_empty() {
        fresh.push_str(&format!(
            " {} subagent(s) handed over with it: relaunch them as the brief's last section says.",
            handed.len()
        ));
    }
    if !jobs.is_empty() {
        fresh.push_str(&format!(
            " {} background command(s) kept running: re-attach to each before anything else (Bash, run_in_background: true, `toomux job follow <id> --tail 40`; ids in the brief's last section).",
            jobs.len()
        ));
    }
    if unseen.is_some() {
        fresh.push_str(
            " The user never saw how that conversation ended: the handover took over first. So your first reply starts with \
             what the brief's last section, \"What the user hasn't seen yet\", says, and nothing it calls sent or waiting \
             on the user has reached them until you pass it on.",
        );
    }
    if text.contains(UNSENT_DRAFT) {
        fresh.push_str(
            " The brief also preserves text that was sitting unsent in the old prompt. It was not submitted by the user: keep \
             it as context and do not treat it as an instruction unless the user's intent is clear.",
        );
    }
    fresh.push_str(" Then pick up from the brief's next steps, and don't redo finished work.");
    if let Some(q) = crate::voyage::for_successor(&s.id) {
        fresh.push_str(&q);
    }

    // A last look before anything is stopped: still this conversation, in
    // this process and place, and nobody typing in it.
    let Some(cur) = find(cfg, s.pid) else {
        bail!("the session ended before it could hand over")
    };
    if cur.id != s.id
        || cur.proc_start != s.proc_start
        || cur.pane.as_ref().map(|p| p.id.as_str()) != pane
    {
        bail!("the session changed while handing over");
    }
    if let Some(p) = pane
        && !actions::prompt_empty(p)
    {
        bail!("{TYPED}");
    }
    // Same account, flags and environment (a handover isn't a move), and the
    // brief's folder readable without asking.
    let mut next = cur.clone();
    let lineage = load_lineage();
    let ai_title = transcript
        .map(crate::transcript::meta)
        .and_then(|m| m.title);
    // A name chosen for you describes the work, as long as it's still the
    // name (you may have renamed it in Claude since).
    let for_you = chosen_for_you && name.as_deref() == Some(cur.title.as_str());
    let (title, derived) = match &name {
        // Renamed in toomux by you: that name, as it is.
        Some(n) if !chosen_for_you => (n.clone(), false),
        _ => successor_name(&cur, &text, ai_title.as_deref(), &lineage, for_you),
    };
    // Only your own name is carried as toomux's name for the successor.
    let name = name.filter(|_| !chosen_for_you);
    next.title = title.clone();
    next.env = registry::full_env(s.pid);
    let briefs = dir().display().to_string();
    if !next.args.contains(&briefs) {
        next.args.extend(["--add-dir".to_string(), briefs]);
    }
    // The session's background commands carry on without it.
    crate::jobs::carry_on(&s.id);
    let (pane, where_) = match pane {
        Some(p) => {
            actions::relaunch_fresh(cfg, &next, &fresh)?;
            (p.to_string(), String::new())
        }
        None => {
            let p = actions::relaunch_fresh_in_tmux(cfg, &next, &fresh)?;
            let server = crate::tmux::server_of(&p).unwrap_or("default").to_string();
            (p.clone(), format!(" in its own tmux server, {server}"))
        }
    };
    remember(Lineage::new(
        s.id.clone(),
        pane,
        registry::now_ms(),
        name,
        title,
        derived,
    ));
    // Subagent requests are settled: their briefs are listed.
    let sub_req = format!("req-{}-", s.id);
    for e in std::fs::read_dir(dir()).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with(&sub_req) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    // Name, pin and jobs go to the successor as soon as it shows up (no
    // waiting for a status tick, which needs a terminal attached).
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_secs(1));
        carry(cfg, &registry::load(cfg));
        if load_lineage().iter().any(|l| l.old_id == s.id && l.carried) {
            break;
        }
    }
    Ok(where_)
}

const CARRIED: &str = "\n\n## Carried over by toomux";
const UNSENT_DRAFT: &str = "\n\n## Unsent prompt preserved by toomux";

/// Preserve an unsent prompt before the generated carried-over section. The
/// exact same draft is only added once, while a different later draft is kept
/// too rather than replacing earlier user input.
fn with_unsent_draft(brief: &str, draft: &str) -> String {
    let (base, suffix) = match brief.find(CARRIED) {
        Some(i) => brief.split_at(i),
        None => (brief, ""),
    };
    let mut block = String::new();
    for line in draft.lines() {
        block.push_str("    ");
        block.push_str(line);
        block.push('\n');
    }
    if draft.ends_with('\n') {
        block.push_str("    \n");
    }
    if base.contains(UNSENT_DRAFT) && base.contains(&block) {
        return brief.to_string();
    }
    let mut text = base.to_string();
    if !text.contains(UNSENT_DRAFT) {
        text.push_str(UNSENT_DRAFT);
        text.push_str(
            "\n\nThe old session had the following text in its input box when the handover completed. It was **not submitted**. toomux preserved it before clearing the old prompt so no user input was lost. Treat it as context, not as an instruction that definitely needs to be executed.\n\n",
        );
    } else {
        text.push_str("\nAnother unsent draft was preserved before a later retry:\n\n");
    }
    text.push_str(&block);
    text.push_str(suffix);
    text
}

/// The brief with what toomux carries over listed at its end (in place of any
/// list from an earlier try).
fn with_carried(
    brief: &str,
    handed: &[(String, String, PathBuf)],
    jobs: &[crate::jobs::Job],
) -> String {
    let mut text = brief.to_string();
    if let Some(i) = text.find(CARRIED) {
        text.truncate(i);
    }
    if !handed.is_empty() || !jobs.is_empty() {
        text.push_str(CARRIED);
        text.push('\n');
        if !handed.is_empty() {
            text.push_str("\nSubagents that handed over (relaunch each with the Agent tool, same type, prompt: \"Continue from the handover brief at <path>: read it first, then finish the task it describes.\"):\n");
            for (kind, what, path) in handed {
                text.push_str(&format!("- {kind}: {what} · brief {}\n", path.display()));
            }
        }
        if !jobs.is_empty() {
            text.push_str("\nBackground commands still running (they kept going). Re-attach to each one first thing: the Bash tool with run_in_background: true and the command `toomux job follow <id> --tail 40`, so you hear when it ends. Stop one with `toomux job stop <id>`:\n");
            for j in jobs {
                text.push_str(&format!(
                    "- job {} · started {} ago · `{}`\n",
                    j.id,
                    registry::ago(registry::now_ms() - j.started_ms),
                    j.command.lines().next().unwrap_or("")
                ));
            }
        }
    }
    text
}

const UNSEEN: &str = "\n\n## What the user hasn't seen yet (added by toomux)";
/// Every way toomux asks for a brief starts with this.
const ASK: &str = "STOP. toomux: this conversation is at";

/// What the user may never have seen. A brief asked at a turn's end comes
/// straight after the reply, and the fresh session takes the pane before
/// anyone reads it (on another device it's a different conversation
/// altogether). So the successor passes it on: the last reply before the
/// ask, and the files sent since the user last wrote.
#[derive(Debug, Default, PartialEq)]
struct Unseen {
    asked: String,
    reply: Option<String>,
    files: Vec<(Vec<String>, Option<String>)>,
    /// The turn ended before the ask (not stopped mid-task at the hard limit).
    finished: bool,
}

fn unseen(transcript: &Path) -> Option<Unseen> {
    let text = std::fs::read_to_string(transcript).ok()?;
    let mut u = Unseen {
        finished: true,
        ..Default::default()
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let content = v.pointer("/message/content");
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                let results: Vec<String> = content
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    .map(|b| text_of(b.get("content")))
                    .collect();
                if results.iter().any(|r| is_ask(r)) {
                    u.finished = false;
                    break;
                }
                // Answers to a question put to them are a message from them.
                if let Some(a) = results.iter().find(|r| {
                    r.starts_with("The user answered") || r.starts_with("User has answered")
                }) {
                    u = from_prompt(
                        u,
                        a.split(" Read the answers carefully").next().unwrap_or(a),
                    );
                    continue;
                }
                let said = text_of(content);
                if is_ask(&said) {
                    break;
                }
                if v.get("isMeta").and_then(Value::as_bool) != Some(true) {
                    u = from_prompt(u, &said);
                }
            }
            // A prompt sent mid-turn.
            Some("attachment")
                if v.pointer("/attachment/type").and_then(Value::as_str)
                    == Some("queued_command") =>
            {
                u = from_prompt(
                    u,
                    v.pointer("/attachment/prompt")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
            }
            Some("assistant") => {
                for b in content.and_then(Value::as_array).into_iter().flatten() {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = b.get("text").and_then(Value::as_str).unwrap_or("").trim();
                            if !t.is_empty()
                                && !t
                                    .trim_end_matches('.')
                                    .eq_ignore_ascii_case("handover written")
                            {
                                u.reply = Some(t.to_string());
                            }
                        }
                        Some("tool_use")
                            if b.get("name").and_then(Value::as_str) == Some("SendUserFile") =>
                        {
                            let files: Vec<String> = b
                                .pointer("/input/files")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(|f| f.as_str().map(str::to_string))
                                .collect();
                            let caption = b
                                .pointer("/input/caption")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            if !files.is_empty() {
                                u.files.push((files, caption));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    (u.reply.is_some() || !u.files.is_empty()).then_some(u)
}

/// The text of a message or tool result: a plain string, or its text blocks.
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// toomux's ask itself, however it arrived: typed in, from the Stop hook, or
/// a tool call the gate refused. Not a mention of it, as when a session reads
/// this file or greps a transcript.
fn is_ask(s: &str) -> bool {
    let s = s.trim_start();
    let s = s.strip_prefix("<tool_use_error>").unwrap_or(s);
    let s = match s.split_once(" hook error: ") {
        Some((hook, rest)) if !hook.contains(char::is_whitespace) => rest,
        _ => s,
    };
    let s = s.strip_prefix("Stop hook feedback:").unwrap_or(s);
    let s = s.strip_prefix("[toomux handover]").unwrap_or(s);
    s.trim_start().starts_with(ASK)
}

/// A message from the user starts afresh: they saw what came before it.
/// toomux's own prompt to a successor starts it too, but isn't theirs.
fn from_prompt(u: Unseen, said: &str) -> Unseen {
    let said = said.trim();
    if said.is_empty()
        || said.starts_with('<')
        || said.starts_with("Caveat:")
        || said.starts_with("[toomux")
    {
        return u;
    }
    let asked = if said.starts_with("Continue from a handover.") {
        String::new()
    } else {
        said.to_string()
    };
    Unseen {
        asked,
        finished: true,
        ..Default::default()
    }
}

/// The brief's section for it.
fn unseen_section(u: &Unseen) -> String {
    const MAX: usize = 12_000;
    let clip = |s: &str, n: usize| match s.char_indices().nth(n) {
        Some((i, _)) => format!("{} […]", &s[..i]),
        None => s.to_string(),
    };
    let mut s = format!("{UNSEEN}\n\n");
    s.push_str(if u.finished {
        "The handover took over before the user read this conversation's last reply. Pass it on before anything else: \
         give the reply in full (change only what has changed since), and send the files again.\n"
    } else {
        "This conversation was stopped mid-task at its limit, so the user never got a closing reply. Before anything \
         else, tell them where the work stood (the brief has it), and send the files again.\n"
    });
    if !u.asked.is_empty() {
        s.push_str(&format!(
            "\nTheir last message:\n\n> {}\n",
            clip(&u.asked, 600).replace('\n', "\n> ")
        ));
    }
    if let Some(r) = &u.reply {
        let what = if u.finished {
            "The last reply"
        } else {
            "Its last words to them"
        };
        s.push_str(&format!("\n{what}:\n\n````\n{}\n````\n", clip(r, MAX)));
    }
    if !u.files.is_empty() {
        s.push_str("\nFiles it sent them (send each again, with SendUserFile if you have it; otherwise give the paths):\n");
        for (files, caption) in &u.files {
            s.push_str(&format!("- {}", files.join(", ")));
            if let Some(c) = caption {
                s.push_str(&format!(" · \"{c}\""));
            }
            s.push('\n');
        }
    }
    s
}

fn mtime_ms(p: &Path) -> Option<i64> {
    let t = std::fs::metadata(p).and_then(|m| m.modified()).ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64)
}

/// A subagent is still working: written to in the last 20s, or (within ten
/// minutes) its last word isn't a final answer, e.g. it's waiting on a long
/// tool call, which writes nothing while it runs.
fn any_active(dir: &Path) -> bool {
    let now = registry::now_ms();
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "jsonl") {
                return false;
            }
            let Some(age) = mtime_ms(&p).map(|t| now - t) else {
                return false;
            };
            age < 20_000 || (age < 10 * 60_000 && !finished(&p))
        })
}

/// An agent's transcript ends with its final answer (text, no tool call).
fn finished(transcript: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(transcript) else {
        return true;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(256 * 1024);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return true;
    }
    let mut buf = Vec::new();
    let _ = f.take(len - from).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let calls = v
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .is_some_and(|c| {
                        c.iter()
                            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    });
                return !calls;
            }
            Some("user") => return false,
            _ => continue,
        }
    }
    true
}

/// (type, description, brief) of each subagent of `session` that handed over
/// since `since_ms`. A read-only subagent gives its brief as its final
/// message; that is saved to the brief's file here.
fn subagent_briefs(
    session: &str,
    subagents: Option<&Path>,
    since_ms: i64,
) -> Vec<(String, String, PathBuf)> {
    let asked = format!("req-{session}-");
    for e in std::fs::read_dir(dir()).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(agent) = name.strip_prefix(&asked) else {
            continue;
        };
        let file = brief_path(session, Some(agent));
        if written(&file) {
            continue;
        }
        let last = subagents.and_then(|d| last_words(&d.join(format!("agent-{agent}.jsonl"))));
        if let Some(text) = last.filter(|t| t.contains("HANDOVER") && t.len() > 200) {
            write_atomic(&file, &text);
        }
    }
    let prefix = format!("{session}-agent-");
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir()).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(agent) = name
            .strip_prefix(&prefix)
            .and_then(|r| r.strip_suffix(".md"))
        else {
            continue;
        };
        if !written(&e.path()) || mtime_ms(&e.path()).is_none_or(|t| t < since_ms) {
            continue;
        }
        let meta: Value = subagents
            .and_then(|d| std::fs::read_to_string(d.join(format!("agent-{agent}.meta.json"))).ok())
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or(Value::Null);
        let kind = meta
            .get("agentType")
            .and_then(Value::as_str)
            .unwrap_or("general-purpose")
            .to_string();
        let what = meta
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("subagent")
            .to_string();
        out.push((kind, what, e.path()));
    }
    out
}

/// The last thing an agent said: its final message.
fn last_words(transcript: &Path) -> Option<String> {
    let text = std::fs::read_to_string(transcript).ok()?;
    text.lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        let parts: Vec<&str> = v
            .pointer("/message/content")?
            .as_array()?
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    })
}

mod successor;
#[cfg(test)]
use successor::brief_title;
use successor::{Lineage, carry, load_lineage, remember, successor_name};
pub use successor::{generic, topics};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ask_names_gone_paths_up_to_a_limit() {
        let files = vec![
            (
                "/m/a.md".to_string(),
                vec!["/home/x/one".to_string(), "/home/x/two".to_string()],
            ),
            (
                "/m/b.md".to_string(),
                (0..25).map(|i| format!("/home/x/p{i}")).collect(),
            ),
            ("/m/c.md".to_string(), vec!["/home/x/late".to_string()]),
        ];
        let s = stale_list(files);
        assert!(
            s.starts_with("a.md cites /home/x/one, /home/x/two; b.md cites /home/x/p0,"),
            "{s}"
        );
        assert!(
            s.contains("/home/x/p17") && !s.contains("/home/x/p18"),
            "20 paths in all: {s}"
        );
        assert!(
            !s.contains("c.md") && s.ends_with("and 8 more, for a later handover"),
            "{s}"
        );
        assert_eq!(stale_list(vec![]), "");
    }

    fn unseen_in(name: &str, lines: &[Value]) -> Option<Unseen> {
        let p =
            std::env::temp_dir().join(format!("toomux-unseen-{name}-{}.jsonl", std::process::id()));
        std::fs::write(
            &p,
            lines
                .iter()
                .map(|l| l.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
        let u = unseen(&p);
        let _ = std::fs::remove_file(&p);
        u
    }

    fn you(text: &str) -> Value {
        json!({"type": "user", "message": {"content": text}})
    }

    fn says(text: &str) -> Value {
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
    }

    #[test]
    fn the_reply_a_handover_hid_is_passed_on() {
        // As on 09-30: the reply and its renders, then the Stop hook's ask,
        // then the brief.
        let sent = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "SendUserFile",
            "input": {"files": ["/a/graph.png", "/b/shot.png"], "caption": "the renders"}}]}});
        let lines = [
            you("an older ask"),
            says("an older answer"),
            you("There should be legends on the tui"),
            says("Adding them."),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "ok"}]}}),
            sent,
            says("Legends are in. Restart toomux to see them."),
            json!({"type": "user", "isMeta": true, "message": {"content": format!("Stop hook feedback:\n{ASK} 243k tokens")}}),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Write", "input": {}}]}}),
            says("handover written"),
        ];
        let u = unseen_in("end", &lines).unwrap();
        assert_eq!(u.asked, "There should be legends on the tui");
        assert_eq!(
            u.reply.as_deref(),
            Some("Legends are in. Restart toomux to see them.")
        );
        assert_eq!(
            u.files,
            vec![(
                vec!["/a/graph.png".to_string(), "/b/shot.png".to_string()],
                Some("the renders".to_string())
            )]
        );
        assert!(u.finished);
        let s = unseen_section(&u);
        assert!(
            s.contains("Restart toomux")
                && s.contains("/a/graph.png, /b/shot.png · \"the renders\"")
                && !s.contains("older"),
            "{s}"
        );

        // Asked by toomux typing into an idle session: the same.
        let typed = [
            you("go"),
            says("Done."),
            you(&format!("[toomux handover] {ASK} 210k")),
            says("handover written"),
        ];
        assert_eq!(
            unseen_in("typed", &typed).unwrap().reply.as_deref(),
            Some("Done.")
        );

        // Stopped mid-task by the gate: its last word, marked unfinished.
        let mid = [
            you("build it"),
            says("Running the tests."),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": format!("PreToolUse:Bash hook error: {ASK} 400k (the limit is 400k)")}]}}),
            says("handover written"),
        ];
        let u = unseen_in("mid", &mid).unwrap();
        assert!(!u.finished && u.reply.as_deref() == Some("Running the tests."));

        // A successor quotes the user, not toomux's prompt to it; a message
        // sent mid-turn counts as theirs.
        let queued = json!({"type": "attachment", "attachment": {"type": "queued_command", "prompt": "add a legend"}});
        let chain = [
            you("Continue from a handover. The previous session here"),
            says("Picked up."),
            says("All done."),
        ];
        assert_eq!(unseen_in("chain", &chain).unwrap().asked, "");
        let u = unseen_in(
            "queued",
            &[you("go"), says("Working."), queued, says("Legend added.")],
        )
        .unwrap();
        assert_eq!(
            (u.asked.as_str(), u.reply.as_deref()),
            ("add a legend", Some("Legend added."))
        );
        assert!(
            !unseen_section(&unseen_in("chain", &chain).unwrap()).contains("Their last message")
        );

        // As on 10-01: a session that reads this file or greps a transcript
        // mentions the ask; only the real one ends it. Answers to a question
        // are the user's message.
        let result = |c: Value| json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": c}]}});
        let lines = [
            you("fix the handover"),
            result(json!(format!("1152:const ASK: &str = \"{ASK}\";"))),
            result(json!([{"type": "text", "text": format!("1\t//! Handover\n2\t{ASK} 204k")}])),
            says("Fixed. Which follow-ups next?"),
            result(json!(
                "The user answered: \"Which fixes?\"=\"all\". Read the answers carefully and follow them."
            )),
            says("All four are in."),
            json!({"type": "user", "isMeta": true, "message": {"content": format!("Stop hook feedback:\n{ASK} 204k tokens")}}),
            says("handover written"),
        ];
        let u = unseen_in("mention", &lines).unwrap();
        assert_eq!(
            (u.asked.as_str(), u.reply.as_deref()),
            (
                "The user answered: \"Which fixes?\"=\"all\".",
                Some("All four are in.")
            )
        );
        assert!(u.finished);

        // Nothing said or sent since the user's last message: nothing to pass on.
        assert!(
            unseen_in(
                "none",
                &[
                    says("old"),
                    you("new ask"),
                    you(&format!("[toomux handover] {ASK} 210k"))
                ]
            )
            .is_none()
        );
    }

    fn cfg(limit: u64) -> Config {
        let mut c = Config::default();
        c.handover_tokens = limit;
        c
    }

    /// A config whose one account's registry lists `sessions` as live,
    /// interactive sessions of this process.
    fn cfg_with(limit: u64, root: &Path, sessions: &[&str]) -> Config {
        let mut c = cfg(limit);
        let acct = root.join("acct");
        std::fs::create_dir_all(acct.join("sessions")).unwrap();
        for (i, id) in sessions.iter().enumerate() {
            let raw = json!({"pid": std::process::id(), "sessionId": id, "cwd": "/", "kind": "interactive"});
            std::fs::write(acct.join(format!("sessions/{i}.json")), raw.to_string()).unwrap();
        }
        c.accounts = vec![crate::config::Account {
            name: "t".into(),
            config_dir: acct.display().to_string(),
        }];
        c
    }

    fn transcript(dir: &Path, name: &str, tokens: u64) -> PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        // A conversation's first call, then its latest.
        let first = json!({"type": "assistant", "message": {"usage": {"input_tokens": 5_000}}});
        let line = json!({"type": "assistant", "message": {"usage": {"input_tokens": 1, "cache_read_input_tokens": tokens - 1}}});
        std::fs::write(&p, format!("{first}\n{line}\n")).unwrap();
        p
    }

    #[test]
    fn the_gate_stops_whoever_is_past_the_limit() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY: tests in this module run with their own state dir.
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.join("state")) };
        let main = transcript(&tmp, "p/s1.jsonl", 120_000);
        transcript(&tmp, "p/s1/subagents/agent-a1.jsonl", 500_000);
        let call = |agent: Option<&str>, tool: &str, file: Option<&str>| {
            let mut v = json!({"session_id": "s1", "transcript_path": main.display().to_string(), "tool_name": tool, "tool_input": {}});
            if let Some(a) = agent {
                v["agent_id"] = json!(a);
                v["agent_type"] = json!("Explore");
            }
            if let Some(f) = file {
                v["tool_input"]["file_path"] = json!(f);
            }
            gate(&cfg_with(400_000, &tmp, &["s1"]), &v)
        };
        assert!(
            call(None, "Bash", None).is_none(),
            "the main session is under the limit"
        );
        let denied = call(Some("a1"), "Grep", None).expect("the subagent is over it");
        assert!(
            denied.contains("\"deny\"") && denied.contains("HANDOVER: this Explore subagent"),
            "{denied}"
        );
        let brief = brief_path("s1", Some("a1"));
        assert!(
            call(Some("a1"), "Write", Some(&brief.display().to_string()))
                .unwrap()
                .contains("\"allow\""),
            "writing its brief is allowed"
        );
        std::fs::write(&brief, "x".repeat(400)).unwrap();
        assert!(
            call(Some("a1"), "Grep", None)
                .unwrap()
                .contains("your brief is written")
        );
        // The main session crossing the limit drains every subagent.
        transcript(&tmp, "p/s1.jsonl", 450_000);
        assert!(
            call(None, "Bash", None)
                .unwrap()
                .contains("continues in a fresh session")
        );
        transcript(&tmp, "p/s1/subagents/agent-a2.jsonl", 10_000);
        assert!(
            call(Some("a2"), "Read", None)
                .unwrap()
                .contains("belongs to is handing over"),
            "a small subagent hands over with its parent"
        );
        assert!(
            call(None, "TaskOutput", None).is_none(),
            "the main session may wait on its subagents"
        );
        let own = brief_path("s1", None).display().to_string();
        assert!(
            call(None, "Write", Some(&own))
                .unwrap()
                .contains("\"allow\""),
            "its brief is written without a permission prompt"
        );
        // Its workspace memory can be brought up to date meanwhile; nothing else.
        std::fs::create_dir_all(tmp.join("p/memory")).unwrap();
        std::fs::write(tmp.join("p/memory/MEMORY.md"), "- rule\n".repeat(160)).unwrap();
        let asked = call(None, "Bash", None).unwrap();
        assert!(
            asked.contains("bring this workspace's memory up to date")
                && asked.contains("160 lines"),
            "{asked}"
        );
        let note = tmp.join("p/memory/deploys.md").display().to_string();
        assert!(
            call(None, "Write", Some(&note)).is_none(),
            "a new topic file"
        );
        assert!(
            call(
                None,
                "Read",
                Some(&tmp.join("p/memory/MEMORY.md").display().to_string())
            )
            .is_none()
        );
        assert!(
            call(
                None,
                "Write",
                Some(&tmp.join("p/src.rs").display().to_string())
            )
            .unwrap()
            .contains("\"deny\""),
            "not the project"
        );
        let sneaky = tmp.join("p/memory/../s1.jsonl").display().to_string();
        assert!(
            call(None, "Write", Some(&sneaky))
                .unwrap()
                .contains("\"deny\""),
            "nor out of the folder"
        );
        assert!(
            call(Some("a2"), "Write", Some(&note))
                .unwrap()
                .contains("\"deny\""),
            "a subagent leaves memory to its session"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_fork_counts_from_the_context_it_was_born_with() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-gate-fork-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.join("state")) };
        let main = transcript(&tmp, "p/s1.jsonl", 120_000);
        let fork = tmp.join("p/s1/subagents/agent-f1.jsonl");
        std::fs::create_dir_all(fork.parent().unwrap()).unwrap();
        let grow = |tokens: u64| {
            let born = json!({"type": "assistant", "message": {"usage": {"input_tokens": 1, "cache_read_input_tokens": 366_999}}});
            let now = json!({"type": "assistant", "message": {"usage": {"input_tokens": 1, "cache_read_input_tokens": tokens - 1}}});
            std::fs::write(&fork, format!("{born}\n{now}\n")).unwrap();
        };
        let mut c = cfg_with(400_000, &tmp, &["s1"]);
        c.subagent_handover_tokens = 250_000;
        let call = || {
            let v = json!({"session_id": "s1", "transcript_path": main.display().to_string(), "tool_name": "Bash", "tool_input": {}, "agent_id": "f1", "agent_type": "fork"});
            gate(&c, &v)
        };
        grow(380_000);
        assert!(
            call().is_none(),
            "born at 367k, a fork still has its own work to do"
        );
        grow(495_000);
        let denied = call().expect("125k of its own work later, it hands over");
        assert!(
            denied.contains("launch a new general-purpose subagent"),
            "{denied}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_gate_lets_go_when_holding_on_is_wrong() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-gate2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.join("state")) };
        let c = cfg_with(400_000, &tmp, &["live", "old"]);
        let call = |id: &str, t: &Path| {
            gate(
                &c,
                &json!({"session_id": id, "transcript_path": t.display().to_string(), "tool_name": "Bash", "tool_input": {}}),
            )
        };
        // `claude -p` and SDK runs aren't in the registry as interactive: toomux
        // couldn't restart them, so it doesn't hold them.
        let p = transcript(&tmp, "p/print.jsonl", 450_000);
        assert!(call("print", &p).is_none());
        assert!(!requested("print"));
        // Asked, then /compact shrank it before a brief was written: released.
        let t = transcript(&tmp, "p/live.jsonl", 450_000);
        assert!(call("live", &t).is_some());
        assert!(requested("live") && drain_path("live").exists());
        transcript(&tmp, "p/live.jsonl", 90_000);
        assert!(call("live", &t).is_none(), "released after /compact");
        assert!(!requested("live") && !drain_path("live").exists());
        // Not while toomux is handing it over, though.
        transcript(&tmp, "p/live.jsonl", 450_000);
        assert!(call("live", &t).is_some());
        write_marker(&Marker {
            pid: 77,
            id: "live".into(),
            pane: String::new(),
            state: "writing".into(),
            at_ms: registry::now_ms(),
            tokens: 1,
            retry_at_ms: 0,
            why: String::new(),
            owner: std::process::id() as i32,
        });
        transcript(&tmp, "p/live.jsonl", 90_000);
        assert!(call("live", &t).is_some(), "held while its handover runs");
        // A conversation that already handed over and was reopened on purpose.
        let o = transcript(&tmp, "p/old.jsonl", 450_000);
        remember(Lineage {
            old_id: "old".into(),
            pane: "%1".into(),
            at_ms: registry::now_ms(),
            name: None,
            carried: false,
            ..Default::default()
        });
        assert!(call("old", &o).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_conversation_born_at_the_limit_is_not_asked() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-born-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.join("state")) };
        let c = cfg_with(40_000, &tmp, &["b"]);
        let t = tmp.join("p/b.jsonl");
        std::fs::create_dir_all(t.parent().unwrap()).unwrap();
        let call = |n: u64| json!({"type": "assistant", "message": {"usage": {"input_tokens": n}}});
        std::fs::write(&t, format!("{}\n{}\n", call(38_000), call(45_000))).unwrap();
        assert_eq!(context_tokens(&t), Some(45_000));
        let v = json!({"session_id": "b", "transcript_path": t.display().to_string(), "tool_name": "Bash", "tool_input": {}});
        assert!(
            gate(&c, &v).is_none(),
            "a fresh session already that big would hand over for ever"
        );
        assert!(
            gate(&cfg_with(60_000, &tmp, &["b"]), &v).is_none(),
            "under the limit"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn past_the_turn_end_limit_it_goes_at_a_break_not_mid_task() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-turnend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.join("state")) };
        let c = cfg_with(400_000, &tmp, &["w", "small", "x", "q"]);
        assert_eq!((c.turn_end_limit(), c.subagent_limit()), (250_000, 250_000));
        let t = transcript(&tmp, "p/w.jsonl", 300_000);
        let tool = |agent: Option<&str>| {
            let mut v = json!({"session_id": "w", "transcript_path": t.display().to_string(), "tool_name": "Bash", "tool_input": {}});
            if let Some(a) = agent {
                v["agent_id"] = json!(a);
            }
            gate(&c, &v)
        };
        let prompt = |id: &str, t: &Path, text: &str| {
            prompt_hook(
                &c,
                &json!({"session_id": id, "transcript_path": t.display().to_string(), "prompt": text}),
            )
        };
        assert!(
            tool(None).is_none(),
            "mid-turn, a conversation goes only at the hard limit"
        );
        transcript(&tmp, "p/w/subagents/agent-a1.jsonl", 260_000);
        assert!(
            tool(Some("a1")).unwrap().contains("the limit is 250k"),
            "a subagent has no turns: its own limit applies"
        );
        let _ = std::fs::remove_dir_all(tmp.join("state"));
        // Your next prompt is the break.
        assert!(
            prompt("w", &t, "/model").is_none(),
            "commands aren't work to move"
        );
        let s = transcript(&tmp, "p/small.jsonl", 200_000);
        assert!(
            prompt("small", &s, "go on").is_none(),
            "under the turn-end limit"
        );
        let out: Value =
            serde_json::from_str(&prompt("w", &t, "now add the export button").unwrap()).unwrap();
        let said = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert!(
            said.contains("past 250k")
                && said.contains("put the request first among the next steps"),
            "{said}"
        );
        assert!(requested("w") && drain_path("w").exists() && asked_at("w", None) == Some(300_000));
        assert!(prompt("w", &t, "and this").is_none(), "asked once");
        let held = tool(None).unwrap();
        assert!(
            held.contains("\"deny\"") && held.contains("where it continues fresh at a turn's end"),
            "the gate holds it to the brief: {held}"
        );
        // The dependable break is the turn's end: the stop is held with the
        // same instruction, once.
        let x = transcript(&tmp, "p/x.jsonl", 260_000);
        let stop = |active: bool| {
            stop_hook(
                &c,
                &json!({"session_id": "x", "transcript_path": x.display().to_string(), "stop_hook_active": active}),
            )
        };
        assert!(
            stop(true).is_none(),
            "a stop after our own instruction is the brief being done"
        );
        assert!(!requested("x"));
        let out: Value = serde_json::from_str(&stop(false).unwrap()).unwrap();
        assert_eq!(out["decision"], "block");
        assert!(
            out["reason"]
                .as_str()
                .unwrap()
                .contains("Write a complete, standalone handover brief")
        );
        assert!(requested("x") && stop(false).is_none(), "asked once");
        // A voyage's loop keeps the Stop hook active every turn: it's asked
        // all the same, and the voyage waits while the brief is written.
        let q = transcript(&tmp, "p/q.jsonl", 260_000);
        crate::voyage::start(
            "q",
            "/",
            "the build is green".into(),
            None,
            None,
            crate::voyage::Persistence::Steady,
        );
        let qstop = json!({"session_id": "q", "transcript_path": q.display().to_string(), "stop_hook_active": true});
        let out: Value = serde_json::from_str(&stop_hook(&c, &qstop).unwrap()).unwrap();
        assert!(
            out["reason"]
                .as_str()
                .unwrap()
                .contains("Write a complete, standalone handover brief")
        );
        assert!(
            requested("q") && crate::voyage::stop_hook(&c, &qstop).is_none(),
            "the voyage doesn't judge the brief"
        );
        assert!(
            crate::voyage::for_successor("q")
                .unwrap()
                .contains("the build is green")
        );
        // It isn't released as if it had compacted: it was asked at 300k, not 400k.
        assert!(!shrank("w", None, 300_000, c.handover_tokens));
        assert!(shrank("w", None, 100_000, c.handover_tokens));
        // Idle: the hard limit after a short quiet, the turn-end one after ten minutes.
        let p = context_policy::resolve(&c, Default::default());
        assert_eq!(idle_limit(&p, 60_000), 400_000);
        assert_eq!(idle_limit(&p, TURN_END_QUIET_MS), 250_000);
        // Off means off.
        let mut off = c.clone();
        off.handover_turn_end_tokens = 0;
        assert_eq!(off.turn_end_limit(), 400_000);
        off.handover_tokens = 0;
        assert_eq!((off.turn_end_limit(), off.subagent_limit()), (0, 0));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_compaction_settles_it() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.join("state")) };
        let c = cfg_with(400_000, &tmp, &["k"]);
        // Its transcript where the session's account keeps it.
        let t = transcript(&tmp, "acct/projects/-nonexistent/k.jsonl", 450_000);
        assert_eq!(context_tokens(&t), Some(450_000));
        let boundary = json!({"type": "system", "subtype": "compact_boundary", "compactMetadata": {"preTokens": 450_000, "postTokens": 12_000}});
        std::fs::write(
            &t,
            format!("{}{boundary}\n", std::fs::read_to_string(&t).unwrap()),
        )
        .unwrap();
        assert_eq!(context_tokens(&t), Some(12_000), "what the compaction left");
        // Asked before it compacted, and no brief: released, not handed over.
        touch(&requested_path("k", None), "450000");
        touch(&drain_path("k"), "");
        let k = session(9, "k", 60_000, true);
        assert!(due(&c, &[k], registry::now_ms()).is_empty());
        assert!(!requested("k") && !drain_path("k").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_successor_is_named_for_its_work() {
        for g in [
            "Handover continuation",
            "Handover from 0bf5026f-5546-4797-8d30-328b1358cd14",
            "Continue from previous session",
            "Resume the handover brief",
        ] {
            assert!(generic(g), "{g}");
        }
        for t in [
            "Handover and auto-restart issues",
            "Claude-Northwind",
            "Engine",
            "Oli & Studio",
            "ComfyUI setup for Intel Arc Pro B70",
        ] {
            assert!(!generic(t), "{t}");
        }
        // Last night's briefs.
        let cases = [
            (
                "# Handover — ComfyUI on Arc Pro B70 + Northwind brand image packs (session d19fe8da)",
                Some("ComfyUI on Arc Pro B70"),
            ),
            (
                "# Handover: Northwind image packs, cut-outs, icons and explainers (session 14e562ec)",
                Some("Northwind image packs"),
            ),
            (
                "# HANDOVER — Command Center TF lane, edge header-stripping, and Sam's auth lockout",
                Some("Command Center TF lane"),
            ),
            (
                "# Handover: toomux review (\"nothing short of exceptional\") — session 9b506217",
                Some("toomux review"),
            ),
            (
                "# HANDOVER — Command Center Terraform lane, 2026-09-29 (evening)",
                Some("Command Center Terraform lane"),
            ),
            ("# Handover Brief\n\n## Goal\nRead three files", None),
            ("no heading at all", None),
        ];
        for (brief, want) in cases {
            assert_eq!(brief_title(brief).as_deref(), want, "{brief}");
        }
        let mut s = session(1, "new", 0, true);
        s.cwd = "/home/x/northwind-packs".into();
        let brief = "# Handover: Northwind image packs, cut-outs, icons and explainers";
        s.title = "Oli & Studio".into();
        assert_eq!(
            successor_name(&s, brief, Some("Studio polish"), &[], false),
            ("Oli & Studio".into(), false),
            "a name someone gave stays"
        );
        s.title = "Handover continuation".into();
        assert_eq!(
            successor_name(&s, brief, None, &[], false),
            ("Northwind image packs".into(), true),
            "a generic one gives way to the brief"
        );
        s.title = "ComfyUI setup for Intel Arc Pro B70".into();
        assert_eq!(
            successor_name(
                &s,
                brief,
                Some("ComfyUI setup for Intel Arc Pro B70"),
                &[],
                false
            )
            .0,
            "Northwind image packs",
            "so does Claude's stale summary"
        );
        let line = vec![
            Lineage {
                old_id: "a".into(),
                new_id: "b".into(),
                title: "Claude-Northwind".into(),
                ..Default::default()
            },
            Lineage {
                old_id: "b".into(),
                new_id: "new".into(),
                title: "Handover from b".into(),
                derived: true,
                ..Default::default()
            },
        ];
        s.title = "Handover from b".into();
        assert_eq!(
            successor_name(&s, "# Handover Brief", None, &line, false).0,
            "Claude-Northwind",
            "else the last real name in its line"
        );
        assert_eq!(
            successor_name(&s, "", None, &[], false).0,
            "northwind-packs",
            "else its folder"
        );
        // A name toomux chose last time is chosen again from the new brief.
        let line = vec![Lineage {
            old_id: "a".into(),
            new_id: "new".into(),
            title: "Northwind image packs".into(),
            derived: true,
            ..Default::default()
        }];
        s.title = "Northwind image packs".into();
        assert_eq!(
            successor_name(
                &s,
                "# Handover — Premium hand-drawn icons in ComfyUI",
                None,
                &line,
                false
            )
            .0,
            "Premium hand-drawn icons in ComfyUI"
        );
    }

    #[test]
    fn your_names_stay_and_chosen_ones_follow_the_work() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.join("state")) };
        let mut s = session(1, "icons", 0, false);
        s.title = "Northwind hand-drawn icon set".into();
        let brief = "# Handover: Explainer refit + Cool-B65 coil";
        assert_eq!(
            successor_name(&s, brief, None, &[], false).0,
            "Northwind hand-drawn icon set",
            "yours stays"
        );
        assert_eq!(
            successor_name(&s, brief, None, &[], true),
            ("Explainer refit + Cool-B65 coil".into(), true),
            "one chosen for you follows the work"
        );
        // Who chose it is recorded with the rename.
        crate::actions::rename(&cfg(0), &s, "Northwind hand-drawn icon set", true).unwrap();
        assert!(crate::state::State::load().chosen.contains("icons"));
        crate::actions::rename(&cfg(0), &s, "Icons", false).unwrap();
        let st = crate::state::State::load();
        assert!(
            !st.chosen.contains("icons") && st.names["icons"] == "Icons",
            "renaming it yourself makes it yours"
        );
        // The topic is the latest brief's heading, whole.
        write_atomic(
            &brief_path("old", None),
            "# Handover: Northwind image packs, cut-outs, icons and explainers (session old)\n\nbody",
        );
        remember(Lineage {
            old_id: "old".into(),
            new_id: "icons".into(),
            carried: true,
            ..Default::default()
        });
        let t = topics(&["icons", "other"]);
        assert_eq!(
            t.get("icons").map(String::as_str),
            Some("Northwind image packs, cut-outs, icons and explainers")
        );
        assert!(!t.contains_key("other"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_marker_counts_only_while_its_owner_lives() {
        let now = registry::now_ms();
        let m = |owner: i32, age: i64| Marker {
            pid: 1,
            id: "x".into(),
            pane: String::new(),
            state: "writing".into(),
            at_ms: now - age,
            tokens: 0,
            retry_at_ms: 0,
            why: String::new(),
            owner,
        };
        assert!(live(&m(std::process::id() as i32, 1_000), now));
        assert!(!live(&m(i32::MAX - 1, 1_000), now), "its process is gone");
        assert!(live(&m(0, 30_000), now), "just claimed");
        assert!(!live(&m(0, 120_000), now), "claimed and never started");
        assert!(
            !live(&m(std::process::id() as i32, 41 * 60_000), now),
            "past every timeout"
        );
    }

    #[test]
    fn a_subagent_is_done_when_its_last_word_calls_no_tool() {
        let tmp = std::env::temp_dir().join(format!("toomux-fin-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let p = tmp.join("a.jsonl");
        let tool = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Bash"}]}});
        let result = json!({"type": "user", "message": {"content": [{"type": "tool_result"}]}});
        let answer = json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "done"}]}});
        std::fs::write(&p, format!("{tool}\n")).unwrap();
        assert!(!finished(&p), "waiting on a tool");
        std::fs::write(&p, format!("{tool}\n{result}\n")).unwrap();
        assert!(!finished(&p), "thinking about the result");
        std::fs::write(
            &p,
            format!("{tool}\n{result}\n{answer}\n{{\"type\":\"system\"}}\n"),
        )
        .unwrap();
        assert!(finished(&p));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn what_is_carried_is_listed_once() {
        let handed = vec![(
            "Explore".to_string(),
            "find it".to_string(),
            PathBuf::from("/b/a1.md"),
        )];
        let job = crate::jobs::Job {
            id: "j1".into(),
            command: "make\nmore".into(),
            started_ms: registry::now_ms(),
            ..Default::default()
        };
        let once = with_carried("# brief", &handed, std::slice::from_ref(&job));
        assert!(
            once.contains("- Explore: find it · brief /b/a1.md")
                && once.contains("- job j1 ")
                && once.contains("`make`")
        );
        let twice = with_carried(&once, &handed, &[job]);
        assert_eq!(twice.matches(CARRIED.trim()).count(), 1);
        assert_eq!(
            with_carried(&once, &[], &[]),
            "# brief",
            "nothing left to carry: the list goes"
        );
    }

    #[test]
    fn unsent_prompt_drafts_survive_retries_without_duplication() {
        let carried = format!("{CARRIED}\n\n- job j1 still running\n");
        let once = with_unsent_draft(&format!("# brief{carried}"), "'d v9");
        assert_eq!(once.matches(UNSENT_DRAFT.trim()).count(), 1);
        assert_eq!(once.matches("    'd v9\n").count(), 1);
        assert!(
            once.find(UNSENT_DRAFT).unwrap() < once.find(CARRIED).unwrap(),
            "the draft must live before generated carried-over text"
        );

        let twice = with_unsent_draft(&once, "'d v9");
        assert_eq!(twice, once, "the same retry must not duplicate the draft");

        let later = with_unsent_draft(&twice, "please keep this too");
        assert_eq!(later.matches(UNSENT_DRAFT.trim()).count(), 1);
        assert!(later.contains("    'd v9\n"));
        assert!(later.contains("    please keep this too\n"));
        assert_eq!(later.matches("Another unsent draft").count(), 1);

        let rebuilt = with_carried(&later, &[], &[]);
        assert!(
            rebuilt.contains("    'd v9\n") && rebuilt.contains("    please keep this too\n"),
            "regenerating carried-over work must not discard preserved input"
        );
    }

    fn session(pid: i32, id: &str, since_ago: i64, pane: bool) -> Session {
        let now = registry::now_ms();
        Session {
            pid,
            proc_start: None,
            id: id.into(),
            cwd: "/nonexistent".into(),
            name: "s".into(),
            title: "s".into(),
            topic: None,
            pr: None,
            queued: None,
            pin: None,
            dormant: false,
            restore: None,
            state: State::Background,
            waiting_for: None,
            limit: None,
            handover: None,
            since_ms: now - since_ago,
            started_ms: now - 3_600_000,
            account: Some(0),
            config_dir: None,
            args: vec![],
            env: vec![],
            tty: None,
            pane: pane.then(|| crate::tmux::Pane {
                id: format!("%{pid}"),
                session: "t".into(),
                window_index: "1".into(),
            }),
        }
    }

    #[test]
    fn who_is_due_to_hand_over() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-due-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.join("state")) };
        let now = registry::now_ms();
        let c = cfg(400_000);
        // Asked by the gate. The same conversation is also open in a stale
        // process in a pane (idle since yesterday) and a live one outside tmux.
        touch(&requested_path("r", None), "620000");
        let stale = session(1, "r", 86_400_000, true);
        let live = session(2, "r", 10_000, false);
        assert!(
            due(&c, &[stale.clone(), live.clone()], now).is_empty(),
            "outside tmux, nothing goes before the brief is written"
        );
        std::fs::write(brief_path("r", None), "x".repeat(400)).unwrap();
        assert_eq!(
            due(&c, &[stale.clone(), live.clone()], now),
            vec![2],
            "only the live copy hands over, once its brief is written"
        );
        remember(Lineage {
            old_id: "r".into(),
            pane: "%9".into(),
            at_ms: now,
            name: None,
            carried: false,
            ..Default::default()
        });
        assert!(
            due(&c, &[stale.clone()], now).is_empty(),
            "once handed over, the old copy left behind stays put"
        );
        // A handover that failed on a typed prompt is tried again a minute later...
        let failed = |at_ms: i64, retry_at_ms: i64| {
            write_marker(&Marker {
                pid: 3,
                id: "t".into(),
                pane: "%3".into(),
                state: "failed".into(),
                at_ms,
                tokens: 1,
                retry_at_ms,
                why: String::new(),
                owner: 0,
            })
        };
        touch(&requested_path("t", None), "500000");
        let t = session(3, "t", 10_000, true);
        failed(now - 1_000, now + 59_000);
        assert!(due(&c, &[t.clone()], now).is_empty());
        assert_eq!(due(&c, &[t.clone()], now + 60_000), vec![3]);
        // ...and at once if its brief has been written since.
        std::fs::write(brief_path("t", None), "x".repeat(400)).unwrap();
        failed(now - 60_000, now + 30 * 60_000);
        assert_eq!(due(&c, &[t], now), vec![3]);
        // A failure holds the conversation, not just the process: relaunched
        // under a new pid, it doesn't try again at once.
        touch(&requested_path("u", None), "500000");
        write_marker(&Marker {
            pid: 5,
            id: "u".into(),
            pane: "%5".into(),
            state: "failed".into(),
            at_ms: now - 1_000,
            tokens: 1,
            retry_at_ms: now + 30 * 60_000,
            why: String::new(),
            owner: 0,
        });
        assert!(due(&c, &[session(6, "u", 10_000, true)], now).is_empty());
        // And a handover under way for it in another process blocks it too.
        write_marker(&Marker {
            pid: 7,
            id: "v".into(),
            pane: "%7".into(),
            state: "writing".into(),
            at_ms: now,
            tokens: 1,
            retry_at_ms: 0,
            why: String::new(),
            owner: std::process::id() as i32,
        });
        touch(&requested_path("v", None), "500000");
        assert!(due(&c, &[session(8, "v", 10_000, true)], now).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
    #[test]
    fn a_big_conversation_briefs_a_fresh_subagent_instead_of_forking() {
        let tmp = std::env::temp_dir().join(format!("toomux-gate-steer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let c = cfg(400_000);
        let ask = |t: &Path, kind: &str| {
            gate(
                &c,
                &json!({"session_id": "s1", "transcript_path": t.display().to_string(), "tool_name": "Agent", "tool_input": {"subagent_type": kind, "prompt": "go"}}),
            )
        };
        let small = transcript(&tmp, "small.jsonl", 150_000);
        assert!(
            ask(&small, "fork").is_none(),
            "below the limit a fork is worth its context"
        );
        let big = transcript(&tmp, "big.jsonl", 230_000);
        let no = ask(&big, "fork").expect("past the limit a fork is refused");
        assert!(
            no.contains("\"deny\"") && no.contains("230k") && no.contains("general-purpose"),
            "{no}"
        );
        assert!(
            ask(&big, "general-purpose").is_none(),
            "a fresh subagent goes ahead"
        );
        let mut off = cfg(400_000);
        off.fork_context_tokens = 0;
        assert!(gate(&off, &json!({"session_id": "s1", "transcript_path": big.display().to_string(), "tool_name": "Agent", "tool_input": {"subagent_type": "fork"}})).is_none());
        let _ = std::fs::remove_dir_all(tmp);
    }
}
