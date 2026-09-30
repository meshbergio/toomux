//! Every session's conversations into memory, as they happen: each exchange
//! becomes one entry, "what you asked" and "what came of it" (the answer that
//! closed the turn), scoped to its project and labelled with its session. So
//! any later session, including one that took over after a handover, can find
//! what was decided. Subagents' tasks and results go in the same way, under
//! their session.
//!
//! Incremental: transcripts are read from where the last pass stopped. A turn
//! ends where Claude Code marks it ended (or after a quiet spell); if more is
//! said in it afterwards (a background task finishing, a Stop hook waking the
//! session) the entry is brought up to date rather than duplicated.
//!
//! Claude Code's own memory files (`<project>/memory/*.md`) go in too, one
//! entry each, under the project they belong to: an edited file replaces its
//! entry and a removed one takes it along. A session loads only its own
//! project's index; this makes every project's notes searchable from any.

use crate::config::Config;
use crate::memory::{self, Memory};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Transcripts older than this are left alone on the first pass.
const BACKFILL_DAYS: u64 = 30;
/// How much transcript one pass reads, so a backfill spreads over passes.
const PASS_BYTES: u64 = 256 << 20;
/// A turn with no end marker is closed once its transcript has been quiet
/// this long.
const QUIET_SECS: u64 = 300;
const PROMPT_CHARS: usize = 1500;
const ANSWER_CHARS: usize = 6000;
/// A memory file past this is kept to its start (none are near it).
const NOTE_CHARS: usize = 256_000;

#[derive(Default, Serialize, Deserialize, Clone)]
struct Turn {
    prompt: String,
    answer: String,
    cwd: String,
    at_ms: i64,
    /// Its number in the conversation once stored (0 before).
    #[serde(default)]
    n: u64,
    /// Every message of the turn, when reading one back whole.
    #[serde(skip)]
    log: Vec<String>,
}

#[derive(Default, Serialize, Deserialize, Clone)]
struct Progress {
    offset: u64,
    turns: u64,
    open: Option<Turn>,
    /// The turn last stored, which later words can still add to.
    #[serde(default)]
    last: Option<Turn>,
    /// Memory searches waiting for their results (for the recall graph).
    #[serde(default)]
    pending: Vec<String>,
    /// Its names in Claude Code: one given with /rename (or by toomux), and
    /// Claude's own summary. Latest of each.
    #[serde(default)]
    custom: Option<String>,
    #[serde(default)]
    ai: Option<String>,
    /// Its names are known: read from the start, or looked up once.
    #[serde(default)]
    titled: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Book {
    files: HashMap<String, Progress>,
    /// Memory files by path, at the modified time they were last read.
    #[serde(default)]
    notes: HashMap<String, i64>,
}

/// One transcript being read: whose it is and how its entries are labelled.
struct Source<'a> {
    /// `session:<id>` or `session:<id>/agent-<agent>`.
    label: String,
    /// For a subagent: "an Explore subagent (find the flag)".
    who: Option<String>,
    mem: &'a Memory,
    /// Reading a turn back whole: keep every message, clip nothing.
    full: bool,
}

fn book_path() -> PathBuf {
    crate::paths::state().join("index.json")
}

fn load_book() -> Book {
    std::fs::read_to_string(book_path()).ok().and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default()
}

fn save_book(book: &Book) -> Result<()> {
    let tmp = book_path().with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string(book)?)?;
    std::fs::rename(tmp, book_path())?;
    Ok(())
}

