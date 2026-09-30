//! Where the tokens go: a report read straight from every transcript on the
//! machine, so each change that is meant to save tokens can be measured
//! before and after rather than estimated.
//!
//! Costs are in dollars at the API's list prices, each call priced by its
//! own model (see `price`): on Opus 5.5 a cache read is $0.20 per million, a
//! cache write $5 (5-minute cache) or $8 (1-hour cache), input $4, output $20.
//!
//! Cache writes are split by why they happened. A turn's own new tokens are
//! growth and can't be avoided; everything else is the conversation being
//! written to the cache again: at a session's start, after the cache expired
//! while a tool call ran (mid-turn) or while you were away (between turns),
//! or early, inside the cache's lifetime (a model switch, a changed prompt).
//!
//! Bash output is measured as it sits in context: every call re-reads each
//! earlier result, so a result's weight is its size times the calls it stays
//! for.

use crate::config::Config;
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::price::{self, Price};
/// Measured on Bash output (M8): characters per token.
const BASH_CHARS_PER_TOKEN: f64 = 2.2;
/// A write this much larger than the call's new content counts as the
/// conversation being written again.
const REWRITE_SLACK: u64 = 4_000;

/// Bash results by size as the agent saw them, and how toomux treated them.
pub const BANDS: [&str; 5] = ["under 500", "500 to 2k", "2k to 6k, whole", "over 6k, trimmed", "over 6k, files"];

#[derive(Default, Serialize, Clone)]
pub struct Side {
    pub calls: u64,
    pub read: u64,
    pub write_5m: u64,
    pub write_1h: u64,
    pub input: u64,
    pub output: u64,
    /// Sum of each call's whole context.
    pub context: u64,
    /// Calls whose context passed the handover limit.
    pub over_limit: u64,
    /// Dollars, by kind.
    pub usd_read: f64,
    pub usd_write: f64,
    pub usd_input: f64,
    pub usd_output: f64,
    /// Each conversation's first call.
    #[serde(skip)]
    pub baselines: Vec<u64>,
    /// Calls by model.
    #[serde(skip)]
    pub model_calls: std::collections::BTreeMap<String, u64>,
}

impl Side {
    pub fn cost(&self) -> f64 {
        self.usd_read + self.usd_write + self.usd_input + self.usd_output
    }
    /// One call, priced for its model.
    fn charge(&mut self, p: Price, read: u64, w5: u64, w1: u64, input: u64, output: u64) {
        self.calls += 1;
        self.read += read;
        self.write_5m += w5;
        self.write_1h += w1;
        self.input += input;
        self.output += output;
        self.context += read + w5 + w1 + input;
        self.usd_read += read as f64 * p.read;
        self.usd_write += w5 as f64 * p.write_5m() + w1 as f64 * p.write_1h();
        self.usd_input += input as f64 * p.input;
        self.usd_output += output as f64 * p.output;
    }
    fn more_output(&mut self, p: Price, output: u64) {
        self.output += output;
        self.usd_output += output as f64 * p.output;
    }
    fn add(&mut self, o: &Side) {
        self.calls += o.calls;
        self.read += o.read;
        self.write_5m += o.write_5m;
        self.write_1h += o.write_1h;
        self.input += o.input;
        self.output += o.output;
        self.context += o.context;
        self.over_limit += o.over_limit;
        self.usd_read += o.usd_read;
        self.usd_write += o.usd_write;
        self.usd_input += o.usd_input;
        self.usd_output += o.usd_output;
        self.baselines.extend_from_slice(&o.baselines);
        for (k, n) in &o.model_calls {
            *self.model_calls.entry(k.clone()).or_default() += n;
        }
    }
}

/// Cache writes of one kind: how many, how many tokens, what they cost.
#[derive(Default, Serialize, Clone, Copy)]
pub struct Cause {
    pub n: u64,
    pub tokens: u64,
    pub usd: f64,
}

impl Cause {
    /// `rate`: dollars per token written.
    fn put(&mut self, tokens: u64, rate: f64) {
        if tokens > 0 {
            self.n += 1;
            self.tokens += tokens;
            self.usd += tokens as f64 * rate;
        }
    }
    fn add(&mut self, o: &Cause) {
        self.n += o.n;
        self.tokens += o.tokens;
        self.usd += o.usd;
    }
}

#[derive(Default, Serialize, Clone)]
pub struct Writes {
    pub growth: Cause,
    pub start: Cause,
    pub expired_mid_turn: Cause,
    pub expired_between_turns: Cause,
    pub early: Cause,
}

impl Writes {
    fn add(&mut self, o: &Writes) {
        self.growth.add(&o.growth);
        self.start.add(&o.start);
        self.expired_mid_turn.add(&o.expired_mid_turn);
        self.expired_between_turns.add(&o.expired_between_turns);
        self.early.add(&o.early);
    }
}

#[derive(Default, Serialize, Clone)]
pub struct Bash {
    /// Characters of results, per band.
    pub added: [u64; 5],
    /// Characters times the calls they stayed in context for, per band.
    pub resident: [u64; 5],
    pub results: [u64; 5],
    /// `toomux out` or the output tool used to read a kept output back.
    pub read_back: u64,
}

impl Bash {
    fn add(&mut self, o: &Bash) {
        for i in 0..5 {
            self.added[i] += o.added[i];
            self.resident[i] += o.resident[i];
            self.results[i] += o.results[i];
        }
        self.read_back += o.read_back;
    }
}

/// What toomux saved, by estimate, in dollars at list prices.
#[derive(Default, Serialize, Clone, Copy)]
pub struct Saved {
    /// Context a continued conversation would have carried without handover.
    pub handover_usd: f64,
    /// Conversations that continued from a handover.
    pub handovers: u64,
    /// The hidden part of trimmed output, as later calls would have re-read it.
    pub trim_usd: f64,
    /// Where Claude Code compacts on its own (tokens before, after): learned
    /// from this machine's transcripts.
    pub compacts_at: u64,
    pub compacts_to: u64,
}

impl Saved {
    pub fn usd(&self) -> f64 {
        self.handover_usd + self.trim_usd
    }
}

/// One conversation, for the handover estimate.
#[derive(Default, Clone)]
struct Link {
    path: PathBuf,
    /// The session it continued from, and that one's context then.
    from: Option<(String, u64)>,
    /// Its first call's context.
    baseline: u64,
    /// Counted calls: context, cache-read price per token.
    calls: Vec<(u64, f64)>,
}

