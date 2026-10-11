//! Carrying on after a usage limit resets.
//!
//! When an account hits its limit, Claude Code ends the turn with a
//! rate-limit error and puts up a dialog ("What do you want to do? 1. Stop
//! and wait for limit to reset ..."). Its default stops: once the limit
//! resets the session sits on that dialog, and toomux saw it as needing you,
//! so nothing waiting on it (a voyage, a handover) went on either.
//!
//! Now, a minute after the reset, toomux closes the dialog (Esc, checked)
//! and, for a session that was mid-work, types a short "carry on". Which
//! sessions count is `resume_after_limit`; a voyage or a handover under way
//! always carries on (they were asked for). Each limit is acted on once:
//! a marker keyed by the session and the limit error's time is claimed first.
//!
//! Nothing here runs as a daemon. Whoever sees a limit (Claude Code's status
//! line command, the tmux status tick, `toomux list`) leaves a detached
//! one-shot `toomux tick --at <reset>` sleeping until it, so a reset in the
//! night is acted on with no terminal attached. It sleeps by the wall clock,
//! so a machine that was asleep acts as soon as it wakes.
//!
//! Claude Code's own "continue automatically at" countdown is left alone: a
//! key pressed into it cancels it. Toomux only steps in if, a minute after
//! the reset, the session still hasn't carried on.

use crate::config::Config;
use crate::registry::{self, Session};
use crate::tmux;
use crate::transcript::{self, LimitHit};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Which sessions carry on once their limit resets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Resume {
    /// A turn the limit cut off mid-work, and voyages and handovers.
    #[default]
    Working,
    /// Also a prompt the limit refused before any work began.
    All,
    /// Only voyages and handovers already under way.
    Off,
}

impl Resume {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "working" => Some(Resume::Working),
            "all" => Some(Resume::All),
            "off" | "false" | "none" => Some(Resume::Off),
            _ => None,
        }
    }
}

/// Past the reset, this long before toomux acts: the reset is read to the
/// minute, and Claude Code's own auto-continue gets to go first.
pub const GRACE_MS: i64 = 60_000;

/// Claude Code drops "continue automatically" for resets further off.
const AUTO_CONTINUE_WITHIN_MS: i64 = 24 * 3_600_000;

/// What toomux types into a session the limit cut off.
pub const RESUME_PROMPT: &str =
    "[toomux] The usage limit has reset. Carry on from where you were cut off.";

// ---- Claude Code's limit screens ---------------------------------------------
//
// Every Claude Code string toomux relies on for limits is here (checked
// against Claude Code 2.1.296). Lines are compared lower-cased, with curly
// apostrophes straightened, runs of spaces collapsed and any leading
// pointer, box-drawing or option number ("❯ 1. ") dropped, and only by their
// start, so a reworded tail or a wrapped line still matches.

/// The limit dialog's title.
const DIALOG_TITLE: &str = "what do you want to do?";
/// Its default option, which stops (just "stop" on usage-based billing).
const STOP_OPTIONS: &[&str] = &["stop and wait for limit to reset", "stop"];
/// Its option that hands the wait to Claude Code: "... at 8pm", "...
/// shortly", "... when the limit resets".
const WAIT_OPTION: &str = "wait here, then continue automatically";
/// Shown instead once Claude Code's own auto-continue is armed.
const DONT_CONTINUE_OPTION: &str = "don't continue automatically";
/// Claude Code's countdown and its reset notice: "Continuing automatically at
/// 8pm · esc to cancel", "Usage limit reached · continuing automatically ...",
/// "Usage limit reset · continuing automatically".
const CONTINUING: &str = "continuing automatically";
const CONTINUING_NOTICES: &[&str] = &["usage limit reached", "usage limit reset"];

/// What a session's pane shows, as far as limits go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// Claude Code's limit dialog, waiting for a choice.
    Dialog,
    /// Claude Code is waiting to continue by itself: leave it be.
    Continuing,
    Other,
}

fn plain(line: &str) -> String {
    let lower = line.to_lowercase().replace(['\u{2019}', '\u{2018}'], "'");
    let words: Vec<&str> = lower.split_whitespace().collect();
    words
        .join(" ")
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .to_string()
}

/// An option line's label: "1. stop and wait…" -> "stop and wait…".
fn option_label(plain: &str) -> Option<&str> {
    let digits = plain.find(|c: char| !c.is_ascii_digit())?;
    (digits > 0)
        .then(|| plain[digits..].strip_prefix(". "))
        .flatten()
}

/// Claude Code draws these at the bottom of the screen: only the last lines
/// count, so a conversation quoting them further up doesn't.
const BOTTOM_LINES: usize = 24;

