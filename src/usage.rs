//! How much of each account's plan is used: the 5-hour and weekly limits.
//!
//! Three sources, the freshest winning:
//! - live: Claude Code gives its status line command the account's
//!   `rate_limits` after every response. toomux is that command
//!   (`toomux statusline`), so any running session keeps its account current.
//! - fetched: an account with no live report for a while is looked up on
//!   Anthropic's usage endpoint (the one behind Claude Code's /usage) with that
//!   account's sign-in. Read only: the token is never refreshed or written, and
//!   is passed to curl on stdin, never on its command line.
//! - a session stopped by a limit, from its transcript.
//!
//! Everything lives in ~/.local/state/toomux/usage.json.

use crate::config::Config;
use crate::registry::Session;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;

/// How recently a session must have talked to the API for the limits it
/// reports to count as current.
const FRESH_MS: i64 = 90_000;
/// A live report older than this sends toomux to the endpoint instead.
pub const STALE_MS: i64 = 10 * 60_000;
const FETCH_EVERY_MS: i64 = 15 * 60_000;
const HISTORY_MS: i64 = 8 * 86_400_000;
/// Warn once per window when it passes this much.
pub const WARN_AT: f64 = 90.0;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Window {
    /// Percent of the limit used, 0-100 (above 100 once over).
    pub used: f64,
    /// Unix seconds.
    pub resets_at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Sample {
    at: i64,
    five: Option<f64>,
    week: Option<f64>,
    five_resets: Option<i64>,
    week_resets: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Fetch {
    last_try_ms: i64,
    next_ms: i64,
    error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AccountBook {
    live: Option<Report>,
    fetched: Option<Report>,
    #[serde(default)]
    history: Vec<Sample>,
    #[serde(default)]
    fetch: Fetch,
    /// (window, resets_at) pairs already warned about.
    #[serde(default)]
    warned: Vec<(String, i64)>,
}

/// What the status line learns about a session besides its account.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Percent of the context window in use.
    pub context: Option<f64>,
    /// Tokens in the context right now.
    #[serde(default)]
    pub tokens: Option<u64>,
    pub model: Option<String>,
    /// Claude Code's own tally of what the session has cost, in dollars at
    /// API prices.
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub model_id: Option<String>,
    pub at_ms: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Book {
    #[serde(default)]
    accounts: HashMap<String, AccountBook>,
    #[serde(default)]
    sessions: HashMap<String, SessionInfo>,
}

fn dir() -> PathBuf {
    crate::paths::state()
}

fn load_book() -> Book {
    std::fs::read_to_string(dir().join("usage.json")).ok().and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default()
}

/// The book as last written, re-read only when the file changes. For readers
/// that look every second (the UI), not for read-modify-write.
fn cached_book<R>(f: impl FnOnce(&Book) -> R) -> R {
    thread_local! {
        static CACHE: std::cell::RefCell<Option<(Option<std::time::SystemTime>, u64, Book)>> = const { std::cell::RefCell::new(None) };
    }
    let md = std::fs::metadata(dir().join("usage.json")).ok();
    let key = (md.as_ref().and_then(|m| m.modified().ok()), md.as_ref().map_or(0, |m| m.len()));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.as_ref().is_none_or(|(t, l, _)| (*t, *l) != key) {
            *c = Some((key.0, key.1, load_book()));
        }
        f(&c.as_ref().unwrap().2)
    })
}

/// Read-modify-write under a lock: status lines from many sessions land at once.
fn update<R>(f: impl FnOnce(&mut Book) -> (R, bool)) -> R {
    let _ = std::fs::create_dir_all(dir());
    let lock = std::fs::OpenOptions::new().create(true).append(true).open(dir().join("usage.lock"));
    let _guard = lock.as_ref().ok().inspect(|f| {
        use std::os::fd::AsRawFd;
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    });
    let mut book = load_book();
    let (r, changed) = f(&mut book);
    if changed
        && let Ok(json) = serde_json::to_string(&book) {
            let tmp = dir().join(format!(".usage.{}", std::process::id()));
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, dir().join("usage.json"));
            }
        }
    r
}

fn window(v: Option<&Value>, percent_key: &str) -> Option<Window> {
    let v = v?;
    let used = v.get(percent_key)?.as_f64()?;
    let resets_at = match v.get("resets_at")? {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s).ok()?.timestamp(),
        _ => return None,
    };
    Some(Window { used, resets_at })
}