/// The indexer's lock: `run` and `flush` share the book (and a memory
/// migration resets it). Take it after opening memory, never before.
pub fn lock(wait: std::time::Duration) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    let dir = book_path().parent()?.to_path_buf();
    std::fs::create_dir_all(&dir).ok()?;
    let f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("index.lock")).ok()?;
    let start = std::time::Instant::now();
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Some(f);
        }
        if start.elapsed() >= wait {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Every account's transcript folders, each once.
pub(crate) fn roots(cfg: &Config) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = (0..cfg.accounts.len()).map(|i| cfg.account_dir(i).join("projects")).collect();
    dirs.push(crate::config::home().join(".claude/projects"));
    for d in dirs {
        let c = crate::config::canon(&d);
        if c.is_dir() && !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// A transcript to read: the conversation's own, or one of its subagents'.
struct File {
    path: PathBuf,
    modified: std::time::SystemTime,
    len: u64,
    session: String,
    agent: Option<String>,
}

/// One pass. Returns how many entries were added or brought up to date.
pub fn run(cfg: &Config) -> Result<usize> {
    // Memory first: opening it may bring it up to date, which resets the book.
    let mem = Memory::open()?;
    // One indexer at a time; a busy one means this pass isn't needed.
    let Some(_lock) = lock(std::time::Duration::ZERO) else { return Ok(0) };
    let mut book = load_book();
    let now = std::time::SystemTime::now();
    let wanted = |path: &Path, md: &std::fs::Metadata, book: &Book| {
        let modified = md.modified().unwrap_or(now);
        let fresh = now.duration_since(modified).map_or(true, |d| d.as_secs() < BACKFILL_DAYS * 86_400);
        let known = book.files.get(&path.display().to_string());
        (fresh || known.is_some()) && !known.is_some_and(|k| k.offset >= md.len() && k.open.is_none())
    };
    let mut files: Vec<File> = Vec::new();
    for root in roots(cfg) {
        let Ok(projects) = std::fs::read_dir(&root) else { continue };
        for p in projects.flatten() {
            let Ok(rd) = std::fs::read_dir(p.path()) else { continue };
            for f in rd.flatten() {
                let path = f.path();
                let Ok(md) = f.metadata() else { continue };
                if md.is_dir() {
                    // <session>/subagents/agent-<id>.jsonl
                    let session = f.file_name().to_string_lossy().into_owned();
                    for a in std::fs::read_dir(path.join("subagents")).into_iter().flatten().flatten() {
                        let ap = a.path();
                        let Some(agent) = ap.file_stem().and_then(|s| s.to_str()).and_then(|s| s.strip_prefix("agent-")).map(str::to_string) else { continue };
                        let Ok(amd) = a.metadata() else { continue };
                        if ap.extension().is_some_and(|x| x == "jsonl") && wanted(&ap, &amd, &book) {
                            files.push(File { modified: amd.modified().unwrap_or(now), len: amd.len(), path: ap, session: session.clone(), agent: Some(agent) });
                        }
                    }
                    continue;
                }
                if path.extension().and_then(|x| x.to_str()) != Some("jsonl") || !wanted(&path, &md, &book) {
                    continue;
                }
                let session = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
                files.push(File { modified: md.modified().unwrap_or(now), len: md.len(), path, session, agent: None });
            }
        }
    }
    // Newest first, so today's work is searchable before the backfill ends.
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    let mut budget = PASS_BYTES;
    let mut added = 0;
    for f in files {
        if budget == 0 {
            break;
        }
        let key = f.path.display().to_string();
        let mut prog = book.files.get(&key).cloned().unwrap_or_default();
        let quiet = now.duration_since(f.modified).is_ok_and(|d| d.as_secs() >= QUIET_SECS);
        let src = source(&f, &mem);
        let (read, n) = mem.batch(|_| {
            let (read, mut n) = read_from(&f.path, prog.offset, budget.min(f.len.saturating_sub(prog.offset)), &mut prog, &src)?;
            if quiet && prog.offset >= f.len {
                n += close(&src, &mut prog)?;
            }
            Ok((read, n))
        })?;
        added += n;
        budget = budget.saturating_sub(read);
        book.files.insert(key, prog);
    }
    added += mem.batch(|m| memory_files(&roots(cfg), &mut book, m))?;
    // Conversations read before their names were kept: Claude Code writes
    // them again every few turns, so the end of each has them.
    let untitled: Vec<String> = book.files.iter().filter(|(k, p)| !p.titled && session_of(k).is_some()).map(|(k, _)| k.clone()).take(TITLES_PER_PASS).collect();
    for k in untitled {
        let m = crate::transcript::meta(Path::new(&k));
        if let Some(p) = book.files.get_mut(&k) {
            (p.custom, p.ai, p.titled) = (m.custom, m.title, true);
        }
    }
    save_book(&book)?;
    Ok(added)
}

const TITLES_PER_PASS: usize = 400;

/// The session a main transcript belongs to (not a subagent's).
fn session_of(path: &str) -> Option<&str> {
    let stem = path.strip_suffix(".jsonl")?.rsplit('/').next()?;
    (stem.len() == 36 && !path.contains("/subagents/")).then_some(stem)
}

/// Every conversation's names in Claude Code, by session id: (the one given
/// it, Claude's own summary).
pub fn titles() -> HashMap<String, (Option<String>, Option<String>)> {
    load_book()
        .files
        .into_iter()
        .filter_map(|(k, p)| Some((session_of(&k)?.to_string(), (p.custom, p.ai))))
        .filter(|(_, t)| t.0.is_some() || t.1.is_some())
        .collect()
}

/// Claude Code's memory files into memory, those new or changed since the
/// last pass; those gone are forgotten. Returns how many entries changed.
fn memory_files(roots: &[PathBuf], book: &mut Book, mem: &Memory) -> Result<usize> {
    let mut seen = std::collections::HashSet::new();
    let mut changed = 0;
    for root in roots {
        for project in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let Ok(rd) = std::fs::read_dir(project.path().join("memory")) else { continue };
            let mut scope: Option<String> = None;
            for f in rd.flatten() {
                let path = f.path();
                let Ok(md) = std::fs::metadata(&path) else { continue };
                if path.extension().is_none_or(|x| x != "md") || !md.is_file() {
                    continue;
                }
                let key = path.display().to_string();
                let at = md
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as i64);
                seen.insert(key.clone());
                if book.notes.get(&key) == Some(&at) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let scope = scope.get_or_insert_with(|| workspace_scope(&project.path()));
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let content = format!("{name}\n\n{}", clip(&text, NOTE_CHARS));
                if mem.put(scope, &format!("memory: {key}"), &content, at)? {
                    changed += 1;
                }
                book.notes.insert(key, at);
            }
        }
    }
    let gone: Vec<String> = book.notes.keys().filter(|k| !seen.contains(*k)).cloned().collect();
    for k in gone {
        changed += mem.delete_source(&format!("memory: {k}"))?;
        book.notes.remove(&k);
    }
    Ok(changed)
}