#[derive(Default, Serialize)]
pub struct Report {
    pub since_ms: i64,
    /// The end of the span; `i64::MAX` for "until now".
    pub until_ms: i64,
    pub conversations: u64,
    pub main: Side,
    pub subagents: Side,
    /// Subagents by type (general-purpose, fork, a custom agent's name).
    pub kinds: std::collections::BTreeMap<String, Side>,
    /// Every call by model ("opus 5.5").
    pub models: std::collections::BTreeMap<String, Side>,
    pub writes: Writes,
    pub bash: Bash,
    /// The handover limit calls are measured against.
    pub limit: u64,
    pub saved: Saved,
    #[serde(skip)]
    links: Vec<Link>,
    /// Claude Code's own compactions: tokens before, after.
    #[serde(skip)]
    compactions: Vec<(u64, u64)>,
}

impl Report {
    fn add(&mut self, o: &Report) {
        self.conversations += o.conversations;
        self.main.add(&o.main);
        self.subagents.add(&o.subagents);
        for (k, v) in &o.kinds {
            self.kinds.entry(k.clone()).or_default().add(v);
        }
        for (k, v) in &o.models {
            self.models.entry(k.clone()).or_default().add(v);
        }
        self.writes.add(&o.writes);
        self.bash.add(&o.bash);
        self.saved.trim_usd += o.saved.trim_usd;
        self.links.extend(o.links.iter().cloned());
        self.compactions.extend_from_slice(&o.compactions);
    }
    pub fn all(&self) -> Side {
        let mut s = self.main.clone();
        s.add(&self.subagents);
        s
    }
}

/// `24h`, `7d`, `90m`, or a local date and time (`2026-09-29 21:43`,
/// `2026-09-29`), to milliseconds since the epoch.
pub fn parse_since(s: &str, now_ms: i64) -> Result<i64> {
    let s = s.trim();
    if let Some((n, unit)) = s.char_indices().last().map(|(i, c)| (&s[..i], c))
        && let Ok(n) = n.parse::<f64>()
    {
        let secs = match unit {
            'm' => 60.0,
            'h' => 3600.0,
            'd' => 86_400.0,
            'w' => 604_800.0,
            _ => bail!("say how long ago as 90m, 24h, 7d or 2w"),
        };
        return Ok(now_ms - (n * secs * 1000.0) as i64);
    }
    use chrono::{NaiveDate, NaiveDateTime, TimeZone};
    let naive = NaiveDateTime::parse_from_str(&s.replace('T', " "), "%Y-%m-%d %H:%M")
        .or_else(|_| NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).unwrap_or_default()));
    match naive.ok().and_then(|n| chrono::Local.from_local_datetime(&n).earliest()) {
        Some(t) => Ok(t.timestamp_millis()),
        None => bail!("'{s}' isn't a time toomux knows: try 24h, 7d or 2026-09-29 21:43"),
    }
}

struct Transcript {
    path: PathBuf,
    subagent: bool,
}

/// Every transcript touched since `since_ms`, the conversations' and their
/// subagents', optionally only one conversation's (a session-id prefix).
fn transcripts(cfg: &Config, since_ms: i64, session: Option<&str>) -> Vec<Transcript> {
    let fresh = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| d.as_millis() as i64 >= since_ms)
    };
    let wanted = |id: &str| session.is_none_or(|s| id.starts_with(s));
    let mut out = Vec::new();
    for root in crate::index::roots(cfg) {
        for project in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(project.path()).into_iter().flatten().flatten() {
                let path = f.path();
                let name = f.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    if !wanted(&name) {
                        continue;
                    }
                    for a in std::fs::read_dir(path.join("subagents")).into_iter().flatten().flatten() {
                        let ap = a.path();
                        if ap.extension().is_some_and(|x| x == "jsonl") && fresh(&ap) {
                            out.push(Transcript { path: ap, subagent: true });
                        }
                    }
                } else if name.ends_with(".jsonl") && wanted(&name) && fresh(&path) {
                    out.push(Transcript { path, subagent: false });
                }
            }
        }
    }
    out
}

/// The report for everything since `since_ms`.
pub fn report(cfg: &Config, since_ms: i64, session: Option<&str>) -> Report {
    report_between(cfg, since_ms, i64::MAX, session)
}

/// The report for calls from `since_ms` up to (not including) `until_ms`.
pub fn report_between(cfg: &Config, since_ms: i64, until_ms: i64, session: Option<&str>) -> Report {
    let files = transcripts(cfg, since_ms, session);
    let limit = if cfg.handover_tokens > 0 { cfg.handover_tokens } else { 400_000 };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let parts: Vec<Report> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut r = Report::default();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(t) = files.get(i) else { break };
                        if let Ok(text) = std::fs::read_to_string(&t.path) {
                            let mut part = read(&text, t.subagent, since_ms, until_ms, limit);
                            if t.subagent {
                                part.kinds.insert(agent_type(&t.path), part.subagents.clone());
                            }
                            for l in &mut part.links {
                                l.path = t.path.clone();
                            }
                            r.add(&part);
                        }
                    }
                    r
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut r = Report { since_ms, until_ms, limit, ..Report::default() };
    for p in &parts {
        r.add(p);
    }
    handover_saving(&mut r, &digest_dir().with_file_name("compaction.json"));
    r
}

/// A subagent's type, from the `.meta.json` beside its transcript.
fn agent_type(path: &Path) -> String {
    std::fs::read_to_string(path.with_extension("meta.json"))
        .ok()
        .and_then(|r| serde_json::from_str::<Value>(&r).ok())
        .and_then(|v| v.get("agentType").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

fn millis(v: &Value) -> Option<i64> {
    let ts = v.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(ts).ok().map(|t| t.timestamp_millis())
}

fn result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect(),
        _ => String::new(),
    }
}

fn band(text: &str) -> usize {
    let n = text.chars().count();
    if text.contains("[toomux · ") && text.contains("All of it is kept") {
        3
    } else if n < 500 {
        0
    } else if n < 2000 {
        1
    } else if n < crate::capture::FULL {
        2
    } else {
        4
    }
}