impl AccountBook {
    fn absorb(&mut self, r: &Report, fetched: bool) {
        if fetched {
            self.fetched = Some(r.clone());
        } else {
            self.live = Some(r.clone());
        }
        let s = Sample {
            at: r.at_ms,
            five: r.five_hour.map(|w| w.used),
            week: r.seven_day.map(|w| w.used),
            five_resets: r.five_hour.map(|w| w.resets_at),
            week_resets: r.seven_day.map(|w| w.resets_at),
        };
        let moved = match self.history.last() {
            None => true,
            Some(l) => {
                let d = |a: Option<f64>, b: Option<f64>| match (a, b) {
                    (Some(a), Some(b)) => (a - b).abs() >= 0.5,
                    (a, b) => a.is_some() != b.is_some(),
                };
                d(l.five, s.five)
                    || d(l.week, s.week)
                    || l.five_resets != s.five_resets
                    || l.week_resets != s.week_resets
                    || s.at - l.at >= 15 * 60_000
            }
        };
        if moved {
            self.history.push(s);
        }
        let cutoff = r.at_ms - HISTORY_MS;
        self.history.retain(|s| s.at >= cutoff);
        let excess = self.history.len().saturating_sub(800);
        self.history.drain(..excess);
    }

    /// The freshest report, whichever way it arrived.
    fn latest(&self) -> Option<(&Report, Source)> {
        match (&self.live, &self.fetched) {
            (Some(l), Some(f)) if f.at_ms > l.at_ms => Some((f, Source::Fetched)),
            (Some(l), _) => Some((l, Source::Live)),
            (None, Some(f)) => Some((f, Source::Fetched)),
            (None, None) => None,
        }
    }
}

// ---- status line ------------------------------------------------------------

/// `toomux statusline`: record what Claude Code reports, print a quiet line.
pub fn statusline(cfg: &Config) -> String {
    let mut raw = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut raw);
    let Ok(v) = serde_json::from_str::<Value>(&raw) else { return String::new() };
    let now = crate::registry::now_ms();
    let account = account_of(cfg, &v);
    // Claude Code hands over the limits from its last API response, however
    // old. Only a session that just talked to the API (its transcript was
    // written moments ago) speaks for the account now.
    let exchanged = v
        .get("transcript_path")
        .and_then(Value::as_str)
        .and_then(|t| std::fs::metadata(t).ok()?.modified().ok())
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .filter(|&t| now - t < FRESH_MS);
    let rl = v.get("rate_limits").filter(|_| exchanged.is_some());
    let report = rl.map(|rl| Report {
        five_hour: window(rl.get("five_hour"), "used_percentage"),
        seven_day: window(rl.get("seven_day"), "used_percentage"),
        at_ms: exchanged.unwrap_or(now).min(now),
    });
    let info = SessionInfo {
        context: v.pointer("/context_window/used_percentage").and_then(Value::as_f64),
        tokens: v.pointer("/context_window/total_input_tokens").and_then(Value::as_u64),
        model: v.pointer("/model/display_name").and_then(Value::as_str).map(str::to_string),
        cost: v.pointer("/cost/total_cost_usd").and_then(Value::as_f64),
        model_id: v.pointer("/model/id").and_then(Value::as_str).map(str::to_string),
        at_ms: now,
    };
    let sid = v.get("session_id").and_then(Value::as_str).unwrap_or_default().to_string();
    let name = account.map(|i| cfg.accounts[i].name.clone());
    update(|b| {
        let mut changed = false;
        if !sid.is_empty() {
            let prev = b.sessions.get(&sid);
            let differs = prev.is_none_or(|p| {
                p.context.map(|c| c.round()) != info.context.map(|c| c.round())
                    || p.tokens.map(|t| t / 1000) != info.tokens.map(|t| t / 1000)
                    || p.model != info.model
                    || p.cost.map(|c| (c * 10.0) as i64) != info.cost.map(|c| (c * 10.0) as i64)
                    || now - p.at_ms > 60_000
            });
            if differs {
                b.sessions.insert(sid.clone(), info.clone());
                b.sessions.retain(|_, s| now - s.at_ms < 86_400_000);
                changed = true;
            }
        }
        if let (Some(name), Some(r)) = (&name, &report) {
            let a = b.accounts.entry(name.clone()).or_default();
            let same = a.live.as_ref().is_some_and(|l| {
                (l.five_hour == r.five_hour && l.seven_day == r.seven_day && r.at_ms - l.at_ms < 60_000) || l.at_ms > r.at_ms
            });
            if !same {
                a.absorb(r, false);
                changed = true;
            }
        }
        ((), changed)
    });
    // Show the account's best-known numbers, not this session's cached ones.
    let current = name.as_ref().and_then(|n| {
        cached_book(|b| b.accounts.get(n).and_then(|a| a.latest().map(|(r, _)| r.clone())))
    });
    let mut line = render_statusline(cfg, name.as_deref(), current.as_ref().or(report.as_ref()), &info, now);
    // A voyage leads the line: the one thing this session is for. Its
    // voyage sails above it.
    let voyage = crate::voyage::open_for(&sid).or_else(|| crate::voyage::landed_for(&sid, now));
    if let Some(q) = voyage {
        let c = &cfg.colors;
        let waiting = q.status == crate::voyage::Status::Paused || q.at_limit;
        let tone = if waiting { &c.attention } else if q.status == crate::voyage::Status::Met { &c.finished } else { &c.accent };
        let mut chip = format!("{}◎ {}", ansi(tone), q.chip(now));
        if q.status == crate::voyage::Status::Met {
            chip.push_str(&format!("{} after {}", ansi(&c.muted), crate::voyage::turns_taken(&q)));
        } else if let Some(p) = q.progress {
            chip.push_str(&format!("{} · about {p}% there, by the judge", ansi(&c.muted)));
        }
        chip.push_str("\x1b[0m");
        line = if line.is_empty() { chip } else { format!("{chip}{} · {line}", ansi(&c.muted)) };
        // Claude Code sets COLUMNS to the terminal's width, and keeps two
        // cells either side of the line.
        let cols = std::env::var("COLUMNS").ok().and_then(|c| c.parse::<usize>().ok()).unwrap_or(80);
        let width = cols.saturating_sub(4).min(crate::scene::MAX_WIDTH);
        if cfg.voyage_scene && width >= crate::scene::MIN_WIDTH {
            let mut art = crate::scene::render(width, &q.scene(now)).join("\n");
            art.push('\n');
            line = art + &line;
        }
    }
    line
}