/// The scope for a transcript folder: the folder its conversations ran in,
/// as they recorded it (the folder's name is that path with every `/` and
/// `.` made `-`, which can't be read back for certain).
fn workspace_scope(dir: &Path) -> String {
    match recorded_cwd(dir) {
        Some(cwd) => memory::project_scope(&cwd),
        None => format!("project:{}", dir.file_name().map(|n| n.to_string_lossy().replace('-', "/")).unwrap_or_default()),
    }
}

/// The folder a transcript folder's conversations ran in, as the first of
/// them to say recorded it.
pub fn recorded_cwd(dir: &Path) -> Option<String> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().find_map(|f| {
        let p = f.path();
        if p.extension().is_none_or(|x| x != "jsonl") {
            return None;
        }
        let mut head = String::new();
        std::fs::File::open(&p).ok()?.take(256 << 10).read_to_string(&mut head).ok();
        head.lines().find_map(|l| serde_json::from_str::<Value>(l).ok()?.get("cwd")?.as_str().map(str::to_string))
    })
}

fn source<'a>(f: &File, mem: &'a Memory) -> Source<'a> {
    match &f.agent {
        None => Source { label: format!("session:{}", f.session), who: None, mem, full: false },
        Some(a) => {
            let meta: Value = std::fs::read_to_string(f.path.with_file_name(format!("agent-{a}.meta.json")))
                .ok()
                .and_then(|r| serde_json::from_str(&r).ok())
                .unwrap_or(Value::Null);
            let kind = meta.get("agentType").and_then(Value::as_str).unwrap_or("general-purpose");
            let who = match meta.get("description").and_then(Value::as_str) {
                Some(d) => format!("A {kind} subagent ({d})"),
                None => format!("A {kind} subagent"),
            };
            Source { label: format!("session:{}/agent-{a}", f.session), who: Some(who), mem, full: false }
        }
    }
}

