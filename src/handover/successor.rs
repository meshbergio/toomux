//! Successor naming, lineage persistence, and carry-forward after a handover.

use super::{brief_path, dir, write_atomic};
use crate::config::Config;
use crate::registry::{self, Session};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;

// ---- successors ---------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub(super) struct Lineage {
    pub(super) old_id: String,
    pub(super) pane: String,
    pub(super) at_ms: i64,
    /// toomux's own name for the old session (a rename), carried as is.
    pub(super) name: Option<String>,
    pub(super) carried: bool,
    /// What the successor was named.
    #[serde(default)]
    pub(super) title: String,
    /// That name was toomux's choice (from the brief), not someone's: the
    /// next handover may choose again.
    #[serde(default)]
    pub(super) derived: bool,
    /// The successor, once it has appeared.
    #[serde(default)]
    pub(super) new_id: String,
}

impl Lineage {
    pub(super) fn new(
        old_id: String,
        pane: String,
        at_ms: i64,
        name: Option<String>,
        title: String,
        derived: bool,
    ) -> Self {
        Self {
            old_id,
            pane,
            at_ms,
            name,
            carried: false,
            title,
            derived,
            new_id: String::new(),
        }
    }
}

// ---- naming a successor ----------------------------------------------------------

/// Words that say only that a session continues another one.
const HANDOVER_WORDS: &[&str] = &[
    "handover",
    "handovers",
    "hand",
    "handed",
    "handing",
    "over",
    "continue",
    "continued",
    "continues",
    "continuing",
    "continuation",
    "resume",
    "resumed",
    "resuming",
    "pick",
    "picking",
    "picked",
    "up",
    "previous",
    "prior",
    "earlier",
    "session",
    "sessions",
    "fresh",
    "new",
    "brief",
    "from",
    "the",
    "a",
    "an",
    "of",
    "and",
    "to",
    "in",
    "with",
    "work",
    "context",
    "read",
    "id",
    "pane",
    "this",
];

/// A title that says nothing about the work: "Handover continuation",
/// "Handover from 0bf5026f-…", "Continue from previous session".
pub fn generic(title: &str) -> bool {
    let hexish = |w: &str| w.len() >= 6 && w.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    !title
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .map(|w| w.trim_matches('-').to_lowercase())
        .any(|w| {
            w.chars().filter(|c| c.is_alphabetic()).count() >= 2
                && !HANDOVER_WORDS.contains(&w.as_str())
                && !hexish(&w)
        })
}

/// The work a brief is about, from its heading: "# Handover: Northwind image
/// packs, cut-outs, icons and explainers (session 14e562ec)" gives "Northwind
/// image packs". Nothing when the heading is only "Handover Brief".
pub(super) fn brief_title(brief: &str) -> Option<String> {
    brief_heading(brief).map(|t| short_name(&t))
}

/// A brief's heading, cleaned of "Handover:" and trailing asides, whole.
fn brief_heading(brief: &str) -> Option<String> {
    let line = brief.lines().map(str::trim).find(|l| l.starts_with('#'))?;
    let mut t = line.trim_start_matches('#').trim().to_string();
    // "Handover —", "HANDOVER:", "Handover brief -"
    let lower = t.to_lowercase();
    for p in ["handover brief", "handover"] {
        if lower.starts_with(p) {
            let rest = t[p.len()..].trim_start();
            if let Some(r) = rest.strip_prefix([':', '—', '–', '-', '·']) {
                t = r.trim().to_string();
            } else if rest.is_empty() {
                return None;
            }
            break;
        }
    }
    // Trailing asides: "(session 14e562ec)", "— session 9b506217", ", 2026-09-29 (evening)".
    loop {
        let before = t.len();
        if t.ends_with(')')
            && let Some(i) = t.rfind(" (")
        {
            t.truncate(i);
        }
        for sep in [" — ", " – ", " - ", ", "] {
            if let Some(i) = t.rfind(sep) {
                let tail = &t[i + sep.len()..];
                if generic(tail)
                    || tail
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '-' || c == ' ')
                {
                    t.truncate(i);
                }
            }
        }
        t = t.trim().trim_end_matches(['.', ',', ':']).to_string();
        if t.len() == before {
            break;
        }
    }
    if t.is_empty() || generic(&t) {
        return None;
    }
    Some(t)
}

/// What each of these sessions is working on, as the brief it continued from
/// says: session id -> that brief's heading.
pub fn topics(ids: &[&str]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for l in load_lineage().into_iter().rev() {
        if l.new_id.is_empty() || !ids.contains(&l.new_id.as_str()) || out.contains_key(&l.new_id) {
            continue;
        }
        let Ok(f) = std::fs::File::open(brief_path(&l.old_id, None)) else {
            continue;
        };
        let mut head = String::new();
        let _ = f.take(4096).read_to_string(&mut head);
        if let Some(h) = brief_heading(&head) {
            out.insert(l.new_id, h);
        }
    }
    out
}

