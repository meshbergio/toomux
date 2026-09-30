//! Memory shared by every session: what was asked and decided, handover
//! briefs, and the short views of large command outputs, searchable by any
//! later session. Two recalls fused by reciprocal rank: words (FTS5, porter
//! stemmed, every word first, then any) and typo-tolerant (a trigram index
//! finds candidates, edit distance per word ranks them).
//!
//! An entry is `(scope, source, content)`, content-addressed so indexing the
//! same thing twice is a no-op. Scopes: `global`, `project:<folder>` (the git
//! root, where there is one). Sources say where an entry came from:
//! `session:<id>#<turn>` (and `session:<id>/agent-<agent>#<turn>` for its
//! subagents), `handover:<id>`, `output:<id> <command>`, `note: <about>`.
//!
//! Lives at ~/.local/state/toomux/memory.db, readable by you only. Nothing
//! goes in without passing through `redact`.

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Candidates the typo-tolerant axis looks at, and how much of each.
const FUZZY_CANDIDATES: usize = 300;
const FUZZY_CHARS: usize = 8000;
const RRF_K: f64 = 60.0;
const VERSION: i32 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: String,
    pub scope: String,
    pub source: String,
    pub snippet: String,
    pub created_ms: i64,
    pub relevance: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: String,
    pub scope: String,
    pub source: String,
    pub content: String,
    pub created_ms: i64,
    pub superseded_by: Option<String>,
}

pub struct Memory {
    conn: Connection,
}

pub fn path() -> PathBuf {
    crate::paths::state().join("memory.db")
}

/// The scope for work in a folder: its git root when it's in a repository,
/// so a session that wandered into a subfolder still files under the project.
pub fn project_scope(cwd: &str) -> String {
    format!("project:{}", project_root(cwd))
}

pub fn project_root(cwd: &str) -> String {
    use std::sync::{LazyLock, Mutex};
    static SEEN: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);
    if let Some(r) = SEEN.lock().ok().and_then(|m| m.get(cwd).cloned()) {
        return r;
    }
    let root = Path::new(cwd)
        .ancestors()
        .find(|d| d.join(".git").exists())
        .map_or_else(
            || cwd.trim_end_matches('/').to_string(),
            |d| d.display().to_string(),
        );
    let root = if root.is_empty() {
        cwd.to_string()
    } else {
        root
    };
    if let Ok(mut m) = SEEN.lock() {
        m.insert(cwd.to_string(), root.clone());
    }
    root
}