/// Close every open turn of one conversation now (before a handover), so
/// the fresh session can find all of it. Waits a little for a running pass.
pub fn flush(path: &Path) -> Result<()> {
    let mem = Memory::open()?;
    let Some(_lock) = lock(std::time::Duration::from_secs(20)) else { anyhow::bail!("the indexer is busy") };
    let mut book = load_book();
    let key = path.display().to_string();
    let mut prog = book.files.get(&key).cloned().unwrap_or_default();
    let session = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let f = File { path: path.to_path_buf(), modified: std::time::SystemTime::now(), len, session, agent: None };
    let src = source(&f, &mem);
    mem.batch(|_| {
        read_from(path, prog.offset, len.saturating_sub(prog.offset), &mut prog, &src)?;
        close(&src, &mut prog)
    })?;
    book.files.insert(key, prog);
    save_book(&book)
}

fn flag(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool) == Some(true)
}

/// Read up to `max` bytes of whole lines from `offset`, turning them into turns.
fn read_from(path: &Path, offset: u64, max: u64, prog: &mut Progress, src: &Source) -> Result<(u64, usize)> {
    if max == 0 {
        return Ok((0, 0));
    }
    if offset == 0 {
        prog.titled = true;
    }
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    // Only whole lines: the writer may be mid-line.
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let added = feed(&String::from_utf8_lossy(&buf[..end]), prog, src)?;
    prog.offset = offset + end as u64;
    Ok((end as u64, added))
}

/// Turn transcript lines into turns.
fn feed(text: &str, prog: &mut Progress, src: &Source) -> Result<usize> {
    let mut added = 0;
    for line in text.lines() {
        // Which entries the session reached for, before results are skipped.
        if !src.full {
            let found = crate::graph::recalls_in(line, &mut prog.pending);
            if !found.is_empty() {
                let at = crate::graph::line_time(line);
                let who = src.label.split('/').next().unwrap_or(&src.label);
                for (target, how) in found {
                    src.mem.recall(who, &target, how, at)?;
                }
            }
        }
        if line.contains("-title\"") && let Ok(v) = serde_json::from_str::<Value>(line) {
            let at = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
            match v.get("type").and_then(Value::as_str) {
                Some("custom-title") => prog.custom = at("customTitle").or(prog.custom.take()),
                Some("ai-title") => prog.ai = at("aiTitle").or(prog.ai.take()),
                _ => {}
            }
            continue;
        }
        // Tool results are most of a transcript and never a prompt or an
        // answer: skip them before parsing (unless reading a turn whole).
        if !src.full && line.contains("\"type\":\"tool_result\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        // A subagent's own transcript is all sidechain; in a session's, it's
        // someone else's.
        if (src.who.is_none() && flag(&v, "isSidechain"))
            || flag(&v, "isMeta")
            || flag(&v, "isCompactSummary")
            || flag(&v, "isVisibleInTranscriptOnly")
        {
            continue;
        }
        let at_ms = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp_millis())
            .unwrap_or(0);
        let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                let Some(prompt) = prompt_of(&v) else {
                    if src.full
                        && let Some(t) = prog.open.as_mut() {
                            t.log.extend(results_of(&v));
                        }
                    continue;
                };
                added += close(src, prog)?;
                prog.last = None;
                // toomux's own handover request closes the turn before it but
                // isn't a conversation worth remembering; its brief is indexed.
                if !prompt.contains("[toomux handover]") {
                    prog.open = Some(Turn { prompt, answer: String::new(), cwd, at_ms, n: 0, log: Vec::new() });
                }
            }
            Some("assistant") => {
                // API errors and usage-limit notices aren't answers.
                if flag(&v, "isApiErrorMessage") || v.get("error").is_some() {
                    continue;
                }
                if src.full
                    && let Some(t) = prog.open.as_mut().or(prog.last.as_mut()) {
                        t.log.extend(calls_of(v.pointer("/message/content")));
                    }
                let Some(text) = text_of(v.pointer("/message/content")) else { continue };
                // The turn's last words are its outcome.
                if let Some(t) = prog.open.as_mut() {
                    t.answer = text;
                } else if let Some(mut t) = prog.last.take() {
                    // More said after the turn ended: bring its entry up to date.
                    t.answer = text;
                    prog.open = Some(t);
                }
            }
            Some("system") => {
                if matches!(v.get("subtype").and_then(Value::as_str), Some("turn_duration" | "stop_hook_summary")) {
                    added += close(src, prog)?;
                }
            }
            _ => {}
        }
    }
    Ok(added)
}