/// Which account a status line call comes from: its CLAUDE_CONFIG_DIR, else
/// the config dir its transcript lives under.
fn account_of(cfg: &Config, v: &Value) -> Option<usize> {
    let env = std::env::var("CLAUDE_CONFIG_DIR").ok();
    if env.is_some() {
        return cfg.account_for(env.as_deref());
    }
    let t = v.get("transcript_path").and_then(Value::as_str).unwrap_or_default();
    (0..cfg.accounts.len())
        .find(|&i| {
            let d = crate::config::canon(&cfg.account_dir(i));
            !t.is_empty() && std::path::Path::new(t).starts_with(&d)
        })
        .or_else(|| cfg.account_for(None))
}

fn ansi(hex: &str) -> String {
    let (r, g, b) = crate::config::hex(hex);
    format!("\x1b[38;2;{r};{g};{b}m")
}

fn render_statusline(cfg: &Config, account: Option<&str>, r: Option<&Report>, info: &SessionInfo, now: i64) -> String {
    let c = &cfg.colors;
    let sep = format!("{} · ", ansi(&c.muted));
    let mut parts: Vec<String> = Vec::new();
    if let Some(a) = account {
        parts.push(format!("{}{a}", ansi(&c.muted)));
    }
    // Context in tokens, measured against where it hands over: amber past
    // the turn-end limit, rose near the hard one.
    match (info.tokens, info.context) {
        (Some(t), _) if cfg.handover_tokens > 0 => {
            let used = 100.0 * t as f64 / cfg.handover_tokens as f64;
            let warn = 100.0 * cfg.turn_end_limit() as f64 / cfg.handover_tokens as f64;
            parts.push(format!("{}ctx {}{}k", ansi(&c.muted), ansi(tone(cfg, used, warn)), t / 1000));
        }
        (Some(t), _) => parts.push(format!("{}ctx {}{}k", ansi(&c.muted), ansi(&c.dim), t / 1000)),
        (None, Some(ctx)) => parts.push(format!("{}ctx {}{:.0}%", ansi(&c.muted), ansi(tone(cfg, ctx, 70.0)), ctx)),
        _ => {}
    }
    if let Some(cost) = info.cost.filter(|c| *c >= 0.01) {
        parts.push(format!("{}${cost:.2}", ansi(&c.muted)));
    }
    for (label, w) in [("5h", r.and_then(|r| r.five_hour)), ("wk", r.and_then(|r| r.seven_day))] {
        let Some(w) = w.filter(|w| w.resets_at * 1000 > now) else { continue };
        let figure = if w.used >= 100.0 { "limit".to_string() } else { format!("{:.0}%", w.used) };
        let mut s = format!("{}{label} {}{figure}", ansi(&c.muted), ansi(tone(cfg, w.used, 80.0)));
        if w.used >= 70.0 {
            s.push_str(&format!("{} resets {}", ansi(&c.muted), crate::registry::duration(w.resets_at * 1000 - now)));
        }
        parts.push(s);
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("{}\x1b[0m", parts.join(&sep))
}

/// Numbers stay quiet until they matter: dim, amber from `warn`, rose near 100.
fn tone(cfg: &Config, used: f64, warn: f64) -> &str {
    let c = &cfg.colors;
    if used >= 95.0 {
        &c.attention
    } else if used >= warn {
        &c.working
    } else {
        &c.dim
    }
}

// ---- reading it back ----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    Live,
    Fetched,
    /// Only known because a session hit the limit.
    Transcript,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pace {
    /// At the recent rate, the limit is reached this many ms from now, before
    /// the window resets.
    LimitIn(i64),
    /// At the recent rate, the window ends at about this percent.
    EndsAt(f64),
}

#[derive(Clone, Debug)]
pub struct Meter {
    pub used: f64,
    /// Unix ms.
    pub resets_ms: i64,
    /// A session has been stopped by this limit.
    pub limited: bool,
    pub pace: Option<Pace>,
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub five: Option<Meter>,
    pub week: Option<Meter>,
    /// When the numbers were reported (ms); 0 if never.
    pub at_ms: i64,
    pub source: Option<Source>,
    /// Why the numbers may be old ("the usage endpoint is rate limiting").
    pub problem: Option<String>,
    /// Weekly percent over the last day, oldest first, for a sparkline.
    pub trend: Vec<f64>,
}

impl Usage {
    /// The window closest to its limit.
    pub fn tightest(&self) -> Option<(&'static str, &Meter)> {
        let five = self.five.as_ref().map(|m| ("5h", m));
        let week = self.week.as_ref().map(|m| ("wk", m));
        match (five, week) {
            (Some(a), Some(b)) => Some(if b.1.limited || (!a.1.limited && b.1.used >= a.1.used) { b } else { a }),
            (a, b) => a.or(b),
        }
    }

    pub fn stale(&self, now: i64) -> bool {
        self.at_ms > 0 && now - self.at_ms > 30 * 60_000
    }
}

/// Usage for every account, in config order.
pub fn summary(cfg: &Config, sessions: &[Session], now: i64) -> Vec<Usage> {
    cached_book(|book| summarize(cfg, book, sessions, now))
}

fn summarize(cfg: &Config, book: &Book, sessions: &[Session], now: i64) -> Vec<Usage> {
    (0..cfg.accounts.len())
        .map(|i| {
            let name = &cfg.accounts[i].name;
            let ab = book.accounts.get(name);
            let mut u = Usage::default();
            if let Some((r, src)) = ab.and_then(|a| a.latest()) {
                u.at_ms = r.at_ms;
                u.source = Some(src);
                let hist = ab.map(|a| a.history.as_slice()).unwrap_or(&[]);
                u.five = r.five_hour.map(|w| meter(w, now, pace(hist, now, w, true)));
                u.week = r.seven_day.map(|w| meter(w, now, pace(hist, now, w, false)));
                u.trend = trend(hist, now);
            }
            if let Some(err) = ab.and_then(|a| a.fetch.error.clone())
                && (u.at_ms == 0 || now - u.at_ms > STALE_MS) {
                    u.problem = Some(err);
                }
            // A session stopped by a limit is the surest sign of all.
            for s in sessions.iter().filter(|s| s.account == Some(i) && !s.dormant) {
                let Some(text) = &s.limit else { continue };
                let Some(at) = crate::transcript::reset_at(text) else { continue };
                let resets_ms = at.timestamp_millis();
                if resets_ms <= now {
                    continue;
                }
                let m = Meter { used: 100.0, resets_ms, limited: true, pace: None };
                let slot = if text.contains("weekly") { &mut u.week } else { &mut u.five };
                match slot {
                    Some(cur) if cur.used >= 100.0 || cur.resets_ms == resets_ms => cur.limited = true,
                    Some(cur) if u.at_ms > s.since_ms => {
                        // A newer report says there's room again.
                        let _ = cur;
                    }
                    _ => *slot = Some(m),
                }
                if u.source.is_none() {
                    u.source = Some(Source::Transcript);
                    u.at_ms = s.since_ms;
                }
            }
            u
        })
        .collect()
}

fn meter(w: Window, now: i64, pace: Option<Pace>) -> Meter {
    let resets_ms = w.resets_at * 1000;
    if resets_ms <= now {
        // The window has rolled over since the report: nothing used yet.
        return Meter { used: 0.0, resets_ms: 0, limited: false, pace: None };
    }
    Meter { used: w.used, resets_ms, limited: w.used >= 100.0, pace }
}

/// Recent burn rate within the current window, projected forward.
fn pace(hist: &[Sample], now: i64, w: Window, five: bool) -> Option<Pace> {
    // At the limit there is nothing left to project ("comes in 0s").
    if w.used >= 100.0 {
        return None;
    }
    let look = if five { 60 * 60_000 } else { 12 * 3_600_000 };
    let pts: Vec<(i64, f64)> = hist
        .iter()
        .filter(|s| now - s.at <= look)
        .filter_map(|s| {
            let (used, resets) = if five { (s.five, s.five_resets) } else { (s.week, s.week_resets) };
            (resets == Some(w.resets_at)).then_some((s.at, used?))
        })
        .collect();
    let (&(t0, u0), &(t1, u1)) = (pts.first()?, pts.last()?);
    let span = t1 - t0;
    if span < 10 * 60_000 || u1 <= u0 {
        return None;
    }
    let rate = (u1 - u0) / span as f64; // percent per ms
    let reset_ms = w.resets_at * 1000;
    let eta = ((100.0 - w.used) / rate) as i64;
    if now + eta < reset_ms {
        Some(Pace::LimitIn(eta))
    } else {
        Some(Pace::EndsAt((w.used + rate * (reset_ms - now) as f64).min(100.0)))
    }
}

/// Weekly percent over the last 24h in 24 hourly buckets (carrying forward).
fn trend(hist: &[Sample], now: i64) -> Vec<f64> {
    let start = now - 86_400_000;
    let mut last = hist.iter().rev().find(|s| s.at < start).and_then(|s| s.week);
    let mut out = Vec::new();
    for h in 0..24 {
        let (a, b) = (start + h * 3_600_000, start + (h + 1) * 3_600_000);
        for s in hist.iter().filter(|s| s.at >= a && s.at < b) {
            if s.week.is_some() {
                last = s.week;
            }
        }
        out.push(last.unwrap_or(0.0));
    }
    // Only worth drawing with most of a day behind it and some movement.
    let covered = hist.first().is_some_and(|s| s.at <= now - 6 * 3_600_000);
    let (lo, hi) = out.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)));
    if covered && hi - lo >= 2.0 { out } else { Vec::new() }
}

