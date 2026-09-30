use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub name: String,
    pub config_dir: String,
}

/// New sessions in `folder` (or below it) start on `account`.
#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub folder: String,
    pub account: String,
}

/// Four statuses and one accent carry meaning; everything else is a neutral.
/// Text neutrals step down text > dim > muted, and faint is only ever a
/// hairline. Surfaces are the grounds toomux paints so that its header, list,
/// preview and overlays read as distinct planes on any terminal background.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Colors {
    pub text: String,
    pub dim: String,
    pub muted: String,
    pub faint: String,
    pub accent: String,
    pub working: String,
    pub attention: String,
    pub finished: String,
    pub selection: String,
    pub hover: String,
    pub base: String,
    pub raised: String,
    pub well: String,
    pub overlay: String,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            text: "#dbe2ec".into(),
            dim: "#97a3b6".into(),
            muted: "#6d7a8e".into(),
            faint: "#263041".into(),
            accent: "#7dd3fc".into(),
            working: "#f5a623".into(),
            attention: "#f7788c".into(),
            finished: "#3ecf8e".into(),
            selection: "#22304a".into(),
            hover: "#151d2a".into(),
            base: "#0f141c".into(),
            raised: "#141b26".into(),
            well: "#0b0f15".into(),
            overlay: "#18202e".into(),
        }
    }
}

