//! Large command output, kept whole but shown short.
//!
//! A PreToolUse hook pipes each Bash command's output through `toomux cap`.
//! Output streams through as it comes; up to `FULL` bytes it arrives exactly as
//! printed. Past that, the whole output is kept on disk and the agent sees its
//! head, the lines that report errors (with a little context), its tail, and
//! where to read the rest (`toomux out <id>`, or the kept file itself). Nothing
//! is lost: every byte stays one command away.
//!
//! File views (cat, sed -n, head, git diff, ...) are never routed: the agent
//! reads those to edit them, and making it ask twice would cost more than it
//! saves. Measured on 72k real calls: capping everything else touches 2% of
//! Bash results and saves ~2.4% of all input tokens.
//!
//! The command runs in Claude's own shell exactly as written (so `cd`,
//! exports, `exit`, `exec` and background jobs behave as they would without
//! toomux); only its stdout and stderr go through a pipe to `toomux cap`.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Streamed live, as whole lines, while the command runs.
const LIVE: usize = 2500;
/// Output up to this many bytes is shown whole.
pub const FULL: usize = 6000;
const TAIL_LINES: usize = 20;
const TAIL_CHARS: usize = 2000;
const ERRORS: usize = 10;
const WARNINGS: usize = 4;
/// Lines shown after an error line (where it happened, the note under it).
const CONTEXT: usize = 2;
const LINE_CHARS: usize = 240;
const KEEP_DAYS: u64 = 7;
/// Claude Code stops a foreground command after 10 minutes at most.
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Room between a spell of waiting and the call's own timeout.
const SPELL_SLACK_MS: u64 = 30_000;
/// What `toomux out` returns at most per call.
const OUT_LINES: usize = 300;
const OUT_MATCHES: usize = 200;
const OUT_CHARS: usize = 20_000;

fn run_dir() -> PathBuf {
    crate::paths::runtime().join("out")
}

/// Where full outputs are kept (survives reboots, pruned after a week).
pub fn store_dir() -> PathBuf {
    crate::paths::state().join("out")
}

/// Outputs can hold anything a command printed: readable by you only.
fn private_dir(p: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
}

fn private_file(p: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(p)
}

// ---- which commands ------------------------------------------------------------