/// Plan name from the account's sign-in record (never the token).
pub fn plan(cfg: &Config, i: usize) -> Option<String> {
    let raw = crate::credentials::read(&cfg.account_dir(i))?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let o = v.get("claudeAiOauth")?;
    let tier = o.get("rateLimitTier").and_then(Value::as_str).unwrap_or_default();
    let sub = o.get("subscriptionType").and_then(Value::as_str)?;
    Some(match tier.rsplit('_').next() {
        Some(x) if x.ends_with('x') && x[..x.len() - 1].chars().all(|c| c.is_ascii_digit()) => format!("{sub} {x}"),
        _ => sub.to_string(),
    })
}

/// Per-session context and model, as last reported by its status line.
pub fn sessions() -> HashMap<String, SessionInfo> {
    cached_book(|b| b.sessions.clone())
}

// ---- the endpoint fallback ----------------------------------------------------

/// Accounts whose numbers are old enough, and not recently tried, to look up.
pub fn due(cfg: &Config, now: i64) -> Vec<usize> {
    let book = load_book();
    (0..cfg.accounts.len())
        .filter(|&i| {
            let a = book.accounts.get(&cfg.accounts[i].name);
            let fresh = a.and_then(|a| a.latest()).is_some_and(|(r, _)| now - r.at_ms < STALE_MS);
            let waiting = a.is_some_and(|a| a.fetch.next_ms > now);
            !fresh && !waiting && crate::credentials::present(&cfg.account_dir(i))
        })
        .collect()
}

