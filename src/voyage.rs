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
changed. Earlier results, summaries and promises don't count, except what toomux's earlier checks confirmed (listed as \
such). Say not_yet if you see tests skipped, disabled, weakened or deleted, stubs, TODOs, hard-coded or special-cased \
results, errors or warnings passed over, or any part of the outcome not addressed. Say impossible or needs_you only when nothing the agent could do would get round it.";

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
                // One try to settle it before stopping for you: a mention of
                // a decision shouldn't stop work that can go on.
                push_backs: 1,
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

/// Why a voyage paused. Only a pause toomux made because the session seemed
/// to stop (idle turns, no headway) may be lifted by something other than
/// you: a background task finishing shows the work went on. A pause for your
/// decision, for the budget, for checks that couldn't be made, or one you
/// made yourself, lifts only on your word.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// The judge said only you can decide what comes next.
    NeedsYou,
    /// You paused it (from toomux).
    You,
    Budget,
    /// Checks in a row that couldn't be made.
    Unchecked,
    /// Turns in a row without a tool call.
    Idle,
    /// Checks in a row with no headway.
    Stall,
}

impl PauseReason {
    /// Whether a finished background task may carry the voyage on.
    pub fn lifts_by_itself(self) -> bool {
        matches!(self, PauseReason::Idle | PauseReason::Stall)
    }
}

/// The Claude Code process a voyage is judged from. Two live copies of one
/// conversation would otherwise both judge it and race.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub pid: i32,
    #[serde(default)]
    pub start: Option<String>,
}

impl Owner {
    fn alive(&self) -> bool {
        registry::alive(self.pid, self.start.as_deref())
    }
}

/// Part of the outcome a check confirmed from evidence it was shown, kept so
/// later checks (and later conversations) know it's done.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Milestone {
    pub text: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub turn: u32,
    #[serde(default)]
    pub at_ms: i64,
}

/// At most this many milestones are kept, newest last.
const LEDGER_MAX: usize = 30;
const MILESTONE_CHARS: usize = 200;

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
    ///
    /// The contract with the limit's recovery: a voyage at a limit stays
    /// `Active` with `at_limit` set (never `Paused`, so nothing waits on you
    /// and `pause_reason` stays as it was). The Stop hook sets it, without
    /// judging or counting the turn, whenever the session's transcript shows
    /// it at a limit or the judge's own run is refused for one; `tick`
    /// clears it and carries the voyage on once the limit has reset.
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
    /// Why it's paused (absent in voyages from before it was kept: see
    /// [`Voyage::paused_for`]).
    #[serde(default)]
    pub pause_reason: Option<PauseReason>,
    /// The process it's judged from; claimed afresh once that one is gone.
    #[serde(default)]
    pub owner: Option<Owner>,
    /// Since when its session has been waiting on background work (agents,
    /// monitors, background commands): turns end unjudged until that work
    /// reports back.
    #[serde(default)]
    pub awaiting_ms: Option<i64>,
    /// What checks have confirmed done, oldest first.
    #[serde(default)]
    pub ledger: Vec<Milestone>,
    /// The cost of its first `spent_closed` conversations, which have ended
    /// and won't cost more: added up once, not every turn.
    #[serde(default)]
    pub spent_closed: usize,
    #[serde(default)]
    pub spent_closed_usd: f64,
}