/// A name short enough for a list row: a long heading gives its first clause
/// ("Command Center TF lane, edge header-stripping, and …" gives "Command
/// Center TF lane"), never a clipped word.
pub(super) fn short_name(t: &str) -> String {
    const MAX: usize = 48;
    if t.chars().count() <= MAX {
        return t.to_string();
    }
    for sep in [", ", " + ", " — ", " – ", " - ", "; ", ": ", " and "] {
        if let Some(i) = t.find(sep) {
            let head = t[..i].trim();
            if head.chars().count() >= 12 {
                return short_name(head);
            }
        }
    }
    let mut out = String::new();
    for w in t.split_whitespace() {
        if !out.is_empty() && out.chars().count() + 1 + w.chars().count() > MAX {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// What a successor is called, and whether that was toomux's choice. A name
/// someone gave the session stays. Otherwise (Claude's own summary, a name
/// toomux chose last time, or a generic one) the brief says what the work is
/// now; failing that, the last real name in its line of handovers, then its
/// folder.
pub(super) fn successor_name(
    s: &Session,
    brief: &str,
    ai_title: Option<&str>,
    lineage: &[Lineage],
    for_you: bool,
) -> (String, bool) {
    let title = s.title.trim();
    let chosen_before = for_you
        || lineage
            .iter()
            .any(|l| l.new_id == s.id && l.derived && l.title == title);
    let own = !title.is_empty() && !generic(title) && !chosen_before && ai_title != Some(title);
    if own {
        return (title.to_string(), false);
    }
    if let Some(t) = brief_title(brief) {
        return (t, true);
    }
    if !title.is_empty() && !generic(title) {
        return (title.to_string(), chosen_before);
    }
    // Back along the line: the session this one continued, and so on.
    let mut id = s.id.as_str();
    for _ in 0..lineage.len() {
        let Some(l) = lineage.iter().find(|l| l.new_id == id) else {
            break;
        };
        if !l.title.is_empty() && !generic(&l.title) {
            return (l.title.clone(), true);
        }
        if let Some(n) = l.name.as_ref().filter(|n| !generic(n)) {
            return (n.clone(), false);
        }
        id = &l.old_id;
    }
    let root = crate::memory::project_root(&s.cwd);
    (
        root.rsplit('/')
            .next()
            .filter(|f| !f.is_empty())
            .unwrap_or("session")
            .to_string(),
        true,
    )
}

fn lineage_path() -> PathBuf {
    dir().join("lineage.json")
}

pub(super) fn load_lineage() -> Vec<Lineage> {
    std::fs::read_to_string(lineage_path())
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_default()
}

/// Lineage is changed by handovers and ticks at once: one at a time.
fn lineage_lock() -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    let _ = std::fs::create_dir_all(dir());
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir().join("lineage.lock"))
        .ok()?;
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    Some(f)
}

pub(super) fn remember(l: Lineage) {
    let _lock = lineage_lock();
    let mut all = load_lineage();
    all.push(l);
    let excess = all.len().saturating_sub(200);
    all.drain(..excess);
    write_atomic(
        &lineage_path(),
        &serde_json::to_string(&all).unwrap_or_default(),
    );
}

/// Give a handed-over session's successor its name, pin and background
/// commands, once it appears in the same pane.
pub(super) fn carry(cfg: &Config, sessions: &[Session]) {
    if load_lineage().iter().all(|l| l.carried) {
        return;
    }
    let _lock = lineage_lock();
    let mut all = load_lineage();
    let mut changed = false;
    for l in all.iter_mut().filter(|l| !l.carried) {
        let Some(new) = sessions.iter().find(|s| {
            !s.dormant
                && s.id != l.old_id
                && s.pane.as_ref().is_some_and(|p| p.id == l.pane)
                && s.started_ms + 60_000 >= l.at_ms
        }) else {
            if registry::now_ms() - l.at_ms > 3_600_000 {
                l.carried = true;
                changed = true;
            }
            continue;
        };
        let _ = crate::state::State::update(|st| {
            if let Some(n) = &l.name {
                st.names.insert(new.id.clone(), n.clone());
            }
            if let Some(slot) = st.slot_of(&l.old_id) {
                st.pins[slot] = Some(new.pin_record(cfg));
            }
        });
        crate::jobs::pass_on(&l.old_id, &new.id);
        crate::voyage::pass_on(&l.old_id, &new.id);
        l.new_id = new.id.clone();
        l.carried = true;
        changed = true;
    }
    if changed {
        write_atomic(
            &lineage_path(),
            &serde_json::to_string(&all).unwrap_or_default(),
        );
    }
}