/// One transcript's share of the report. Everything before `since_ms` is
/// read too (earlier results are still in context), but only calls from
/// then on are counted.
fn read(text: &str, subagent: bool, since_ms: i64, until_ms: i64, limit: u64) -> Report {
    let inside = |t: i64| t >= since_ms && t < until_ms;
    let mut r = Report::default();
    let mut side = Side::default();
    // Each message's output so far, and its price.
    let mut seen: HashMap<String, (u64, Price, String)> = HashMap::new();
    let mut bash_ids: HashSet<String> = HashSet::new();
    // Each earlier Bash result: band, characters, characters toomux hid.
    let mut resident: Vec<(usize, u64, u64)> = Vec::new();
    let mut link = Link::default();
    // The previous call: when, its whole context.
    let mut prev: Option<(i64, u64)> = None;
    let mut prompted = false;
    let mut counted_any = false;
    for line in text.lines() {
        // Cheap filter: only these lines matter.
        if !(line.contains("\"usage\"") || line.contains("tool_result") || line.contains("\"user\"") || line.contains("compact_boundary")) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let msg = v.get("message");
        match kind {
            "system" if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => {
                resident.clear();
                let m = v.get("compactMetadata");
                let n = |k: &str| m.and_then(|m| m.get(k)).and_then(Value::as_u64).unwrap_or(0);
                if m.and_then(|m| m.get("trigger")).and_then(Value::as_str) == Some("auto") && n("preTokens") > n("postTokens") {
                    r.compactions.push((n("preTokens"), n("postTokens")));
                }
            }
            "user" => {
                let content = msg.and_then(|m| m.get("content"));
                match content {
                    Some(Value::String(s)) if !s.is_empty() && v.get("isMeta").is_none() => {
                        prompted = true;
                        if prev.is_none() && link.from.is_none() {
                            link.from = continued_from(s);
                        }
                    }
                    Some(Value::Array(blocks)) => {
                        let mut results = false;
                        for b in blocks {
                            if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                                continue;
                            }
                            results = true;
                            let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                            if bash_ids.contains(id) {
                                let t = result_text(b);
                                let (k, n) = (band(&t), t.chars().count() as u64);
                                resident.push((k, n, if k == 3 { trimmed_total(&t).saturating_sub(n) } else { 0 }));
                                if millis(&v).is_some_and(inside) {
                                    r.bash.added[k] += n;
                                    r.bash.results[k] += 1;
                                }
                            }
                        }
                        if !results && v.get("isMeta").is_none() && blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("text")) {
                            prompted = true;
                            if prev.is_none() && link.from.is_none() {
                                link.from = blocks.iter().find_map(|b| b.get("text").and_then(Value::as_str).and_then(continued_from));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                let Some(m) = msg else { continue };
                let t = millis(&v).unwrap_or(0);
                for b in m.get("content").and_then(Value::as_array).into_iter().flatten() {
                    if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                    let id = b.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let command = b.pointer("/input/command").and_then(Value::as_str).unwrap_or("");
                    if inside(t) && (name == "mcp__toomux__output" || (name == "Bash" && command.contains("toomux out "))) {
                        r.bash.read_back += 1;
                    }
                    if name == "Bash" {
                        bash_ids.insert(id);
                    }
                }
                let Some(u) = m.get("usage") else { continue };
                let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                let id = m.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                // Each content block repeats the message's usage; output can
                // grow along the way.
                if let Some((out, p, model)) = seen.get_mut(&id) {
                    if inside(t) && n("output_tokens") > *out {
                        let more = n("output_tokens") - *out;
                        side.more_output(*p, more);
                        r.models.entry(model.clone()).or_default().more_output(*p, more);
                        *out = n("output_tokens");
                    }
                    continue;
                }
                let (input, write, cached) = (n("input_tokens"), n("cache_creation_input_tokens"), n("cache_read_input_tokens"));
                let ctx = input + write + cached;
                if ctx == 0 {
                    continue;
                }
                let model = m.get("model").and_then(Value::as_str).unwrap_or("");
                let p = price::of(model, u.get("speed").and_then(Value::as_str) == Some("fast"));
                seen.insert(id, (n("output_tokens"), p, price::name(model)));
                let cc = u.get("cache_creation");
                let w5 = cc.and_then(|c| c.get("ephemeral_5m_input_tokens")).and_then(Value::as_u64);
                let w1 = cc.and_then(|c| c.get("ephemeral_1h_input_tokens")).and_then(Value::as_u64);
                // Transcripts without the split: subagents' caches last 5
                // minutes, conversations' an hour.
                let (w5, w1) = match (w5, w1) {
                    (Some(a), Some(b)) if a + b > 0 => (a, b),
                    _ if subagent => (write, 0),
                    _ => (0, write),
                };
                if inside(t) {
                    if !counted_any {
                        counted_any = true;
                        r.conversations += u64::from(!subagent);
                    }
                    side.charge(p, cached, w5, w1, input, n("output_tokens"));
                    *side.model_calls.entry(price::name(model)).or_default() += 1;
                    r.models.entry(price::name(model)).or_default().charge(p, cached, w5, w1, input, n("output_tokens"));
                    if ctx > limit {
                        side.over_limit += 1;
                    }
                    let weight = if write > 0 { (w5 as f64 * p.write_5m() + w1 as f64 * p.write_1h()) / write as f64 } else { 0.0 };
                    match prev {
                        None => {
                            side.baselines.push(ctx);
                            r.writes.start.put(write, weight);
                        }
                        Some((pt, pctx)) => {
                            let fresh = ctx.saturating_sub(pctx).min(write);
                            let again = write - fresh;
                            r.writes.growth.put(fresh, weight);
                            if again > REWRITE_SLACK {
                                let ttl = if w5 > w1 { 300_000 } else { 3_600_000 };
                                let cause = match (t - pt > ttl, prompted) {
                                    (true, false) => &mut r.writes.expired_mid_turn,
                                    (true, true) => &mut r.writes.expired_between_turns,
                                    (false, _) => &mut r.writes.early,
                                };
                                cause.put(again, weight);
                            } else {
                                r.writes.growth.put(again, weight);
                            }
                        }
                    }
                    for &(k, chars, hidden) in &resident {
                        r.bash.resident[k] += chars;
                        r.saved.trim_usd += hidden as f64 / BASH_CHARS_PER_TOKEN * p.read;
                    }
                    if !subagent {
                        link.calls.push((ctx, p.read));
                    }
                }
                if prev.is_none() {
                    link.baseline = ctx;
                }
                prev = Some((t, ctx));
                prompted = false;
            }
            _ => {}
        }
    }
    if subagent {
        r.subagents = side;
    } else {
        r.main = side;
        r.links.push(link);
    }
    r
}

/// A handover prompt's "previous session here (id X) reached Nk tokens".
fn continued_from(text: &str) -> Option<(String, u64)> {
    let rest = text.split("The previous session here (id ").nth(1)?;
    let (id, rest) = rest.split_once(") reached ")?;
    let k: u64 = rest.split('k').next()?.parse().ok()?;
    Some((id.to_string(), k * 1000))
}

/// A trimmed result's whole size in characters, from "…, 80k chars in all."
fn trimmed_total(text: &str) -> u64 {
    let Some(before) = text.split(" chars in all").next().filter(|b| b.len() < text.len()) else { return 0 };
    let n = before.rsplit([' ', ',']).next().unwrap_or("");
    match n.strip_suffix('k') {
        Some(k) => k.parse::<u64>().unwrap_or(0) * 1000,
        None => n.parse().unwrap_or(0),
    }
}

/// A conversation's first lines only: whom it continued from, its first
/// context. For sessions outside the report's span.
fn head_link(path: &Path) -> Option<Link> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    let mut link = Link { path: path.to_path_buf(), ..Link::default() };
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok).take(2000) {
        if line.contains("The previous session here (id ") && link.from.is_none() {
            link.from = continued_from(&line.replace("\\n", " "));
        }
        if line.contains("\"usage\"") {
            let v: Value = serde_json::from_str(&line).ok()?;
            let u = v.pointer("/message/usage")?;
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            link.baseline = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
            return Some(link);
        }
    }
    None
}