/// Start `toomux usage --fetch` in the background if anything is due. Cheap
/// enough to call from every status bar refresh.
pub fn fetch_in_background(cfg: &Config, now: i64) {
    if due(cfg, now).is_empty() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["usage", "--fetch"])
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
    let _ = cmd.spawn();
}

/// Look up every due account now. One fetcher at a time.
pub fn fetch_due(cfg: &Config) -> Vec<String> {
    let _ = std::fs::create_dir_all(dir());
    let Ok(lock) = std::fs::OpenOptions::new().create(true).append(true).open(dir().join("fetch.lock")) else {
        return vec![];
    };
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return vec!["another lookup is running".into()];
    }
    let now = crate::registry::now_ms();
    let mut said = Vec::new();
    for i in due(cfg, now) {
        let name = cfg.accounts[i].name.clone();
        // Claim the slot first so a status bar tick doesn't start another.
        update(|b| {
            let f = &mut b.accounts.entry(name.clone()).or_default().fetch;
            f.last_try_ms = now;
            f.next_ms = now + FETCH_EVERY_MS;
            ((), true)
        });
        let (report, error, backoff) = match fetch(cfg, i) {
            Ok(r) => (Some(r), None, FETCH_EVERY_MS),
            Err(Failure { msg, retry_ms }) => (None, Some(msg), retry_ms),
        };
        said.push(match (&report, &error) {
            (Some(_), _) => format!("{name}: updated"),
            (_, Some(e)) => format!("{name}: {e}"),
            _ => unreachable!(),
        });
        update(|b| {
            let a = b.accounts.entry(name.clone()).or_default();
            if let Some(r) = &report {
                a.absorb(r, true);
            }
            a.fetch.error = error.clone();
            a.fetch.next_ms = crate::registry::now_ms() + backoff;
            ((), true)
        });
    }
    said
}