/// Store the open turn, if it has an outcome, and keep it as the last one.
fn close(src: &Source, prog: &mut Progress) -> Result<usize> {
    let Some(mut t) = prog.open.take() else { return Ok(0) };
    // Nothing came of it, or it ran somewhere throwaway.
    if t.answer.trim().is_empty() || t.cwd.is_empty() || t.cwd.starts_with("/tmp/") {
        return Ok(0);
    }
    if t.n == 0 {
        prog.turns += 1;
        t.n = prog.turns;
    }
    let content = if src.full {
        format!("You asked: {}\n\n{}", t.prompt, t.log.join("\n\n"))
    } else {
        match &src.who {
            None => format!("You asked: {}\n\nOutcome: {}", clip(&t.prompt, PROMPT_CHARS), clip(&t.answer, ANSWER_CHARS)),
            Some(who) => format!("{who} was asked: {}\n\nOutcome: {}", clip(&t.prompt, PROMPT_CHARS), clip(&t.answer, ANSWER_CHARS)),
        }
    };
    let changed = src.mem.put(&memory::project_scope(&t.cwd), &format!("{}#{}", src.label, t.n), &content, t.at_ms)?;
    prog.last = Some(t);
    Ok(usize::from(changed))
}

/// An assistant message whole: its words, and each tool call with its input.
fn calls_of(content: Option<&Value>) -> Vec<String> {
    let Some(items) = content.and_then(Value::as_array) else { return Vec::new() };
    items
        .iter()
        .filter_map(|i| match i.get("type").and_then(Value::as_str) {
            Some("text") => i.get("text").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).map(str::to_string),
            Some("tool_use") => Some(format!(
                "→ {} {}",
                i.get("name").and_then(Value::as_str).unwrap_or("tool"),
                i.get("input").map(Value::to_string).unwrap_or_default()
            )),
            _ => None,
        })
        .collect()
}

/// What each tool call in a user message returned.
fn results_of(v: &Value) -> Vec<String> {
    let Some(items) = v.pointer("/message/content").and_then(Value::as_array) else { return Vec::new() };
    items
        .iter()
        .filter(|i| i.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|i| {
            let body = match i.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .map(|p| p.get("text").and_then(Value::as_str).map_or_else(|| "[image]".to_string(), str::to_string))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            format!("← {body}")
        })
        .collect()
}

/// One turn read back whole (every message and tool call, nothing clipped),
/// by its memory source (`session:<id>#<n>` or `session:<id>/agent-<a>#<n>`),
/// from the transcript or, once Claude Code has deleted it, the archive.
pub fn whole_turn(cfg: &Config, source: &str) -> Result<String> {
    let rest = source.strip_prefix("session:").ok_or_else(|| anyhow::anyhow!("{source} isn't a conversation turn"))?;
    let (who, n) = rest.rsplit_once('#').ok_or_else(|| anyhow::anyhow!("{source} has no turn number"))?;
    let (session, agent) = match who.split_once("/agent-") {
        Some((s, a)) => (s, Some(a)),
        None => (who, None),
    };
    let text = crate::archive::transcript(cfg, session, agent)
        .ok_or_else(|| anyhow::anyhow!("no transcript for {session}, here or in the archive"))?;
    let mem = Memory::in_memory()?;
    let src = Source { label: format!("session:{who}"), who: agent.map(|_| String::new()), mem: &mem, full: true };
    let mut prog = Progress::default();
    feed(&text, &mut prog, &src)?;
    close(&src, &mut prog)?;
    mem.by_source(source)?.ok_or_else(|| anyhow::anyhow!("turn {n} isn't in that transcript"))
}