/// Where Claude Code compacts on its own, from the compactions seen, else
/// the last ones kept; with none ever seen, the largest context in the
/// report, so the estimate errs low.
fn compaction_point(r: &Report, kept: &Path) -> (u64, u64) {
    if !r.compactions.is_empty() {
        let pre: Vec<u64> = r.compactions.iter().map(|c| c.0).collect();
        let post: Vec<u64> = r.compactions.iter().map(|c| c.1).collect();
        let point = (median(&pre), median(&post));
        let _ = std::fs::write(kept, format!("[{}, {}]", point.0, point.1));
        return point;
    }
    if let Some((a, b)) = std::fs::read_to_string(kept).ok().and_then(|t| serde_json::from_str::<(u64, u64)>(&t).ok()) {
        return (a, b);
    }
    let most = r.links.iter().flat_map(|l| l.calls.iter().map(|c| c.0)).max().unwrap_or(0);
    let least = r.links.iter().map(|l| l.baseline).filter(|b| *b > 0).min().unwrap_or(0);
    (most, least)
}

/// Handover's saving: each continued conversation re-priced as if it had
/// carried the one before it (and that one's, and so on), compacting where
/// Claude Code does.
fn handover_saving(r: &mut Report, kept: &Path) {
    let (cap, floor) = compaction_point(r, kept);
    r.saved.compacts_at = cap;
    r.saved.compacts_to = floor;
    let id = |p: &Path| p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut known: HashMap<String, Link> = r.links.iter().map(|l| (id(&l.path), l.clone())).collect();
    // Context carried in from before: the chain's growth, unrolled.
    fn carry(l: &Link, known: &mut HashMap<String, Link>, depth: usize) -> u64 {
        let Some((from, k)) = &l.from else { return 0 };
        if depth > 200 {
            return 0;
        }
        let before = match known.get(from) {
            Some(p) => Some(p.clone()),
            None => head_link(&l.path.with_file_name(format!("{from}.jsonl"))),
        };
        let earlier = before.map(|p| {
            known.insert(from.clone(), p.clone());
            carry(&p, known, depth + 1)
        });
        (k + earlier.unwrap_or(0)).saturating_sub(l.baseline)
    }
    let wrap = |x: u64| if cap > floor + 10_000 && x > cap { floor + (x - floor) % (cap - floor) } else { x };
    let links = std::mem::take(&mut r.links);
    for l in &links {
        if l.from.is_none() || l.calls.is_empty() {
            continue;
        }
        let extra = carry(l, &mut known, 0);
        r.saved.handovers += 1;
        r.saved.handover_usd += l.calls.iter().map(|&(ctx, rate)| (wrap(ctx + extra) as f64 - ctx as f64) * rate).sum::<f64>();
    }
    r.links = links;
}

// ---- the daily digest ------------------------------------------------------------

fn digest_dir() -> PathBuf {
    crate::memory::path().with_file_name("digest")
}

/// A local day's first millisecond and the next day's.
fn day_bounds(day: chrono::NaiveDate) -> Option<(i64, i64)> {
    use chrono::TimeZone;
    let start = |d: chrono::NaiveDate| chrono::Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest().map(|t| t.timestamp_millis());
    Some((start(day)?, start(day.succ_opt()?)?))
}

/// The day a digest is due for: yesterday, once it's past eight in the
/// morning, if it hasn't been made (or tried) yet. Marks it tried.
pub fn digest_due(now_ms: i64) -> Option<chrono::NaiveDate> {
    use chrono::{TimeZone, Timelike};
    let now = chrono::Local.timestamp_millis_opt(now_ms).single()?;
    if now.hour() < 8 {
        return None;
    }
    let day = now.date_naive().pred_opt()?;
    let dir = digest_dir();
    let tried = dir.join(format!(".{day}.tried"));
    if dir.join(format!("{day}.txt")).exists() || tried.exists() {
        return None;
    }
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(&tried, b"").ok()?;
    Some(day)
}

/// What a kept day cost, from its digest.
fn kept_cost(day: chrono::NaiveDate) -> Option<f64> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(digest_dir().join(format!("{day}.json"))).ok()?).ok()?;
    let sum = |side: &str| ["usd_read", "usd_write", "usd_input", "usd_output"].iter().filter_map(|k| v.pointer(&format!("/{side}/{k}"))?.as_f64()).sum::<f64>();
    Some(sum("main") + sum("subagents"))
}