struct Failure {
    msg: String,
    retry_ms: i64,
}

fn fail(msg: &str, retry_min: i64) -> Failure {
    Failure { msg: msg.into(), retry_ms: retry_min * 60_000 }
}

fn fetch(cfg: &Config, i: usize) -> Result<Report, Failure> {
    let raw = crate::credentials::read(&cfg.account_dir(i)).ok_or_else(|| fail("no sign-in found", 60))?;
    let v: Value = serde_json::from_str(&raw).map_err(|_| fail("sign-in record unreadable", 60))?;
    let o = v.get("claudeAiOauth").ok_or_else(|| fail("not signed in with a claude plan", 240))?;
    let token = o.get("accessToken").and_then(Value::as_str).ok_or_else(|| fail("not signed in", 60))?;
    let expires = o.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    if expires > 0 && expires < crate::registry::now_ms() {
        return Err(fail("sign-in has lapsed · it renews next time a session runs on it", 30));
    }
    let mut child = std::process::Command::new("curl")
        .args(["-sS", "--max-time", "8", "-H", "@-", "-w", "\n%{http_code}", "https://api.anthropic.com/api/oauth/usage"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| fail("curl isn't installed", 240))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = write!(
            stdin,
            "Authorization: Bearer {token}\nanthropic-beta: oauth-2025-04-20\nAccept: application/json\nUser-Agent: toomux/{}\n",
            env!("CARGO_PKG_VERSION")
        );
    }
    let out = child.wait_with_output().map_err(|_| fail("lookup failed", 10))?;
    let body = String::from_utf8_lossy(&out.stdout);
    let (json, code) = body.rsplit_once('\n').unwrap_or(("", body.as_ref()));
    match code.trim() {
        "200" => {}
        "401" | "403" => return Err(fail("sign-in was refused · it renews next time a session runs on it", 60)),
        "429" => return Err(fail("the usage lookup is rate limited · trying again in 30m", 30)),
        "000" | "" => return Err(fail("couldn't reach anthropic", 10)),
        c => return Err(fail(&format!("usage lookup answered {c}"), 30)),
    }
    let v: Value = serde_json::from_str(json).map_err(|_| fail("usage lookup gave an unexpected answer", 60))?;
    Ok(Report {
        five_hour: window(v.get("five_hour"), "utilization"),
        seven_day: window(v.get("seven_day"), "utilization"),
        at_ms: crate::registry::now_ms(),
    })
}