/// A prompt you typed (not a tool result, command plumbing or a notification).
fn prompt_of(v: &Value) -> Option<String> {
    let text = match v.pointer("/message/content")? {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            if items.iter().any(|i| i.get("type").and_then(Value::as_str) == Some("tool_result")) {
                return None;
            }
            items.iter().filter_map(|i| i.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
        }
        _ => return None,
    };
    let t = text.trim();
    // Claude Code's own plumbing arrives as user messages too; pasted text is
    // wrapped in a tag but is a real prompt.
    const PLUMBING: &[&str] = &[
        "<command-", "<local-command", "<task-notification", "<system-reminder", "<bash-", "<user-memory-input", "<teammate-message",
    ];
    if t.is_empty() || t.starts_with("Caveat:") || t.starts_with("[Request interrupted") || PLUMBING.iter().any(|p| t.starts_with(p)) {
        return None;
    }
    let unwrapped = t.strip_prefix('<').and_then(|r| r.split_once('>')).filter(|(tag, _)| tag.starts_with("pasted_content"));
    let t = match unwrapped {
        Some((_, rest)) => rest.rsplit_once("</pasted_content").map_or(rest, |(a, _)| a).trim(),
        None => t,
    };
    (!t.is_empty()).then(|| t.to_string())
}

fn text_of(content: Option<&Value>) -> Option<String> {
    let items = content?.as_array()?;
    let text: Vec<&str> = items
        .iter()
        .filter(|i| i.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|i| i.get("text").and_then(Value::as_str))
        .collect();
    let t = text.join("\n");
    (!t.trim().is_empty()).then_some(t)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src<'a>(mem: &'a Memory, who: Option<&str>) -> Source<'a> {
        Source { label: "session:abc".into(), who: who.map(str::to_string), mem, full: false }
    }

    fn feed(path: &Path, lines: &[&str], prog: &mut Progress, src: &Source) -> usize {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        std::io::Write::write_all(&mut f, (lines.join("\n") + "\n").as_bytes()).unwrap();
        let len = std::fs::metadata(path).unwrap().len();
        read_from(path, prog.offset, len - prog.offset, prog, src).unwrap().1
    }

    #[test]
    fn a_conversation_keeps_its_latest_names() {
        let dir = std::env::temp_dir().join(format!("toomux-index-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.jsonl");
        let _ = std::fs::remove_file(&path);
        let mem = Memory::in_memory().unwrap();
        let mut prog = Progress::default();
        let lines = [
            r#"{"type":"ai-title","aiTitle":"Deploy failing"}"#,
            r#"{"type":"custom-title","customTitle":"Release"}"#,
            r#"{"type":"ai-title","aiTitle":"Stale image tag"}"#,
            r#"{"type":"ai-title","aiTitle":" "}"#,
        ];
        feed(&path, &lines, &mut prog, &src(&mem, None));
        assert_eq!((prog.custom.as_deref(), prog.ai.as_deref(), prog.titled), (Some("Release"), Some("Stale image tag"), true));
        assert_eq!(session_of("/c/projects/-w/0ff6405c-c71f-4a6a-8d59-de4d9a0cca4c.jsonl"), Some("0ff6405c-c71f-4a6a-8d59-de4d9a0cca4c"));
        assert_eq!(session_of("/c/projects/-w/0ff6405c-c71f-4a6a-8d59-de4d9a0cca4c/subagents/agent-a0123456789abcdef0123.jsonl"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn turns_become_memories() {
        let dir = std::env::temp_dir().join(format!("toomux-index-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.jsonl");
        let _ = std::fs::remove_file(&path);
        let lines = [
            r#"{"type":"user","cwd":"/p","timestamp":"2026-09-29T10:00:00Z","message":{"content":"why is the deploy failing?"}}"#,
            r#"{"type":"assistant","cwd":"/p","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
            r#"{"type":"user","cwd":"/p","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"x"}]}}"#,
            r#"{"type":"assistant","cwd":"/p","message":{"content":[{"type":"text","text":"The image tag was stale; bumped it to 1.4.2."}]}}"#,
            r#"{"type":"user","cwd":"/p","message":{"content":"<command-name>/clear</command-name>"}}"#,
            r#"{"type":"user","cwd":"/p","timestamp":"2026-09-29T10:05:00Z","message":{"content":"<pasted_content id=\"1\">\nship it\n</pasted_content id=\"1\">"}}"#,
        ];
        let mem = Memory::in_memory().unwrap();
        let mut prog = Progress::default();
        let n = feed(&path, &lines, &mut prog, &src(&mem, None));
        assert_eq!(n, 1, "the first exchange closed when the next prompt came");
        assert_eq!(prog.open.as_ref().unwrap().prompt, "ship it", "the second is still open");
        let hits = mem.search(&[], None, "deploy image tag", 5).unwrap();
        assert_eq!(hits[0].source, "session:abc#1");
        let e = mem.get(&hits[0].id).unwrap().unwrap();
        assert!(e.content.contains("why is the deploy failing") && e.content.contains("bumped it to 1.4.2"));
        assert!(!e.content.contains("Looking."), "the outcome is the turn's last words");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_turn_ends_where_claude_says_and_later_words_update_it() {
        let dir = std::env::temp_dir().join(format!("toomux-index2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.jsonl");
        let _ = std::fs::remove_file(&path);
        let mem = Memory::in_memory().unwrap();
        let s = src(&mem, None);
        let mut prog = Progress::default();
        let n = feed(
            &path,
            &[
                r#"{"type":"user","cwd":"/p","message":{"content":"run the migration"}}"#,
                r#"{"type":"assistant","cwd":"/p","message":{"content":[{"type":"text","text":"Started it in the background; I'll report when it's done."}]}}"#,
                r#"{"type":"system","subtype":"turn_duration"}"#,
            ],
            &mut prog,
            &s,
        );
        assert_eq!(n, 1, "stored as soon as the turn ended");
        // The background task finishes and the session says how it went.
        let n = feed(
            &path,
            &[
                r#"{"type":"user","cwd":"/p","message":{"content":"<task-notification>done</task-notification>"}}"#,
                r#"{"type":"assistant","cwd":"/p","message":{"content":[{"type":"text","text":"The migration finished: 42 tables moved."}]}}"#,
                r#"{"type":"system","subtype":"stop_hook_summary"}"#,
            ],
            &mut prog,
            &s,
        );
        assert_eq!(n, 1);
        let hits = mem.search(&[], None, "migration", 5).unwrap();
        assert_eq!(hits.len(), 1, "one entry for the turn, brought up to date");
        assert!(mem.get(&hits[0].id).unwrap().unwrap().content.contains("42 tables moved"));
        // Noise is not a conversation.
        let n = feed(
            &path,
            &[
                r#"{"type":"user","cwd":"/p","message":{"content":"[Request interrupted by user]"}}"#,
                r#"{"type":"user","cwd":"/p","isCompactSummary":true,"message":{"content":"This session is being continued from a previous conversation"}}"#,
                r#"{"type":"assistant","cwd":"/p","isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"API Error: 500"}]}}"#,
                r#"{"type":"user","cwd":"/tmp/x","message":{"content":"scratch work"}}"#,
                r#"{"type":"assistant","cwd":"/tmp/x","message":{"content":[{"type":"text","text":"scratch answer"}]}}"#,
                r#"{"type":"system","subtype":"turn_duration"}"#,
            ],
            &mut prog,
            &s,
        );
        assert_eq!(n, 0);
        assert!(mem.search(&[], None, "API Error continued scratch", 5).unwrap().is_empty());
        assert_eq!(mem.count().unwrap(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_subagent_is_remembered_under_its_session() {
        let dir = std::env::temp_dir().join(format!("toomux-index3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-a1.jsonl");
        let _ = std::fs::remove_file(&path);
        let mem = Memory::in_memory().unwrap();
        let s = Source { label: "session:abc/agent-a1".into(), who: Some("An Explore subagent (find the flag)".into()), mem: &mem, full: false };
        let mut prog = Progress::default();
        feed(
            &path,
            &[
                r#"{"type":"user","isSidechain":true,"cwd":"/p","message":{"content":"Where is the retry flag set?"}}"#,
                r#"{"type":"assistant","isSidechain":true,"cwd":"/p","message":{"content":[{"type":"text","text":"In config.rs, line 40: retry_limit."}]}}"#,
            ],
            &mut prog,
            &s,
        );
        close(&s, &mut prog).unwrap();
        let hits = mem.search(&[], Some("session:abc"), "retry flag", 5).unwrap();
        assert_eq!(hits[0].source, "session:abc/agent-a1#1");
        assert!(mem.get(&hits[0].id).unwrap().unwrap().content.starts_with("An Explore subagent (find the flag) was asked"));
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn memory_files_are_kept_as_they_are() {
        let root = std::env::temp_dir().join(format!("toomux-index-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("-work-app");
        std::fs::create_dir_all(project.join("memory")).unwrap();
        std::fs::write(project.join("s1.jsonl"), "{\"type\":\"user\",\"cwd\":\"/work/app\"}\n").unwrap();
        let note = project.join("memory/deploys.md");
        std::fs::write(&note, "deploys go to perth through the bastion").unwrap();
        let mem = Memory::in_memory().unwrap();
        let mut book = Book::default();
        let roots = vec![root.clone()];
        assert_eq!(memory_files(&roots, &mut book, &mem).unwrap(), 1);
        let hits = mem.search(&["project:/work/app".into()], None, "bastion", 5).unwrap();
        assert_eq!(hits.len(), 1, "found under the project its conversations ran in");
        assert_eq!(hits[0].source, format!("memory: {}", note.display()));
        assert_eq!(memory_files(&roots, &mut book, &mem).unwrap(), 0, "unchanged, not read again");
        std::fs::write(&note, "deploys go to oregon now").unwrap();
        book.notes.insert(note.display().to_string(), 0);
        assert_eq!(memory_files(&roots, &mut book, &mem).unwrap(), 1);
        assert!(mem.search(&[], None, "bastion", 5).unwrap().is_empty(), "the old text is replaced");
        assert_eq!(mem.count().unwrap(), 1);
        std::fs::remove_file(&note).unwrap();
        assert_eq!(memory_files(&roots, &mut book, &mem).unwrap(), 1);
        assert_eq!(mem.count().unwrap(), 0, "a removed file is forgotten");
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn a_turn_reads_back_whole() {
        let mem = Memory::in_memory().unwrap();
        let lines = [
            r#"{"type":"user","cwd":"/p","message":{"content":"Why does the build fail?"}}"#,
            r#"{"type":"assistant","cwd":"/p","message":{"content":[{"type":"text","text":"Checking the log."},{"type":"tool_use","name":"Bash","input":{"command":"cargo build"}}]}}"#,
            r#"{"type":"user","cwd":"/p","message":{"content":[{"type":"tool_result","content":"error[E0425]: cannot find value `x`"}]}}"#,
            r#"{"type":"assistant","cwd":"/p","message":{"content":[{"type":"text","text":"A missing binding; fixed."}]}}"#,
            r#"{"type":"system","subtype":"turn_duration"}"#,
        ]
        .join("\n");
        let clipped = Source { label: "session:abc".into(), who: None, mem: &mem, full: false };
        super::feed(&lines, &mut Progress::default(), &clipped).unwrap();
        let short = mem.by_source("session:abc#1").unwrap().unwrap();
        assert!(!short.contains("E0425"), "the index keeps the prompt and the outcome: {short}");
        let whole_mem = Memory::in_memory().unwrap();
        let whole = Source { label: "session:abc".into(), who: None, mem: &whole_mem, full: true };
        super::feed(&lines, &mut Progress::default(), &whole).unwrap();
        let all = whole_mem.by_source("session:abc#1").unwrap().unwrap();
        for part in ["Checking the log.", "→ Bash", "cargo build", "← error[E0425]", "A missing binding; fixed."] {
            assert!(all.contains(part), "{part} missing from {all}");
        }
    }
}