/// One day's report, kept beside the others, and a line saying how the day
/// went (None for a day nothing ran).
pub fn digest(cfg: &Config, day: chrono::NaiveDate) -> Result<(String, Option<String>)> {
    let Some((a, b)) = day_bounds(day) else { bail!("{day} has no midnight here") };
    let r = report_between(cfg, a, b, None);
    let text = render(&r, b, None);
    let dir = digest_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("{day}.txt")), &text)?;
    std::fs::write(dir.join(format!("{day}.json")), serde_json::to_string(&r)?)?;
    let all = r.all();
    if all.calls == 0 {
        return Ok((text, None));
    }
    let cost = all.cost();
    let before = day.pred_opt().and_then(|d| {
        kept_cost(d).or_else(|| {
            let (a, b) = day_bounds(d)?;
            Some(report_between(cfg, a, b, None).all().cost())
        })
    });
    let change = match before {
        Some(p) if p >= 1.0 => {
            let d = 100.0 * (cost - p) / p;
            if d.abs() < 1.0 { ", the same as the day before".to_string() } else { format!(", {:.0}% {} than the day before", d.abs(), if d < 0.0 { "less" } else { "more" }) }
        }
        _ => String::new(),
    };
    let w = &r.writes;
    let rewrites = w.expired_mid_turn.usd + w.expired_between_turns.usd + w.early.usd;
    let when = if chrono::Local::now().date_naive().pred_opt() == Some(day) { "yesterday".to_string() } else { day.format("%a %-d %b").to_string().to_lowercase() };
    let line = format!(
        "{when} cost {} at API prices{change} · toomux saved about {} · subagents {} · cache written again {} · toomux digest for the rest",
        money(cost),
        money(r.saved.usd()),
        pct(r.subagents.cost(), cost),
        money(rewrites),
    );
    Ok((text, Some(line)))
}

/// A kept digest, if there is one.
pub fn kept(day: chrono::NaiveDate) -> Option<String> {
    std::fs::read_to_string(digest_dir().join(format!("{day}.txt"))).ok()
}

// ---- the report, as text ---------------------------------------------------------

fn short(n: f64) -> String {
    if n >= 1e9 {
        format!("{:.1}B", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.0}k", n / 1e3)
    } else {
        format!("{n:.0}")
    }
}

/// "24h", "90m", "7d".
fn ago_span(ms: i64) -> String {
    let m = ms / 60_000;
    if m % 1440 == 0 && m >= 2880 { format!("{}d", m / 1440) } else if m % 60 == 0 { format!("{}h", m / 60) } else { format!("{m}m") }
}

/// "$0.42", "$12.30", "$1,234".
fn money(v: f64) -> String {
    if v >= 100.0 { format!("${}", thousands(v.round() as u64)) } else { format!("${v:.2}") }
}

fn pct(a: f64, b: f64) -> String {
    if b <= 0.0 {
        return "-".into();
    }
    let p = 100.0 * a / b;
    if p > 0.0 && p < 1.0 { format!("{p:.1}%") } else { format!("{p:.0}%") }
}

fn median(v: &[u64]) -> u64 {
    let mut v = v.to_vec();
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "53%", "<1%": a share of a section, for reading at a glance.
fn share(a: f64, b: f64) -> String {
    if b <= 0.0 {
        return "-".into();
    }
    let p = 100.0 * a / b;
    if p > 0.0 && p < 0.5 { "<1%".into() } else { format!("{p:.0}%") }
}

/// The report's colours: the config's palette on a terminal, none in a pipe
/// or a kept digest.
struct Ink<'a>(Option<&'a crate::config::Colors>);

impl Ink<'_> {
    fn rgb((r, g, b): (u8, u8, u8)) -> String {
        format!("\x1b[38;2;{r};{g};{b}m")
    }
    fn fg(&self, pick: fn(&crate::config::Colors) -> &String) -> String {
        self.0.map(|c| Self::rgb(crate::config::hex(pick(c)))).unwrap_or_default()
    }
    /// A meter's unused rest: the same bar, faint, in colour; a rule without.
    fn track(&self, cells: usize) -> String {
        match self.0 {
            Some(c) => format!("{}{}", Self::rgb(crate::config::hex(&c.faint)), "▅".repeat(cells)),
            None => "─".repeat(cells),
        }
    }
    fn bold(&self) -> &'static str {
        if self.0.is_some() { "\x1b[1m" } else { "" }
    }
    fn reset(&self) -> &'static str {
        if self.0.is_some() { "\x1b[0m" } else { "" }
    }
}

/// How a row's meter is coloured: the usual accent, amber for money that
/// could have been saved, green for what toomux already saved.
#[derive(Clone, Copy)]
enum Tone {
    Plain,
    Waste,
    Saved,
}

struct Row {
    label: String,
    value: String,
    part: f64,
    of: f64,
    tone: Tone,
    cols: Vec<String>,
    note: String,
}

impl Row {
    fn new(label: &str, value: String, part: f64, of: f64) -> Self {
        Row { label: label.into(), value, part, of, tone: Tone::Plain, cols: Vec::new(), note: String::new() }
    }
    fn cols(mut self, cols: &[String]) -> Self {
        self.cols = cols.to_vec();
        self
    }
    fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }
    fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }
}

const LABEL: usize = 18;
const VALUE: usize = 7;
const METER: usize = 24;

/// One section: a heading with its column names, a row per part, each with a
/// meter of its share of the section, then a line or two of plain reading.
fn section(s: &mut String, ink: &Ink, heading: &str, heads: &[&str], rows: &[Row], after: &[String]) {
    use std::fmt::Write;
    let (text, dim, muted) = (ink.fg(|c| &c.text), ink.fg(|c| &c.dim), ink.fg(|c| &c.muted));
    let widths: Vec<usize> = (0..heads.len())
        .map(|i| rows.iter().filter_map(|r| r.cols.get(i)).map(|c| c.chars().count()).chain([heads[i].chars().count()]).max().unwrap_or(0))
        .collect();
    let lead = 2 + LABEL + 1 + VALUE + 2 + METER + 1 + 4;
    let mut head = format!("{text}{}{heading}{}", ink.bold(), ink.reset());
    if !heads.is_empty() {
        head.push_str(&" ".repeat(lead.saturating_sub(heading.chars().count()).max(2)));
        head.push_str(&muted);
        for (h, w) in heads.iter().zip(&widths) {
            head.push_str(&format!("   {h:>w$}"));
        }
    }
    let _ = writeln!(s, "{head}{}", ink.reset());
    for r in rows {
        let tone = match r.tone {
            Tone::Plain => ink.fg(|c| &c.accent),
            Tone::Waste => ink.fg(|c| &c.working),
            Tone::Saved => ink.fg(|c| &c.finished),
        };
        let f = if r.of > 0.0 { (r.part / r.of).clamp(0.0, 1.0) } else { 0.0 };
        // Whole cells, short of the full height so rows stay apart.
        let cells = (f * METER as f64).round() as usize;
        let meter = format!("{tone}{}{}", "▅".repeat(cells), ink.track(METER - cells));
        let mut line = format!("  {text}{:<LABEL$} {:>VALUE$}  {meter} {dim}{:>4}", r.label, r.value, share(r.part, r.of));
        for (c, w) in r.cols.iter().zip(&widths) {
            line.push_str(&format!("   {c:>w$}"));
        }
        if !r.note.is_empty() {
            line.push_str(&format!("   {muted}{}", r.note));
        }
        let _ = writeln!(s, "{line}{}", ink.reset());
    }
    for a in after {
        let _ = writeln!(s, "  {muted}{a}{}", ink.reset());
    }
    s.push('\n');
}