/// A command's segments (split on `;`, `&&`, `||` and newlines, outside
/// quotes), each with leading `(`, env assignments and `sudo`/`time` removed.
fn segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut sq, mut dq) = (false, false);
    let mut it = cmd.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\'' if !dq => sq = !sq,
            '"' if !sq => dq = !dq,
            ';' | '\n' if !sq && !dq => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            '&' | '|' if !sq && !dq && it.peek() == Some(&c) => {
                it.next();
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter()
        .map(|s| {
            let mut words: Vec<&str> = s
                .trim()
                .trim_start_matches(['(', '{', ' '])
                .split_whitespace()
                .collect();
            while let Some(w) = words.first() {
                let assign = w.contains('=')
                    && !w.starts_with('-')
                    && w.split('=').next().is_some_and(|k| {
                        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    });
                if assign || matches!(*w, "sudo" | "time" | "command" | "nice" | "env") {
                    words.remove(0);
                } else {
                    break;
                }
            }
            words.join(" ")
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Commands whose output the agent reads in full: file contents and diffs.
pub fn is_view(cmd: &str) -> bool {
    let segs = segments(cmd);
    // The first thing it does, past any `cd`.
    let Some(first) = segs.iter().find(|s| !s.starts_with("cd ") && *s != "cd") else {
        return false;
    };
    let stage = first.split('|').next().unwrap_or("").trim();
    let words: Vec<&str> = stage.split_whitespace().collect();
    let word = words.first().copied().unwrap_or("");
    let base = word.rsplit('/').next().unwrap_or(word);
    match base {
        "cat" | "head" | "tail" | "nl" | "bat" | "batcat" | "less" | "more" | "diff" | "xxd"
        | "hexdump" | "od" => true,
        "sed" => stage.contains(" -n"),
        "awk" | "gawk" => stage.contains("NR"),
        "jq" => words.len() >= 3 && !words.last().is_some_and(|w| w.starts_with('-')),
        "git" => {
            // git [-C dir] [--no-pager] [-c k=v] (diff|show|log -p)
            let mut rest = words[1..].iter().copied();
            let mut sub = "";
            while let Some(w) = rest.next() {
                match w {
                    "-C" | "-c" => {
                        rest.next();
                    }
                    w if w.starts_with('-') => {}
                    w => {
                        sub = w;
                        break;
                    }
                }
            }
            let tail: Vec<&str> = rest.collect();
            matches!(sub, "diff" | "show")
                || (sub == "log"
                    && tail
                        .iter()
                        .any(|w| *w == "-p" || *w == "--patch" || w.starts_with("-p")))
        }
        _ => false,
    }
}

/// toomux's own commands (and jobs) are left alone.
fn is_ours(cmd: &str) -> bool {
    let first = segments(cmd).into_iter().next().unwrap_or_default();
    let word = first
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(['\'', '"']);
    word.rsplit('/').next() == Some("toomux")
}

/// Whether bash can parse `script` (an unterminated heredoc, say, would
/// swallow the wrapper).
fn parses(script: &str) -> bool {
    std::process::Command::new("bash")
        .args(["-n", "-c", script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// ---- the hook ------------------------------------------------------------------

/// `toomux hook pre-tool`: PreToolUse hook. Prints the rewritten input, or
/// nothing to leave the call as it is.
pub fn pre_tool_hook(cfg: &crate::config::Config) -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().take(1 << 20).read_to_string(&mut raw)?;
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    // First: is this agent past its context limit? Then it hands over.
    if let Some(out) = crate::handover::gate(cfg, &v) {
        print!("{out}");
        return Ok(());
    }
    if !cfg.capture_bash {
        return Ok(());
    }
    if let Some(out) = rewrite(&v) {
        print!("{out}");
    }
    Ok(())
}

fn rewrite(v: &Value) -> Option<String> {
    if v.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let input = v.get("tool_input")?;
    let cmd = input.get("command").and_then(Value::as_str)?;
    let background = input.get("run_in_background").and_then(Value::as_bool) == Some(true);
    let subagent = v
        .get("agent_id")
        .and_then(Value::as_str)
        .is_some_and(|a| !a.is_empty());
    // toomux's own commands print what they mean to; a subagent's `job
    // follow --more` is the rest of a command's output, so it's kept like
    // any other.
    if is_ours(cmd) && !(subagent && !background && is_follow_more(cmd)) {
        return None;
    }
    let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("");
    let session = v.get("session_id").and_then(Value::as_str).unwrap_or("");
    let bin = std::env::current_exe().ok()?.display().to_string();
    // Background commands run under toomux, so a handover can carry them.
    if background {
        let id = new_id();
        crate::jobs::create(&id, cmd, cwd, session, 0).ok()?;
        let mut input = input.clone();
        input["command"] = Value::String(format!("{} job run {id}", shell_words::quote(&bin)));
        return Some(
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": input}})
                .to_string(),
        );
    }
    let blank = cmd
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'));
    if blank || is_view(cmd) {
        return None;
    }
    let mut input = input.clone();
    let mut cmd = cmd.to_string();
    // A subagent's cache lasts five minutes: a command it may wait on for
    // longer runs as a job it waits on in shorter spells (see jobs).
    let warm = crate::jobs::keep_warm().as_millis() as u64;
    let timeout = input.get("timeout").and_then(Value::as_u64).unwrap_or(0);
    if subagent && !is_ours(&cmd) && timeout > warm + SPELL_SLACK_MS {
        let id = new_id();
        crate::jobs::create(&id, &cmd, cwd, session, timeout.min(MAX_TIMEOUT_MS) as i64).ok()?;
        cmd = format!(
            "{} job run {id} --until {}",
            shell_words::quote(&bin),
            warm / 1000
        );
        input["timeout"] = json!(warm + SPELL_SLACK_MS);
    }
    let cmd = cmd.as_str();
    let id = new_id();
    let dir = run_dir();
    private_dir(&dir).ok()?;
    // What the capture needs to label it, kept beside the output.
    let meta = dir.join(format!("{id}.json"));
    private_file(&meta)
        .ok()?
        .write_all(
            json!({"command": cmd, "cwd": cwd, "session": session})
                .to_string()
                .as_bytes(),
        )
        .ok()?;
    // Absolute paths, fixed now: the command runs in the same shell, and an
    // `export XDG_RUNTIME_DIR=...` in it must not move anything.
    let cap = format!(
        "{} cap {id} --dir {} --store {}",
        shell_words::quote(&bin),
        shell_words::quote(&dir.display().to_string()),
        shell_words::quote(&store_dir().display().to_string())
    );
    // The command's output goes to a pipe into `cap`, opened on a descriptor
    // of its own that is closed again inside the group, so background jobs
    // the command starts don't hold the pipe open. The blank line ends a
    // trailing backslash; the exit status comes back unchanged.
    let wrapped = format!(
        "exec {{__toomux}}> >(exec {cap}); {{ exec {{__toomux}}>&-; {cmd}\n\n}} >&$__toomux 2>&1; __toomux=$?; (exit $__toomux)"
    );
    if !parses(&wrapped) {
        let _ = std::fs::remove_file(&meta);
        return None;
    }
    input["command"] = Value::String(wrapped);
    Some(
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": input}})
            .to_string(),
    )
}

/// `toomux job follow <id> --more`, alone.
fn is_follow_more(cmd: &str) -> bool {
    let segs = segments(cmd);
    let words: Vec<&str> = segs
        .first()
        .map(|s| s.split_whitespace().collect())
        .unwrap_or_default();
    segs.len() == 1 && words.get(1..3) == Some(&["job", "follow"][..]) && words.contains(&"--more")
}

fn new_id() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = (t ^ (std::process::id() as u128) << 40) % 36u128.pow(7);
    let mut s = String::new();
    let mut x = n;
    for _ in 0..7 {
        let d = (x % 36) as u8;
        s.push(if d < 10 {
            (b'0' + d) as char
        } else {
            (b'a' + d - 10) as char
        });
        x /= 36;
    }
    s
}

// ---- capturing -----------------------------------------------------------------

static STOPPED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_: libc::c_int) {
    STOPPED.store(true, Ordering::SeqCst);
}

/// Stopping the command (a timeout, ctrl-c) signals its whole group, `cap`
/// included: note it and finish the view rather than die mid-way.
fn catch_stops() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_stop as *const () as usize;
        // No SA_RESTART: a blocked read returns so the loop can see it.
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

#[derive(Default)]
struct Mark {
    no: usize,
    text: String,
    context: Vec<String>,
}

/// Reads the output line by line, remembering only what the view needs.
#[derive(Default)]
struct Scan {
    pos: usize,
    chars: usize,
    lines: usize,
    line_start: usize,
    cur: Vec<u8>,
    /// Lines starting at or after this byte are hidden (not in the head).
    hide_from: Option<usize>,
    errors: Vec<Mark>,
    warnings: Vec<Mark>,
    n_errors: usize,
    n_warnings: usize,
    /// Errors still collecting their context lines.
    open: Vec<(usize, usize)>,
    tail: VecDeque<(usize, String)>,
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… [line continues, {} chars]", s.chars().count())
}

/// A line as a terminal would leave it: only what follows the last carriage
/// return (progress bars), colour codes removed.
fn settle(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim_end_matches('\r');
    let s = s.rsplit('\r').next().unwrap_or(s);
    strip_ansi(s)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn is_error(l: &str) -> bool {
    let l = l.to_lowercase();
    let clean = [
        " 0 failed",
        "0 errors",
        "no errors",
        "0 error(s)",
        "error: 0",
        "errors: 0",
        "failed: 0",
        "0 failures",
    ];
    if clean.iter().any(|c| l.contains(c)) {
        return false;
    }
    [
        "error",
        "panicked",
        "panic:",
        "fatal",
        "traceback",
        "exception",
        "failed",
        "failure",
        "denied",
        "not found",
        "segmentation fault",
        "abort",
        "cannot ",
        "unable to",
    ]
    .iter()
    .any(|k| l.contains(k))
}

fn is_warning(l: &str) -> bool {
    let l = l.to_lowercase();
    l.contains("warning") || l.contains("warn:") || l.contains("[warn") || l.contains("deprecated")
}

impl Scan {
    fn feed(&mut self, mut bytes: &[u8]) {
        self.chars += bytes.iter().filter(|b| (**b & 0xC0) != 0x80).count();
        while let Some(i) = bytes.iter().position(|b| *b == b'\n') {
            self.take(&bytes[..i]);
            self.end_line();
            self.pos += i + 1;
            self.line_start = self.pos;
            bytes = &bytes[i + 1..];
        }
        self.take(bytes);
        self.pos += bytes.len();
    }

    fn take(&mut self, b: &[u8]) {
        // Only the start of a very long line is needed.
        let room = (LINE_CHARS * 4 + 64).saturating_sub(self.cur.len());
        self.cur.extend_from_slice(&b[..b.len().min(room)]);
    }

    fn end_line(&mut self) {
        self.lines += 1;
        let no = self.lines;
        let text = settle(&self.cur);
        self.cur.clear();
        let hidden = self.hide_from.is_some_and(|h| self.line_start >= h);
        if !hidden {
            return;
        }
        let line = clip(&text, LINE_CHARS);
        // Context for errors above this line.
        let error = is_error(&text);
        if !error {
            for (i, left) in self.open.iter_mut() {
                if *left > 0 {
                    self.errors[*i].context.push(line.clone());
                    *left -= 1;
                }
            }
        }
        self.open.retain(|(_, left)| *left > 0);
        if error {
            self.open.clear();
            self.n_errors += 1;
            if self.errors.len() < ERRORS {
                self.errors.push(Mark {
                    no,
                    text: line.clone(),
                    context: Vec::new(),
                });
                self.open.push((self.errors.len() - 1, CONTEXT));
            }
        } else if is_warning(&text) {
            self.n_warnings += 1;
            if self.warnings.len() < WARNINGS {
                self.warnings.push(Mark {
                    no,
                    text: line.clone(),
                    context: Vec::new(),
                });
            }
        }
        self.tail.push_back((no, line));
        if self.tail.len() > TAIL_LINES {
            self.tail.pop_front();
        }
    }

    fn finish(&mut self) {
        if !self.cur.is_empty() || self.pos > self.line_start {
            self.end_line();
        }
    }
}

/// `toomux cap <id>`: pass the output on, whole or short.
pub fn cap(id: &str, dir: Option<PathBuf>, store: Option<PathBuf>) -> Result<()> {
    catch_stops();
    let dir = dir.unwrap_or_else(run_dir);
    let store = store.unwrap_or_else(store_dir);
    let meta: Value = std::fs::read_to_string(dir.join(format!("{id}.json")))
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or(Value::Null);
    let _ = std::fs::remove_file(dir.join(format!("{id}.json")));

    let mut input = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    // The first FULL+1 bytes; past that, everything goes to the kept file.
    let mut first: Vec<u8> = Vec::with_capacity(FULL + 1);
    let mut streamed = 0usize;
    let mut total = 0usize;
    let mut kept: Option<(PathBuf, std::fs::File)> = None;
    // Couldn't keep it: pass everything through, as if toomux weren't here.
    let mut raw = false;
    let mut scan = Scan::default();
    loop {
        if STOPPED.load(Ordering::SeqCst) {
            break;
        }
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let chunk = &buf[..n];
        total += n;
        if raw {
            if out.write_all(chunk).and_then(|_| out.flush()).is_err() {
                return Ok(());
            }
            continue;
        }
        let take = n.min((FULL + 1).saturating_sub(first.len()));
        first.extend_from_slice(&chunk[..take]);
        // Stream whole lines while within LIVE.
        if streamed < LIVE {
            let upto = first.len().min(LIVE);
            if let Some(nl) = first[streamed..upto].iter().rposition(|b| *b == b'\n') {
                let end = streamed + nl + 1;
                if out
                    .write_all(&first[streamed..end])
                    .and_then(|_| out.flush())
                    .is_err()
                {
                    return Ok(());
                }
                streamed = end;
            }
        }
        // Once past LIVE, the head is fixed: what follows may be hidden.
        if scan.hide_from.is_none() && first.len() >= LIVE {
            scan.hide_from = Some(streamed);
        }
        scan.feed(chunk);
        if total > FULL {
            match &mut kept {
                Some((_, f)) => {
                    let _ = f.write_all(&chunk[take..]);
                }
                None => {
                    let path = store.join(format!("{id}.txt"));
                    let opened = private_dir(&store)
                        .and_then(|_| private_file(&path))
                        .and_then(|mut f| {
                            f.write_all(&first)?;
                            f.write_all(&chunk[take..])?;
                            Ok(f)
                        });
                    match opened {
                        Ok(f) => kept = Some((path, f)),
                        Err(_) => {
                            raw = true;
                            let _ = out.write_all(&first[streamed..]);
                            let _ = out.write_all(&chunk[take..]);
                            let _ = out.flush();
                        }
                    }
                }
            }
        }
    }
    let stopped = STOPPED.load(Ordering::SeqCst);
    if raw {
        return Ok(());
    }
    let Some((path, file)) = kept else {
        // Small: exactly what the command printed.
        let _ = out.write_all(&first[streamed..]);
        if stopped {
            let _ = out.write_all(b"\n[toomux: the command was stopped]\n");
        }
        let _ = out.flush();
        return Ok(());
    };
    drop(file);
    scan.finish();
    // One line longer than LIVE: nothing streamed, so show its start.
    let mut head = String::new();
    if streamed == 0 {
        let cut = String::from_utf8_lossy(&first[..LIVE.min(first.len())])
            .trim_end_matches('\u{fffd}')
            .to_string();
        head = format!("{cut}…\n");
    }
    let shown =
        String::from_utf8_lossy(&first[..streamed]).lines().count() + usize::from(streamed == 0);
    let rest = view(id, &path, &scan, shown, stopped);
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(rest.as_bytes());
    let _ = out.flush();
    drop(out);

    // The short view goes into memory, so later sessions can find it; the
    // whole output stays in the file for a week.
    let command = meta.get("command").and_then(Value::as_str).unwrap_or("");
    let cwd = meta.get("cwd").and_then(Value::as_str).unwrap_or("");
    let session = meta.get("session").and_then(Value::as_str).unwrap_or("");
    if let Ok(m) = crate::memory::Memory::open() {
        let scope = if cwd.is_empty() {
            "global".to_string()
        } else {
            crate::memory::project_scope(cwd)
        };
        let label: String = command
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        let source = format!("output:{id} session:{session} $ {label}");
        let text = crate::redact::redact(&format!(
            "{}{head}{rest}",
            String::from_utf8_lossy(&first[..streamed])
        ));
        let _ = m.index(&scope, &source, &text, crate::registry::now_ms());
    }
    prune(&store, &dir);
    Ok(())
}

/// After the streamed head: what was hidden, the lines that report trouble,
/// the tail, and where the rest is.
fn view(id: &str, path: &Path, scan: &Scan, shown: usize, stopped: bool) -> String {
    let tail_from = {
        // Up to TAIL_LINES lines, within TAIL_CHARS.
        let mut chars = 0;
        let mut from = scan.tail.len();
        for (i, (_, l)) in scan.tail.iter().enumerate().rev() {
            chars += l.chars().count() + 1;
            if chars > TAIL_CHARS && from < scan.tail.len() {
                break;
            }
            from = i;
        }
        from
    };
    let tail: Vec<&(usize, String)> = scan.tail.iter().skip(tail_from).collect();
    let first_tail = tail.first().map(|(n, _)| *n).unwrap_or(usize::MAX);
    let marks: Vec<&Mark> = scan
        .errors
        .iter()
        .chain(scan.warnings.iter())
        .filter(|m| m.no < first_tail)
        .collect();
    let mut marks = marks;
    marks.sort_by_key(|m| m.no);

    let size = if scan.chars >= 10_000 {
        format!("{}k chars", scan.chars / 1000)
    } else {
        format!("{} chars", scan.chars)
    };
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    let mut found = Vec::new();
    if scan.n_errors > 0 {
        let more = if scan.n_errors > ERRORS {
            format!(" (the first {ERRORS} shown)")
        } else {
            String::new()
        };
        found.push(format!(
            "{}{more}",
            plural(
                scan.n_errors,
                "line reporting an error",
                "lines reporting errors"
            )
        ));
    }
    if scan.n_warnings > 0 {
        let more = if scan.n_warnings > WARNINGS {
            format!(" (the first {WARNINGS} shown)")
        } else {
            String::new()
        };
        found.push(format!(
            "{}{more}",
            plural(scan.n_warnings, "warning", "warnings")
        ));
    }
    let mut out = String::new();
    out.push_str(&format!(
        "\n[toomux · {}{} lines, {size} in all. Shown: the first {shown} lines{}, and the last {}. All of it is kept in {}: \
         `toomux out {id} --lines 200-300` for a range, `toomux out {id} --grep 'regex'` to find lines, or read that file \
         directly (rg, sed -n, jq).]\n",
        if stopped { "the command was stopped · " } else { "" },
        scan.lines,
        if found.is_empty() { String::new() } else { format!(", {}", found.join(" and ")) },
        tail.len(),
        path.display()
    ));
    for m in &marks {
        out.push_str(&format!("{:>6}: {}\n", m.no, m.text));
        for (k, c) in m.context.iter().enumerate() {
            if m.no + k + 1 < first_tail {
                out.push_str(&format!("{:>6}  {c}\n", m.no + k + 1));
            }
        }
    }
    if !marks.is_empty() {
        out.push('\n');
    }
    for (_, l) in tail {
        out.push_str(l);
        out.push('\n');
    }
    out
}

// ---- reading kept output ------------------------------------------------------

fn kept_path(id: &str) -> Result<PathBuf> {
    let id = id.trim_start_matches("out:").trim_start_matches("output:");
    if id.is_empty() || id.len() > 16 || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("{id:?} isn't an output id (they look like 4f2kq9z)");
    }
    Ok(store_dir().join(format!("{id}.txt")))
}

/// `toomux out <id> [--lines a-b] [--grep regex] [--chars a-b]`.
pub fn out(
    id: &str,
    lines: Option<&str>,
    grep: Option<&str>,
    chars: Option<&str>,
) -> Result<String> {
    let path = kept_path(id)?;
    let bytes = std::fs::read(&path)
        .with_context(|| format!("no kept output {id} (outputs are kept for {KEEP_DAYS} days)"))?;
    let text = String::from_utf8_lossy(&bytes);
    let range = |s: &str, max: usize| -> (usize, usize) {
        let (a, b) = s.split_once('-').unwrap_or((s, ""));
        let a = a.trim().parse::<usize>().unwrap_or(1).max(1);
        let b = b.trim().parse::<usize>().unwrap_or(max).min(max);
        (a, b.max(a))
    };
    if let Some(c) = chars {
        let all: Vec<char> = text.chars().collect();
        let (a, b) = range(c, all.len());
        let b = b.min(a - 1 + OUT_CHARS);
        let mut s: String = all[(a - 1).min(all.len())..b.min(all.len())]
            .iter()
            .collect();
        if b < all.len() {
            s.push_str(&format!(
                "\n[toomux · chars {a}-{b} of {}; `--chars {}-{}` continues]\n",
                all.len(),
                b + 1,
                (b + OUT_CHARS).min(all.len())
            ));
        }
        return Ok(s);
    }
    let all: Vec<&str> = text.lines().collect();
    let line = |i: usize, l: &str| format!("{:>6}: {}\n", i + 1, clip(&settle(l.as_bytes()), 2000));
    let mut out = String::new();
    if let Some(pat) = grep {
        let re = regex::RegexBuilder::new(pat)
            .case_insensitive(true)
            .build()
            .or_else(|_| {
                regex::RegexBuilder::new(&regex::escape(pat))
                    .case_insensitive(true)
                    .build()
            })?;
        let hits: Vec<usize> = (0..all.len()).filter(|&i| re.is_match(all[i])).collect();
        for &i in hits.iter().take(OUT_MATCHES) {
            out.push_str(&line(i, all[i]));
        }
        if hits.is_empty() {
            out = format!("no line of {id} matches {pat:?}\n");
        } else if hits.len() > OUT_MATCHES {
            out.push_str(&format!(
                "[toomux · {} more matching lines: narrow the pattern, or use --lines]\n",
                hits.len() - OUT_MATCHES
            ));
        }
        return Ok(out);
    }
    let (a, b) = lines.map(|l| range(l, all.len())).unwrap_or((1, all.len()));
    let b = b.min(a - 1 + OUT_LINES);
    for (i, l) in all.iter().enumerate().take(b).skip(a - 1) {
        out.push_str(&line(i, l));
    }
    if b < all.len() {
        out.push_str(&format!(
            "[toomux · lines {a}-{b} of {}; `--lines {}-{}` continues]\n",
            all.len(),
            b + 1,
            (b + OUT_LINES).min(all.len())
        ));
    }
    Ok(out)
}

/// Kept outputs go after a week, with their memory entries; captures that
/// never finished (a killed shell) after a day.
fn prune(store: &Path, run: &Path) {
    let now = std::time::SystemTime::now();
    let old = |p: &Path, secs: u64| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|d| d.as_secs() > secs))
    };
    let mut gone = Vec::new();
    for e in std::fs::read_dir(store).into_iter().flatten().flatten() {
        let p = e.path();
        if old(&p, KEEP_DAYS * 86_400)
            && std::fs::remove_file(&p).is_ok()
            && let Some(id) = p.file_stem().and_then(|s| s.to_str())
        {
            gone.push(id.to_string());
        }
    }
    for e in std::fs::read_dir(run).into_iter().flatten().flatten() {
        if old(&e.path(), 86_400) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    if !gone.is_empty()
        && let Ok(m) = crate::memory::Memory::open()
    {
        for id in gone {
            let _ = m.delete_source_prefix(&format!("output:{id} "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_views_pass_through() {
        for c in [
            "cat src/main.rs",
            "sed -n 1,80p x.rs",
            "cd ~/p && head -50 README.md",
            "git diff HEAD~1",
            "tail -f log | grep x",
            "cd src; cat main.rs",
            "(cd src && cat main.rs)",
            "git --no-pager diff",
            "git -C repo show HEAD",
            "GIT_PAGER=cat git log -p -3",
            "awk 'NR>=100&&NR<=300' f",
            "jq . f.json",
            "sudo cat /etc/x",
        ] {
            assert!(is_view(c), "{c}");
        }
        for c in [
            "cargo test 2>&1 | tail -5",
            "find . -name '*.rs'",
            "grep -rn foo src",
            "ls -la",
            "git log --oneline",
            "curl -s x | jq .",
        ] {
            assert!(!is_view(c), "{c}");
        }
    }

    #[test]
    fn only_toomux_itself_is_ours() {
        assert!(is_ours("toomux out abc"));
        assert!(is_ours("'/home/x/.cargo/bin/toomux' job follow abc"));
        assert!(!is_ours("cd /home/x/toomux && cargo test"));
    }

    #[test]
    fn hook_rewrites_only_what_it_should() {
        // The background case records a job: keep it out of the real state.
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-rewrite-{}", std::process::id()));
        unsafe {
            std::env::set_var("XDG_STATE_HOME", &tmp);
            std::env::set_var("XDG_RUNTIME_DIR", &tmp);
        }
        let v = |cmd: &str, bg: bool| json!({"tool_name": "Bash", "cwd": "/tmp", "session_id": "s", "tool_input": {"command": cmd, "run_in_background": bg}});
        assert!(rewrite(&v("cat x", false)).is_none());
        let bg: Value = serde_json::from_str(&rewrite(&v("npm run dev", true)).unwrap()).unwrap();
        assert!(
            bg["hookSpecificOutput"]["updatedInput"]["command"]
                .as_str()
                .unwrap()
                .contains(" job run "),
            "background commands run as toomux jobs"
        );
        assert!(
            rewrite(&v("toomux job follow abc", true)).is_none(),
            "our own commands are left alone"
        );
        let out: Value = serde_json::from_str(&rewrite(&v("npm test", false)).unwrap()).unwrap();
        let cmd = out["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        assert!(
            cmd.contains("exec {__toomux}>&-; npm test\n\n}") && cmd.contains("(exit $__toomux)"),
            "{cmd}"
        );
        assert_eq!(
            out["hookSpecificOutput"]["updatedInput"]["run_in_background"], false,
            "the rest of the input is kept"
        );
        assert!(
            rewrite(&v("", false)).is_none() && rewrite(&v("# just a note", false)).is_none(),
            "nothing to run: left alone"
        );
        assert!(
            rewrite(&v("python3 - <<EOF\nprint(1)", false)).is_none(),
            "a command bash can't parse wrapped is left alone"
        );
        assert!(rewrite(&json!({"tool_name": "Read", "tool_input": {}})).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_subagent_waits_on_long_commands_in_spells() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-spells-{}", std::process::id()));
        unsafe {
            std::env::set_var("XDG_STATE_HOME", &tmp);
            std::env::set_var("XDG_RUNTIME_DIR", &tmp);
        }
        let v = |cmd: &str, agent: Option<&str>, timeout: u64| json!({"tool_name": "Bash", "cwd": "/tmp", "session_id": "s", "agent_id": agent, "tool_input": {"command": cmd, "timeout": timeout}});
        let input = |r: Option<String>| {
            serde_json::from_str::<Value>(&r.unwrap()).unwrap()["hookSpecificOutput"]["updatedInput"].clone()
        };
        let long = input(rewrite(&v("cargo nextest run", Some("a1"), 3_600_000)));
        let cmd = long["command"].as_str().unwrap();
        assert!(
            cmd.contains(" job run ") && cmd.contains("--until 270") && cmd.contains(" cap "),
            "a job, its output kept as usual: {cmd}"
        );
        assert_eq!(
            long["timeout"], 300_000,
            "Claude's own timeout covers one spell"
        );
        let id = cmd
            .split(" job run ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        let job = crate::jobs::load(id).unwrap();
        assert_eq!(job.command, "cargo nextest run");
        assert_eq!(
            job.deadline_ms - job.started_ms,
            600_000,
            "stopped where Claude would have: at most 10 minutes"
        );
        let short = input(rewrite(&v("cargo nextest run", Some("a1"), 120_000)));
        assert!(
            !short["command"].as_str().unwrap().contains(" job run "),
            "a wait inside the cache's life runs as before"
        );
        let main = input(rewrite(&v("cargo nextest run", None, 600_000)));
        assert!(
            !main["command"].as_str().unwrap().contains(" job run "),
            "conversations' caches last an hour"
        );
        let more = input(rewrite(&v(
            "toomux job follow abc --more",
            Some("a1"),
            600_000,
        )));
        assert!(
            more["command"].as_str().unwrap().contains(" cap "),
            "the rest of the output is kept like the rest"
        );
        assert!(rewrite(&v("toomux job follow abc --more", None, 0)).is_none());
        assert!(rewrite(&v("toomux out abc", Some("a1"), 600_000)).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn scan(text: &str) -> Scan {
        let mut s = Scan {
            hide_from: Some(0),
            ..Default::default()
        };
        s.feed(text.as_bytes());
        s.finish();
        s
    }

    #[test]
    fn errors_outrank_warnings_and_bring_their_context() {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(&format!("warning: unused {i}\n  --> src/a.rs:{i}:1\n"));
        }
        text.push_str("error[E0308]: mismatched types\n  --> src/main.rs:412:17\n   |\n");
        for i in 0..40 {
            text.push_str(&format!("warning: unused {i}\n"));
        }
        text.push_str("error: could not compile `x`\n");
        let s = scan(&text);
        let v = view("abc1234", Path::new("/k/abc1234.txt"), &s, 0, false);
        assert!(
            v.contains("error[E0308]: mismatched types") && v.contains("--> src/main.rs:412:17"),
            "{v}"
        );
        assert!(
            v.contains("80 warnings (the first 4 shown)") && v.contains("/k/abc1234.txt"),
            "{v}"
        );
        assert!(
            v.trim_end().ends_with("error: could not compile `x`"),
            "the tail is never dropped:\n{v}"
        );
    }

    #[test]
    fn progress_bars_settle_and_clean_summaries_are_not_errors() {
        assert_eq!(settle(b"10%\r50%\r100% done\r"), "100% done");
        assert!(!is_error("test result: ok. 52 passed; 0 failed; 1 ignored"));
        assert!(is_error("thread 'main' panicked at src/x.rs:3"));
    }
}