impl Memory {
    pub fn open() -> Result<Self> {
        let p = path();
        let dir = p.parent().unwrap();
        std::fs::create_dir_all(dir)?;
        let conn = Connection::open(&p)?;
        // Many writers (hooks, the indexer, MCP servers) share one file.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            for f in [
                p.clone(),
                p.with_extension("db-wal"),
                p.with_extension("db-shm"),
            ] {
                let _ = std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o600));
            }
        }
        let m = Self { conn };
        m.recall_table()?;
        if m.version()? < VERSION {
            // No indexer pass may be mid-way (it would save its old place
            // over the reset). Past a minute, go ahead regardless.
            let _held = crate::index::lock(std::time::Duration::from_secs(60));
            if m.migrate()? {
                // Every conversation is read again by the fixed indexer.
                let _ = std::fs::remove_file(dir.join("index.json"));
            }
        }
        Ok(m)
    }

    pub fn in_memory() -> Result<Self> {
        let m = Self {
            conn: Connection::open_in_memory()?,
        };
        m.migrate()?;
        m.recall_table()?;
        Ok(m)
    }

    /// Which session reached for which entry, for the memory graph. Kept
    /// apart from the entries: no version bump, nothing to index again.
    fn recall_table(&self) -> Result<()> {
        // A read first: creating takes the write lock, which the indexer may hold.
        let have: i64 = self.conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'recall'",
            [],
            |r| r.get(0),
        )?;
        if have > 0 {
            return Ok(());
        }
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS recall (
               session TEXT NOT NULL,
               target  TEXT NOT NULL,
               how     TEXT NOT NULL,
               at_ms   INTEGER NOT NULL,
               PRIMARY KEY (session, target, how)
             ) WITHOUT ROWID;",
        )?;
        Ok(())
    }

    /// Note that `session` (a `session:<id>` label) reached `target`: `id:<prefix>`
    /// for an entry, `file:<path>` for a memory file it read. `how` is `read`
    /// (it asked for that one) or `found` (a search turned it up). The first
    /// time counts.
    pub fn recall(&self, session: &str, target: &str, how: &str, at_ms: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO recall(session, target, how, at_ms) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session, target, how, at_ms],
        )?;
        Ok(())
    }

    /// Every recall: (session, target, how, at_ms).
    pub fn recalls(&self) -> Result<Vec<(String, String, String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT session, target, how, at_ms FROM recall")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Every entry still in use, oldest first.
    pub fn all(&self) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, scope, source, content, created_ms, superseded_by FROM memory WHERE superseded_by IS NULL ORDER BY created_ms",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Entry {
                    id: r.get(0)?,
                    scope: r.get(1)?,
                    source: r.get(2)?,
                    content: r.get(3)?,
                    created_ms: r.get(4)?,
                    superseded_by: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn version(&self) -> Result<i32> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Bring the schema up to date. Returns whether conversations need
    /// indexing again.
    fn migrate(&self) -> Result<bool> {
        let version =
            |c: &Connection| c.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0));
        if version(&self.conn)? >= VERSION {
            return Ok(false);
        }
        // One process migrates; the rest wait and find it done.
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<bool> {
            let from = version(&self.conn)?;
            if from >= VERSION {
                return Ok(false);
            }
            let existed: bool = self.conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'memory'",
                [],
                |r| r.get::<_, i64>(0),
            )? > 0;
            self.conn.execute_batch(
                "DROP TRIGGER IF EXISTS memory_ai; DROP TRIGGER IF EXISTS memory_ad; DROP TRIGGER IF EXISTS memory_au;
                 DROP TABLE IF EXISTS memory_fts; DROP TABLE IF EXISTS memory_tri;",
            )?;
            if existed {
                // Conversations are indexed again from their transcripts (the
                // old indexer kept interim answers and noise); everything else
                // stays, with anything that looks like a credential removed.
                self.conn
                    .execute("DELETE FROM memory WHERE source LIKE 'session:%'", [])?;
                let rows: Vec<(i64, String)> = self
                    .conn
                    .prepare("SELECT rowid, content FROM memory")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                for (rowid, content) in rows {
                    let clean = crate::redact::redact(&content);
                    if clean != content {
                        self.conn.execute(
                            "UPDATE memory SET content = ?2 WHERE rowid = ?1",
                            rusqlite::params![rowid, clean],
                        )?;
                    }
                }
            }
            // The indexes (and their triggers) only now, built from what's left.
            self.conn.execute_batch(SCHEMA)?;
            if existed {
                self.conn.execute_batch("INSERT INTO memory_fts(memory_fts) VALUES ('rebuild'); INSERT INTO memory_tri(memory_tri) VALUES ('rebuild');")?;
            }
            self.conn
                .execute_batch(&format!("PRAGMA user_version = {VERSION}"))?;
            Ok(existed)
        })();
        match result {
            Ok(r) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(r)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Run `f` as one transaction (an indexing pass over a file, say).
    pub fn batch<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Index `content` (redacted); returns its id and whether it was already there.
    pub fn index(
        &self,
        scope: &str,
        source: &str,
        content: &str,
        created_ms: i64,
    ) -> Result<(String, bool)> {
        let content = crate::redact::redact(content);
        let id = address(scope, source, &content);
        self.conn.execute(
            "INSERT OR IGNORE INTO memory(id, scope, source, content, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, scope, source, content, created_ms],
        )?;
        Ok((id.clone(), self.conn.changes() == 0))
    }

    /// Index `content` as the one entry for `source`, replacing what was
    /// there (a conversation turn whose outcome grew). Returns whether it
    /// changed anything.
    pub fn put(&self, scope: &str, source: &str, content: &str, created_ms: i64) -> Result<bool> {
        let clean = crate::redact::redact(content);
        let id = address(scope, source, &clean);
        if self
            .conn
            .query_row("SELECT 1 FROM memory WHERE id = ?1", [&id], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Ok(false);
        }
        self.conn
            .execute("DELETE FROM memory WHERE source = ?1", [source])?;
        self.index(scope, source, &clean, created_ms)?;
        Ok(true)
    }

    /// Remove every entry whose source starts with `prefix` (for good: the
    /// full-text index goes with it). Returns how many.
    pub fn delete_source_prefix(&self, prefix: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM memory WHERE substr(source, 1, length(?1)) = ?1",
            [prefix],
        )?)
    }

    /// The content of the entry for exactly this source.
    pub fn by_source(&self, source: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content FROM memory WHERE source = ?1 LIMIT 1",
                [source],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Remove the entries of exactly this source. Returns how many.
    pub fn delete_source(&self, source: &str) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM memory WHERE source = ?1", [source])?)
    }

    /// One entry by its id or a prefix of it (8 characters at least, as the
    /// search shows 12). An ambiguous prefix is an error, not a guess.
    pub fn get(&self, id: &str) -> Result<Option<Entry>> {
        let id = id.trim().to_ascii_lowercase();
        if id.len() < 8 || id.len() > 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("a memory id is 8 to 64 hex characters, as mem_search shows it");
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, scope, source, content, created_ms, superseded_by FROM memory WHERE substr(id, 1, length(?1)) = ?1 LIMIT 2",
        )?;
        let mut found: Vec<Entry> = stmt
            .query_map([&id], |r| {
                Ok(Entry {
                    id: r.get(0)?,
                    scope: r.get(1)?,
                    source: r.get(2)?,
                    content: r.get(3)?,
                    created_ms: r.get(4)?,
                    superseded_by: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        if found.len() > 1 {
            bail!("{id} is the start of more than one memory: give more of it");
        }
        Ok(found.pop())
    }

    /// Delete one entry for good. Returns what it was.
    pub fn forget(&self, id: &str) -> Result<Entry> {
        let e = self
            .get(id)?
            .ok_or_else(|| anyhow::anyhow!("no memory {id}"))?;
        self.conn.execute(
            "UPDATE memory SET superseded_by = NULL WHERE superseded_by = ?1",
            [&e.id],
        )?;
        self.conn
            .execute("DELETE FROM memory WHERE id = ?1", [&e.id])?;
        Ok(e)
    }

    /// Mark `old` as replaced by `new`: it drops out of search but stays
    /// readable. Refuses unknown ids, self-links and cycles.
    pub fn supersede(&self, old: &str, new: &str) -> Result<()> {
        let (Some(o), Some(n)) = (self.get(old)?, self.get(new)?) else {
            bail!("no such memory")
        };
        if o.id == n.id {
            bail!("a memory can't replace itself");
        }
        // Walk forward from the replacement: reaching the old one is a cycle.
        let mut at = Some(n.id.clone());
        while let Some(id) = at {
            if id == o.id {
                bail!("that would make a cycle of replacements");
            }
            at = self.get(&id)?.and_then(|e| e.superseded_by);
        }
        self.conn.execute(
            "UPDATE memory SET superseded_by = ?2 WHERE id = ?1",
            rusqlite::params![o.id, n.id],
        )?;
        Ok(())
    }

    /// Word search (BM25, stemmed) within `scopes` (empty = all), best first:
    /// entries with every word, then entries with any.
    fn words(
        &self,
        scopes: &[String],
        source_prefix: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        let terms = terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = self.fts(
            "memory_fts",
            scopes,
            source_prefix,
            &terms.join(" AND "),
            limit,
        )?;
        if out.len() < limit && terms.len() > 1 {
            let seen: HashSet<String> = out.iter().map(|h| h.id.clone()).collect();
            let more = self.fts(
                "memory_fts",
                scopes,
                source_prefix,
                &terms.join(" OR "),
                limit,
            )?;
            out.extend(more.into_iter().filter(|h| !seen.contains(&h.id)));
            out.truncate(limit);
        }
        Ok(out)
    }

    fn fts(
        &self,
        table: &str,
        scopes: &[String],
        source_prefix: Option<&str>,
        expr: &str,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        let sql = format!(
            "SELECT m.id, m.scope, m.source, snippet({table}, 0, '[', ']', '…', 16), m.created_ms, -bm25({table}) \
             FROM {table} JOIN memory m ON m.rowid = {table}.rowid \
             WHERE {table} MATCH ?1 AND m.superseded_by IS NULL \
               AND (?2 IS NULL OR substr(m.source, 1, length(?2)) = ?2) \
               AND (?3 IS NULL OR m.scope IN (SELECT value FROM json_each(?3))) \
             ORDER BY bm25({table}) LIMIT ?4"
        );
        let scopes =
            (!scopes.is_empty()).then(|| serde_json::to_string(scopes).unwrap_or_default());
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params![expr, source_prefix, scopes, limit as i64],
            |r| {
                Ok(Hit {
                    id: r.get(0)?,
                    scope: r.get(1)?,
                    source: r.get(2)?,
                    snippet: r.get(3)?,
                    created_ms: r.get(4)?,
                    relevance: r.get(5)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Typo- and variant-tolerant search: the trigram index proposes entries
    /// sharing pieces of the query's words; each is scored by how closely its
    /// words match them (edit distance, prefixes count).
    fn fuzzy(
        &self,
        scopes: &[String],
        source_prefix: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        let words: Vec<String> = query_words(query)
            .into_iter()
            .filter(|w| w.chars().count() >= 3)
            .collect();
        if words.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let grams: HashSet<String> = words
            .iter()
            .flat_map(|w| {
                let c: Vec<char> = w.chars().collect();
                c.windows(3)
                    .map(|g| g.iter().collect::<String>())
                    .collect::<Vec<_>>()
            })
            .collect();
        let expr = grams
            .iter()
            .map(|g| format!("\"{}\"", g.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let candidates = self.fts("memory_tri", scopes, source_prefix, &expr, FUZZY_CANDIDATES)?;
        let mut hits = Vec::new();
        for mut h in candidates {
            let content: String = self.conn.query_row(
                "SELECT substr(content, 1, ?2) FROM memory WHERE id = ?1",
                rusqlite::params![h.id, FUZZY_CHARS as i64],
                |r| r.get(0),
            )?;
            let score = closeness(&words, &content);
            if score >= 0.6 {
                h.relevance = score;
                h.snippet = excerpt_near(&content, &words);
                hits.push(h);
            }
        }
        hits.sort_by(|a, b| {
            b.relevance
                .total_cmp(&a.relevance)
                .then_with(|| b.created_ms.cmp(&a.created_ms))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// Words and fuzzy fused by reciprocal rank: what both find ranks first.
    pub fn search(
        &self,
        scopes: &[String],
        source_prefix: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        let pool = (limit * 4).max(20);
        let words = self.words(scopes, source_prefix, query, pool)?;
        let fuzzy = self.fuzzy(scopes, source_prefix, query, pool)?;
        Ok(fuse(&[words, fuzzy], limit))
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM memory WHERE superseded_by IS NULL",
            [],
            |r| r.get(0),
        )?)
    }
}

fn address(scope: &str, source: &str, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(scope.as_bytes());
    h.update([0x1f]);
    h.update(source.as_bytes());
    h.update([0x1f]);
    h.update(content.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "did", "do", "does", "for", "from", "how",
    "i", "in", "is", "it", "of", "on", "or", "the", "that", "this", "to", "was", "we", "what",
    "when", "where", "which", "who", "why", "with", "you",
];

/// The query's words, lowercased, without stopwords (unless that's all it has).
fn query_words(raw: &str) -> Vec<String> {
    let all: Vec<String> = raw
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect();
    let kept: Vec<String> = all
        .iter()
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .cloned()
        .collect();
    if kept.is_empty() { all } else { kept }
}

/// The query as FTS5 terms, each safely quoted: plain words, and compounds
/// (`handover_tokens`, `northwind-rmm`, `index.rs`) as phrases.
fn terms(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for chunk in raw.split_whitespace() {
        let parts: Vec<String> = chunk
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_lowercase)
            .collect();
        let parts: Vec<String> = if parts.len() == 1 {
            parts
                .into_iter()
                .filter(|p| !STOPWORDS.contains(&p.as_str()))
                .collect()
        } else {
            parts
        };
        if !parts.is_empty() {
            let t = format!("\"{}\"", parts.join(" "));
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    if out.is_empty() {
        // Only stopwords: search for them after all.
        out = query_words(raw)
            .into_iter()
            .map(|w| format!("\"{w}\""))
            .collect();
    }
    out
}

/// How well `content`'s words match every query word (0 to 1): each query
/// word takes its best match, by edit distance or as a prefix.
fn closeness(words: &[String], content: &str) -> f64 {
    let lower = content.to_lowercase();
    let vocab: HashSet<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .collect();
    let mut total = 0.0;
    for w in words {
        let wl = w.chars().count();
        let best = vocab
            .iter()
            .map(|v| {
                if *v == w {
                    return 1.0;
                }
                let vl = v.chars().count();
                if vl > wl && v.starts_with(w.as_str()) {
                    return 0.9;
                }
                if vl.abs_diff(wl) > 2 {
                    return 0.0;
                }
                1.0 - osa(w, v) as f64 / wl.max(vl) as f64
            })
            .fold(0.0, f64::max);
        total += best;
    }
    total / words.len() as f64
}

/// Edit distance counting a swap of neighbours as one edit.
fn osa(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// About 160 characters of `content` around the first close match.
fn excerpt_near(content: &str, words: &[String]) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = flat.to_lowercase();
    let at = words
        .iter()
        .filter_map(|w| lower.find(&w.chars().take(4).collect::<String>()))
        .min()
        .unwrap_or(0);
    let chars: Vec<char> = flat.chars().collect();
    let start_char = lower
        .get(..at)
        .map_or(0, |s| s.chars().count())
        .saturating_sub(40);
    let piece: String = chars.iter().skip(start_char).take(160).collect();
    format!(
        "{}{piece}{}",
        if start_char > 0 { "…" } else { "" },
        if start_char + 160 < chars.len() {
            "…"
        } else {
            ""
        }
    )
}

fn fuse(lists: &[Vec<Hit>], limit: usize) -> Vec<Hit> {
    let mut score: HashMap<String, f64> = HashMap::new();
    let mut kept: HashMap<String, Hit> = HashMap::new();
    for list in lists {
        for (rank, h) in list.iter().enumerate() {
            *score.entry(h.id.clone()).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            kept.entry(h.id.clone()).or_insert_with(|| h.clone());
        }
    }
    let mut out: Vec<Hit> = kept
        .into_values()
        .map(|mut h| {
            h.relevance = score[&h.id];
            h
        })
        .collect();
    out.sort_by(|a, b| {
        b.relevance
            .total_cmp(&a.relevance)
            .then_with(|| b.created_ms.cmp(&a.created_ms))
    });
    out.truncate(limit);
    out
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memory (
  id            TEXT PRIMARY KEY,
  scope         TEXT NOT NULL,
  source        TEXT NOT NULL,
  content       TEXT NOT NULL,
  created_ms    INTEGER NOT NULL,
  superseded_by TEXT
);
CREATE INDEX IF NOT EXISTS memory_scope ON memory(scope);
CREATE INDEX IF NOT EXISTS memory_created ON memory(created_ms);
CREATE INDEX IF NOT EXISTS memory_source ON memory(source);
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(content, source, content='memory', content_rowid='rowid', tokenize='porter unicode61');
CREATE VIRTUAL TABLE IF NOT EXISTS memory_tri USING fts5(content, content='memory', content_rowid='rowid', tokenize='trigram');
CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory BEGIN
  INSERT INTO memory_fts(rowid, content, source) VALUES (new.rowid, new.content, new.source);
  INSERT INTO memory_tri(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, content, source) VALUES ('delete', old.rowid, old.content, old.source);
  INSERT INTO memory_tri(memory_tri, rowid, content) VALUES ('delete', old.rowid, old.content);
END;
CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE OF content, source ON memory BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, content, source) VALUES ('delete', old.rowid, old.content, old.source);
  INSERT INTO memory_tri(memory_tri, rowid, content) VALUES ('delete', old.rowid, old.content);
  INSERT INTO memory_fts(rowid, content, source) VALUES (new.rowid, new.content, new.source);
  INSERT INTO memory_tri(rowid, content) VALUES (new.rowid, new.content);
END;
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_is_idempotent_and_searchable() {
        let m = Memory::in_memory().unwrap();
        let (id, dup) = m
            .index(
                "project:/x",
                "session:abc#1",
                "Rotated the MySQL root password on Watson",
                1,
            )
            .unwrap();
        assert!(!dup);
        assert_eq!(
            m.index(
                "project:/x",
                "session:abc#1",
                "Rotated the MySQL root password on Watson",
                2
            )
            .unwrap(),
            (id.clone(), true)
        );
        let hits = m.search(&[], None, "mysql password", 5).unwrap();
        assert_eq!(hits[0].id, id);
        assert!(
            m.get(&id[..10]).unwrap().is_some(),
            "an id prefix is enough"
        );
    }

    #[test]
    fn variants_and_typos_are_found() {
        let m = Memory::in_memory().unwrap();
        m.index("global", "note", "refreshing the oauth token every hour", 1)
            .unwrap();
        assert!(
            !m.words(&[], None, "refresh", 5).unwrap().is_empty(),
            "stemmed words find the variant"
        );
        assert!(
            m.words(&[], None, "refersh oauht", 5).unwrap().is_empty(),
            "exact words miss the typos"
        );
        assert!(
            !m.search(&[], None, "refersh oauht", 5).unwrap().is_empty(),
            "the fused search finds them"
        );
        // However many newer entries there are.
        for i in 0..5000 {
            m.index(
                "global",
                "note",
                &format!("unrelated entry number {i} about oregon deploys"),
                10 + i,
            )
            .unwrap();
        }
        assert_eq!(
            m.search(&[], None, "refersh oauht", 5).unwrap()[0].snippet,
            "refreshing the oauth token every hour"
        );
    }

    #[test]
    fn every_word_ranks_above_any_word() {
        let m = Memory::in_memory().unwrap();
        for i in 0..30 {
            m.index(
                "global",
                "note",
                &format!("rotate the watson password, step {i}"),
                i,
            )
            .unwrap();
        }
        let (both, _) = m
            .index("global", "note", "rotate the mysql key on the replica", 100)
            .unwrap();
        let hits = m.search(&[], None, "rotate mysql replica", 3).unwrap();
        assert_eq!(hits[0].id, both);
        // Compounds are phrases, not loose words.
        let (c, _) = m
            .index("global", "note", "set handover_tokens to 400000", 101)
            .unwrap();
        m.index("global", "note", "tokens for the handover are listed", 102)
            .unwrap();
        assert_eq!(
            m.words(&[], None, "handover_tokens", 5)
                .unwrap()
                .iter()
                .map(|h| &h.id)
                .collect::<Vec<_>>(),
            [&c]
        );
    }

    #[test]
    fn a_scope_is_searched_whole() {
        let m = Memory::in_memory().unwrap();
        // Plenty elsewhere that matches better, and one here.
        for i in 0..200 {
            m.index(
                "project:/elsewhere",
                "note",
                &format!("deploy deploy deploy target {i}"),
                i,
            )
            .unwrap();
        }
        let (here, _) = m
            .index(
                "project:/here",
                "note",
                "the deploy target is perth, after a long discussion of many other things",
                500,
            )
            .unwrap();
        let hits = m
            .search(&["project:/here".to_string()], None, "deploy target", 5)
            .unwrap();
        assert_eq!(hits.iter().map(|h| &h.id).collect::<Vec<_>>(), [&here]);
    }

    #[test]
    fn ids_are_exact_prefixes_and_forgetting_deletes() {
        let m = Memory::in_memory().unwrap();
        let (id, _) = m.index("global", "note", "keep this", 1).unwrap();
        assert!(
            m.get("%").is_err()
                && m.get("abc").is_err()
                && m.get(&format!("{}%", &id[..8])).is_err(),
            "no patterns, no short ids"
        );
        assert_eq!(m.get(&id[..8]).unwrap().unwrap().id, id);
        let (other, _) = m.index("global", "note", "and this", 2).unwrap();
        m.supersede(&other, &id).unwrap();
        m.forget(&id).unwrap();
        assert!(m.get(&id).unwrap().is_none(), "gone for good");
        assert!(
            m.words(&[], None, "keep", 5).unwrap().is_empty()
                && m.fuzzy(&[], None, "keep", 5).unwrap().is_empty(),
            "and from both indexes"
        );
        assert!(
            m.get(&other).unwrap().unwrap().superseded_by.is_none(),
            "what it replaced is back in search"
        );
    }

    #[test]
    fn an_old_memory_is_brought_up_to_date() {
        let conn = Connection::open_in_memory().unwrap();
        // The first schema, with a turn and a note holding a key.
        conn.execute_batch(
            "CREATE TABLE memory (id TEXT PRIMARY KEY, scope TEXT NOT NULL, source TEXT NOT NULL, content TEXT NOT NULL, created_ms INTEGER NOT NULL, superseded_by TEXT);
             CREATE VIRTUAL TABLE memory_fts USING fts5(content, source, content='memory', content_rowid='rowid');
             CREATE TRIGGER memory_ai AFTER INSERT ON memory BEGIN INSERT INTO memory_fts(rowid, content, source) VALUES (new.rowid, new.content, new.source); END;
             INSERT INTO memory VALUES ('a1', 'project:/p', 'session:x#1', 'You asked: hi', 1, NULL);
             INSERT INTO memory VALUES ('b2', 'global', 'note: aws', 'the key is AKIAABCDEFGHIJKLMNOP for staging', 2, NULL); -- gitleaks:allow",
        )
        .unwrap();
        let m = Memory { conn };
        assert!(
            m.migrate().unwrap(),
            "conversations are to be indexed again"
        );
        assert_eq!(m.count().unwrap(), 1, "old turns go");
        let hits = m.search(&[], None, "staging key", 5).unwrap();
        assert!(
            hits[0].snippet.contains("[redacted]") && !hits[0].snippet.contains("AKIA"),
            "{}",
            hits[0].snippet
        );
        assert!(!m.migrate().unwrap(), "once");
    }

    #[test]
    fn scopes_sources_and_supersession() {
        let m = Memory::in_memory().unwrap();
        let (old, _) = m
            .index("project:/a", "session:1#1", "deploy target is oregon", 1)
            .unwrap();
        m.index("project:/b", "session:2#1", "deploy target is melbourne", 2)
            .unwrap();
        let a = vec!["project:/a".to_string()];
        assert_eq!(m.search(&a, None, "deploy target", 5).unwrap().len(), 1);
        assert_eq!(
            m.search(&[], Some("session:2"), "deploy", 5).unwrap().len(),
            1
        );
        let (new, _) = m
            .index(
                "project:/a",
                "session:1#9",
                "deploy target moved to perth",
                3,
            )
            .unwrap();
        m.supersede(&old, &new).unwrap();
        let hits = m.search(&a, None, "deploy target", 5).unwrap();
        assert!(
            hits.iter().all(|h| h.id != old),
            "a replaced memory leaves search"
        );
        assert!(
            m.get(&old).unwrap().unwrap().superseded_by.is_some(),
            "but stays readable"
        );
        assert!(m.supersede(&new, &old).is_err(), "no cycles");
    }

    #[test]
    fn hostile_queries_are_safe() {
        let m = Memory::in_memory().unwrap();
        m.index("global", "x", "foo bar", 1).unwrap();
        for q in ["foo(bar", "\"", "NEAR(", "*", ""] {
            m.search(&[], None, q, 5).unwrap();
        }
    }
}
