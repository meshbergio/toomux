//! Voyages: an outcome a session works toward, turn after turn, until it's
//! done. `/voyage <outcome>` in any session sets one. After every turn a small
//! model reads the end of the conversation and says whether the outcome is
//! reached; if not, the session is sent back to it with what's missing. It
//! goes on across handovers (the fresh session inherits the voyage), waits out
//! a usage limit and carries on once it resets, stops at a budget, and asks
//! for you when only you can decide. Claude Code's /goal does the loop within
//! one conversation; a voyage is the loop for the whole chain.
//!
//! Kept in ~/.local/state/toomux/voyages: one `<id>.json` per voyage and an
//! `<id>.log` of every check.

use crate::config::Config;
use crate::registry::{self, Session, State as St};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Set in the judge's own `claude -p`, so its hooks (if any load) do nothing.
pub const JUDGE_ENV: &str = "TOOMUX_VOYAGE_JUDGE";
const JUDGE_TIMEOUT: Duration = Duration::from_secs(120);
const CHECK_TIMEOUT: Duration = Duration::from_secs(8 * 60);
/// How much of the conversation's end the judge reads.
const EVIDENCE_CHARS: usize = 16_000;
const OUTCOME_CHARS: usize = 4_000;
/// A limit that has lifted: the voyage carries on once the session has sat
/// this long at an empty prompt.
const RESUME_AFTER_MS: i64 = 60_000;

/// How hard a voyage pushes on when the going gets tough, and how hard
/// "done" is to reach. Each tier keeps everything of the one below it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Persistence {
    /// The judge's word is enough, and it stops soon when stuck.
    Light,
    /// The judge wants evidence shown.
    #[default]
    Steady,
    /// A stronger judge that counts only fresh evidence, a proof lap before
    /// done, a nudge when stuck, and one push back before it stops for you
    /// or gives up.
    Hard,
    /// The strongest judge, two proof laps, a sceptical review of every
    /// change since it began, and two push backs.
    Relentless,
}

/// What a tier does, turn by turn.
pub struct Rules {
    /// Turns in a row with no tool call before it pauses.
    pub idle_turns: u32,
    /// Checks in a row that couldn't be made before it pauses.
    pub failures: u32,
    /// Added to what the judge is told.
    pub judge_rules: &'static str,
    /// Times a "met" sends the session back to prove it before it counts.
    pub proof_laps: u32,
    /// A sceptical reviewer reads the changes before it counts as done.
    pub review: bool,
    /// Checks without the estimate rising before it's told to change tack,
    /// and again every as many after.
    pub stall_after: Option<u32>,
    /// Checks without the estimate rising before it pauses for you.
    pub stall_stop: Option<u32>,
    /// Times "needs you" or "can't be done" is sent back to try again first.
    pub push_backs: u32,
}

const LENIENT: &str = "\nA clear, specific statement from the agent that the outcome is reached counts as evidence, unless \
something shown contradicts it.";
const STRICT: &str = "\nBe strict. Count only evidence from these turns: output of commands actually run, files actually \
changed. Earlier results, summaries and promises don't count. Say not_yet if you see tests skipped, disabled, weakened or \
deleted, stubs, TODOs, hard-coded or special-cased results, errors or warnings passed over, or any part of the outcome not \
addressed. Say impossible or needs_you only when nothing the agent could do would get round it.";

impl Persistence {
    pub const ALL: [Persistence; 4] = [
        Persistence::Light,
        Persistence::Steady,
        Persistence::Hard,
        Persistence::Relentless,
    ];