// ---- warnings -------------------------------------------------------------------

/// Windows that just passed WARN_AT, each reported once. For the status bar.
pub fn crossed(cfg: &Config, usage: &[Usage]) -> Vec<String> {
    let mut hits: Vec<(String, String, i64, String)> = Vec::new();
    let now = crate::registry::now_ms();
    for (i, u) in usage.iter().enumerate() {
        for (key, label, m) in [("5h", "5-hour", &u.five), ("wk", "weekly", &u.week)] {
            let Some(m) = m.as_ref().filter(|m| m.used >= WARN_AT && !m.limited && m.resets_ms > now) else { continue };
            let name = &cfg.accounts[i].name;
            let msg = format!(
                "{name} has used {:.0}% of its {label} limit · resets in {}",
                m.used,
                crate::registry::duration(m.resets_ms - now)
            );
            hits.push((name.clone(), key.to_string(), m.resets_ms / 1000, msg));
        }
    }
    if hits.is_empty() {
        return Vec::new();
    }
    update(|b| {
        let mut out = Vec::new();
        for (name, key, resets, msg) in hits {
            let a = b.accounts.entry(name).or_default();
            if !a.warned.iter().any(|(k, r)| *k == key && *r == resets) {
                a.warned.push((key, resets));
                let excess = a.warned.len().saturating_sub(20);
                a.warned.drain(..excess);
                out.push(msg);
            }
        }
        let changed = !out.is_empty();
        (out, changed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at: i64, five: f64, resets: i64) -> Sample {
        Sample { at, five: Some(five), week: None, five_resets: Some(resets), week_resets: None }
    }

    #[test]
    fn pace_projects_within_the_window() {
        let now = 1_000_000_000_000;
        let resets = now / 1000 + 3 * 3600;
        // 10% more in 30 minutes: 20%/h, 50% left -> 2.5h, before the 3h reset.
        let hist = vec![sample(now - 30 * 60_000, 40.0, resets), sample(now, 50.0, resets)];
        match pace(&hist, now, Window { used: 50.0, resets_at: resets }, true) {
            Some(Pace::LimitIn(ms)) => assert!((ms - 150 * 60_000).abs() < 60_000, "{ms}"),
            p => panic!("{p:?}"),
        }
        // Slow burn: ends the window under the limit.
        let hist = vec![sample(now - 30 * 60_000, 49.0, resets), sample(now, 50.0, resets)];
        assert!(matches!(pace(&hist, now, Window { used: 50.0, resets_at: resets }, true), Some(Pace::EndsAt(p)) if p < 60.0));
        // Samples from a previous window don't count.
        let hist = vec![sample(now - 30 * 60_000, 10.0, resets - 18000), sample(now, 50.0, resets)];
        assert_eq!(pace(&hist, now, Window { used: 50.0, resets_at: resets }, true), None);
        // Already at the limit: no pace, however fast it got there.
        let hist = vec![sample(now - 30 * 60_000, 60.0, resets), sample(now, 100.0, resets)];
        assert_eq!(pace(&hist, now, Window { used: 100.0, resets_at: resets }, true), None);
    }

    #[test]
    fn endpoint_and_statusline_shapes_parse() {
        let api: Value = serde_json::from_str(
            r#"{"five_hour":{"utilization":19.0,"resets_at":"2026-09-29T09:00:00.499375+00:00"},"seven_day":null}"#,
        )
        .unwrap();
        let w = window(api.get("five_hour"), "utilization").unwrap();
        assert_eq!((w.used, w.resets_at), (19.0, 1790672400));
        assert!(window(api.get("seven_day"), "utilization").is_none());
        let sl: Value = serde_json::from_str(r#"{"seven_day":{"used_percentage":42.5,"resets_at":1790672400}}"#).unwrap();
        assert_eq!(window(sl.get("seven_day"), "used_percentage").unwrap().used, 42.5);
    }

    #[test]
    fn a_rolled_over_window_reads_empty() {
        let m = meter(Window { used: 80.0, resets_at: 100 }, 200_000, None);
        assert_eq!((m.used, m.limited), (0.0, false));
    }
}