pub fn screen_in(text: &str) -> Screen {
    let all: Vec<String> = text.lines().map(plain).collect();
    let end = all.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1);
    let lines = &all[end.saturating_sub(BOTTOM_LINES)..end];
    if lines.iter().any(|l| {
        l.starts_with(CONTINUING)
            || (CONTINUING_NOTICES.iter().any(|n| l.starts_with(n)) && l.contains(CONTINUING))
    }) {
        return Screen::Continuing;
    }
    let Some(title) = lines.iter().rposition(|l| l.starts_with(DIALOG_TITLE)) else {
        return Screen::Other;
    };
    let labels: Vec<&str> = lines[title + 1..]
        .iter()
        .take(16)
        .filter_map(|l| option_label(l))
        .collect();
    if labels.iter().any(|l| l.starts_with(DONT_CONTINUE_OPTION)) {
        return Screen::Continuing;
    }
    let stop = |l: &&str| {
        STOP_OPTIONS
            .iter()
            .any(|s| *l == *s || (s.len() > 4 && l.starts_with(s)))
    };
    if labels.iter().any(stop) || labels.iter().any(|l| l.starts_with(WAIT_OPTION)) {
        return Screen::Dialog;
    }
    Screen::Other
}

/// The visible screen of a pane, without colour. None: the pane is gone.
fn capture(pane: &str) -> Option<String> {
    tmux::run(&["capture-pane", "-p", "-J", "-t", pane]).ok()
}

pub fn screen(pane: &str) -> Option<Screen> {
    capture(pane).map(|t| screen_in(&t))
}

/// Claude Code is counting down to continue by itself: a key pressed now,
/// a relaunch or a typed prompt would cancel it.
pub fn claude_continuing(pane: &str) -> bool {
    screen(pane) == Some(Screen::Continuing)
}

/// Close the limit dialog (Esc), and make sure it closed. A second Esc only
/// if it's still up after a pause: two in quick succession at an empty
/// prompt would open Claude Code's rewind menu.
pub fn dismiss(pane: &str) -> Result<()> {
    for _ in 0..2 {
        if screen(pane) != Some(Screen::Dialog) {
            return Ok(());
        }
        tmux::run(&["send-keys", "-t", pane, "Escape"])?;
        std::thread::sleep(Duration::from_millis(1500));
    }
    match screen(pane) {
        Some(Screen::Dialog) => bail!("the limit dialog didn't close"),
        _ => Ok(()),
    }
}

/// Steps from the highlighted option to "Wait here, then continue
/// automatically", if both are on screen.
fn steps_to_wait(text: &str) -> Option<i32> {
    let mut at = None;
    let mut wait = None;
    let mut n = 0;
    for raw in text.lines() {
        let p = plain(raw);
        let Some(label) = option_label(&p) else {
            continue;
        };
        let before_number = raw.split(|c: char| c.is_ascii_digit()).next();
        if before_number.is_some_and(|b| b.contains('❯')) {
            at = Some(n);
        }
        if label.starts_with(WAIT_OPTION) {
            wait = Some(n);
        }
        n += 1;
    }
    Some(wait? - at?)
}

/// Choose Claude Code's own "Wait here, then continue automatically" on its
/// limit dialog: move to it, check it's the one highlighted, then Enter.
pub fn choose_wait(pane: &str) -> Result<()> {
    let Some(text) = capture(pane) else {
        bail!("the pane is gone");
    };
    if screen_in(&text) != Screen::Dialog {
        bail!("no limit dialog");
    }
    let Some(steps) = steps_to_wait(&text) else {
        bail!("the dialog has no wait option");
    };
    let key = if steps < 0 { "Up" } else { "Down" };
    for _ in 0..steps.abs() {
        tmux::run(&["send-keys", "-t", pane, key])?;
        std::thread::sleep(Duration::from_millis(150));
    }
    std::thread::sleep(Duration::from_millis(300));
    if capture(pane).and_then(|t| steps_to_wait(&t)) != Some(0) {
        bail!("couldn't highlight the wait option");
    }
    tmux::run(&["send-keys", "-t", pane, "Enter"])?;
    std::thread::sleep(Duration::from_millis(1500));
    match screen(pane) {
        Some(Screen::Dialog) => bail!("the dialog is still up"),
        _ => Ok(()),
    }
}

// ---- what toomux keeps --------------------------------------------------------

fn dir() -> PathBuf {
    crate::paths::state().join("limits")
}

/// One limit event of one session.
fn key(session: &str, hit: &LimitHit) -> String {
    let at = hit
        .at
        .or_else(|| hit.reset())
        .map_or(0, |t| t.timestamp_millis());
    format!("{session}-{at}")
}

/// Claim `name` once: true only for the first caller, ever.
fn claim(name: &str) -> bool {
    let _ = std::fs::create_dir_all(dir());
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir().join(name))
        .is_ok()
}