    pub fn word(self) -> &'static str {
        match self {
            Persistence::Light => "light",
            Persistence::Steady => "steady",
            Persistence::Hard => "hard",
            Persistence::Relentless => "relentless",
        }
    }

    pub fn parse(s: &str) -> Option<Persistence> {
        Persistence::ALL
            .into_iter()
            .find(|p| p.word() == s.trim().to_lowercase())
    }

    pub fn rules(self) -> Rules {
        match self {
            Persistence::Light => Rules {
                idle_turns: 2,
                failures: 2,
                judge_rules: LENIENT,
                proof_laps: 0,
                review: false,
                stall_after: None,
                stall_stop: Some(4),
                push_backs: 0,
            },
            Persistence::Steady => Rules {
                idle_turns: 4,
                failures: 3,
                judge_rules: "",
                proof_laps: 0,
                review: false,
                stall_after: Some(4),
                stall_stop: Some(8),
                push_backs: 0,
            },
            Persistence::Hard => Rules {
                idle_turns: 6,
                failures: 4,
                judge_rules: STRICT,
                proof_laps: 1,
                review: false,
                stall_after: Some(3),
                stall_stop: Some(12),
                push_backs: 1,
            },
            Persistence::Relentless => Rules {
                idle_turns: 8,
                failures: 5,
                judge_rules: STRICT,
                proof_laps: 2,
                review: true,
                stall_after: Some(2),
                stall_stop: Some(20),
                push_backs: 2,
            },
        }
    }

    /// The judge's model: `voyage_judge_model` up to steady, then
    /// `voyage_hard_model` and `voyage_relentless_model`.
    pub fn model(self, cfg: &Config) -> &str {
        match self {
            Persistence::Light | Persistence::Steady => &cfg.voyage_judge_model,
            Persistence::Hard => &cfg.voyage_hard_model,
            Persistence::Relentless => &cfg.voyage_relentless_model,
        }
    }

    /// What the session is told about its tier when the voyage is set.
    fn told(self) -> &'static str {
        match self {
            Persistence::Light | Persistence::Steady => "",
            Persistence::Hard => {
                " Persistence is hard: the judge counts only evidence from these turns, you'll be asked for a proof \
                lap before it counts as done, and before it stops for the user or gives up you'll be asked to try once more."
            }
            Persistence::Relentless => {
                " Persistence is relentless: the judge counts only evidence from these turns, you'll be asked \
                for two proof laps, a sceptical reviewer reads every change since the voyage began before it counts as done, and \
                before it stops for the user or gives up you'll be asked to try twice more."
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    /// Waiting on you: it needs a decision, went quiet, hit its budget, or
    /// couldn't be checked. Your next prompt (or `/voyage resume`) goes on.
    Paused,
    Met,
    Impossible,
    Cleared,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Active => "on",
            Status::Paused => "paused",
            Status::Met => "done",
            Status::Impossible => "impossible",
            Status::Cleared => "cleared",
        }
    }
    pub fn open(self) -> bool {
        matches!(self, Status::Active | Status::Paused)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Voyage {
    pub id: String,
    pub outcome: String,
    /// A command that must exit 0 before the voyage counts as done.
    #[serde(default)]
    pub check: Option<String>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    pub cwd: String,
    /// Every conversation it has run in, first to current.
    pub sessions: Vec<String>,
    pub started_ms: i64,
    pub status: Status,
    /// Why it paused or ended.
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub ended_ms: Option<i64>,
    /// Turns checked.
    #[serde(default)]
    pub turns: u32,
    /// Turns in a row that called no tool.
    #[serde(default)]
    pub idle: u32,
    /// Checks in a row that couldn't be made.
    #[serde(default)]
    pub failures: u32,
    /// The last check's reason.
    #[serde(default)]
    pub last: Option<String>,
    #[serde(default)]
    pub judge_usd: f64,
    /// What the chain has cost since the voyage began, as of the last check.
    #[serde(default)]
    pub spent_usd: f64,
    /// Its session stopped at a usage limit: it carries on once that lifts.
    #[serde(default)]
    pub at_limit: bool,
    /// The judge's last estimate of how much of it is done, in percent.
    #[serde(default)]
    pub progress: Option<u8>,
    #[serde(default)]
    pub persistence: Persistence,
    /// The commit its folder was at when it began, for the review.
    #[serde(default)]
    pub base: Option<String>,
    /// "Met" verdicts in a row that were sent back to prove it.
    #[serde(default)]
    pub laps: u32,
    /// Times it was sent back instead of stopping for you or giving up.
    #[serde(default)]
    pub pushed: u32,
    /// The highest estimate so far, and checks in a row that didn't beat it.
    #[serde(default)]
    pub best: u8,
    #[serde(default)]
    pub flat: u32,
}

impl Voyage {
    pub fn session(&self) -> &str {
        self.sessions.last().map(String::as_str).unwrap_or("")
    }
    pub fn handovers(&self) -> usize {
        self.sessions.len().saturating_sub(1)
    }
    /// "hard voyage 1h 20m", "steady voyage paused": the chip beside a
    /// session, its persistence first.
    pub fn chip(&self, now: i64) -> String {
        let tier = self.persistence.word();
        match self.status {
            Status::Active if self.at_limit => format!("{tier} voyage waits for the limit"),
            Status::Active if self.laps > 0 => format!(
                "{tier} voyage, proof lap {} of {}",
                self.laps,
                self.persistence.rules().proof_laps
            ),
            Status::Active => format!(
                "{tier} voyage {}",
                registry::duration(now - self.started_ms)
            ),
            s => format!("{tier} voyage {}", s.word()),
        }
    }
    /// Its voyage: under sail at the judge's estimate, at anchor while it
    /// waits, landed once it's met.
    pub fn scene(&self, now: i64) -> crate::scene::Scene {
        use crate::scene::Sea;
        let sea = match self.status {
            Status::Met => Sea::Landed,
            Status::Paused => Sea::Anchored,
            _ if self.at_limit => Sea::Anchored,
            _ => Sea::Sailing,
        };
        crate::scene::Scene {
            progress: f64::from(self.progress.unwrap_or(0)) / 100.0,
            sea,
            frame: (now / 1000) as u64,
        }
    }
}

/// How long a met voyage's landing stays on its session's status line.
const LANDED_MS: i64 = 10 * 60_000;

fn landed_file(session: &str) -> PathBuf {
    dir().join("landed").join(format!("{session}.json"))
}

/// A voyage this conversation met in the last few minutes: its status line
/// shows the landing a while.
pub fn landed_for(session: &str, now: i64) -> Option<Voyage> {
    let path = landed_file(session);
    let q: Voyage = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    if now - q.ended_ms.unwrap_or(0) > LANDED_MS {
        let _ = std::fs::remove_file(path);
        return None;
    }
    Some(q)
}

/// Every voyage met in the last few minutes, for the toomux list.
pub fn landed(now: i64) -> Vec<Voyage> {
    read_dir(&dir().join("landed"))
        .into_iter()
        .filter(|q| now - q.ended_ms.unwrap_or(0) <= LANDED_MS)
        .collect()
}

pub fn dir() -> PathBuf {
    let dir = crate::paths::state().join("voyages");
    // Voyages were quests once, kept in quests/.
    if !dir.exists() {
        let _ = std::fs::rename(crate::paths::state().join("quests"), &dir);
    }
    dir
}

/// Voyages that ended live a folder down, so finding an open one (every
/// status line, every turn) reads only the open ones.
fn ended_dir() -> PathBuf {
    dir().join("ended")
}

fn log_file(id: &str) -> PathBuf {
    dir().join(format!("{id}.log"))
}

fn save(q: &Voyage) {
    let home = if q.status.open() { dir() } else { ended_dir() };
    let _ = std::fs::create_dir_all(&home);
    let tmp = home.join(format!(".{}.{}", q.id, std::process::id()));
    if std::fs::write(&tmp, serde_json::to_string_pretty(q).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, home.join(format!("{}.json", q.id)));
    }
    if !q.status.open() {
        let _ = std::fs::remove_file(dir().join(format!("{}.json", q.id)));
    }
}

fn read_dir(d: &Path) -> Vec<Voyage> {
    std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
        .collect()
}

fn log(q: &Voyage, what: &str, reason: &str) {
    let line = json!({"at_ms": registry::now_ms(), "turn": q.turns, "session": q.session(), "what": what, "reason": reason});
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file(&q.id))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Every voyage, newest first.
pub fn all() -> Vec<Voyage> {
    let mut out = open();
    out.extend(read_dir(&ended_dir()));
    out.sort_by_key(|q: &Voyage| std::cmp::Reverse(q.started_ms));
    out
}

/// The voyages still going (or paused), newest first.
pub fn open() -> Vec<Voyage> {
    let mut out: Vec<Voyage> = read_dir(&dir())
        .into_iter()
        .filter(|q| q.status.open())
        .collect();
    out.sort_by_key(|q: &Voyage| std::cmp::Reverse(q.started_ms));
    out
}

/// The open voyage this conversation is on.
pub fn open_for(session: &str) -> Option<Voyage> {
    open().into_iter().find(|q| q.session() == session)
}

fn find(target: &str) -> Result<Voyage> {
    let all = all();
    if let Some(q) = all
        .iter()
        .find(|q| q.id == target || (target.len() >= 4 && q.id.starts_with(target)))
    {
        return Ok(q.clone());
    }
    if let Some(q) = all
        .iter()
        .find(|q| q.status.open() && target.len() >= 4 && q.session().starts_with(target))
    {
        return Ok(q.clone());
    }
    bail!("no voyage {target}")
}