impl Colors {
    /// Values the first palette shipped with. A config that still carries one
    /// of them never chose it, so it follows the current default instead.
    fn upgrade(&mut self) {
        let new = Colors::default();
        for (field, old, fresh) in [
            (&mut self.text, "#cbd5e1", new.text),
            (&mut self.dim, "#64748b", new.dim),
            (&mut self.faint, "#334155", new.faint),
            (&mut self.working, "#f59e0b", new.working),
            (&mut self.attention, "#fb7185", new.attention),
            (&mut self.finished, "#34d399", new.finished),
            (&mut self.selection, "#1e293b", new.selection),
        ] {
            if field.eq_ignore_ascii_case(old) {
                *field = fresh;
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub accounts: Vec<Account>,
    /// Binary used to relaunch sessions; the account is chosen via CLAUDE_CONFIG_DIR.
    pub claude_bin: String,
    /// An idle session counts as "finished" (rather than just idle) for this long.
    pub finished_minutes: u64,
    /// A session that worked at least this long gets a notice when it stops.
    pub notify_after_secs: u64,
    /// Optional shell command run for each notice (e.g. a phone push), with
    /// TOOMUX_MESSAGE, TOOMUX_TITLE, TOOMUX_EVENT and TOOMUX_SESSION_ID set.
    pub notify_command: Option<String>,
    /// Launch flags for sessions started with ctrl-n. Unset: the flags most
    /// running sessions were started with.
    pub new_session_args: Option<Vec<String>>,
    /// Idle sessions older than this are offered for cleanup (ctrl-s).
    pub stale_days: u64,
    /// After a reboot, reopen what was running (each in its own tmux server)
    /// at the first status tick, instead of only offering it.
    pub reopen_after_restart: bool,
    /// A session idle with more context than this hands over to a fresh one
    /// (it writes a complete brief first), stopping mid-turn if it must. 0
    /// turns handover off.
    pub handover_tokens: u64,
    /// Past this, a conversation hands over where a turn ends: when you send
    /// it your next prompt, or once it has sat idle a while. 0: only at
    /// `handover_tokens`.
    pub handover_turn_end_tokens: u64,
    /// A subagent past this hands over to a fresh one mid-task (it has no
    /// turns to wait for). 0: at `handover_tokens`.
    pub subagent_handover_tokens: u64,
    /// A fork copies its caller's whole context into every call it makes:
    /// past this many tokens the caller is asked to brief a fresh
    /// general-purpose subagent instead. 0: forks are always allowed.
    pub fork_context_tokens: u64,
    /// The model that checks a voyage after every turn (`/voyage`): a small
    /// one reads the end of the conversation in a few seconds.
    #[serde(alias = "quest_judge_model")]
    pub voyage_judge_model: String,
    /// The judge at hard persistence: a stronger model, strict about
    /// evidence.
    pub voyage_hard_model: String,
    /// The judge and reviewer at relentless persistence.
    pub voyage_relentless_model: String,
    /// A voyage's scene above its session's status line: a ship sailing to
    /// the island, as far along as the judge reckons.
    #[serde(alias = "quest_voyage")]
    pub voyage_scene: bool,
    /// A voyage's persistence when `/voyage` doesn't say: light, steady,
    /// hard or relentless.
    pub voyage_persistence: String,
    /// Bash commands run through toomux: long output kept whole and shown
    /// short, background commands carried across a handover.
    pub capture_bash: bool,
    /// Upkeep corrects paths in memory that moved, and brings a moved
    /// workspace's memory to its new folder (each edit's prior text kept).
    pub fix_memory: bool,
    /// Upkeep removes git worktrees that are clean, merged and idle a day.
    pub tidy_worktrees: bool,
    /// Upkeep keeps a compressed copy of every transcript, which Claude
    /// Code otherwise deletes after 30 days.
    pub archive_transcripts: bool,
    pub rules: Vec<Rule>,
    pub colors: Colors,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            accounts: discover_accounts(),
            claude_bin: find_claude(),
            finished_minutes: 15,
            notify_after_secs: 90,
            notify_command: None,
            new_session_args: None,
            stale_days: 3,
            reopen_after_restart: true,
            handover_tokens: 400_000,
            handover_turn_end_tokens: 250_000,
            subagent_handover_tokens: 250_000,
            fork_context_tokens: 200_000,
            voyage_judge_model: "haiku".into(),
            voyage_hard_model: "sonnet".into(),
            voyage_relentless_model: "opus".into(),
            voyage_scene: true,
            voyage_persistence: "steady".into(),
            capture_bash: true,
            fix_memory: true,
            tidy_worktrees: true,
            archive_transcripts: true,
            rules: Vec::new(),
            colors: Colors::default(),
        }
    }
}

/// `fs::canonicalize`, remembered: account and session dirs are resolved for
/// every session on every reload, and they don't move while toomux runs.
pub fn canon(p: &Path) -> PathBuf {
    thread_local! {
        static SEEN: std::cell::RefCell<std::collections::HashMap<PathBuf, PathBuf>> = Default::default();
    }
    if let Some(hit) = SEEN.with(|m| m.borrow().get(p).cloned()) {
        return hit;
    }
    let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    SEEN.with(|m| m.borrow_mut().insert(p.to_path_buf(), c.clone()));
    c
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if p == "~" => home(),
        None => PathBuf::from(p),
    }
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `/home/sam/storage/x` -> `~/storage/x`
pub fn tilde(p: &str) -> String {
    let h = home();
    match Path::new(p).strip_prefix(&h) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.to_string(),
    }
}

/// Where Claude Code is: its usual install place, or else the first
/// `claude` on PATH.
fn find_claude() -> String {
    const USUAL: &str = "~/.local/bin/claude";
    if expand(USUAL).is_file() {
        return USUAL.into();
    }
    std::env::var_os("PATH")
        .and_then(|p| std::env::split_paths(&p).map(|d| d.join("claude")).find(|c| c.is_file()))
        .map(|c| tilde(&c.display().to_string()))
        .unwrap_or_else(|| USUAL.into())
}

/// Every `~/.claude` / `~/.claude-*` dir that holds its own login is an account.
fn discover_accounts() -> Vec<Account> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(home()) else { return out };
    let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n == ".claude" || n.starts_with(".claude-"))
        .filter(|n| !n.contains(".bak") && !n.contains("backup") && !crate::accounts::is_group_dir(n))
        .collect();
    names.sort();
    for n in names {
        let dir = home().join(&n);
        if !crate::credentials::present(&dir) {
            continue;
        }
        let name = n.strip_prefix(".claude-").unwrap_or("default").to_string();
        out.push(Account { name, config_dir: format!("~/{n}") });
    }
    out
}

impl Config {
    pub fn path() -> PathBuf {
        if let Some(p) = std::env::var_os("TOOMUX_CONFIG") {
            return PathBuf::from(p);
        }
        crate::paths::config().join("config.toml")
    }