fn claimed(name: &str) -> bool {
    dir().join(name).exists()
}

fn log(session: &str, hit: &LimitHit, what: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir().join("log.jsonl"))
    {
        let v = json!({
            "at": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string(),
            "session": session,
            "limit": hit.text,
            "what": what,
        });
        let _ = writeln!(f, "{v}");
    }
}

/// Markers and wake-ups no pass will look at again.
fn prune(now: i64) {
    for e in std::fs::read_dir(dir()).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !["done-", "chose-", "wake-", "pass-"]
            .iter()
            .any(|p| name.starts_with(p))
        {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| now - (d.as_millis() as i64) > 14 * 86_400_000);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

// ---- waking up for a reset ----------------------------------------------------

/// A one-shot `toomux tick --at` for this session and time, unless one is
/// already waiting. True if this call claimed it.
fn wake(session: &str, at_ms: i64) -> bool {
    if !claim(&format!("wake-{session}-{}", at_ms / 1000)) {
        return false;
    }
    spawn_tick(at_ms);
    true
}

/// A pass now, at most once every two minutes per limit event.
fn pass_soon(name: &str, now: i64) {
    let stamp = dir().join(format!("pass-{name}"));
    let fresh = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|d| now - (d.as_millis() as i64) < 120_000);
    if fresh {
        return;
    }
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(&stamp, "");
    spawn_tick(now);
}

/// Detached from whoever spawned it (setsid, then a second fork), so it
/// outlives the hook, the status tick and the terminal.
fn spawn_tick(at_ms: i64) {
    if cfg!(test) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["tick", "--at", &at_ms.to_string()])
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

/// What a limit event needs from here: a wake-up at its reset, a pass now
/// (its reset has passed, or the dialog is to be handed to Claude Code's
/// own wait), or nothing.
fn plan(cfg: &Config, session: &str, hit: &LimitHit, now: i64) {
    let Some(reset) = hit.reset().map(|t| t.timestamp_millis()) else {
        return;
    };
    let name = key(session, hit);
    if claimed(&format!("done-{name}")) {
        return;
    }
    let at = reset + GRACE_MS;
    if at > now {
        wake(session, at);
        if cfg.auto_continue_at_limit
            && reset - now < AUTO_CONTINUE_WITHIN_MS
            && !claimed(&format!("chose-{name}"))
        {
            pass_soon(&name, now);
        }
    } else {
        pass_soon(&name, now);
    }
}

/// The status tick and `toomux list`: every session stopped by a limit has a
/// wake-up waiting for its reset.
pub fn observe(cfg: &Config, sessions: &[Session], now: i64) {
    for s in sessions.iter().filter(|s| !s.dormant && s.pane.is_some()) {
        let Some(path) = s.transcript(cfg) else {
            continue;
        };
        if let Some(hit) = transcript::meta(&path).limit_hit() {
            plan(cfg, &s.id, hit, now);
        }
    }
}

/// Bytes of a transcript's end the status line looks at.
const STATUSLINE_TAIL: u64 = 32 * 1024;

/// Claude Code's status line command (every second or so, in every
/// session, attached or not): a transcript written in the last few minutes
/// whose latest reply is a limit error gets its wake-up. Only a stat
/// otherwise.
pub fn seen_in_statusline(cfg: &Config, v: &Value, now: i64) {
    let (Some(session), Some(path)) = (
        v.get("session_id").and_then(Value::as_str),
        v.get("transcript_path").and_then(Value::as_str),
    ) else {
        return;
    };
    let path = Path::new(path);
    let recent = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|d| now - (d.as_millis() as i64) < 5 * 60_000);
    if !recent {
        return;
    }
    if let Some(hit) = last_reply_limit(path) {
        plan(cfg, session, &hit, now);
    }
}

/// The latest main-chain reply in a transcript's end, if it's a limit error.
fn last_reply_limit(path: &Path) -> Option<LimitHit> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(STATUSLINE_TAIL);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    f.take(STATUSLINE_TAIL).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines().rev() {
        if !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("assistant")
            || v.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        return LimitHit::from_entry(&v);
    }
    None
}

// ---- after the reset ------------------------------------------------------------

/// Why a session carries on once its limit lifts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    /// A voyage waits on it: the voyage's tick tells it to carry on.
    Voyage,
    /// A handover is under way: it goes once the session is idle.
    Handover,
    /// The limit cut a turn off mid-work.
    CutOff,
    /// The limit was the only answer its prompt got.
    Refused,
}