/// The calls' main model: "opus 5.5", or "opus 5.5 +1" when others carry
/// more than a tenth of them.
fn main_model(side: &Side) -> String {
    let Some((name, n)) = side.model_calls.iter().max_by_key(|(_, n)| **n) else { return String::new() };
    let others = side.model_calls.len() - 1;
    if others > 0 && (*n as f64) < 0.9 * side.calls as f64 { format!("{name} +{others}") } else { name.clone() }
}

/// The report, for reading. `colors`: the palette on a terminal, `None` for
/// plain text.
pub fn render(r: &Report, now_ms: i64, colors: Option<&crate::config::Colors>) -> String {
    use std::fmt::Write;
    let ink = Ink(colors);
    let (accent, text, dim, muted) = (ink.fg(|c| &c.accent), ink.fg(|c| &c.text), ink.fg(|c| &c.dim), ink.fg(|c| &c.muted));
    let all = r.all();
    let total = all.cost();
    let since = chrono::DateTime::from_timestamp_millis(r.since_ms)
        .map(|t| t.with_timezone(&chrono::Local).format("%a %-d %b %H:%M").to_string().to_lowercase())
        .unwrap_or_default();
    let end = now_ms.min(r.until_ms);
    let days = (end - r.since_ms) as f64 / 86_400_000.0;
    let span = if r.until_ms < i64::MAX { format!("from {since} for {}", ago_span(r.until_ms - r.since_ms)) } else { format!("since {since}") };
    let span = if r.until_ms == i64::MAX && days >= 1.0 { format!("{span} · {:.0} {}", days, if days.round() == 1.0 { "day" } else { "days" }) } else { span };
    let mut s = String::new();
    let _ = writeln!(s, "{muted}toomux tokens {dim}{span}{}\n", ink.reset());
    if all.calls == 0 {
        let _ = writeln!(s, "  no calls");
        return s;
    }
    // Say what the number is: actual use, priced, not a saving or an estimate.
    let money_w = money(total).chars().count();
    let saved = r.saved.usd();
    let wide = money_w.max(money(saved).chars().count());
    let _ = writeln!(s, "  {accent}{}{:>wide$}{}   {text}what Claude Code actually used, at API list prices{}", ink.bold(), money(total), ink.reset(), ink.reset());
    if saved >= 0.01 {
        let _ = writeln!(
            s,
            "  {}{}{:>wide$}{}   {text}saved by toomux, by estimate: it would have been {}{}",
            ink.fg(|c| &c.finished),
            ink.bold(),
            money(saved),
            ink.reset(),
            money(total + saved),
            ink.reset()
        );
    }
    let mut facts = Vec::new();
    if days >= 1.5 {
        facts.push(format!("{} a day", money(total / days)));
    }
    facts.push(format!("{} calls in {} conversations", thousands(all.calls), r.conversations));
    let _ = writeln!(s, "  {}   {dim}{}{}\n", " ".repeat(wide), facts.join(" · "), ink.reset());

    if saved >= 0.01 {
        let v = &r.saved;
        let mut rows = Vec::new();
        if v.handover_usd.abs() >= 0.01 {
            let n = if v.handovers == 1 { "1 handover".to_string() } else { format!("{} handovers", thousands(v.handovers)) };
            rows.push(Row::new("handover", money(v.handover_usd), v.handover_usd.max(0.0), saved).tone(Tone::Saved).note(n));
        }
        if v.trim_usd >= 0.01 {
            let n = r.bash.results[3];
            rows.push(Row::new("trimmed output", money(v.trim_usd), v.trim_usd, saved).tone(Tone::Saved).note(format!("{} {} trimmed", thousands(n), if n == 1 { "result" } else { "results" })));
        }
        let how = if v.compacts_at > 0 {
            vec![format!("as if no handover had happened, compacting at {} like Claude Code", short(v.compacts_at as f64))]
        } else {
            Vec::new()
        };
        section(&mut s, &ink, "What toomux saved", &[], &rows, &how);
    }

    let mut kinds = vec![
        Row::new("output", money(all.usd_output), all.usd_output, total).cols(&[short(all.output as f64)]),
        Row::new("cache reads", money(all.usd_read), all.usd_read, total).cols(&[short(all.read as f64)]),
        Row::new("cache writes", money(all.usd_write), all.usd_write, total).cols(&[short((all.write_5m + all.write_1h) as f64)]),
    ];
    if all.input > 0 {
        kinds.push(Row::new("uncached input", money(all.usd_input), all.usd_input, total).cols(&[short(all.input as f64)]));
    }
    kinds.sort_by(|a, b| b.part.total_cmp(&a.part));
    section(&mut s, &ink, "What it paid for", &["tokens"], &kinds, &[]);

    let mut who = Vec::new();
    let person = |label: &str, side: &Side| {
        Row::new(label, money(side.cost()), side.cost(), total)
            .cols(&[thousands(side.calls), short(side.context as f64 / side.calls.max(1) as f64)])
            .note(main_model(side))
    };
    if r.main.calls > 0 {
        who.push(person("conversations", &r.main));
    }
    let mut subs: Vec<(&String, &Side)> = r.kinds.iter().filter(|(_, s)| s.calls > 0).collect();
    subs.sort_by(|a, b| b.1.cost().total_cmp(&a.1.cost()));
    for (k, side) in subs.iter().take(6) {
        who.push(person(k, side));
    }
    let mut after = Vec::new();
    if r.subagents.calls > 0 {
        let mut starts = Vec::new();
        if !r.main.baselines.is_empty() {
            starts.push(format!("a conversation starts at {}", short(median(&r.main.baselines) as f64)));
        }
        if !r.subagents.baselines.is_empty() {
            starts.push(format!("a subagent at {}", short(median(&r.subagents.baselines) as f64)));
        }
        let mut line = format!("subagents together {}, {}", money(r.subagents.cost()), share(r.subagents.cost(), total));
        if !starts.is_empty() {
            line.push_str(&format!(" · {}", starts.join(", ")));
        }
        after.push(line);
    }
    if r.limit > 0 {
        let over = all.over_limit;
        after.push(if over == 0 {
            format!("no call went past {}, where a session hands over mid-turn", short(r.limit as f64))
        } else {
            format!("{} of calls went past {}, where a session hands over mid-turn", pct(over as f64, all.calls as f64), short(r.limit as f64))
        });
    }
    section(&mut s, &ink, "Who spent it", &["calls", "avg context"], &who, &after);

    let w = &r.writes;
    let writes = w.growth.usd + w.start.usd + w.expired_mid_turn.usd + w.expired_between_turns.usd + w.early.usd;
    if writes > 0.0 {
        let rows: Vec<Row> = [
            ("new content", w.growth, Tone::Plain),
            ("session starts", w.start, Tone::Plain),
            ("expired mid-turn", w.expired_mid_turn, Tone::Waste),
            ("expired while away", w.expired_between_turns, Tone::Waste),
            ("rewritten early", w.early, Tone::Waste),
        ]
        .into_iter()
        .filter(|(_, c, _)| c.n > 0)
        .map(|(label, c, tone)| Row::new(label, money(c.usd), c.usd, writes).cols(&[thousands(c.n)]).tone(tone))
        .collect();
        let again = w.expired_mid_turn.usd + w.expired_between_turns.usd + w.early.usd;
        let line = if again > 0.0 { format!("{} of it was a conversation written a second time", money(again)) } else { "nothing was written twice".to_string() };
        section(&mut s, &ink, "Why the cache was written", &["times"], &rows, &[line]);
    }

    let b = &r.bash;
    let held: u64 = b.resident.iter().sum();
    if held > 0 {
        let read = all.read as f64 + all.write_5m as f64 + all.write_1h as f64 + all.input as f64;
        let rows: Vec<Row> = BANDS
            .iter()
            .enumerate()
            .filter(|(i, _)| b.results[*i] > 0 || b.resident[*i] > 0)
            .map(|(i, label)| {
                let row = Row::new(label, short(b.resident[i] as f64 / BASH_CHARS_PER_TOKEN), b.resident[i] as f64, held as f64).cols(&[thousands(b.results[i])]);
                if i == 3 { row.tone(Tone::Saved).note(if b.read_back > 0 { format!("{} read back in full", b.read_back) } else { String::new() }) } else { row }
            })
            .collect();
        let heading = format!("Old command output, {} of all context", share(held as f64 / BASH_CHARS_PER_TOKEN, read));
        section(&mut s, &ink, &heading, &["results"], &rows, &["tokens re-read by later calls, by the output's size in characters".to_string()]);
    }
    let _ = writeln!(s, "{muted}opus 5.5 per million tokens: cache read $0.20, cache write $5 (5 min) or $8 (1 hour),");
    let _ = writeln!(s, "input $4, output $20. Every call is priced by its own model.{}", ink.reset());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, ts: &str, input: u64, write: u64, read: u64, five: bool, tools: &str) -> String {
        let (w5, w1) = if five { (write, 0) } else { (0, write) };
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","content":[{tools}],"usage":{{"input_tokens":{input},"cache_creation_input_tokens":{write},"cache_read_input_tokens":{read},"output_tokens":100,"cache_creation":{{"ephemeral_5m_input_tokens":{w5},"ephemeral_1h_input_tokens":{w1}}}}}}}}}"#
        )
    }

    fn bash_use(id: &str) -> String {
        format!(r#"{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"cargo test"}}}}"#)
    }

    fn result(id: &str, ts: &str, text: &str) -> String {
        format!(r#"{{"type":"user","timestamp":"{ts}","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","content":{}}}]}}}}"#, serde_json::to_string(text).unwrap())
    }

    #[test]
    fn writes_are_split_by_why() {
        let lines = [
            r#"{"type":"user","timestamp":"2026-09-30T10:00:00Z","message":{"content":"go"}}"#.to_string(),
            call("a", "2026-09-30T10:00:01Z", 0, 50_000, 0, true, &bash_use("t1")),
            result("t1", "2026-09-30T10:00:30Z", &"x".repeat(3000)),
            // Growth: 3k new tokens on top of the cached 50k.
            call("b", "2026-09-30T10:00:31Z", 0, 3_000, 50_000, true, &bash_use("t2")),
            // Its block repeated with more output: counted once.
            call("b", "2026-09-30T10:00:31Z", 0, 3_000, 50_000, true, ""),
            result("t2", "2026-09-30T10:10:00Z", "ok"),
            // Ten minutes in a tool call: the 5-minute cache expired mid-turn.
            call("c", "2026-09-30T10:10:01Z", 0, 54_000, 0, true, ""),
            r#"{"type":"user","timestamp":"2026-09-30T10:11:00Z","message":{"content":"and now?"}}"#.to_string(),
            call("d", "2026-09-30T10:20:00Z", 0, 55_000, 0, true, ""),
        ];
        let r = read(&lines.join("\n"), true, 0, i64::MAX, 400_000);
        let s = &r.subagents;
        assert_eq!((s.calls, s.write_5m, s.read), (4, 50_000 + 3_000 + 54_000 + 55_000, 50_000));
        assert_eq!(s.baselines, vec![50_000]);
        assert_eq!(r.writes.start.tokens, 50_000);
        assert_eq!(r.writes.expired_mid_turn.tokens, 53_000, "all but the call's 1k of new content");
        assert_eq!(r.writes.expired_between_turns.tokens, 54_000);
        assert_eq!(r.writes.growth.tokens, 3_000 + 1_000 + 1_000);
        // No model named: priced as opus 5.5, $5 per million written for 5 minutes.
        assert!((r.writes.start.usd - 50_000.0 * 5.0 / 1e6).abs() < 1e-9);
        // The 3k result sat in context for calls c and d; "ok" for d... and c.
        assert_eq!(r.bash.results[2], 1);
        assert_eq!(r.bash.resident[2], 3000 * 3, "held by calls b, c and d");
        assert_eq!(r.bash.resident[0], 2 * 2);
    }

    #[test]
    fn only_calls_since_count_but_earlier_output_is_still_held() {
        let lines = [
            call("a", "2026-09-30T09:00:00Z", 0, 50_000, 0, false, &bash_use("t1")),
            result("t1", "2026-09-30T09:00:10Z", "[toomux · 900 lines, 80k in all. All of it is kept in /x]"),
            call("b", "2026-09-30T11:00:00Z", 0, 1_000, 50_000, false, ""),
        ];
        let since = chrono::DateTime::parse_from_rfc3339("2026-09-30T10:00:00Z").unwrap().timestamp_millis();
        let r = read(&lines.join("\n"), false, since, i64::MAX, 400_000);
        assert_eq!(r.main.calls, 1);
        assert!(r.main.baselines.is_empty(), "the conversation started before");
        assert_eq!(r.bash.results[3], 0, "the result came before");
        assert!(r.bash.resident[3] > 0, "but was still read by call b");
        assert_eq!(r.writes.growth.tokens, 1_000);
    }

    #[test]
    fn a_day_runs_midnight_to_midnight() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let (a, b) = day_bounds(day).unwrap();
        assert_eq!(ago_span(b - a), "24h");
        let lines = [
            call("a", "2026-09-28T23:59:59Z", 0, 1_000, 0, true, ""),
            call("b", "2026-09-29T02:00:00Z", 0, 1_000, 1_000, true, ""),
            call("c", "2026-09-30T00:00:00Z", 0, 1_000, 2_000, true, ""),
        ];
        let from = chrono::DateTime::parse_from_rfc3339("2026-09-29T00:00:00Z").unwrap().timestamp_millis();
        let r = read(&lines.join("\n"), true, from, from + 86_400_000, 400_000);
        assert_eq!((r.subagents.calls, r.subagents.read), (1, 1_000), "only call b is inside the day");
    }

    #[test]
    fn since_takes_spans_and_dates() {
        let now = 10 * 86_400_000;
        assert_eq!(parse_since("24h", now).unwrap(), 9 * 86_400_000);
        assert_eq!(parse_since("2d", now).unwrap(), 8 * 86_400_000);
        assert!(parse_since("2026-09-29 21:43", now).is_ok());
        assert!(parse_since("2026-09-29", now).is_ok());
        assert!(parse_since("soon", now).is_err());
    }

    #[test]
    fn the_report_reads_as_bars_with_colour_only_on_a_terminal() {
        let mut r = Report { since_ms: 0, until_ms: i64::MAX, limit: 400_000, conversations: 1, ..Report::default() };
        r.main.charge(price::of("claude-opus-5-5", false), 90_000, 0, 10_000, 0, 2_000);
        r.main.model_calls.insert("opus 5.5".into(), 1);
        r.writes.growth.put(10_000, 8.0 / 1e6);
        let plain = render(&r, 86_400_000 * 2, None);
        assert!(!plain.contains('\u{1b}'), "{plain}");
        let row = plain.lines().find(|l| l.trim_start().starts_with("conversations")).unwrap();
        assert!(row.contains("▅▅▅▅") && row.contains("100%") && row.ends_with("opus 5.5"), "{row}");
        assert!(plain.contains("no call went past 400k") && plain.contains("nothing was written twice"), "{plain}");
        let colour = render(&r, 86_400_000 * 2, Some(&crate::config::Colors::default()));
        assert!(colour.contains("\x1b[38;2;"), "{colour}");
    }

    #[test]
    fn handover_and_trim_markers_parse() {
        let prompt = "Continue from a handover. The previous session here (id df50e5f2-cabb) reached 238k tokens of context and handed over";
        assert_eq!(continued_from(prompt), Some(("df50e5f2-cabb".into(), 238_000)));
        assert_eq!(continued_from("fix the bug"), None);
        assert_eq!(trimmed_total("head\n[toomux · 900 lines, 80k chars in all. Shown: the first 40 lines"), 80_000);
        assert_eq!(trimmed_total("[toomux · 12 lines, 7400 chars in all. Shown"), 7_400);
        assert_eq!(trimmed_total("plain output"), 0);
    }

    #[test]
    fn a_continued_conversation_is_priced_as_if_it_kept_the_one_before() {
        let dir = std::env::temp_dir().join(format!("toomux-saving-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let rate = 0.2 / 1e6;
        let link = |id: &str, from: Option<(&str, u64)>, calls: &[u64]| Link {
            path: dir.join(format!("{id}.jsonl")),
            from: from.map(|(f, k)| (f.to_string(), k)),
            baseline: calls[0],
            calls: calls.iter().map(|&c| (c, rate)).collect(),
        };
        // a hands over at 200k to b, b at 200k to c; each starts at 20k.
        let mut r = Report {
            links: vec![link("a", None, &[20_000, 200_000]), link("b", Some(("a", 200_000)), &[20_000, 200_000]), link("c", Some(("b", 200_000)), &[20_000, 100_000])],
            compactions: vec![(500_000, 20_000)],
            ..Report::default()
        };
        handover_saving(&mut r, &dir.join("compaction.json"));
        // b carries 180k; c carries b's 180k plus b's own 180k.
        let want = (180_000.0 * 2.0 + 360_000.0 * 2.0) * rate;
        assert_eq!(r.saved.handovers, 2);
        assert!((r.saved.handover_usd - want).abs() < 1e-9, "{} vs {want}", r.saved.handover_usd);
        // Past where Claude Code compacts, the carried context wraps round.
        let mut r = Report {
            links: vec![link("a", None, &[20_000]), link("b", Some(("a", 480_000)), &[20_000, 60_000])],
            compactions: vec![(500_000, 20_000)],
            ..Report::default()
        };
        handover_saving(&mut r, &dir.join("compaction.json"));
        // 20k + 460k fits; 60k + 460k = 520k is past 500k, so it compacted
        // to 20k and has grown 20k since: 40k, less than with handover.
        let want = (460_000.0 - 20_000.0) * rate;
        assert!((r.saved.handover_usd - want).abs() < 1e-9, "{} vs {want}", r.saved.handover_usd);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