    pub fn load() -> Result<Self> {
        let p = Self::path();
        if !p.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let table: toml::Table = toml::from_str(&raw).with_context(|| format!("parsing {}", p.display()))?;
        let mut cfg: Config = lift_settings(table).try_into().with_context(|| format!("parsing {}", p.display()))?;
        if cfg.accounts.is_empty() {
            cfg.accounts = discover_accounts();
        }
        cfg.colors.upgrade();
        Ok(cfg)
    }

    /// Where a conversation hands over at a turn's end (0: handover is off).
    pub fn turn_end_limit(&self) -> u64 {
        match (self.handover_tokens, self.handover_turn_end_tokens) {
            (0, _) => 0,
            (hard, 0) => hard,
            (hard, soft) => soft.min(hard),
        }
    }

    /// Where a subagent hands over (0: handover is off).
    pub fn subagent_limit(&self) -> u64 {
        match (self.handover_tokens, self.subagent_handover_tokens) {
            (0, _) => 0,
            (hard, 0) => hard,
            (hard, sub) => sub.min(hard),
        }
    }

    /// A voyage's persistence when `/voyage` doesn't say.
    pub fn persistence(&self) -> crate::voyage::Persistence {
        crate::voyage::Persistence::parse(&self.voyage_persistence).unwrap_or_default()
    }

    pub fn account_dir(&self, i: usize) -> PathBuf {
        expand(&self.accounts[i].config_dir)
    }

    /// Which account a CLAUDE_CONFIG_DIR value (None = unset) belongs to.
    pub fn account_for(&self, config_dir: Option<&str>) -> Option<usize> {
        let target = canon(&config_dir.map(expand).unwrap_or_else(|| home().join(".claude")));
        self.accounts.iter().position(|a| canon(&expand(&a.config_dir)) == target)
    }

    pub fn account_by_name(&self, name: &str) -> Option<usize> {
        self.accounts.iter().position(|a| a.name.eq_ignore_ascii_case(name))
    }