fn new_id() -> String {
    use sha2::{Digest, Sha256};
    let seed = format!(
        "{}{}{:?}",
        registry::now_ms(),
        std::process::id(),
        Instant::now()
    );
    let h = Sha256::digest(seed.as_bytes());
    h.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

// ---- what you type -----------------------------------------------------------

/// What `/voyage …` asks for.
#[derive(Debug, PartialEq)]
pub enum Ask {
    Show,
    Clear,
    Resume,
    Set {
        outcome: String,
        check: Option<String>,
        budget: Option<f64>,
        persistence: Option<Persistence>,
    },
}

/// `/voyage` arguments: an outcome with optional `--check "<command>"`,
/// `--budget <dollars>` and `--persistence <tier>`, or one of show, clear,
/// resume.
pub fn parse(args: &str) -> Result<Ask> {
    let a = args.trim();
    match a.to_lowercase().as_str() {
        "" | "status" | "show" => return Ok(Ask::Show),
        "clear" | "stop" | "off" | "cancel" | "drop" | "none" | "reset" => return Ok(Ask::Clear),
        "resume" | "continue" | "go" => return Ok(Ask::Resume),
        _ => {}
    }
    // A space in front, so flags alone (no outcome) are found too.
    let mut outcome = format!(" {a}");
    let mut check = None;
    let mut budget = None;
    let mut persistence = None;
    // Flags come after the outcome; take them off the end, last first.
    while let Some(i) = outcome.rfind(" --") {
        let tail = outcome[i + 3..].to_string();
        let (flag, value) = tail
            .split_once(char::is_whitespace)
            .map(|(f, v)| (f.to_string(), v.trim().to_string()))
            .unwrap_or((tail.clone(), String::new()));
        match flag.as_str() {
            "check" => {
                let v = unquote(&value);
                if v.is_empty() {
                    bail!("--check needs a command, e.g. --check \"cargo test\"");
                }
                check = Some(v);
            }
            "budget" => {
                let v = value.trim_start_matches('$').replace(',', "");
                let n: f64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--budget takes dollars, e.g. --budget 40"))?;
                if n <= 0.0 {
                    bail!("--budget takes dollars above 0");
                }
                budget = Some(n);
            }
            "persistence" | "persist" => {
                persistence = Some(
                    Persistence::parse(&value)
                        .context("--persistence is light, steady, hard or relentless")?,
                );
            }
            _ => break,
        }
        outcome.truncate(i);
    }
    let outcome = outcome.trim().to_string();
    if outcome.is_empty() {
        bail!("say what done looks like: /voyage <outcome>");
    }
    if outcome.chars().count() > OUTCOME_CHARS {
        bail!("an outcome is at most {OUTCOME_CHARS} characters");
    }
    Ok(Ask::Set {
        outcome,
        check,
        budget,
        persistence,
    })
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    for q in ['"', '\''] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

/// Start a voyage in this conversation (any open one there ends).
pub fn start(
    session: &str,
    cwd: &str,
    outcome: String,
    check: Option<String>,
    budget: Option<f64>,
    persistence: Persistence,
) -> Voyage {
    if let Some(mut old) = open_for(session) {
        end(&mut old, Status::Cleared, "a new voyage took its place");
    }
    let q = Voyage {
        id: new_id(),
        outcome,
        check,
        budget_usd: budget,
        cwd: cwd.to_string(),
        sessions: vec![session.to_string()],
        started_ms: registry::now_ms(),
        status: Status::Active,
        why: None,
        ended_ms: None,
        turns: 0,
        idle: 0,
        failures: 0,
        last: None,
        judge_usd: 0.0,
        spent_usd: 0.0,
        at_limit: false,
        progress: None,
        persistence,
        base: if persistence.rules().review {
            head_commit(cwd)
        } else {
            None
        },
        laps: 0,
        pushed: 0,
        best: 0,
        flat: 0,
    };
    save(&q);
    log(&q, "set", &q.outcome);
    q
}

fn end(q: &mut Voyage, status: Status, why: &str) {
    q.status = status;
    q.why = Some(why.to_string());
    q.ended_ms = Some(registry::now_ms());
    q.at_limit = false;
    if status == Status::Met {
        q.progress = Some(100);
        let path = landed_file(q.session());
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")));
        let _ = std::fs::write(path, serde_json::to_string(q).unwrap_or_default());
    }
    save(q);
    log(q, status.word(), why);
}

fn pause(q: &mut Voyage, why: &str) {
    q.status = Status::Paused;
    q.why = Some(why.to_string());
    save(q);
    log(q, "paused", why);
}

fn resume(q: &mut Voyage) {
    q.status = Status::Active;
    q.why = None;
    q.idle = 0;
    q.failures = 0;
    q.pushed = 0;
    save(q);
    log(q, "resumed", "");
}

/// A few lines on a voyage: what it is, how long, what the last check said.
pub fn describe(q: &Voyage, now: i64) -> String {
    let mut s = format!("voyage {} · {}\n  {}\n", q.id, q.status.word(), q.outcome);
    let took = registry::duration(q.ended_ms.unwrap_or(now) - q.started_ms);
    let mut facts = vec![
        took,
        turns(q.turns),
        format!("{} persistence", q.persistence.word()),
    ];
    if q.status == Status::Active && q.laps > 0 {
        facts.push(format!(
            "proof lap {} of {}",
            q.laps,
            q.persistence.rules().proof_laps
        ));
    }
    if q.handovers() > 0 {
        facts.push(format!(
            "{} handover{}",
            q.handovers(),
            if q.handovers() == 1 { "" } else { "s" }
        ));
    }
    if q.spent_usd > 0.0 {
        facts.push(match q.budget_usd {
            Some(b) => format!("${:.2} of ${b:.0}", q.spent_usd),
            None => format!("${:.2}", q.spent_usd),
        });
    } else if let Some(b) = q.budget_usd {
        facts.push(format!("budget ${b:.0}"));
    }
    s.push_str(&format!("  {}\n", facts.join(" · ")));
    if let Some(c) = &q.check {
        s.push_str(&format!("  done only when `{c}` passes\n"));
    }
    if let Some(w) = &q.why {
        s.push_str(&format!(
            "  {}: {w}\n",
            if q.status.open() {
                "waiting on you"
            } else {
                "why"
            }
        ));
    } else if let Some(l) = &q.last {
        s.push_str(&format!("  last check: {l}\n"));
    }
    s
}

/// `toomux hook prompt`, before anything else: `/voyage …` is handled here.
/// Show, clear and resume answer at once without a model turn; setting one
/// lets the prompt through, so the session starts on it.
pub fn prompt_hook(cfg: &Config, v: &Value) -> Option<String> {
    let prompt = v
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_start();
    let session = v.get("session_id").and_then(Value::as_str)?;
    let block = |reason: String| Some(json!({"decision": "block", "reason": reason}).to_string());
    let Some(rest) = prompt
        .strip_prefix("/voyage")
        .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
    else {
        // Only something the person actually typed carries a paused voyage on.
        // Claude Code also fires UserPromptSubmit for synthetic user-channel
        // events such as <task-notification>; those must leave it paused.
        if crate::index::user_authored_prompt(prompt).is_some()
            && !prompt.starts_with('/')
            && !prompt.starts_with("[toomux")
            && let Some(mut q) = open_for(session)
            && q.status == Status::Paused
            && !q.why.as_deref().is_some_and(|w| w.starts_with("budget"))
        {
            resume(&mut q);
        }
        return None;
    };
    let now = registry::now_ms();
    match parse(rest) {
        Err(e) => block(format!("toomux voyage: {e}")),
        Ok(Ask::Show) => block(match open_for(session) {
            Some(q) => describe(&q, now),
            None => "no voyage here. /voyage <outcome> sets one: toomux keeps this session at it, turn after turn and across handovers, until it's done.".into(),
        }),
        Ok(Ask::Clear) => block(match open_for(session) {
            Some(mut q) => {
                end(&mut q, Status::Cleared, "you cleared it");
                format!("voyage cleared: {}", q.outcome)
            }
            None => "no voyage here".into(),
        }),
        Ok(Ask::Resume) => match open_for(session) {
            Some(mut q) => {
                if q.why.as_deref().is_some_and(|w| w.starts_with("budget")) {
                    q.budget_usd = None;
                }
                resume(&mut q);
                // Through to the model, with the voyage in front of it.
                Some(json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": format!("toomux: the voyage carries on. {}", brief_line(&q))}}).to_string())
            }
            None => block("no voyage here to resume".into()),
        },
        Ok(Ask::Set { outcome, check, budget, persistence }) => {
            let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("");
            let q = start(session, cwd, outcome, check, budget, persistence.unwrap_or_else(|| cfg.persistence()));
            let mut said = format!(
                "toomux set voyage {} for this session. The outcome: {}\nWork toward it now and keep going. After each turn toomux checks \
                 whether it's reached (from what the conversation shows) and, if not, sends you back with what's missing. It carries on \
                 across handovers. When it's done, end the turn by showing the evidence: the command you ran and its result.",
                q.id, q.outcome
            );
            if let Some(c) = &q.check {
                said.push_str(&format!(" It only counts as done once `{c}` exits 0 in {}.", q.cwd));
            }
            if let Some(b) = q.budget_usd {
                said.push_str(&format!(" It stops at ${b:.0} spent."));
            }
            said.push_str(q.persistence.told());
            said.push_str(" If you reach a decision only the user can make, say so plainly and stop: toomux will ask them.");
            Some(json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": said}}).to_string())
        }
    }
}

/// One line to put the voyage in front of a session.
fn brief_line(q: &Voyage) -> String {
    let mut s = format!("Voyage {}: {}", q.id, q.outcome);
    if q.persistence != Persistence::Steady {
        s.push_str(&format!(" ({} persistence)", q.persistence.word()));
    }
    if let Some(c) = &q.check {
        s.push_str(&format!(" (done only when `{c}` exits 0)"));
    }
    if let Some(l) = &q.last {
        s.push_str(&format!(". Last check: {l}"));
    }
    s
}

// ---- after every turn --------------------------------------------------------

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Verdict {
    Met,
    NotYet,
    Impossible,
    NeedsYou,
}

/// `toomux hook stop`, once the handover has had its say: judge the turn and
/// either send the session back to its voyage or let it stop.
pub fn stop_hook(cfg: &Config, v: &Value) -> Option<String> {
    if std::env::var_os(JUDGE_ENV).is_some() {
        return None;
    }
    let session = v.get("session_id").and_then(Value::as_str)?;
    let mut q = open_for(session)?;
    if q.status != Status::Active || crate::handover::requested(session) {
        return None;
    }
    let transcript = PathBuf::from(v.get("transcript_path").and_then(Value::as_str)?);
    // The turn's last lines land a moment after the hook starts.
    std::thread::sleep(Duration::from_millis(300));
    let turn = last_turn(&transcript);
    q.turns += 1;
    q.at_limit = false;
    q.idle = if turn.tools == 0 { q.idle + 1 } else { 0 };
    q.spent_usd = spent(cfg, &q) + q.judge_usd;

    let say = |msg: String| Some(json!({"systemMessage": msg}).to_string());
    if let Some(b) = q.budget_usd
        && q.spent_usd >= b
    {
        let why = format!(
            "budget of ${b:.0} spent (${:.2}). /voyage resume carries on without one",
            q.spent_usd
        );
        pause(&mut q, &why);
        announce(
            cfg,
            &q,
            &format!("voyage paused, its ${b:.0} budget is spent"),
        );
        return say(format!(
            "toomux voyage paused: ${:.2} spent of its ${b:.0} budget. /voyage resume carries on without a budget.",
            q.spent_usd
        ));
    }
    let rules = q.persistence.rules();
    if q.idle >= rules.idle_turns {
        let n = rules.idle_turns;
        pause(
            &mut q,
            &format!("{n} turns in a row without doing anything"),
        );
        announce(cfg, &q, "voyage paused, it stopped making progress");
        return say(format!(
            "toomux voyage paused: {n} turns without a tool call. Say what to do next and it carries on."
        ));
    }

    let evidence = evidence(&transcript, turn.start, EVIDENCE_CHARS);
    let unchecked = |q: &mut Voyage, e: anyhow::Error| {
        q.failures += 1;
        log(q, "unchecked", &e.to_string());
        if q.failures >= rules.failures {
            pause(
                q,
                &format!(
                    "couldn't be checked {} times in a row ({e})",
                    rules.failures
                ),
            );
            announce(cfg, q, "voyage paused, it couldn't be checked");
            return say(format!(
                "toomux voyage paused: couldn't check it ({e}). /voyage resume tries again."
            ));
        }
        save(q);
        block(
            q,
            &format!("toomux couldn't check this turn ({e}), so keep going"),
        )
    };
    let (mut verdict, mut reason) = match judge(cfg, &q, &evidence) {
        Ok((v, r, progress, usd)) => {
            q.progress = progress.or(q.progress);
            q.judge_usd += usd;
            q.spent_usd += usd;
            q.failures = 0;
            (v, r)
        }
        Err(e) => return unchecked(&mut q, e),
    };
    // A "met" has to hold up: the check first (it's cheap and certain),
    // then the proof laps, then the review. Any of them failing makes it
    // "not yet".
    if verdict == Verdict::Met
        && let Some(c) = q.check.clone()
        && let Err(out) = run_check(&c, &q.cwd)
    {
        verdict = Verdict::NotYet;
        reason = format!("`{c}` didn't pass: {out}");
    }
    if verdict == Verdict::Met && q.laps < rules.proof_laps {
        q.laps += 1;
        q.last = Some(reason.clone());
        log(&q, "proof lap", &reason);
        save(&q);
        return block(&q, &proof_lap(q.laps, rules.proof_laps, &reason));
    }
    if verdict == Verdict::Met && rules.review {
        match review(cfg, &q, &evidence) {
            Ok((true, _, usd)) => {
                q.judge_usd += usd;
                q.spent_usd += usd;
            }
            Ok((false, why, usd)) => {
                q.judge_usd += usd;
                q.spent_usd += usd;
                verdict = Verdict::NotYet;
                reason = format!("a sceptical review of your changes found: {why}");
            }
            Err(e) => return unchecked(&mut q, e),
        }
    }
    // Stopping for you or giving up: first, a push back to try again.
    if matches!(verdict, Verdict::NeedsYou | Verdict::Impossible) && q.pushed < rules.push_backs {
        q.pushed += 1;
        q.last = Some(reason.clone());
        log(&q, "pushed back", &reason);
        save(&q);
        return block(&q, &push_back(verdict, &reason));
    }
    q.last = Some(reason.clone());
    match verdict {
        Verdict::NotYet => {
            q.laps = 0;
            log(&q, "not yet", &reason);
            let mut why = format!("not done yet: {reason}");
            let p = q.progress.unwrap_or(0);
            if p > q.best {
                (q.best, q.flat) = (p, 0);
            } else {
                q.flat += 1;
            }
            if let Some(n) = rules.stall_stop
                && q.flat >= n
            {
                let why = format!(
                    "{n} checks in a row with no headway, stuck at about {}%",
                    q.best
                );
                q.flat = 0;
                pause(&mut q, &why);
                announce(cfg, &q, "voyage paused, it's stuck");
                return say(format!(
                    "toomux voyage paused: {why}. Say how to get past it and it carries on."
                ));
            }
            if let Some(n) = rules.stall_after
                && q.flat > 0
                && q.flat.is_multiple_of(n)
            {
                log(&q, "stalled", &format!("{} checks at {}%", q.flat, q.best));
                why.push_str(&stall_nudge(q.flat / n, q.flat, q.best));
            }
            save(&q);
            block(&q, &why)
        }
        Verdict::Met => {
            end(&mut q, Status::Met, &reason);
            let took = registry::duration(registry::now_ms() - q.started_ms);
            announce(cfg, &q, &format!("voyage done after {took}"));
            say(format!(
                "toomux voyage done after {took}, {}: {reason}",
                turns(q.turns)
            ))
        }
        Verdict::Impossible => {
            end(&mut q, Status::Impossible, &reason);
            announce(cfg, &q, "voyage can't be done");
            say(format!("toomux voyage ended, it can't be done: {reason}"))
        }
        Verdict::NeedsYou => {
            pause(&mut q, &reason);
            announce(cfg, &q, "voyage needs you");
            say(format!(
                "toomux voyage waiting on you: {reason}. Your next message carries it on."
            ))
        }
    }
}

/// The judge said met; before it counts, the session proves it.
fn proof_lap(lap: u32, laps: u32, reason: &str) -> String {
    let task = if lap == 1 {
        "re-run what shows the outcome from a clean state (a fresh build and the full test run, not a cached or partial \
         one) and show the output. Read over your changes for anything left undone: TODOs, stubs, skipped tests, edge cases. \
         Fix what you find"
    } else {
        "try to break it. Test the edge cases and failure paths the outcome implies, use it end to end the way a user \
         would, and show the output. Fix what you find"
    };
    format!(
        "the judge says it's done ({reason}), but before it counts, prove it. Proof lap {lap} of {laps}: {task}"
    )
}

/// Before a voyage stops for you or gives up, the session tries once more.
fn push_back(verdict: Verdict, reason: &str) -> String {
    if verdict == Verdict::NeedsYou {
        format!(
            "the judge thinks this needs the user ({reason}). Before stopping for them: if you can settle it yourself with a \
             sensible choice that's easy to undo, make it, say what you chose and why, and carry on. Stop only if it truly \
             needs them: access, money, or a decision about what they want"
        )
    } else {
        format!(
            "the judge thinks this can't be done ({reason}). Before giving up, look hard for a way: another approach, a \
             workaround, a smaller path to the same outcome. If it truly can't be done, say exactly why and stop"
        )
    }
}

/// Stuck: told to change tack, more firmly each time.
fn stall_nudge(nth: u32, checks: u32, best: u8) -> String {
    if nth <= 1 {
        format!(
            ". You've been stuck at about {best}% for {checks} checks: step back. Say what's blocking you, question your \
             assumptions, and try a different approach rather than more of the same"
        )
    } else {
        format!(
            ". Still stuck at about {best}%, {checks} checks now. Stop and write down each approach you've tried and why it \
             failed. Then pick one you haven't tried: a smaller piece first, a fresh reading of the error, the docs or the \
             source, or a subagent with a clean look. If it truly needs the user, say so plainly"
        )
    }
}

fn block(q: &Voyage, why: &str) -> Option<String> {
    Some(
        json!({"decision": "block", "reason": format!(
            "[toomux voyage {}] {}.\nThe voyage: {}\nKeep going. When it's done, end the turn showing the evidence. If only the user can decide what comes next, say so plainly and stop.",
            q.id,
            why.trim_end_matches('.'),
            q.outcome
        )})
        .to_string(),
    )
}

/// "1h 20m, 5 turns": what a voyage took.
pub fn turns_taken(q: &Voyage) -> String {
    format!(
        "{}, {}",
        registry::duration(q.ended_ms.unwrap_or_else(registry::now_ms) - q.started_ms),
        turns(q.turns)
    )
}

fn turns(n: u32) -> String {
    format!("{n} turn{}", if n == 1 { "" } else { "s" })
}

fn announce(cfg: &Config, q: &Voyage, msg: &str) {
    let title = crate::state::State::load()
        .names
        .get(q.session())
        .cloned()
        .unwrap_or_else(|| short(&q.outcome, 50));
    crate::watch::announce_text(
        cfg,
        &format!("{title}: {msg}"),
        "voyage",
        &title,
        q.session(),
    );
}

fn short(s: &str, n: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        return one;
    }
    format!("{}…", one.chars().take(n - 1).collect::<String>())
}

/// What the chain has cost at list prices since the voyage began.
fn spent(cfg: &Config, q: &Voyage) -> f64 {
    q.sessions
        .iter()
        .map(|s| {
            crate::tokens::report(cfg, q.started_ms, Some(s))
                .all()
                .cost()
        })
        .sum()
}

mod evaluate;
use evaluate::{evidence, head_commit, judge, last_turn, review, run_check};
#[cfg(test)]
use evaluate::{parse_progress, parse_verdict};

// ---- across handovers and limits --------------------------------------------

/// A handover's fresh session takes the voyage on.
pub fn pass_on(old: &str, new: &str) {
    if let Some(mut q) = open_for(old) {
        q.sessions.push(new.to_string());
        save(&q);
        log(&q, "handed over", new);
    }
}

/// For the fresh session's first prompt.
pub fn for_successor(session: &str) -> Option<String> {
    let q = open_for(session)?;
    let state = if q.status == Status::Paused {
        " It was paused, waiting on the user; don't pick it up until they say so."
    } else {
        ""
    };
    Some(format!(
        " A voyage carries on with you: {}. toomux checks it after every turn and keeps you at it until it's done.{state}",
        brief_line(&q)
    ))
}

/// For the brief, so the voyage is in the record too.
pub const BRIEF_HEADING: &str = "\n\n## Voyage carried over by toomux";

pub fn brief_section(session: &str) -> Option<String> {
    let q = open_for(session)?;
    Some(format!(
        "{BRIEF_HEADING}\n\n{}",
        describe(&q, registry::now_ms())
    ))
}

/// From the status tick: a voyage whose session stopped at a usage limit
/// carries on once the limit lifts.
pub fn tick(sessions: &[Session], now: i64) {
    for mut q in open().into_iter().filter(|q| q.status == Status::Active) {
        let Some(s) = sessions.iter().find(|s| s.id == q.session() && !s.dormant) else {
            continue;
        };
        if s.limit.is_some() {
            if !q.at_limit {
                q.at_limit = true;
                save(&q);
                log(&q, "at limit", s.limit.as_deref().unwrap_or(""));
            }
            continue;
        }
        if !q.at_limit
            || s.state != St::Idle
            || now - s.since_ms < RESUME_AFTER_MS
            || crate::handover::requested(&s.id)
        {
            continue;
        }
        let Some(pane) = s.pane.as_ref().map(|p| p.id.clone()) else {
            continue;
        };
        if !crate::actions::prompt_empty(&pane) {
            continue;
        }
        q.at_limit = false;
        save(&q);
        log(&q, "limit lifted", "");
        let _ = crate::actions::type_prompt(
            &pane,
            &format!(
                "[toomux voyage {}] The usage limit has lifted: carry on. {}",
                q.id,
                brief_line(&q)
            ),
        );
    }
}

// ---- toomux voyage ------------------------------------------------------------

/// `toomux voyage`: every open voyage, then the last few that ended.
pub fn list(now: i64) -> String {
    let all = all();
    if all.is_empty() {
        return "no voyages yet. In any session, /voyage <outcome> sets one.\n".into();
    }
    let mut s = String::new();
    let (open, ended): (Vec<&Voyage>, Vec<&Voyage>) = all.iter().partition(|q| q.status.open());
    for q in &open {
        s.push_str(&describe(q, now));
        s.push('\n');
    }
    if !ended.is_empty() {
        s.push_str("ended\n");
        for q in ended.iter().take(10) {
            let took = registry::duration(q.ended_ms.unwrap_or(now) - q.started_ms);
            s.push_str(&format!(
                "  {}  {:10} {:>7} ago  {:>6}  {}\n",
                q.id,
                q.status.word(),
                registry::ago(now - q.ended_ms.unwrap_or(now)),
                took,
                short(&q.outcome, 60)
            ));
        }
    }
    s
}

/// `toomux voyage show <id>`: the voyage and every check, in order; its
/// scene first, `scene` cells wide, when there's a terminal to draw on.
pub fn show(target: &str, now: i64, scene: Option<usize>) -> Result<String> {
    let q = find(target)?;
    let mut s = String::new();
    if let Some(w) = scene.filter(|w| {
        *w >= crate::scene::MIN_WIDTH
            && matches!(q.status, Status::Active | Status::Paused | Status::Met)
    }) {
        for l in crate::scene::render(w.min(crate::scene::MAX_WIDTH), &q.scene(now)) {
            s.push_str(&l);
            s.push('\n');
        }
        s.push('\n');
    }
    s.push_str(&describe(&q, now));
    s.push('\n');
    let raw = std::fs::read_to_string(log_file(&q.id)).unwrap_or_default();
    for l in raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        let at = v.get("at_ms").and_then(Value::as_i64).unwrap_or(0);
        let when = chrono::DateTime::from_timestamp_millis(at)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%a %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        let what = v.get("what").and_then(Value::as_str).unwrap_or("");
        let turn = v.get("turn").and_then(Value::as_u64).unwrap_or(0);
        let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
        s.push_str(&format!(
            "  {when}  turn {turn:<3} {what:<12} {}\n",
            short(reason, 110)
        ));
    }
    Ok(s)
}

/// `toomux voyage set <session> <outcome>`: from outside the session. It
/// starts on it at once if it's at rest at an empty prompt.
pub fn set_from_outside(cfg: &Config, target: &str, words: &str) -> Result<String> {
    let Ask::Set {
        outcome,
        check,
        budget,
        persistence,
    } = parse(words)?
    else {
        bail!("say what done looks like")
    };
    let all = registry::load(cfg);
    let s = registry::find(&all, target)?;
    let q = start(
        &s.id,
        &s.cwd,
        outcome,
        check,
        budget,
        persistence.unwrap_or_else(|| cfg.persistence()),
    );
    let pane = s.pane.as_ref().map(|p| p.id.clone());
    match pane {
        // Finished is at rest too: a turn ended and nobody has answered yet.
        Some(p)
            if matches!(s.state, St::Idle | St::Finished | St::Background)
                && crate::actions::prompt_empty(&p) =>
        {
            crate::actions::type_prompt(
                &p,
                &format!(
                    "[toomux voyage {}] You have a voyage. {} Work toward it now and keep going; toomux checks after every turn.",
                    q.id,
                    brief_line(&q)
                ),
            )?;
            Ok(format!("voyage {} set on {} and started", q.id, s.title))
        }
        _ => Ok(format!(
            "voyage {} set on {}: it's checked from the end of its next turn",
            q.id, s.title
        )),
    }
}

pub fn clear(target: &str) -> Result<String> {
    let mut q = find(target)?;
    if !q.status.open() {
        bail!("voyage {} already ended ({})", q.id, q.status.word());
    }
    end(&mut q, Status::Cleared, "cleared from toomux");
    Ok(format!("voyage {} cleared", q.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voyage_arguments_parse() {
        assert_eq!(parse("").unwrap(), Ask::Show);
        assert_eq!(parse(" clear ").unwrap(), Ask::Clear);
        assert_eq!(parse("resume").unwrap(), Ask::Resume);
        assert_eq!(
            parse("all tests in auth pass --check \"cargo test auth\" --budget $40").unwrap(),
            Ask::Set {
                outcome: "all tests in auth pass".into(),
                check: Some("cargo test auth".into()),
                budget: Some(40.0),
                persistence: None
            }
        );
        assert_eq!(
            parse("the site builds --budget 25").unwrap(),
            Ask::Set {
                outcome: "the site builds".into(),
                check: None,
                budget: Some(25.0),
                persistence: None
            }
        );
        // A double dash inside the outcome that isn't a flag stays.
        assert_eq!(
            parse("rename --verbose to --loud").unwrap(),
            Ask::Set {
                outcome: "rename --verbose to --loud".into(),
                check: None,
                budget: None,
                persistence: None
            }
        );
        assert_eq!(
            parse("the parser reads every table --persistence Relentless --budget 60").unwrap(),
            Ask::Set {
                outcome: "the parser reads every table".into(),
                check: None,
                budget: Some(60.0),
                persistence: Some(Persistence::Relentless)
            }
        );
        assert!(parse("x --persistence stubborn").is_err());
        assert!(parse("x --budget lots").is_err());
        assert!(parse("--check \"make\"").is_err());
    }

    #[test]
    fn the_judges_estimate_is_read_whatever_its_shape() {
        assert_eq!(
            parse_progress(r#"{"verdict":"not_yet","reason":"x","progress":45}"#),
            Some(45)
        );
        assert_eq!(
            parse_progress(r#"{"verdict":"not_yet","reason":"x","progress":"60%"}"#),
            Some(60)
        );
        assert_eq!(
            parse_progress(r#"{"verdict":"met","reason":"x","progress":130}"#),
            Some(100)
        );
        assert_eq!(
            parse_progress(r#"{"verdict":"not_yet","reason":"x"}"#),
            None
        );
    }

    #[test]
    fn verdicts_parse_from_what_the_judge_says() {
        let (v, r) =
            parse_verdict("```json\n{\"verdict\":\"not_yet\",\"reason\":\"2 tests fail\"}\n```")
                .unwrap();
        assert_eq!((v, r.as_str()), (Verdict::NotYet, "2 tests fail"));
        assert_eq!(
            parse_verdict("{\"verdict\":\"met\",\"reason\":\"ok\"}")
                .unwrap()
                .0,
            Verdict::Met
        );
        assert_eq!(
            parse_verdict("{\"verdict\":\"needs you\",\"reason\":\"which db\"}")
                .unwrap()
                .0,
            Verdict::NeedsYou
        );
        assert!(parse_verdict("sure, looks done").is_err());
    }

    fn line(v: Value) -> String {
        v.to_string() + "\n"
    }

    #[test]
    fn a_turn_starts_at_a_prompt_or_a_stop_hook_and_evidence_keeps_the_newest() {
        let dir = std::env::temp_dir().join(format!("toomux-voyage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("t.jsonl");
        let mut s = String::new();
        s += &line(json!({"type": "user", "message": {"content": "make the tests pass"}}));
        s += &line(
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "on it"}, {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}}]}}),
        );
        s += &line(
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "2 failed", "is_error": true}]}}),
        );
        s += &line(
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "two fail"}]}}),
        );
        s += &line(
            json!({"type": "user", "isMeta": true, "message": {"content": "Stop hook feedback:\n[toomux voyage ab] not done yet"}}),
        );
        s += &line(
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "thinking"}]}}),
        );
        std::fs::write(&t, &s).unwrap();
        let turn = last_turn(&t);
        assert_eq!(
            (turn.start, turn.tools),
            (4, 0),
            "the stop hook started the last turn, which called nothing"
        );
        let e = evidence(&t, turn.start, 10_000);
        assert!(
            e.starts_with("[user] make the tests pass"),
            "the turn before comes too: {e}"
        );
        assert!(
            e.contains("[tool Bash] cargo test")
                && e.contains("[result, error] 2 failed")
                && e.contains("[toomux] Stop hook feedback"),
            "{e}"
        );
        let cut = evidence(&t, turn.start, 40);
        assert!(
            cut.ends_with("[claude] thinking") && cut.contains("cut"),
            "{cut}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_met_voyage_lands_on_its_status_line_for_a_while() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("toomux-voyage-landed-{}", std::process::id()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &root) };
        let mut q = start(
            "s9",
            "/tmp",
            "the docs build".into(),
            None,
            None,
            Persistence::Steady,
        );
        q.progress = Some(70);
        assert_eq!(q.scene(0).sea, crate::scene::Sea::Sailing);
        end(&mut q, Status::Met, "built");
        let now = registry::now_ms();
        let landed = landed_for("s9", now).unwrap();
        assert_eq!(
            (landed.progress, landed.scene(now).sea),
            (Some(100), crate::scene::Sea::Landed)
        );
        assert!(landed_for("s9", now + LANDED_MS + 1).is_none());
        assert!(!landed_file("s9").exists(), "an old landing is tidied away");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A stand-in `claude` that answers with the next of `answers`, and
    /// notes the model each call asked for.
    fn fake_judge(dir: &Path, answers: &[&str]) -> Config {
        std::fs::create_dir_all(dir).unwrap();
        let lines: Vec<String> = answers
            .iter()
            .map(|a| json!({"result": a, "total_cost_usd": 0.01}).to_string())
            .collect();
        std::fs::write(dir.join("answers"), lines.join("\n") + "\n").unwrap();
        let bin = dir.join("claude");
        let d = dir.display();
        std::fs::write(&bin, format!(
            "#!/bin/sh\nn=$(( $(cat {d}/n 2>/dev/null || echo 0) + 1 ))\necho $n > {d}/n\ncat > {d}/ask$n\necho \"$3\" >> {d}/models\nsed -n \"${{n}}p\" {d}/answers\n"
        )).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Config {
            claude_bin: bin.display().to_string(),
            ..Config::default()
        }
    }

    fn verdict(v: &str, reason: &str, progress: u8) -> String {
        json!({"verdict": v, "reason": reason, "progress": progress}).to_string()
    }

    #[test]
    fn each_persistence_makes_done_harder_to_reach_and_stopping_harder_to_do() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("toomux-voyage-grit-{}", std::process::id()));
        unsafe { std::env::set_var("XDG_STATE_HOME", root.join("state")) };
        let t = root.join("t.jsonl");
        std::fs::create_dir_all(&root).unwrap();
        // Every turn ran a command, so none of them is idle.
        std::fs::write(&t, line(json!({"type": "user", "message": {"content": "go"}}))
            + &line(json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}}]}}))).unwrap();
        let stop = |cfg: &Config, id: &str| -> Value {
            serde_json::from_str(
                &stop_hook(
                    cfg,
                    &json!({"session_id": id, "transcript_path": t.display().to_string()}),
                )
                .unwrap(),
            )
            .unwrap()
        };

        // Steady: the judge's met is the end of it.
        let cfg = fake_judge(&root.join("steady"), &[&verdict("met", "tests pass", 100)]);
        start(
            "st",
            "/tmp",
            "tests pass".into(),
            None,
            None,
            Persistence::Steady,
        );
        assert!(
            stop(&cfg, "st")["systemMessage"]
                .as_str()
                .unwrap()
                .contains("done")
        );

        // Hard: its own model judges, a met is sent back for one proof lap,
        // and a "needs you" gets one push back before it stops.
        let hard = root.join("hard");
        let mut cfg = fake_judge(
            &hard,
            &[
                &verdict("met", "tests pass", 95),
                &verdict("not_yet", "a test was skipped", 80),
                &verdict("needs_you", "which database?", 80),
                &verdict("needs_you", "still which database", 80),
            ],
        );
        cfg.voyage_hard_model = "claude-sonnet-5-5".into();
        start(
            "hd",
            "/tmp",
            "tests pass".into(),
            None,
            None,
            Persistence::Hard,
        );
        let lap = stop(&cfg, "hd");
        assert!(
            lap["reason"].as_str().unwrap().contains("Proof lap 1 of 1"),
            "{lap}"
        );
        assert!(
            open_for("hd")
                .unwrap()
                .chip(registry::now_ms())
                .contains("hard voyage, proof lap 1 of 1")
        );
        let back = stop(&cfg, "hd");
        assert!(
            back["reason"]
                .as_str()
                .unwrap()
                .contains("a test was skipped"),
            "{back}"
        );
        let ask = std::fs::read_to_string(hard.join("ask2")).unwrap();
        assert!(
            ask.contains("sent on proof lap 1 of 1"),
            "the judge knows a lap was asked for: {ask}"
        );
        assert_eq!(
            open_for("hd").unwrap().laps,
            0,
            "a not yet means the lap starts over"
        );
        let pushed = stop(&cfg, "hd");
        assert!(
            pushed["reason"]
                .as_str()
                .unwrap()
                .contains("settle it yourself"),
            "{pushed}"
        );
        assert_eq!(open_for("hd").unwrap().status, Status::Active);
        assert!(
            stop(&cfg, "hd")["systemMessage"]
                .as_str()
                .unwrap()
                .contains("waiting on you")
        );
        assert_eq!(open_for("hd").unwrap().status, Status::Paused);
        assert!(
            std::fs::read_to_string(hard.join("models"))
                .unwrap()
                .lines()
                .all(|m| m == "claude-sonnet-5-5"),
            "voyage_hard_model judges"
        );

        // A check runs before any proof lap: a met that fails it is not yet.
        let cfg = fake_judge(&root.join("checked"), &[&verdict("met", "tests pass", 95)]);
        start(
            "ck",
            "/tmp",
            "tests pass".into(),
            Some("false".into()),
            None,
            Persistence::Hard,
        );
        let failed = stop(&cfg, "ck");
        assert!(
            failed["reason"]
                .as_str()
                .unwrap()
                .contains("`false` didn't pass"),
            "{failed}"
        );
        assert_eq!(open_for("ck").unwrap().laps, 0);

        // Steady: a nudge after 4 checks with no headway, a pause after 8.
        let flat: Vec<String> = (0..9)
            .map(|_| verdict("not_yet", "the parser still fails", 30))
            .collect();
        let cfg = fake_judge(
            &root.join("stuck"),
            &flat.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        start(
            "sk",
            "/tmp",
            "the parser works".into(),
            None,
            None,
            Persistence::Steady,
        );
        let said: Vec<Value> = (0..9).map(|_| stop(&cfg, "sk")).collect();
        assert!(
            said[4]["reason"]
                .as_str()
                .unwrap()
                .contains("stuck at about 30% for 4 checks"),
            "{}",
            said[4]
        );
        assert!(!said[3]["reason"].as_str().unwrap().contains("stuck"));
        assert!(
            said[8]["systemMessage"]
                .as_str()
                .unwrap()
                .contains("paused: 8 checks in a row with no headway"),
            "{}",
            said[8]
        );
        assert_eq!(open_for("sk").unwrap().status, Status::Paused);

        // Relentless: opus, two proof laps, then a sceptical review that
        // can still say not yet; three checks at one estimate get a nudge.
        let rel = root.join("relentless");
        let cfg = fake_judge(
            &rel,
            &[
                &verdict("met", "done", 90),
                &verdict("met", "done again", 90),
                &verdict("met", "done a third time", 90),
                &verdict("not_yet", "a TODO is left in parse.rs", 0),
                &verdict("not_yet", "still the TODO", 90),
                &verdict("not_yet", "still the TODO", 90),
                &verdict("not_yet", "still the TODO", 90),
                &verdict("not_yet", "still the TODO", 90),
            ],
        );
        start(
            "rl",
            "/tmp",
            "tests pass".into(),
            None,
            None,
            Persistence::Relentless,
        );
        assert!(
            stop(&cfg, "rl")["reason"]
                .as_str()
                .unwrap()
                .contains("Proof lap 1 of 2")
        );
        assert!(
            stop(&cfg, "rl")["reason"]
                .as_str()
                .unwrap()
                .contains("Proof lap 2 of 2")
        );
        let reviewed = stop(&cfg, "rl");
        assert!(
            reviewed["reason"]
                .as_str()
                .unwrap()
                .contains("sceptical review of your changes found: a TODO is left"),
            "{reviewed}"
        );
        let (a, b) = (stop(&cfg, "rl"), stop(&cfg, "rl"));
        assert!(
            !a["reason"].as_str().unwrap().contains("stuck")
                && b["reason"]
                    .as_str()
                    .unwrap()
                    .contains("stuck at about 90% for 2 checks"),
            "{b}"
        );
        let (_, d) = (stop(&cfg, "rl"), stop(&cfg, "rl"));
        assert!(
            d["reason"]
                .as_str()
                .unwrap()
                .contains("Still stuck at about 90%, 4 checks now"),
            "the second nudge is firmer: {d}"
        );
        assert!(
            std::fs::read_to_string(rel.join("models"))
                .unwrap()
                .lines()
                .all(|m| m == "opus")
        );

        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_voyage_is_set_shown_carried_and_cleared_from_the_prompt() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("toomux-voyage-state-{}", std::process::id()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &root) };
        let cfg = Config::default();
        let hook = |p: &str| {
            prompt_hook(
                &cfg,
                &json!({"session_id": "s1", "cwd": "/tmp", "prompt": p}),
            )
        };

        let out: Value =
            serde_json::from_str(&hook("/voyage the docs build --budget 10").unwrap()).unwrap();
        let said = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            said.contains("the docs build") && said.contains("$10"),
            "{said}"
        );
        let q = open_for("s1").unwrap();
        assert_eq!((q.status, q.budget_usd), (Status::Active, Some(10.0)));

        let shown: Value = serde_json::from_str(&hook("/voyage").unwrap()).unwrap();
        assert_eq!(shown["decision"], "block");
        assert!(shown["reason"].as_str().unwrap().contains("the docs build"));
        assert!(hook("/voyager idea").is_none(), "only /voyage itself");

        pass_on("s1", "s2");
        assert!(open_for("s1").is_none());
        assert!(for_successor("s2").unwrap().contains("the docs build"));
        let mut q = open_for("s2").unwrap();
        pause(&mut q, "which database?");
        // Claude's background-task completion arrives through UserPromptSubmit,
        // but it is not the user's answer and must not resume the voyage.
        assert!(
            prompt_hook(
                &cfg,
                &json!({"session_id": "s2", "prompt": "<task-notification>agent finished</task-notification>"}),
            )
            .is_none()
        );
        assert_eq!(open_for("s2").unwrap().status, Status::Paused);
        // Your next message carries a paused voyage on.
        assert!(
            prompt_hook(&cfg, &json!({"session_id": "s2", "prompt": "use postgres"})).is_none()
        );
        assert_eq!(open_for("s2").unwrap().status, Status::Active);

        let cleared: Value = serde_json::from_str(
            &prompt_hook(
                &cfg,
                &json!({"session_id": "s2", "prompt": "/voyage clear"}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            cleared["reason"]
                .as_str()
                .unwrap()
                .starts_with("voyage cleared")
        );
        assert!(open_for("s2").is_none());
        assert_eq!(all()[0].status, Status::Cleared);
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let _ = std::fs::remove_dir_all(&root);
    }
}