fn why(cfg: &Config, s: &Session, transcript: &Path) -> Option<Why> {
    if crate::voyage::note_limit(&s.id) {
        return Some(Why::Voyage);
    }
    if s.handover.is_some() || crate::handover::requested(&s.id) {
        return Some(Why::Handover);
    }
    let cut = transcript::cut_off_by_limit(transcript)?;
    match (cfg.limit_resume(), cut) {
        (Resume::Off, _) => None,
        (_, true) => Some(Why::CutOff),
        (Resume::All, false) => Some(Why::Refused),
        (Resume::Working, false) => None,
    }
}

/// One session: before the reset, maybe hand the dialog to Claude Code's own
/// wait; after it, close the dialog and carry on, once per limit. What was
/// done, for the log.
pub fn recover(cfg: &Config, s: &Session, now: i64) -> Option<String> {
    if s.dormant {
        return None;
    }
    let pane = s.pane.as_ref()?.id.clone();
    let path = s.transcript(cfg)?;
    let hit = transcript::meta(&path).limit_hit()?.clone();
    let reset = hit.reset()?.timestamp_millis();
    let name = key(&s.id, &hit);
    if now < reset + GRACE_MS {
        if cfg.auto_continue_at_limit
            && reset - now < AUTO_CONTINUE_WITHIN_MS
            && screen(&pane) == Some(Screen::Dialog)
            && claim(&format!("chose-{name}"))
        {
            let what = match choose_wait(&pane) {
                Ok(()) => "chose Claude Code's own wait".to_string(),
                Err(e) => format!("couldn't choose Claude Code's own wait: {e}"),
            };
            log(&s.id, &hit, &what);
            return Some(what);
        }
        return None;
    }
    if claimed(&format!("done-{name}")) {
        return None;
    }
    let Some(why) = why(cfg, s, &path) else {
        // Not one to carry on: settled, so no pass looks at it again.
        if claim(&format!("done-{name}")) {
            log(
                &s.id,
                &hit,
                "limit lifted; left as it was (resume_after_limit)",
            );
        }
        return None;
    };
    match screen(&pane)? {
        // Claude Code carries on by itself; its reply will clear the limit.
        Screen::Continuing => return None,
        Screen::Dialog => {
            if let Err(e) = dismiss(&pane) {
                let what = format!("{e}; tried again on the next pass");
                log(&s.id, &hit, &what);
                return Some(what);
            }
        }
        Screen::Other => {}
    }
    // Claimed before anything is typed: never twice for one limit.
    if !claim(&format!("done-{name}")) {
        return None;
    }
    let what = match why {
        Why::Voyage => "limit lifted; its voyage carries on".to_string(),
        Why::Handover => "limit lifted; its handover goes on".to_string(),
        Why::CutOff | Why::Refused => {
            if !crate::actions::prompt_empty(&pane) {
                "limit lifted; something is typed in its prompt, left alone".to_string()
            } else {
                match crate::actions::type_prompt(&pane, RESUME_PROMPT) {
                    Ok(()) => {
                        if !cfg!(test) {
                            crate::watch::announce_text(
                                cfg,
                                &format!("{}: carried on after the usage limit reset", s.title),
                                "limit",
                                &s.title,
                                &s.id,
                            );
                        }
                        "limit lifted; told to carry on".to_string()
                    }
                    Err(e) => format!("limit lifted; couldn't type into it: {e}"),
                }
            }
        }
    };
    log(&s.id, &hit, &what);
    Some(what)
}

/// One pass over every session, one pass at a time.
pub fn pass(cfg: &Config) {
    use std::os::fd::AsRawFd;
    let _ = std::fs::create_dir_all(dir());
    let Ok(lock) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir().join("pass.lock"))
    else {
        return;
    };
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let now = registry::now_ms();
    let all = registry::load(cfg);
    let mut acted = false;
    for s in &all {
        acted |= recover(cfg, s, now).is_some();
    }
    // A closed dialog leaves the session idle: what waited on it can go.
    let all = if acted { registry::load(cfg) } else { all };
    let now = registry::now_ms();
    crate::voyage::tick(&all, now);
    crate::handover::start_due(cfg, &all, now);
    prune(now);
}

/// Seconds between the passes of one wake-up: the first right at the reset
/// (plus grace), the rest to see a closed dialog through to a voyage's or a
/// handover's turn (each waits for a minute or so of quiet).
const PASSES: &[u64] = &[0, 75, 75, 120];

/// `toomux tick --at <ms>`: sleep until then by the wall clock (in short
/// naps, so time spent suspended counts), then a few passes.
pub fn wake_at(cfg: &Config, at_ms: i64) {
    loop {
        let left = at_ms - registry::now_ms();
        if left <= 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(left.min(30_000) as u64));
    }
    for gap in PASSES {
        std::thread::sleep(Duration::from_secs(*gap));
        pass(cfg);
    }
}

#[cfg(test)]
mod tests;