    /// Distinct session registries across accounts (usually one shared dir).
    pub fn session_dirs(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        let mut dirs: Vec<PathBuf> = (0..self.accounts.len()).map(|i| self.account_dir(i)).collect();
        dirs.push(home().join(".claude"));
        for d in dirs {
            let s = d.join("sessions");
            if s.is_dir() {
                let c = canon(&s);
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
        out
    }

    pub fn render_default() -> String {
        let accounts = discover_accounts()
            .iter()
            .map(|a| format!("[[accounts]]\nname = \"{}\"\nconfig_dir = \"{}\"\n", a.name, a.config_dir))
            .collect::<Vec<_>>()
            .join("\n");
        let claude = find_claude();
        format!(
            r##"# toomux configuration. Changes apply the next time toomux opens.

# Binary used when a session is relaunched (account switch, bringing into tmux).
claude_bin = "{claude}"

# An idle session shows as "finished" for this many minutes, then settles to idle.
finished_minutes = 15

# A session that worked for at least this long gets a quiet notice in the
# status bar when it finishes. Needing you always gets one.
notify_after_secs = 90

# Optional command for each notice, e.g. a phone push via ntfy:
# notify_command = 'curl -s -d "$TOOMUX_MESSAGE" ntfy.sh/your-topic'
# or a macOS notification (the guide has one for WSL too):
# notify_command = '''osascript -e 'on run argv' -e 'display notification (item 1 of argv) with title (item 2 of argv)' -e 'end run' "$TOOMUX_MESSAGE" "$TOOMUX_TITLE"'''

# Idle sessions older than this many days are offered for cleanup (ctrl-s).
stale_days = 3

# After a reboot, reopen the sessions that were running, each in its own tmux
# server, as soon as tmux is up. Off: they're offered under "before the restart".
reopen_after_restart = true

# A session idle with more context than this many tokens writes a complete
# handover brief and continues in a fresh session (its old conversation stays
# searchable). Every call re-reads the whole context, so this is where tokens
# go; 0 turns it off.
handover_tokens = 400000
# Past this many, a conversation hands over at a natural break instead: when
# you send it your next prompt (the fresh session does what you asked), or
# once it has sat idle ten minutes. 0: only at handover_tokens.
handover_turn_end_tokens = 250000
# Subagents have no turns to wait for: past this many they hand over to a
# fresh subagent mid-task. 0: at handover_tokens.
subagent_handover_tokens = 250000
# A fork starts with a copy of its caller's whole context and re-reads it on
# every call. Past this many, the caller is asked to launch a general-purpose
# subagent with a complete brief instead. 0: forks are always allowed.
fork_context_tokens = 200000

# /voyage <outcome> keeps a session at it, turn after turn and across
# handovers, until it's done. After each turn this model reads the end of the
# conversation and says whether it is (light and steady persistence).
voyage_judge_model = "haiku"
# The judge at hard persistence, and the judge and reviewer at relentless.
voyage_hard_model = "sonnet"
voyage_relentless_model = "opus"
# While a voyage runs, its session's status line shows its scene: a ship
# sailing to an island, as far along as the judge reckons, landing when it's
# done. Five lines tall; the toomux detail pane and sidebar show it too.
# false: the one-line chip only.
voyage_scene = true
# How hard a voyage pushes, when /voyage doesn't say (--persistence):
#   light       the judge's word is enough; pauses after 2 idle turns or 4
#               checks with no headway
#   steady      the judge wants evidence shown; a nudge after 4 checks with
#               no headway, a pause after 8
#   hard        voyage_hard_model judges, wanting fresh evidence; a proof lap
#               before done; a nudge every 3 checks with no headway, a pause
#               after 12; one push back before it stops for you or gives up
#   relentless  voyage_relentless_model judges; two proof laps and a
#               sceptical review of every change since it began; a nudge
#               every 2 checks with no headway, a pause after 20; two push
#               backs
voyage_persistence = "steady"

# Bash commands run through toomux: long output is kept whole and shown short
# (the whole of it a command away), and background commands carry over a
# handover. false: commands run as Claude Code gives them.
capture_bash = true

# Upkeep, hourly. Each edit keeps the file's prior text; `toomux upkeep
# --dry-run` shows what it would do.
# Correct paths in memory that moved, and bring a moved workspace's memory to
# its new folder. false: only reported.
fix_memory = true
# Remove git worktrees that are clean, merged and idle a day (never branches,
# never uncommitted work). false: only reported.
tidy_worktrees = true
# Keep a compressed copy of every transcript (Claude Code deletes them after 30
# days), so old conversations stay searchable and whole.
archive_transcripts = true

# Launch flags for new sessions (ctrl-n). Leave unset to use the flags most of
# your running sessions were started with.
# new_session_args = ["--dangerously-skip-permissions"]

# Accounts are Claude Code config dirs, selected with CLAUDE_CONFIG_DIR.
# Sessions can be moved between any of these with ctrl-a.
{accounts}
# New sessions in a folder (or below it) start on the given account.
# [[rules]]
# folder = "~/work"
# account = "work"
# Colours. Four statuses and one accent carry meaning; the rest are neutrals.
# text > dim > muted step down for less important words, faint is hairlines.
# base, raised, well and overlay are the grounds toomux paints: the list,
# header and footer bands, the live preview, and pop-overs.
[colors]
text      = "#dbe2ec"
dim       = "#97a3b6"
muted     = "#6d7a8e"
faint     = "#263041"
accent    = "#7dd3fc"
working   = "#f5a623"
attention = "#f7788c"
finished  = "#3ecf8e"
selection = "#22304a"
hover     = "#151d2a"
base      = "#0f141c"
raised    = "#141b26"
well      = "#0b0f15"
overlay   = "#18202e"
"##
        )
    }
}

/// Write the accounts into the config file, keeping everything else as it was
/// written (comments too). `renamed` also carries rules over to a new name.
pub fn save_accounts(accounts: &[Account], renamed: Option<(&str, &str)>) -> Result<()> {
    let path = Config::path();
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| Config::render_default());
    let mut doc: toml_edit::DocumentMut = raw.parse().with_context(|| format!("parsing {}", path.display()))?;
    let mut list = toml_edit::ArrayOfTables::new();
    for a in accounts {
        let mut t = toml_edit::Table::new();
        t["name"] = toml_edit::value(a.name.as_str());
        t["config_dir"] = toml_edit::value(a.config_dir.as_str());
        list.push(t);
    }
    // In place when there were accounts already, so they stay where they were.
    match doc.get_mut("accounts").and_then(|i| i.as_array_of_tables_mut()) {
        Some(old) => {
            old.clear();
            for t in list.iter() {
                old.push(t.clone());
            }
        }
        None => doc["accounts"] = toml_edit::Item::ArrayOfTables(list),
    }
    if let Some((from, to)) = renamed
        && let Some(rules) = doc.get_mut("rules").and_then(|i| i.as_array_of_tables_mut()) {
            for r in rules.iter_mut() {
                if r.get("account").and_then(|v| v.as_str()) == Some(from) {
                    r["account"] = toml_edit::value(to);
                }
            }
        }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension(format!("toml.{}", std::process::id()));
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(tmp, &path)?;
    Ok(())
}

/// Settings written below an `[[accounts]]` table belong to that account as
/// far as TOML is concerned (toomux's first config template did this). Move
/// them back to the top level so they take effect.
fn lift_settings(mut t: toml::Table) -> toml::Table {
    const TOP: &[&str] = &[
        "claude_bin",
        "finished_minutes",
        "notify_after_secs",
        "notify_command",
        "new_session_args",
        "stale_days",
        "reopen_after_restart",
        "handover_tokens",
        "handover_turn_end_tokens",
        "subagent_handover_tokens",
        "fork_context_tokens",
        "voyage_judge_model",
        "voyage_hard_model",
        "voyage_relentless_model",
        "voyage_scene",
        "voyage_persistence",
        "capture_bash",
        "fix_memory",
        "tidy_worktrees",
        "archive_transcripts",
    ];
    let mut lifted = Vec::new();
    if let Some(toml::Value::Array(accounts)) = t.get_mut("accounts") {
        for a in accounts.iter_mut() {
            if let toml::Value::Table(a) = a {
                for k in TOP {
                    if let Some(v) = a.remove(*k) {
                        lifted.push((k.to_string(), v));
                    }
                }
            }
        }
    }
    for (k, v) in lifted {
        t.entry(k).or_insert(v);
    }
    t
}

pub fn hex(s: &str) -> (u8, u8, u8) {
    let s = s.trim_start_matches('#');
    let p = |i: usize| u8::from_str_radix(s.get(i..i + 2).unwrap_or("80"), 16).unwrap_or(128);
    (p(0), p(2), p(4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_settings_are_top_level() {
        let raw = Config::render_default().replace("finished_minutes = 15", "finished_minutes = 7");
        let raw = raw + "\n[[accounts]]\nname = \"x\"\nconfig_dir = \"/x\"\n";
        let t: toml::Table = toml::from_str(&raw).unwrap();
        let cfg: Config = lift_settings(t).try_into().unwrap();
        assert_eq!(cfg.finished_minutes, 7);
    }

    #[test]
    fn a_quest_setting_still_sets_its_voyage() {
        let t: toml::Table = toml::from_str("quest_judge_model = \"sonnet\"\nquest_voyage = false\n").unwrap();
        let cfg: Config = lift_settings(t).try_into().unwrap();
        assert_eq!((cfg.voyage_judge_model.as_str(), cfg.voyage_scene), ("sonnet", false));
    }

    #[test]
    fn settings_under_an_account_are_lifted() {
        let raw = "[[accounts]]\nname = \"a\"\nconfig_dir = \"/a\"\nfinished_minutes = 3\nnotify_after_secs = 5\n";
        let t: toml::Table = toml::from_str(raw).unwrap();
        let cfg: Config = lift_settings(t).try_into().unwrap();
        assert_eq!((cfg.finished_minutes, cfg.notify_after_secs), (3, 5));
        assert_eq!(cfg.accounts.len(), 1);
    }
}