impl Voyage {
    pub fn session(&self) -> &str {
        self.sessions.last().map(String::as_str).unwrap_or("")
    }
    pub fn handovers(&self) -> usize {
        self.sessions.len().saturating_sub(1)
    }
    /// Why it's paused, if it is. Voyages paused before the reason was kept
    /// have it read back from what they said.
    pub fn paused_for(&self) -> Option<PauseReason> {
        if self.status != Status::Paused {
            return None;
        }
        if self.pause_reason.is_some() {
            return self.pause_reason;
        }
        let why = self.why.as_deref().unwrap_or("");
        Some(if why.starts_with("budget") {
            PauseReason::Budget
        } else if why.contains("turns in a row without doing anything") {
            PauseReason::Idle
        } else if why.contains("checks in a row with no headway") {
            PauseReason::Stall
        } else if why.starts_with("couldn't be checked") {
            PauseReason::Unchecked
        } else {
            PauseReason::NeedsYou
        })
    }
    /// "hard voyage 1h 20m", "steady voyage paused": the chip beside a
    /// session, its persistence first.
    pub fn chip(&self, now: i64) -> String {
        let tier = self.persistence.word();
        match self.status {
            Status::Active if self.at_limit => format!("{tier} voyage waits for the limit"),
            Status::Active if self.awaiting_ms.is_some() => {
                format!("{tier} voyage waits on background work")
            }
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

/// Every read/modify/write of a voyage holds this, so the Stop hook, the
/// prompt hook, `toomux voyage` and a handover's carry (several processes)
/// never write over each other. Never held across a judge or a check: those
/// run unlocked, and the voyage is read again before their verdict is saved.
/// Not re-entrant: take it only at the entry points, never inside a helper.
fn lock() -> Option<std::fs::File> {
    crate::lock::exclusive(&dir().join(".lock")).ok()
}

/// Whether voyage `id` has ended (its file is in ended/).
fn ended(id: &str) -> bool {
    ended_dir().join(format!("{id}.json")).exists()
}

/// Voyage `id` as it is now, if it's still open.
fn load(id: &str) -> Option<Voyage> {
    if ended(id) {
        return None;
    }
    let raw = std::fs::read_to_string(dir().join(format!("{id}.json"))).ok()?;
    serde_json::from_str::<Voyage>(&raw)
        .ok()
        .filter(|q| q.status.open())
}

/// Write a voyage where it belongs. An ended voyage is never brought back:
/// a stale copy of one that was open (a Stop hook that judged while you
/// cleared it) is dropped, not saved as a second, live voyage.
fn save(q: &Voyage) {
    if q.status.open() && ended(&q.id) {
        let _ = std::fs::remove_file(dir().join(format!("{}.json", q.id)));
        return;
    }
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
        .filter(|q| q.status.open() && !ended(&q.id))
        .collect();
    out.sort_by_key(|q: &Voyage| std::cmp::Reverse(q.started_ms));
    out
}

/// The open voyage this conversation is on.
pub fn open_for(session: &str) -> Option<Voyage> {
    open().into_iter().find(|q| q.session() == session)
}

/// The process a hook runs for, as an owner, with the session it records.
type HookProcess = Option<(Owner, Option<String>)>;

fn hook_process(cfg: &Config) -> HookProcess {
    lineage::hook_process(cfg).map(|(pid, start, id)| (Owner { pid, start }, id))
}

/// The open voyage conversation `session` is on, following the conversation
/// when its id moved on: a voyage whose conversation continued in this one
/// (Claude Code's `continued-in`), whose handover handed over to it (before
/// the handover's carry passes it on), or whose own process has moved on to
/// this new conversation. A voyage found that way is rebound to `session`.
/// Hold the lock.
fn bind(
    cfg: &Config,
    session: &str,
    transcript: Option<&Path>,
    me: &HookProcess,
) -> Option<Voyage> {
    let open = open();
    if let Some(q) = open.iter().find(|q| q.session() == session) {
        return Some(q.clone());
    }
    for q in &open {
        let next = lineage::successors(cfg, &q.cwd, q.session(), transcript);
        if let Some(at) = next.iter().position(|s| s == session) {
            let mut q = q.clone();
            rebind(&mut q, &next[..=at], "continued in");
            return Some(q);
        }
    }
    if let Some(prev) = transcript.and_then(lineage::handed_over_from_transcript)
        && let Some(q) = open.iter().find(|q| q.session() == prev)
    {
        let mut q = q.clone();
        rebind(&mut q, &[session.to_string()], "handed over");
        return Some(q);
    }
    // The same Claude Code process, now in a new conversation (begun after
    // the voyage's last one was last written: a resumed older conversation
    // isn't this voyage's).
    let (me, my_session) = me.as_ref()?;
    if my_session.as_deref() != Some(session) {
        return None;
    }
    let began = transcript.and_then(lineage::began_ms)?;
    let q = open.iter().find(|q| {
        q.owner.as_ref() == Some(me)
            && lineage::transcript_of(cfg, &q.cwd, q.session(), transcript)
                .and_then(|p| lineage::modified_ms(&p))
                .is_some_and(|last| began + 5_000 >= last)
    })?;
    let mut q = q.clone();
    rebind(&mut q, &[session.to_string()], "moved on in its process to");
    Some(q)
}

/// Carry a voyage on to the conversations its own went on in.
fn rebind(q: &mut Voyage, next: &[String], how: &str) {
    for s in next {
        if q.session() != s {
            q.sessions.push(s.clone());
        }
    }
    save(q);
    log(q, "rebound", &format!("{how} {}", q.session()));
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

/// `/voyage` arguments: an outcome, then optional flags; or one of show,
/// clear, resume.
///
/// The grammar: the outcome is free text up to the first known flag
/// (`--check`, `--budget`, `--persistence`/`--persist`, each also as
/// `--flag=value`) standing as a word of its own. From there on the text is
/// read as shell words, quotes respected, and must be flags:
///
/// - `--check` takes everything up to the next known flag. One word (quoted
///   if it has spaces) is the command; several words are the command as
///   typed, quotes kept, so `--check cargo test -- --nocapture` works. A
///   command containing a known flag itself (`git diff --check`) must be
///   quoted, or it's an error.
/// - `--budget` takes one number of dollars, `--persistence` one tier.
///
/// Anything else after the first flag is an error, never silently dropped
/// or folded into the outcome.
pub fn parse(args: &str) -> Result<Ask> {
    let a = args.trim();
    match a.to_lowercase().as_str() {
        "" | "status" | "show" => return Ok(Ask::Show),
        "clear" | "stop" | "off" | "cancel" | "drop" | "none" | "reset" => return Ok(Ask::Clear),
        "resume" | "continue" | "go" => return Ok(Ask::Resume),
        _ => {}
    }
    let at = first_flag(a).unwrap_or(a.len());
    let words = shell_words(&a[at..])?;
    set_from_words(&a[..at], words)
}

/// `toomux voyage set` arguments, already split into words by the shell:
/// the outcome's words up to the first known flag, then flags as for
/// [`parse`]. One word with the flags inside it reads as `/voyage` would.
pub fn parse_words(words: &[String]) -> Result<Ask> {
    let Some(at) = words.iter().position(|w| flag_word(w).is_some()) else {
        return parse(&words.join(" "));
    };
    let words_after = words[at..]
        .iter()
        .map(|w| Word {
            text: w.clone(),
            // The shell has taken its quotes off already: a flag is a flag.
            raw: if flag_word(w).is_some() {
                w.clone()
            } else {
                shell_quote(w)
            },
        })
        .collect();
    set_from_words(&words[..at].join(" "), words_after)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Flag {
    Check,
    Budget,
    Persistence,
}

const FLAGS: [(&str, Flag); 4] = [
    ("--check", Flag::Check),
    ("--budget", Flag::Budget),
    ("--persistence", Flag::Persistence),
    ("--persist", Flag::Persistence),
];

/// A known flag, and its `=value` if it came with one.
fn flag_word(w: &str) -> Option<(Flag, Option<String>)> {
    FLAGS.iter().find_map(|(name, f)| {
        let rest = w.strip_prefix(name)?;
        if rest.is_empty() {
            Some((*f, None))
        } else {
            rest.strip_prefix('=').map(|v| (*f, Some(v.to_string())))
        }
    })
}

/// Where the first known flag stands as a word of its own.
fn first_flag(a: &str) -> Option<usize> {
    let mut after_space = true;
    for (i, c) in a.char_indices() {
        if after_space
            && c == '-'
            && flag_word(a[i..].split(char::is_whitespace).next().unwrap_or("")).is_some()
        {
            return Some(i);
        }
        after_space = c.is_whitespace();
    }
    None
}

/// A shell word: what it means, and how it was typed.
#[derive(Debug, Clone)]
struct Word {
    text: String,
    raw: String,
}

impl Word {
    /// A known flag typed bare (a quoted "--check" is someone's text).
    fn flag(&self) -> Option<(Flag, Option<String>)> {
        flag_word(&self.text).filter(|_| self.raw.starts_with("--"))
    }
}

/// Split text into shell words: whitespace between, '…' as it is, "…" with
/// \" \\ \$ \` escapes, and a backslash outside quotes escaping the next
/// character. An unclosed quote is an error.
fn shell_words(s: &str) -> Result<Vec<Word>> {
    let mut out = Vec::new();
    let mut it = s.char_indices().peekable();
    while let Some(&(start, c)) = it.peek() {
        if c.is_whitespace() {
            it.next();
            continue;
        }
        let mut text = String::new();
        let mut end = s.len();
        while let Some(&(i, c)) = it.peek() {
            if c.is_whitespace() {
                end = i;
                break;
            }
            it.next();
            match c {
                '\'' => loop {
                    match it.next() {
                        Some((_, '\'')) => break,
                        Some((_, ch)) => text.push(ch),
                        None => bail!("a ' isn't closed in: {}", &s[start..]),
                    }
                },
                '"' => loop {
                    match it.next() {
                        Some((_, '"')) => break,
                        Some((_, '\\')) => match it.next() {
                            Some((_, ch @ ('"' | '\\' | '$' | '`'))) => text.push(ch),
                            Some((_, ch)) => {
                                text.push('\\');
                                text.push(ch);
                            }
                            None => bail!("a \" isn't closed in: {}", &s[start..]),
                        },
                        Some((_, ch)) => text.push(ch),
                        None => bail!("a \" isn't closed in: {}", &s[start..]),
                    }
                },
                '\\' => {
                    if let Some((_, ch)) = it.next() {
                        text.push(ch);
                    }
                }
                ch => text.push(ch),
            }
        }
        out.push(Word {
            text,
            raw: s[start..end].to_string(),
        });
    }
    Ok(out)
}

/// A word as it would be typed to a shell.
fn shell_quote(w: &str) -> String {
    let plain = !w.is_empty()
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', r"'\''"))
    }
}

/// The outcome and the flags after it (see [`parse`] for the grammar).
fn set_from_words(outcome: &str, words: Vec<Word>) -> Result<Ask> {
    let (mut check, mut budget, mut persistence) = (None, None, None);
    let mut i = 0;
    while i < words.len() {
        let Some((flag, inline)) = words[i].flag() else {
            bail!(
                "`{}` isn't a voyage flag. The outcome comes first, then --check \"<command>\", --budget <dollars> \
                 and --persistence <tier>; quote a --check command that itself contains one of those flags",
                words[i].raw
            );
        };
        let name = FLAGS
            .iter()
            .find(|(_, f)| *f == flag)
            .map_or("", |(n, _)| *n);
        i += 1;
        let mut values: Vec<Word> = Vec::new();
        if let Some(v) = inline {
            values.push(Word {
                raw: shell_quote(&v),
                text: v,
            });
        } else {
            while i < words.len() && words[i].flag().is_none() {
                values.push(words[i].clone());
                i += 1;
            }
        }
        let raw = |ws: &[Word]| {
            ws.iter()
                .map(|w| w.raw.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let one = |what: &str| -> Result<String> {
            match values.as_slice() {
                [v] => Ok(v.text.clone()),
                [] => bail!("{name} needs {what}"),
                more => bail!("{name} takes one value, not `{}`", raw(more)),
            }
        };
        match flag {
            Flag::Check => {
                if check.is_some() {
                    bail!(
                        "--check is given twice: quote a command that contains --check itself, e.g. --check \"git diff --check\""
                    );
                }
                // One word is the command; several are the command as typed.
                let v = match values.as_slice() {
                    [v] => v.text.trim().to_string(),
                    more => raw(more),
                };
                if v.is_empty() {
                    bail!("--check needs a command, e.g. --check \"cargo test\"");
                }
                check = Some(v);
            }
            Flag::Budget => {
                if budget.is_some() {
                    bail!("--budget is given twice");
                }
                let v = one("dollars, e.g. --budget 40")?;
                let n: f64 = v
                    .trim_start_matches('$')
                    .replace(',', "")
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--budget takes dollars, e.g. --budget 40"))?;
                if !(n > 0.0 && n.is_finite()) {
                    bail!("--budget takes dollars above 0");
                }
                budget = Some(n);
            }
            Flag::Persistence => {
                if persistence.is_some() {
                    bail!("--persistence is given twice");
                }
                let v = one("a tier: light, steady, hard or relentless")?;
                persistence = Some(
                    Persistence::parse(&v)
                        .context("--persistence is light, steady, hard or relentless")?,
                );
            }
        }
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

/// Start a voyage in this conversation (any open one there ends).
pub fn start(
    session: &str,
    cwd: &str,
    outcome: String,
    check: Option<String>,
    budget: Option<f64>,
    persistence: Persistence,
) -> Voyage {
    let _lock = lock();
    begin(session, cwd, outcome, check, budget, persistence, None)
}

/// [`start`], owned by a process. Hold the lock.
fn begin(
    session: &str,
    cwd: &str,
    outcome: String,
    check: Option<String>,
    budget: Option<f64>,
    persistence: Persistence,
    owner: Option<Owner>,
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
        pause_reason: None,
        owner,
        awaiting_ms: None,
        ledger: Vec::new(),
        spent_closed: 0,
        spent_closed_usd: 0.0,
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
    q.awaiting_ms = None;
    if status == Status::Met {
        q.progress = Some(100);
        let path = landed_file(q.session());
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")));
        let _ = std::fs::write(path, serde_json::to_string(q).unwrap_or_default());
    }
    save(q);
    log(q, status.word(), why);
}

fn pause(q: &mut Voyage, reason: PauseReason, why: &str) {
    q.status = Status::Paused;
    q.pause_reason = Some(reason);
    q.why = Some(why.to_string());
    q.awaiting_ms = None;
    save(q);
    log(q, "paused", why);
}

fn resume(q: &mut Voyage, how: &str) {
    q.status = Status::Active;
    q.why = None;
    q.pause_reason = None;
    q.idle = 0;
    q.failures = 0;
    q.pushed = 0;
    save(q);
    log(q, "resumed", how);
}

/// A few lines on a voyage: what it is, how long, what the last check said.
pub fn describe(q: &Voyage, now: i64) -> String {
    describe_as(q, now, None)
}

/// [`describe`], with what toomux found about its conversation (an orphan's
/// note) in place of its status.
fn describe_as(q: &Voyage, now: i64, note: Option<(&str, String)>) -> String {
    let word = note.as_ref().map_or(q.status.word(), |(w, _)| w);
    let mut s = format!("voyage {} · {word}\n  {}\n", q.id, q.outcome);
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
    if let Some((_, n)) = &note {
        s.push_str(&format!("  {n}\n"));
    }
    if q.status == Status::Active
        && let Some(at) = q.awaiting_ms
    {
        s.push_str(&format!(
            "  waiting on background work for {}: the next turn after it reports back is checked\n",
            registry::duration(now - at)
        ));
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

/// Whether a prompt is a background task reporting it has finished.
fn task_finished(prompt: &str) -> bool {
    prompt.starts_with("<task-notification")
        && prompt
            .split("<status>")
            .nth(1)
            .and_then(|r| r.split("</status>").next())
            .is_some_and(|s| s.trim() != "running")
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
    let transcript = v
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let block = |reason: String| Some(json!({"decision": "block", "reason": reason}).to_string());
    let Some(rest) = prompt
        .strip_prefix("/voyage")
        .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
    else {
        if open().is_empty() {
            return None;
        }
        let _lock = lock();
        // A handover's successor takes the voyage on at its first prompt
        // (the handover's own carry may come later). That prompt is
        // toomux's, not yours: a paused voyage stays paused.
        if let Some(prev) = lineage::handed_over_from(prompt) {
            if open_for(session).is_none()
                && let Some(mut q) = open_for(&prev)
            {
                rebind(&mut q, &[session.to_string()], "handed over");
            }
            return None;
        }
        let me = hook_process(cfg);
        let mut q = bind(cfg, session, transcript.as_deref(), &me)?;
        if q.status != Status::Paused {
            return None;
        }
        let reason = q.paused_for();
        // Only something the person actually typed carries a paused voyage
        // on. Claude Code also fires UserPromptSubmit for synthetic
        // user-channel events such as <task-notification>; those leave a
        // pause for you as it is. But a pause toomux made because the
        // session seemed to have stopped is lifted by background work
        // finishing: the work went on, and this turn is checked.
        if crate::index::user_authored_prompt(prompt).is_some()
            && !prompt.starts_with('/')
            && !prompt.starts_with("[toomux")
            && reason != Some(PauseReason::Budget)
        {
            resume(&mut q, "");
        } else if task_finished(prompt) && reason.is_some_and(PauseReason::lifts_by_itself) {
            resume(&mut q, "background work finished");
        }
        return None;
    };
    let now = registry::now_ms();
    let _lock = lock();
    let me = hook_process(cfg);
    let here = |me: &HookProcess| bind(cfg, session, transcript.as_deref(), me);
    match parse(rest) {
        Err(e) => block(format!("toomux voyage: {e}")),
        Ok(Ask::Show) => block(match here(&me) {
            Some(q) => describe(&q, now),
            None => "no voyage here. /voyage <outcome> sets one: toomux keeps this session at it, turn after turn and across handovers, until it's done.".into(),
        }),
        Ok(Ask::Clear) => block(match here(&me) {
            Some(mut q) => {
                end(&mut q, Status::Cleared, "you cleared it");
                format!("voyage cleared: {}", q.outcome)
            }
            None => "no voyage here".into(),
        }),
        Ok(Ask::Resume) => match here(&me) {
            Some(mut q) => {
                if q.paused_for() == Some(PauseReason::Budget) {
                    q.budget_usd = None;
                }
                resume(&mut q, "");
                // Through to the model, with the voyage in front of it.
                Some(json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": format!("toomux: the voyage carries on. {}", brief_line(&q))}}).to_string())
            }
            None => block("no voyage here to resume".into()),
        },
        Ok(Ask::Set { outcome, check, budget, persistence }) => {
            let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("");
            let owner = me.map(|(o, _)| o);
            let q = begin(session, cwd, outcome, check, budget, persistence.unwrap_or_else(|| cfg.persistence()), owner);
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

/// Claude Code waits this long for the Stop hook (setup.rs installs it with
/// a 600s timeout). Everything the hook does, the judge, the check and the
/// review, fits within this, with room to spare.
const HOOK_BUDGET: Duration = Duration::from_secs(540);
/// With less than this left, a step isn't started: the check is deferred.
const STEP_MIN: Duration = Duration::from_secs(20);

/// What judging the turn found.
enum Found {
    Judged(Judged),
    /// No time was left to judge it.
    Deferred(String),
    /// The judge's own run was refused at a usage limit.
    AtLimit(String),
    /// It couldn't be judged (and what that cost).
    Unchecked(String, f64),
}

struct Judged {
    verdict: Verdict,
    reason: String,
    progress: Option<u8>,
    usd: f64,
    /// Parts of the outcome the judge saw shown done.
    milestones: Vec<String>,
    /// For "needs you": there's still work that doesn't wait on the user.
    other_work: bool,
    /// The judge said met, but the check or review couldn't finish in time.
    deferred: Option<String>,
}

/// `toomux hook stop`, once the handover has had its say: judge the turn and
/// either send the session back to its voyage or let it stop.
///
/// It reads and counts the turn under the voyage lock, judges unlocked (that
/// can take minutes), then reads the voyage again under the lock and applies
/// the verdict only if it's still on and still this conversation's: a voyage
/// cleared, paused or handed over meanwhile is left as it now is.
pub fn stop_hook(cfg: &Config, v: &Value) -> Option<String> {
    if std::env::var_os(JUDGE_ENV).is_some() || open().is_empty() {
        return None;
    }
    stop_as(cfg, v, hook_process(cfg))
}

/// [`stop_hook`], for the Claude Code process `me`.
fn stop_as(cfg: &Config, v: &Value, me: HookProcess) -> Option<String> {
    let began = Instant::now();
    let session = v.get("session_id").and_then(Value::as_str)?;
    let transcript = PathBuf::from(v.get("transcript_path").and_then(Value::as_str)?);
    // The turn's last lines land a moment after the hook starts.
    std::thread::sleep(Duration::from_millis(300));
    let say = |msg: String| Some(json!({"systemMessage": msg}).to_string());
    let (snap, turn_start) = {
        let _lock = lock();
        let mut q = bind(cfg, session, Some(&transcript), &me)?;
        if q.status != Status::Active || crate::handover::requested(session) {
            return None;
        }
        // One process judges: another live copy of this conversation lets
        // its turns end unjudged. An owner that's gone is replaced.
        if let Some((mine, _)) = &me {
            match &q.owner {
                Some(o) if o != mine && o.alive() => return None,
                Some(o) if o == mine => {}
                _ => q.owner = Some(mine.clone()),
            }
        }
        // At a usage limit there's nothing to judge, and the judge's own run
        // would be refused too: it waits, uncounted, for tick to carry it on.
        if let Some(limit) = crate::transcript::meta(&transcript).limit() {
            if !q.at_limit {
                q.at_limit = true;
                log(&q, "at limit", &limit);
            }
            save(&q);
            return None;
        }
        q.at_limit = false;
        // Waiting on its own background work (agents, monitors, background
        // commands): the turn ends unjudged and uncounted, and the turn the
        // work's report starts is checked instead. Judging now would send it
        // round no-op polling turns that count as idle and as no headway.
        if let Some(what) = background(session, &transcript) {
            if q.awaiting_ms.is_none() {
                q.awaiting_ms = Some(registry::now_ms());
                log(&q, "waits", &what);
            }
            save(&q);
            return None;
        }
        q.awaiting_ms = None;
        let turn = last_turn(&transcript);
        q.turns += 1;
        q.idle = if turn.tools == 0 { q.idle + 1 } else { 0 };
        q.spent_usd = spent(cfg, &mut q) + q.judge_usd;
        if let Some(b) = q.budget_usd
            && q.spent_usd >= b
        {
            let why = format!(
                "budget of ${b:.0} spent (${:.2}). /voyage resume carries on without one",
                q.spent_usd
            );
            pause(&mut q, PauseReason::Budget, &why);
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
                PauseReason::Idle,
                &format!("{n} turns in a row without doing anything"),
            );
            announce(cfg, &q, "voyage paused, it stopped making progress");
            return say(format!(
                "toomux voyage paused: {n} turns without a tool call. Say what to do next and it carries on."
            ));
        }
        save(&q);
        (q, turn.start)
    };

    let evidence = evidence(&transcript, turn_start, EVIDENCE_CHARS);
    let found = assess(cfg, &snap, &evidence, began + HOOK_BUDGET);

    let _lock = lock();
    let mut q = load(&snap.id)?;
    if q.status != Status::Active || q.session() != snap.session() {
        log(
            &q,
            "set aside",
            "the voyage changed while this turn was checked",
        );
        return None;
    }
    apply(cfg, &mut q, found)
}

/// Judge the turn, then hold a "met" up to the check and the review, all
/// before `deadline`. Takes no lock: it can take minutes.
fn assess(cfg: &Config, q: &Voyage, evidence: &str, deadline: Instant) -> Found {
    let left = || deadline.saturating_duration_since(Instant::now());
    if left() < STEP_MIN {
        return Found::Deferred("no time was left to judge it".into());
    }
    let refused = |e: anyhow::Error, usd: f64| {
        let e = e.to_string();
        if evaluate::limit_error(&e) {
            Found::AtLimit(e)
        } else {
            Found::Unchecked(e, usd)
        }
    };
    let verified = verified_context(q);
    let mut j = match judge(cfg, q, &verified, evidence, left().min(JUDGE_TIMEOUT)) {
        Ok(v) => Judged {
            verdict: v.verdict,
            reason: v.reason,
            progress: v.progress,
            usd: v.usd,
            milestones: v.milestones,
            other_work: v.other_work,
            deferred: None,
        },
        Err(e) => return refused(e, 0.0),
    };
    let rules = q.persistence.rules();
    let reviews = rules.review && q.laps >= rules.proof_laps;
    // A "met" has to hold up: the check first (it's cheap and certain),
    // then the proof laps, then the review. Any of them failing makes it
    // "not yet"; running out of time defers it to a later turn.
    if j.verdict == Verdict::Met
        && let Some(c) = &q.check
    {
        let reserve = if reviews {
            JUDGE_TIMEOUT / 2
        } else {
            Duration::ZERO
        };
        let limit = left().saturating_sub(reserve).min(CHECK_TIMEOUT);
        if limit < STEP_MIN {
            j.deferred = Some(format!("no time was left to run `{c}`"));
            return Found::Judged(j);
        }
        match run_check(c, &q.cwd, limit) {
            Ok(()) => {}
            Err(evaluate::CheckFail::TimedOut(_)) if limit < CHECK_TIMEOUT => {
                j.deferred = Some(format!(
                    "`{c}` didn't finish in the {}s this turn had left",
                    limit.as_secs()
                ));
                return Found::Judged(j);
            }
            Err(f) => {
                j.verdict = Verdict::NotYet;
                j.reason = format!("`{c}` didn't pass: {f}");
            }
        }
    }
    if j.verdict == Verdict::Met && reviews {
        if left() < STEP_MIN {
            j.deferred = Some("no time was left for the review".into());
            return Found::Judged(j);
        }
        match review(cfg, q, evidence, left().min(JUDGE_TIMEOUT)) {
            Ok((true, _, usd)) => j.usd += usd,
            Ok((false, why, usd)) => {
                j.usd += usd;
                j.verdict = Verdict::NotYet;
                j.reason = format!("a sceptical review of your changes found: {why}");
            }
            Err(e) => return refused(e, j.usd),
        }
    }
    Found::Judged(j)
}

/// What the judge found, applied to the voyage as it is now. Hold the lock.
fn apply(cfg: &Config, q: &mut Voyage, found: Found) -> Option<String> {
    let rules = q.persistence.rules();
    let say = |msg: String| Some(json!({"systemMessage": msg}).to_string());
    let j = match found {
        Found::Deferred(why) => {
            log(q, "deferred", &why);
            save(q);
            return block(
                q,
                &format!("toomux couldn't check this turn in time ({why}), so keep going"),
            );
        }
        Found::AtLimit(e) => {
            q.at_limit = true;
            log(q, "at limit", &e);
            save(q);
            return None;
        }
        Found::Unchecked(e, usd) => {
            q.judge_usd += usd;
            q.spent_usd += usd;
            q.failures += 1;
            log(q, "unchecked", &e);
            if q.failures >= rules.failures {
                pause(
                    q,
                    PauseReason::Unchecked,
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
            return block(
                q,
                &format!("toomux couldn't check this turn ({e}), so keep going"),
            );
        }
        Found::Judged(j) => j,
    };
    q.progress = j.progress.or(q.progress);
    q.judge_usd += j.usd;
    q.spent_usd += j.usd;
    q.failures = 0;
    record(q, &j.milestones);
    if let Some(why) = j.deferred {
        q.last = Some(j.reason.clone());
        log(q, "deferred", &why);
        save(q);
        return block(
            q,
            &format!(
                "the judge says it's done ({}), but {why}. End a turn showing the evidence again and it's checked then",
                j.reason
            ),
        );
    }
    let (mut verdict, mut reason) = (j.verdict, j.reason);
    // A decision for the user doesn't stop the voyage while there's work it
    // doesn't hold up: that goes on, and the question waits for the end.
    if verdict == Verdict::NeedsYou && j.other_work {
        log(q, "asks later", &reason);
        verdict = Verdict::NotYet;
        reason = format!(
            "{reason}. That waits on the user, but other work toward the outcome doesn't: carry on with that, and put \
             the question to them when nothing else is left"
        );
    }
    if verdict == Verdict::Met && q.laps < rules.proof_laps {
        q.laps += 1;
        q.last = Some(reason.clone());
        log(q, "proof lap", &reason);
        save(q);
        return block(q, &proof_lap(q.laps, rules.proof_laps, &reason));
    }
    // Stopping for you or giving up: first, a push back to try again.
    if matches!(verdict, Verdict::NeedsYou | Verdict::Impossible) && q.pushed < rules.push_backs {
        q.pushed += 1;
        q.last = Some(reason.clone());
        log(q, "pushed back", &reason);
        save(q);
        return block(q, &push_back(verdict, &reason));
    }
    q.last = Some(reason.clone());
    match verdict {
        Verdict::NotYet => {
            q.laps = 0;
            log(q, "not yet", &reason);
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
                pause(q, PauseReason::Stall, &why);
                announce(cfg, q, "voyage paused, it's stuck");
                return say(format!(
                    "toomux voyage paused: {why}. Say how to get past it and it carries on."
                ));
            }
            if let Some(n) = rules.stall_after
                && q.flat > 0
                && q.flat.is_multiple_of(n)
            {
                log(q, "stalled", &format!("{} checks at {}%", q.flat, q.best));
                why.push_str(&stall_nudge(q.flat / n, q.flat, q.best));
            }
            save(q);
            block(q, &why)
        }
        Verdict::Met => {
            end(q, Status::Met, &reason);
            let took = registry::duration(registry::now_ms() - q.started_ms);
            announce(cfg, q, &format!("voyage done after {took}"));
            say(format!(
                "toomux voyage done after {took}, {}: {reason}",
                turns(q.turns)
            ))
        }
        Verdict::Impossible => {
            end(q, Status::Impossible, &reason);
            announce(cfg, q, "voyage can't be done");
            say(format!("toomux voyage ended, it can't be done: {reason}"))
        }
        Verdict::NeedsYou => {
            pause(q, PauseReason::NeedsYou, &reason);
            announce(cfg, q, "voyage needs you");
            say(format!(
                "toomux voyage waiting on you: {reason}. Your next message carries it on."
            ))
        }
    }
}

/// The session's own background work still running, said briefly: toomux's
/// jobs, and agents, monitors and background commands its transcript shows
/// started and not yet reported back.
fn background(session: &str, transcript: &Path) -> Option<String> {
    let jobs = crate::jobs::running_for(session).len();
    let tasks = evaluate::background_tasks(transcript, registry::now_ms());
    let mut said = Vec::new();
    if jobs > 0 {
        said.push(format!(
            "{jobs} toomux job{}",
            if jobs == 1 { "" } else { "s" }
        ));
    }
    if tasks > 0 {
        said.push(format!(
            "{tasks} background task{}",
            if tasks == 1 { "" } else { "s" }
        ));
    }
    (!said.is_empty()).then(|| said.join(" and "))
}

/// Milestones the judge confirmed, added to the ledger (each once, the
/// newest kept when it's full).
fn record(q: &mut Voyage, milestones: &[String]) {
    let session = q.session().to_string();
    for m in milestones {
        let text = short(m, MILESTONE_CHARS);
        if text.is_empty() || q.ledger.iter().any(|l| l.text.eq_ignore_ascii_case(&text)) {
            continue;
        }
        q.ledger.push(Milestone {
            text,
            session: session.clone(),
            turn: q.turns,
            at_ms: registry::now_ms(),
        });
    }
    let excess = q.ledger.len().saturating_sub(LEDGER_MAX);
    q.ledger.drain(..excess);
}

/// How much of the handover briefs the judge reads, newest first.
const BRIEF_CHARS: usize = 6_000;

/// What the judge is told was settled before this conversation's end: the
/// ledger of milestones earlier checks confirmed, and the handover briefs
/// the earlier conversations wrote.
fn verified_context(q: &Voyage) -> String {
    let mut s = String::new();
    if !q.ledger.is_empty() {
        s.push_str("Confirmed by toomux's earlier checks, from evidence they were shown (count these as done unless this turn's evidence undoes them):\n");
        for m in &q.ledger {
            s.push_str(&format!(
                "- {} (turn {}, conversation {})\n",
                m.text,
                m.turn,
                &m.session[..8.min(m.session.len())]
            ));
        }
    }
    let mut room = BRIEF_CHARS;
    for prev in q.sessions.iter().rev().skip(1) {
        if room < 500 {
            break;
        }
        let Ok(brief) = std::fs::read_to_string(crate::handover::brief_path(prev, None)) else {
            continue;
        };
        let brief = match brief.find(BRIEF_HEADING) {
            Some(i) => &brief[..i],
            None => brief.as_str(),
        };
        let take: String = brief.chars().take(room).collect();
        room -= take.chars().count();
        s.push_str(&format!(
            "\nThe handover brief conversation {} wrote for the next (its own account of the work: context for what earlier \
             conversations did, not proof):\n{take}\n",
            &prev[..8.min(prev.len())]
        ));
    }
    s
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
/// Conversations before the current one have ended, so their cost is added
/// up once and kept; only the current one is read each turn.
fn spent(cfg: &Config, q: &mut Voyage) -> f64 {
    let cost = |s: &str| {
        crate::tokens::report(cfg, q.started_ms, Some(s))
            .all()
            .cost()
    };
    let closed = q.sessions.len().saturating_sub(1);
    if q.spent_closed > closed {
        (q.spent_closed, q.spent_closed_usd) = (0, 0.0);
    }
    while q.spent_closed < closed {
        q.spent_closed_usd += cost(&q.sessions[q.spent_closed]);
        q.spent_closed += 1;
    }
    q.spent_closed_usd + cost(q.session())
}

mod evaluate;
mod lineage;
use evaluate::{evidence, head_commit, judge, last_turn, review, run_check};
#[cfg(test)]
use evaluate::{parse_progress, parse_verdict};

// ---- across handovers and limits --------------------------------------------

/// A handover's fresh session takes the voyage on.
/// (Its first prompt may have taken it on already: then this does nothing.)
pub fn pass_on(old: &str, new: &str) {
    let _lock = lock();
    if open_for(new).is_none()
        && let Some(mut q) = open_for(old)
    {
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

/// A voyage that waits out a usage limit and carries on after it: an active
/// one, or one paused while at the limit (its judge's `claude -p` can't run
/// at the limit either, so its checks fail and it pauses). A pause for its
/// budget stays a pause.
fn held_by_limit(q: &Voyage) -> bool {
    let paused_at_limit = q.status == Status::Paused
        && !q.why.as_deref().is_some_and(|w| w.starts_with("budget"))
        && (q.at_limit
            || q.why.as_deref().is_some_and(|w| {
                w.starts_with("couldn't be checked") && w.to_lowercase().contains("limit")
            }));
    q.status == Status::Active || paused_at_limit
}

/// crate::limits found this session's limit lifted (perhaps with nothing
/// having seen it while it held): if a voyage waits on it, it's marked at the
/// limit so the next tick carries it on. Whether one does.
pub fn note_limit(session: &str) -> bool {
    let Some(mut q) = open_for(session).filter(held_by_limit) else {
        return false;
    };
    if !q.at_limit {
        q.at_limit = true;
        save(&q);
        log(&q, "at limit", "seen after the reset");
    }
    true
}

/// From the status tick and the limit wake-up: a voyage whose session stopped
/// at a usage limit carries on once the limit lifts.
///
/// The contract around `at_limit` ("this voyage waits out a usage limit"):
/// - stop_hook sets it when a turn ends at a limit, and may pause the voyage
///   while it's set (a judge that can't run at the limit); a turn that ends
///   normally clears it;
/// - this tick sets it on seeing the session limited, and crate::limits
///   (note_limit) on finding a limit that lifted unseen;
/// - crate::limits closes Claude Code's limit dialog after the reset, so the
///   session goes idle; here, once it has sat a minute at an empty prompt,
///   an active voyage at the limit, or one paused at it (held_by_limit), is
///   resumed and the session told to carry on. Nothing is typed while Claude
///   Code counts down to continue by itself.
pub fn tick(sessions: &[Session], now: i64) {
    for mut q in open().into_iter().filter(held_by_limit) {
        let Some(s) = sessions.iter().find(|s| s.id == q.session() && !s.dormant) else {
            continue;
        };
        // Paused for the limit before at_limit was kept for pauses.
        let waiting = q.at_limit || q.status == Status::Paused;
        if s.limit.is_some() {
            if !q.at_limit {
                q.at_limit = true;
                save(&q);
                log(&q, "at limit", s.limit.as_deref().unwrap_or(""));
            }
            continue;
        }
        if !waiting
            || !s.is_idle()
            || now - s.since_ms < RESUME_AFTER_MS
            || crate::handover::requested(&s.id)
        {
            continue;
        }
        let Some(pane) = s.pane.as_ref().map(|p| p.id.clone()) else {
            continue;
        };
        if !crate::actions::prompt_empty(&pane) || crate::limits::claude_continuing(&pane) {
            continue;
        }
        // Typed first: a message that didn't send leaves the voyage waiting,
        // for the next tick to try again.
        if let Err(e) = crate::actions::type_prompt(
            &pane,
            &format!(
                "[toomux voyage {}] The usage limit has lifted: carry on. {}",
                q.id,
                brief_line(&q)
            ),
        ) {
            log(
                &q,
                "limit lifted",
                &format!("couldn't tell it to carry on: {e}"),
            );
            continue;
        }
        if q.status == Status::Paused {
            resume(&mut q, "the usage limit lifted");
        }
        q.at_limit = false;
        save(&q);
        log(&q, "limit lifted", "");
    }
}

// ---- toomux voyage ------------------------------------------------------------

/// Where an open voyage's conversation has gone, if it isn't where the
/// voyage thinks: (a word for its state, a line on what to do). A voyage on
/// a conversation that continued elsewhere moves there at that
/// conversation's next turn; one whose conversation isn't running and
/// didn't continue anywhere is an orphan, and says how to move or end it.
pub fn orphan(cfg: &Config, q: &Voyage, live: &[Session]) -> Option<(&'static str, String)> {
    if !q.status.open() {
        return None;
    }
    let short_id = |s: &str| s[..8.min(s.len())].to_string();
    let next = lineage::successors(cfg, &q.cwd, q.session(), None);
    if let Some(n) = next.last() {
        return Some((
            "moved",
            format!(
                "its conversation {} continued in {}: the voyage follows at that one's next turn, or now with \
                 `toomux voyage rebind {} {}`",
                short_id(q.session()),
                short_id(n),
                q.id,
                n
            ),
        ));
    }
    if live.iter().any(|s| s.id == q.session()) {
        return None;
    }
    Some((
        "orphaned",
        format!(
            "its conversation {} isn't running and didn't continue anywhere: \
             `toomux reopen {}` carries the voyage on there, `toomux voyage rebind {} <session>` moves it, \
             `toomux voyage clear {}` ends it",
            short_id(q.session()),
            short_id(q.session()),
            q.id,
            q.id
        ),
    ))
}

/// `toomux voyage`: every open voyage, then the last few that ended.
pub fn list(cfg: &Config, now: i64) -> String {
    let all = all();
    if all.is_empty() {
        return "no voyages yet. In any session, /voyage <outcome> sets one.\n".into();
    }
    let mut s = String::new();
    let (open, ended): (Vec<&Voyage>, Vec<&Voyage>) = all.iter().partition(|q| q.status.open());
    let live = if open.is_empty() {
        Vec::new()
    } else {
        registry::load(cfg)
    };
    for q in &open {
        s.push_str(&describe_as(q, now, orphan(cfg, q, &live)));
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
    if !q.ledger.is_empty() {
        s.push_str("  confirmed so far:\n");
        for m in &q.ledger {
            s.push_str(&format!("    - {}\n", short(&m.text, 110)));
        }
    }
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

/// A running session named on the command line, and the conversation it's
/// really in now: the registry's id can lag behind a /clear or a resume
/// into a new session, so `continued-in` records are followed from it.
fn session_now<'a>(
    cfg: &Config,
    all: &'a [Session],
    target: &str,
) -> Result<(&'a Session, String)> {
    let s = registry::find(all, target)?;
    let id = lineage::successors(cfg, &s.cwd, &s.id, s.transcript(cfg).as_deref())
        .pop()
        .unwrap_or_else(|| s.id.clone());
    Ok((s, id))
}

/// Type a line into a session at rest at an empty prompt, so it starts on
/// the voyage now. Whether it did.
fn nudge(s: &Session, line: &str) -> Result<bool> {
    match s.pane.as_ref().map(|p| p.id.clone()) {
        // Finished is at rest too: a turn ended and nobody has answered yet.
        Some(p)
            if matches!(s.state, St::Idle | St::Finished | St::Background)
                && crate::actions::prompt_empty(&p) =>
        {
            crate::actions::type_prompt(&p, line)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// `toomux voyage set <session> <outcome>`: from outside the session. It
/// starts on it at once if it's at rest at an empty prompt.
pub fn set_from_outside(cfg: &Config, target: &str, words: &[String]) -> Result<String> {
    let Ask::Set {
        outcome,
        check,
        budget,
        persistence,
    } = parse_words(words)?
    else {
        bail!("say what done looks like")
    };
    let all = registry::load(cfg);
    let (s, id) = session_now(cfg, &all, target)?;
    let q = {
        let _lock = lock();
        begin(
            &id,
            &s.cwd,
            outcome,
            check,
            budget,
            persistence.unwrap_or_else(|| cfg.persistence()),
            Some(Owner {
                pid: s.pid,
                start: s.proc_start.clone(),
            }),
        )
    };
    let started = nudge(
        s,
        &format!(
            "[toomux voyage {}] You have a voyage. {} Work toward it now and keep going; toomux checks after every turn.",
            q.id,
            brief_line(&q)
        ),
    )?;
    Ok(if started {
        format!("voyage {} set on {} and started", q.id, s.title)
    } else {
        format!(
            "voyage {} set on {}: it's checked from the end of its next turn",
            q.id, s.title
        )
    })
}

/// End a voyage from outside its session (`toomux voyage clear`, the TUI).
pub fn clear(target: &str) -> Result<String> {
    let _lock = lock();
    let mut q = find(target)?;
    if !q.status.open() {
        bail!("voyage {} already ended ({})", q.id, q.status.word());
    }
    end(&mut q, Status::Cleared, "cleared from toomux");
    Ok(format!("voyage {} cleared", q.id))
}

/// Pause a voyage from outside its session (`toomux voyage pause`, the
/// TUI). Only you lift it: your next prompt there, or [`resume_from_outside`].
pub fn pause_from_outside(target: &str) -> Result<String> {
    let _lock = lock();
    let mut q = find(target)?;
    if !q.status.open() {
        bail!("voyage {} already ended ({})", q.id, q.status.word());
    }
    pause(&mut q, PauseReason::You, "you paused it from toomux");
    Ok(format!("voyage {} paused", q.id))
}

/// Carry a paused voyage on from outside its session (`toomux voyage
/// resume`, the TUI); a session at rest at an empty prompt starts on it.
pub fn resume_from_outside(cfg: &Config, target: &str) -> Result<String> {
    let q = {
        let _lock = lock();
        let mut q = find(target)?;
        if !q.status.open() {
            bail!("voyage {} already ended ({})", q.id, q.status.word());
        }
        if q.paused_for() == Some(PauseReason::Budget) {
            q.budget_usd = None;
        }
        q.at_limit = false;
        resume(&mut q, "from toomux");
        q
    };
    let all = registry::load(cfg);
    let started = match all.iter().find(|s| s.id == q.session()) {
        Some(s) => nudge(
            s,
            &format!(
                "[toomux voyage {}] The voyage carries on. {}",
                q.id,
                brief_line(&q)
            ),
        )?,
        None => false,
    };
    Ok(if started {
        format!("voyage {} resumed and its session started on it", q.id)
    } else {
        format!(
            "voyage {} resumed: it's checked from the end of its session's next turn",
            q.id
        )
    })
}

/// `toomux voyage rebind <voyage> <session>`: move an open voyage onto
/// another conversation (a running session by pid, id prefix or name, or a
/// conversation id), for one left behind on an id nobody uses any more.
pub fn rebind_to(cfg: &Config, target: &str, session: &str) -> Result<String> {
    let all = registry::load(cfg);
    let (id, owner) = match session_now(cfg, &all, session) {
        Ok((s, id)) => (
            id,
            Some(Owner {
                pid: s.pid,
                start: s.proc_start.clone(),
            }),
        ),
        // Not running: a whole conversation id is taken as typed (the
        // voyage follows it once it's resumed).
        Err(_)
            if session.len() >= 32
                && session.chars().all(|c| c.is_ascii_hexdigit() || c == '-') =>
        {
            (session.to_string(), None)
        }
        Err(e) => return Err(e),
    };
    let _lock = lock();
    let mut q = find(target)?;
    if !q.status.open() {
        bail!("voyage {} already ended ({})", q.id, q.status.word());
    }
    if q.session() == id {
        return Ok(format!("voyage {} is already on {id}", q.id));
    }
    if let Some(other) = open_for(&id) {
        bail!("{id} already has voyage {}: clear it first", other.id);
    }
    q.owner = owner;
    rebind(&mut q, &[id.clone()], "moved by you to");
    Ok(format!("voyage {} moved to {id}", q.id))
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

    fn checked(args: &str) -> (String, Option<String>, Option<Persistence>) {
        match parse(args).unwrap() {
            Ask::Set {
                outcome,
                check,
                persistence,
                ..
            } => (outcome, check, persistence),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_check_command_keeps_its_own_double_dashes_and_no_flag_leaks_into_the_outcome() {
        // The flags that were lost to the outcome text in real voyages.
        let gradle = "cd native-tv && ./gradlew --no-daemon :app:testDebugUnitTest :app:assembleDebug && cd .. && \
                      ./node_modules/.bin/vitest run cloudflare/worker && git diff --check";
        let (outcome, check, tier) = checked(&format!(
            "finish Midnight to the handover's Definition of Done; don't weaken gates. --persistence relentless --check \"{gradle}\""
        ));
        assert_eq!(
            outcome,
            "finish Midnight to the handover's Definition of Done; don't weaken gates."
        );
        assert_eq!(check.as_deref(), Some(gradle));
        assert_eq!(tier, Some(Persistence::Relentless));

        // Unquoted, a check runs to the next known flag.
        let (outcome, check, tier) =
            checked("tests pass --check cargo test -- --nocapture --persistence hard");
        assert_eq!(outcome, "tests pass");
        assert_eq!(check.as_deref(), Some("cargo test -- --nocapture"));
        assert_eq!(tier, Some(Persistence::Hard));
        let (_, check, _) = checked("it builds --check './gradlew --no-daemon build' --budget 5");
        assert_eq!(check.as_deref(), Some("./gradlew --no-daemon build"));
        // Several words keep their quotes for the shell that runs them.
        let (_, check, _) = checked(r#"logs are clean --check grep -q "all good" out.log"#);
        assert_eq!(check.as_deref(), Some(r#"grep -q "all good" out.log"#));
        let (_, check, tier) = checked("x --check=make --persist=light");
        assert_eq!(
            (check.as_deref(), tier),
            (Some("make"), Some(Persistence::Light))
        );

        // Ambiguous or unknown is an error, never silently dropped.
        assert!(
            parse("clean diff --check git diff --check")
                .unwrap_err()
                .to_string()
                .contains("quote")
        );
        assert!(parse("x --budget 5 --verbose").is_err());
        assert!(parse("x --budget 5 and then some").is_err());
        assert!(parse("x --check \"unclosed").is_err());
        assert!(parse("x --budget 5 --budget 6").is_err());
        assert!(parse("x --check").is_err());
    }

    #[test]
    fn words_from_the_command_line_keep_their_shell_split() {
        let w = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        match parse_words(&w(&[
            "tests",
            "pass",
            "--check",
            "cd native-tv && ./gradlew --no-daemon test && git diff --check",
            "--persistence",
            "relentless",
        ]))
        .unwrap()
        {
            Ask::Set {
                outcome,
                check,
                persistence,
                ..
            } => {
                assert_eq!(outcome, "tests pass");
                assert_eq!(
                    check.as_deref(),
                    Some("cd native-tv && ./gradlew --no-daemon test && git diff --check")
                );
                assert_eq!(persistence, Some(Persistence::Relentless));
            }
            other => panic!("{other:?}"),
        }
        match parse_words(&w(&[
            "it", "works", "--check", "grep", "-q", "all good", "log",
        ]))
        .unwrap()
        {
            Ask::Set { check, .. } => assert_eq!(check.as_deref(), Some("grep -q 'all good' log")),
            other => panic!("{other:?}"),
        }
        // The whole thing as one quoted word reads as /voyage would.
        match parse_words(&w(&["it works --budget 3"])).unwrap() {
            Ask::Set {
                outcome, budget, ..
            } => {
                assert_eq!((outcome.as_str(), budget), ("it works", Some(3.0)))
            }
            other => panic!("{other:?}"),
        }
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
        pause(&mut q, PauseReason::NeedsYou, "which database?");
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

    /// A state folder of its own for a test, under the env lock.
    fn state(name: &str) -> (std::sync::MutexGuard<'static, ()>, PathBuf) {
        let guard = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("toomux-voyage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", root.join("state")) };
        (guard, root)
    }

    fn done(root: &Path) {
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let _ = std::fs::remove_dir_all(root);
    }

    /// A stand-in `claude` that answers each call with the next whole line
    /// of `lines` (JSON as `claude -p --output-format json` prints it).
    fn fake_claude(dir: &Path, lines: &[String]) -> Config {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("answers"), lines.join("\n") + "\n").unwrap();
        let bin = dir.join("claude");
        let d = dir.display();
        std::fs::write(&bin, format!(
            "#!/bin/sh\nn=$(( $(cat {d}/n 2>/dev/null || echo 0) + 1 ))\necho $n > {d}/n\ncat > {d}/ask$n\nsed -n \"${{n}}p\" {d}/answers\n"
        )).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Config {
            claude_bin: bin.display().to_string(),
            ..Config::default()
        }
    }

    fn said(v: Value) -> String {
        json!({"result": v.to_string(), "total_cost_usd": 0.01}).to_string()
    }

    /// A turn that ran a command.
    fn worked() -> String {
        line(json!({"type": "user", "message": {"content": "go"}}))
            + &line(
                json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}}]}}),
            )
    }

    fn hook(session: &str, t: &Path) -> Value {
        json!({"session_id": session, "transcript_path": t.display().to_string(), "cwd": "/tmp"})
    }

    fn reason_of(out: Option<String>) -> String {
        let v: Value = serde_json::from_str(&out.expect("the hook answered")).unwrap();
        v["reason"]
            .as_str()
            .or(v["systemMessage"].as_str())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn a_turn_that_ends_waiting_on_background_work_is_neither_judged_nor_counted() {
        let (_env, root) = state("bg");
        let t = root.join("t.jsonl");
        let now = chrono::Utc::now().to_rfc3339();
        std::fs::write(
            &t,
            worked()
                + &line(json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "gh run watch", "run_in_background": true}}]}}))
                + &line(json!({"type": "user", "timestamp": now, "toolUseResult": {"backgroundTaskId": "b1"}, "message": {"content": [{"type": "tool_result", "tool_use_id": "tu1", "content": "Command running in background with ID: b1."}]}}))
                + &line(json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Waiting for CI."}]}})),
        )
        .unwrap();
        let j = root.join("judge");
        let cfg = fake_claude(
            &j,
            &[said(
                json!({"verdict": "not_yet", "reason": "CI isn't green yet", "progress": 40}),
            )],
        );
        start(
            "bg",
            "/tmp",
            "CI is green".into(),
            None,
            None,
            Persistence::Steady,
        );
        assert!(
            stop_as(&cfg, &hook("bg", &t), None).is_none(),
            "the stop goes through"
        );
        let q = open_for("bg").unwrap();
        assert_eq!(
            (q.turns, q.idle, q.flat, q.status),
            (0, 0, 0, Status::Active)
        );
        assert!(q.awaiting_ms.is_some());
        assert!(
            q.chip(registry::now_ms())
                .contains("waits on background work")
        );
        assert!(!j.join("n").exists(), "the judge isn't asked");

        // The task reports back: the turn that starts is checked.
        let mut s = std::fs::read_to_string(&t).unwrap();
        s += &line(
            json!({"type": "user", "message": {"content": "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n<summary>Background command \"gh run watch\" completed (exit code 0)</summary>\n</task-notification>"}}),
        );
        s += &line(
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Bash", "input": {"command": "gh run view"}}]}}),
        );
        std::fs::write(&t, s).unwrap();
        assert!(reason_of(stop_as(&cfg, &hook("bg", &t), None)).contains("CI isn't green yet"));
        let q = open_for("bg").unwrap();
        assert_eq!((q.turns, q.awaiting_ms), (1, None));
        done(&root);
    }

    #[test]
    fn background_tasks_count_until_they_report_back_or_are_stopped_or_expire() {
        let dir = std::env::temp_dir().join(format!("toomux-voyage-tasks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("t.jsonl");
        let now = registry::now_ms();
        let at = |ms: i64| {
            chrono::DateTime::from_timestamp_millis(ms)
                .unwrap()
                .to_rfc3339()
        };
        let launched = |r: Value, ms: i64| {
            line(
                json!({"type": "user", "timestamp": at(ms), "toolUseResult": r, "message": {"content": [{"type": "tool_result", "content": "started"}]}}),
            )
        };
        let mut s = launched(json!({"backgroundTaskId": "bash1"}), now)
            + &launched(json!({"isAsync": true, "status": "async_launched", "agentId": "agent1"}), now)
            + &launched(json!({"taskId": "mon1", "timeoutMs": 600_000, "persistent": false}), now)
            // Long gone: a monitor that expired, a command from yesterday.
            + &launched(json!({"taskId": "mon0", "timeoutMs": 60_000, "persistent": false}), now - 3_600_000)
            + &launched(json!({"backgroundTaskId": "bash0"}), now - 86_400_000);
        std::fs::write(&t, &s).unwrap();
        assert_eq!(evaluate::background_tasks(&t, now), 3);
        // A monitor's event isn't its end; a report is.
        s += &line(
            json!({"type": "queue-operation", "content": "<task-notification>\n<task-id>mon1</task-id>\n<status>running</status>\n<event>ready</event>\n</task-notification>"}),
        );
        s += &line(
            json!({"type": "attachment", "attachment": {"type": "queued_command", "prompt": "<task-notification>\n<task-id>agent1</task-id>\n<status>completed</status>\n</task-notification>"}}),
        );
        s += &line(
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "TaskStop", "input": {"task_id": "bash1"}}]}}),
        );
        std::fs::write(&t, &s).unwrap();
        assert_eq!(
            evaluate::background_tasks(&t, now),
            1,
            "only the monitor runs on"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_pause_toomux_made_for_seeming_stopped_lifts_when_background_work_reports_back() {
        let (_env, root) = state("lift");
        let cfg = Config::default();
        let note = json!({"session_id": "ps", "prompt": "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n</task-notification>"});
        start(
            "ps",
            "/tmp",
            "it ships".into(),
            None,
            None,
            Persistence::Steady,
        );
        for (reason, lifts) in [
            (PauseReason::Stall, true),
            (PauseReason::Idle, true),
            (PauseReason::NeedsYou, false),
            (PauseReason::You, false),
            (PauseReason::Budget, false),
            (PauseReason::Unchecked, false),
        ] {
            let mut q = open_for("ps").unwrap();
            pause(&mut q, reason, "why");
            assert!(prompt_hook(&cfg, &note).is_none());
            assert_eq!(
                open_for("ps").unwrap().status == Status::Active,
                lifts,
                "{reason:?}"
            );
            if lifts {
                assert_eq!(open_for("ps").unwrap().pause_reason, None);
            }
        }
        // A pause from before the reason was kept is read from what it said.
        let mut q = all().remove(0);
        q.status = Status::Paused;
        q.pause_reason = None;
        for (why, reason) in [
            (
                "8 checks in a row with no headway, stuck at about 30%",
                PauseReason::Stall,
            ),
            ("4 turns in a row without doing anything", PauseReason::Idle),
            (
                "budget of $5 spent ($5.10). /voyage resume carries on without one",
                PauseReason::Budget,
            ),
            (
                "couldn't be checked 3 times in a row (x)",
                PauseReason::Unchecked,
            ),
            ("which database?", PauseReason::NeedsYou),
        ] {
            q.why = Some(why.into());
            assert_eq!(q.paused_for(), Some(reason));
        }
        done(&root);
    }

    #[test]
    fn a_voyage_file_from_before_still_loads() {
        let (_env, root) = state("compat");
        std::fs::create_dir_all(dir()).unwrap();
        std::fs::write(
            dir().join("5e7a11ed.json"),
            r#"{"id": "5e7a11ed", "outcome": "finish it", "check": "make", "budget_usd": null,
                "cwd": "/tmp", "sessions": ["0dd5e55a"], "started_ms": 1791127071291, "status": "paused",
                "why": "8 checks in a row with no headway, stuck at about 30%", "ended_ms": null, "turns": 0,
                "idle": 0, "failures": 0, "last": null, "judge_usd": 0.0, "spent_usd": 0.0, "at_limit": false,
                "progress": null, "persistence": "relentless", "base": null, "laps": 0, "pushed": 0, "best": 0,
                "flat": 0}"#,
        )
        .unwrap();
        let q = open_for("0dd5e55a").expect("it loads");
        assert_eq!(
            (
                q.paused_for(),
                q.owner.clone(),
                q.ledger.len(),
                q.spent_closed
            ),
            (Some(PauseReason::Stall), None, 0, 0)
        );
        done(&root);
    }

    #[test]
    fn an_ended_voyage_is_never_brought_back_by_a_stale_copy() {
        let (_env, root) = state("stale");
        let q = start(
            "st",
            "/tmp",
            "it ships".into(),
            None,
            None,
            Persistence::Steady,
        );
        let stale = q.clone();
        clear(&q.id).unwrap();
        save(&stale);
        assert!(open().is_empty() && open_for("st").is_none());
        assert_eq!(all().len(), 1, "one voyage, not two copies");
        assert_eq!(all()[0].status, Status::Cleared);
        assert!(!dir().join(format!("{}.json", q.id)).exists());

        // Cleared while its turn was being judged: the verdict is set aside.
        let j = root.join("slow");
        std::fs::create_dir_all(&j).unwrap();
        std::fs::write(
            j.join("answer"),
            said(json!({"verdict": "not_yet", "reason": "more to do", "progress": 10})),
        )
        .unwrap();
        let bin = j.join("claude");
        let d = j.display();
        std::fs::write(
            &bin,
            format!("#!/bin/sh\ncat > /dev/null\ntouch {d}/started\nsleep 2\ncat {d}/answer\n"),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = Config {
            claude_bin: bin.display().to_string(),
            ..Config::default()
        };
        let t = root.join("t.jsonl");
        std::fs::write(&t, worked()).unwrap();
        let q = start(
            "sl",
            "/tmp",
            "it ships".into(),
            None,
            None,
            Persistence::Steady,
        );
        let hooked = {
            let (cfg, v) = (cfg.clone(), hook("sl", &t));
            std::thread::spawn(move || stop_as(&cfg, &v, None))
        };
        let t0 = Instant::now();
        while !j.join("started").exists() && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(50));
        }
        clear(&q.id).unwrap();
        assert!(hooked.join().unwrap().is_none(), "its stop goes through");
        assert!(open_for("sl").is_none(), "and it stays cleared");
        assert_eq!(all().iter().filter(|x| x.id == q.id).count(), 1);
        done(&root);
    }

    #[test]
    fn only_the_process_that_owns_a_voyage_judges_it() {
        let (_env, root) = state("owner");
        let t = root.join("t.jsonl");
        std::fs::write(&t, worked()).unwrap();
        let cfg = fake_claude(
            &root.join("j"),
            &[said(
                json!({"verdict": "not_yet", "reason": "more", "progress": 10}),
            )],
        );
        let mut q = start(
            "ow",
            "/tmp",
            "it ships".into(),
            None,
            None,
            Persistence::Steady,
        );
        let mine = Owner {
            pid: std::process::id() as i32,
            start: registry::proc_start(std::process::id() as i32),
        };
        q.owner = Some(mine.clone());
        save(&q);
        // Another live copy of the conversation lets its turn end unjudged.
        let other = Owner {
            pid: 1,
            start: registry::proc_start(1),
        };
        assert!(stop_as(&cfg, &hook("ow", &t), Some((other, Some("ow".into())))).is_none());
        assert_eq!(open_for("ow").unwrap().turns, 0);
        assert!(
            reason_of(stop_as(
                &cfg,
                &hook("ow", &t),
                Some((mine, Some("ow".into())))
            ))
            .contains("more")
        );
        // An owner that's gone is replaced by the next process to stop.
        let mut q = open_for("ow").unwrap();
        q.owner = Some(Owner {
            pid: i32::MAX - 1,
            start: None,
        });
        save(&q);
        let other = Owner {
            pid: 1,
            start: registry::proc_start(1),
        };
        let _ = stop_as(&cfg, &hook("ow", &t), Some((other.clone(), None)));
        assert_eq!(open_for("ow").unwrap().owner, Some(other));
        done(&root);
    }

    #[test]
    fn at_a_usage_limit_the_turn_is_neither_judged_nor_counted_and_waits_for_it_to_lift() {
        let (_env, root) = state("limit");
        let t = root.join("t.jsonl");
        std::fs::write(
            &t,
            worked()
                + &line(json!({"type": "assistant", "error": "rate_limit", "timestamp": chrono::Utc::now().to_rfc3339(), "message": {"content": [{"type": "text", "text": "You've hit your usage limit"}]}})),
        )
        .unwrap();
        let j = root.join("j");
        let cfg = fake_claude(
            &j,
            &[json!({"is_error": true, "result": "You've hit your limit · resets 5pm", "total_cost_usd": 0.0}).to_string()],
        );
        start(
            "lm",
            "/tmp",
            "it ships".into(),
            None,
            None,
            Persistence::Steady,
        );
        assert!(stop_as(&cfg, &hook("lm", &t), None).is_none());
        let q = open_for("lm").unwrap();
        assert_eq!((q.at_limit, q.turns, q.status), (true, 0, Status::Active));
        assert!(!j.join("n").exists(), "the judge isn't asked");

        // The session's fine but the judge's own run is refused for a limit:
        // not a failed check.
        let t2 = root.join("t2.jsonl");
        std::fs::write(&t2, worked()).unwrap();
        assert!(stop_as(&cfg, &hook("lm", &t2), None).is_none());
        let q = open_for("lm").unwrap();
        assert_eq!(
            (q.at_limit, q.failures, q.status),
            (true, 0, Status::Active)
        );
        done(&root);
    }

    #[test]
    fn a_voyage_follows_its_conversation_when_claude_code_gives_it_a_new_id() {
        let (_env, root) = state("moved");
        // An account of its own, so the transcripts are found by folder too.
        let proj = root.join("acct/projects/-tmp");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("old-a.jsonl"),
            line(json!({"type": "user", "message": {"content": "hi"}}))
                + &line(json!({"type": "continued-in", "sessionId": "old-a", "continuedInSessionId": "new-b"})),
        )
        .unwrap();
        let tb = proj.join("new-b.jsonl");
        std::fs::write(&tb, worked()).unwrap();
        let mut cfg = fake_claude(
            &root.join("j"),
            &[said(
                json!({"verdict": "not_yet", "reason": "more", "progress": 10}),
            )],
        );
        cfg.accounts = vec![crate::config::Account {
            name: "t".into(),
            config_dir: root.join("acct").display().to_string(),
        }];
        start(
            "old-a",
            "/tmp",
            "it ships".into(),
            None,
            None,
            Persistence::Steady,
        );
        let q = open_for("old-a").unwrap();
        let (word, note) = orphan(&cfg, &q, &[]).expect("it's not where it was");
        assert_eq!(word, "moved");
        assert!(note.contains("continued in new-b"), "{note}");
        assert!(reason_of(stop_as(&cfg, &hook("new-b", &tb), None)).contains("more"));
        let q = open_for("new-b").expect("the voyage moved with it");
        assert_eq!(q.sessions, vec!["old-a".to_string(), "new-b".to_string()]);
        assert!(open_for("old-a").is_none());
        let log = std::fs::read_to_string(log_file(&q.id)).unwrap();
        assert!(log.contains("\"rebound\""), "{log}");

        // Nothing running it and nothing continued it: an orphan, said so.
        let lost = start(
            "gone-z",
            "/tmp",
            "x".into(),
            None,
            None,
            Persistence::Steady,
        );
        let (word, note) = orphan(&cfg, &lost, &[]).unwrap();
        assert_eq!(word, "orphaned");
        assert!(note.contains("toomux voyage rebind"), "{note}");
        assert!(describe_as(&lost, registry::now_ms(), Some((word, note))).contains("· orphaned"));
        done(&root);
    }

    #[test]
    fn a_handover_successor_takes_the_voyage_on_at_its_first_prompt_without_resuming_it() {
        let (_env, root) = state("succ");
        let cfg = Config::default();
        let mut q = start(
            "old-1",
            "/tmp",
            "it ships".into(),
            None,
            None,
            Persistence::Steady,
        );
        pause(&mut q, PauseReason::NeedsYou, "which database?");
        let first = "Continue from a handover. The previous session here (id old-1) reached 300k tokens of context";
        assert!(prompt_hook(&cfg, &json!({"session_id": "new-2", "prompt": first})).is_none());
        let q = open_for("new-2").expect("taken on before the carry");
        assert_eq!(
            q.status,
            Status::Paused,
            "a handover's prompt isn't the user's word"
        );
        pass_on("old-1", "new-2");
        assert_eq!(
            open_for("new-2").unwrap().sessions.len(),
            2,
            "the carry adds nothing"
        );

        // Its first turn ends before either: the stop hook finds it from the
        // transcript's first prompt.
        start(
            "old-3",
            "/tmp",
            "it ships".into(),
            None,
            None,
            Persistence::Steady,
        );
        let t = root.join("new-4.jsonl");
        std::fs::write(
            &t,
            line(json!({"type": "user", "message": {"content": first.replace("old-1", "old-3")}}))
                + &line(json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Read", "input": {"file_path": "/tmp/brief.md"}}]}})),
        )
        .unwrap();
        let cfg = fake_claude(
            &root.join("j"),
            &[said(
                json!({"verdict": "not_yet", "reason": "more", "progress": 10}),
            )],
        );
        assert!(reason_of(stop_as(&cfg, &hook("new-4", &t), None)).contains("more"));
        assert_eq!(open_for("new-4").unwrap().turns, 1);
        done(&root);
    }

    #[test]
    fn what_a_check_confirms_is_kept_and_shown_to_later_checks() {
        let (_env, root) = state("ledger");
        let t = root.join("t.jsonl");
        std::fs::write(&t, worked()).unwrap();
        let j = root.join("j");
        let cfg = fake_claude(
            &j,
            &[
                said(
                    json!({"verdict": "not_yet", "reason": "the docs", "progress": 50, "milestones": ["auth tests pass"]}),
                ),
                said(
                    json!({"verdict": "needs_you", "reason": "which licence?", "progress": 60, "other_work": true}),
                ),
                said(
                    json!({"verdict": "needs_you", "reason": "which licence?", "progress": 90, "other_work": false}),
                ),
                said(
                    json!({"verdict": "needs_you", "reason": "which licence?", "progress": 90, "other_work": false}),
                ),
            ],
        );
        start(
            "pr",
            "/tmp",
            "it ships".into(),
            None,
            None,
            Persistence::Steady,
        );
        pass_on("pr", "ld");
        std::fs::create_dir_all(crate::handover::dir()).unwrap();
        std::fs::write(
            crate::handover::brief_path("pr", None),
            "# Handover\n\nThe migration is written and its tests pass.",
        )
        .unwrap();
        assert!(reason_of(stop_as(&cfg, &hook("ld", &t), None)).contains("the docs"));
        let q = open_for("ld").unwrap();
        assert_eq!(q.ledger.len(), 1);
        assert_eq!(
            (q.ledger[0].text.as_str(), q.ledger[0].session.as_str()),
            ("auth tests pass", "ld")
        );
        let ask1 = std::fs::read_to_string(j.join("ask1")).unwrap();
        assert!(
            ask1.contains("The migration is written"),
            "the brief is read: {ask1}"
        );

        // A question for the user doesn't stop work that can go on.
        let r = reason_of(stop_as(&cfg, &hook("ld", &t), None));
        assert!(
            r.contains("waits on the user") && r.contains("carry on"),
            "{r}"
        );
        assert_eq!(open_for("ld").unwrap().status, Status::Active);
        let ask2 = std::fs::read_to_string(j.join("ask2")).unwrap();
        assert!(
            ask2.contains("Confirmed by toomux's earlier checks")
                && ask2.contains("- auth tests pass"),
            "{ask2}"
        );
        // Blocked entirely on the user: steady tries once to settle it, then stops.
        assert!(reason_of(stop_as(&cfg, &hook("ld", &t), None)).contains("settle it yourself"));
        assert!(reason_of(stop_as(&cfg, &hook("ld", &t), None)).contains("waiting on you"));
        let q = open_for("ld").unwrap();
        assert_eq!(
            (q.status, q.paused_for()),
            (Status::Paused, Some(PauseReason::NeedsYou))
        );
        done(&root);
    }

    #[test]
    fn the_ledger_keeps_each_milestone_once_and_the_newest_when_full() {
        let mut q = start_free();
        record(
            &mut q,
            &[
                "Auth tests pass".into(),
                "auth tests pass".into(),
                " ".into(),
            ],
        );
        assert_eq!(q.ledger.len(), 1);
        let many: Vec<String> = (0..LEDGER_MAX + 5).map(|i| format!("step {i}")).collect();
        record(&mut q, &many);
        assert_eq!(q.ledger.len(), LEDGER_MAX);
        assert_eq!(
            q.ledger.last().unwrap().text,
            format!("step {}", LEDGER_MAX + 4)
        );
    }

    /// A voyage that's never saved.
    fn start_free() -> Voyage {
        serde_json::from_value(
            json!({"id": "free", "outcome": "x", "cwd": "/tmp", "sessions": ["a", "b", "c"],
            "started_ms": registry::now_ms(), "status": "active"}),
        )
        .unwrap()
    }

    #[test]
    fn ended_conversations_are_costed_once() {
        let mut q = start_free();
        let cfg = Config::default();
        let _ = spent(&cfg, &mut q);
        assert_eq!(q.spent_closed, 2, "a and b are added up and kept");
        q.sessions.push("d".into());
        let _ = spent(&cfg, &mut q);
        assert_eq!(q.spent_closed, 3);
    }

    #[test]
    fn a_turn_with_no_time_left_is_deferred_not_failed() {
        let q = start_free();
        let cfg = Config::default();
        assert!(matches!(
            assess(&cfg, &q, "", Instant::now() + Duration::from_secs(5)),
            Found::Deferred(_)
        ));
        let mut q = start_free();
        let r = reason_of(apply(&cfg, &mut q, Found::Deferred("no time".into())));
        assert!(r.contains("couldn't check this turn in time"), "{r}");
        assert_eq!(q.failures, 0);
        assert!(matches!(
            run_check("sleep 5", "/tmp", Duration::from_millis(200)),
            Err(evaluate::CheckFail::TimedOut(_))
        ));
        assert!(
            HOOK_BUDGET + Duration::from_secs(30) < Duration::from_secs(600),
            "under setup.rs's Stop hook timeout"
        );
    }

    #[test]
    fn a_turn_starts_at_a_prompt_not_at_what_claude_code_slips_in_mid_turn() {
        let dir = std::env::temp_dir().join(format!("toomux-voyage-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("t.jsonl");
        let tool = |c: &str| {
            line(
                json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Bash", "input": {"command": c}}]}}),
            )
        };
        std::fs::write(
            &t,
            line(json!({"type": "user", "message": {"content": "ship it"}}))
                + &tool("cargo build")
                + &line(json!({"type": "user", "isMeta": true, "message": {"content": [{"type": "text", "text": "Base directory for this skill: …"}]}}))
                + &line(json!({"type": "user", "message": {"content": "<system-reminder>Todo list changed</system-reminder>"}}))
                + &line(json!({"type": "user", "message": {"content": [{"type": "text", "text": "<system-reminder>x</system-reminder>"}]}}))
                + &tool("cargo test"),
        )
        .unwrap();
        let turn = last_turn(&t);
        assert_eq!((turn.start, turn.tools), (0, 2));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn steady_tries_once_to_settle_a_decision_before_stopping() {
        assert_eq!(Persistence::Steady.rules().push_backs, 1);
        assert_eq!(Persistence::Light.rules().push_backs, 0);
    }
}
