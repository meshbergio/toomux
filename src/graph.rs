//! The memory graph: what toomux remembers, as things and the links between
//! them, for toomux's memory screen and `toomux graph --open`.
//!
//! - Projects hold memory files, notes and sessions.
//! - A memory file links to others by `[[name]]` or a markdown link; a
//!   MEMORY.md index links to the files it lists.
//! - A session holds its turns and the outputs it kept, writes a handover
//!   brief, and the brief is continued by the session that picked it up.
//! - A session recalls entries: one it asked for (mem_get, or reading a
//!   memory file) is `read`, one a search turned up is `found`.

use crate::memory::{Entry, Memory};
use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Project,
    Index,
    File,
    Note,
    Session,
    Handover,
    Turn,
    Output,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Project => "project",
            Kind::Index => "memory index",
            Kind::File => "memory file",
            Kind::Note => "note",
            Kind::Session => "session",
            Kind::Handover => "handover brief",
            Kind::Turn => "turn",
            Kind::Output => "kept output",
        }
    }
    /// Turns and outputs are the fine grain: shown around their session.
    pub fn fine(self) -> bool {
        matches!(self, Kind::Turn | Kind::Output)
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Link {
    /// Belongs to a project.
    In,
    /// A memory file names another.
    Links,
    /// A session's turn or kept output.
    Holds,
    /// A session wrote this handover brief.
    Wrote,
    /// A brief was picked up by this session.
    Continued,
    /// A session asked for this entry.
    Read,
    /// A session's search turned this entry up.
    Found,
}

impl Link {
    pub fn word(self) -> &'static str {
        match self {
            Link::In => "in",
            Link::Links => "links to",
            Link::Holds => "holds",
            Link::Wrote => "wrote",
            Link::Continued => "continued by",
            Link::Read => "read",
            Link::Found => "found",
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Node {
    pub id: String,
    pub kind: Kind,
    pub label: String,
    /// The project it belongs to (a node id), for a project itself none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// When it was made (a session: its first turn).
    pub at: i64,
    /// When it was last touched (a session: its last turn).
    pub last: i64,
    /// How much there is to it: turns for a session, entries for a project,
    /// characters otherwise.
    pub size: u64,
    /// A few lines of what it says.
    pub text: String,
    /// Its file, or its project's folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The memory entry behind it, where there is one (12 characters).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Edge {
    #[serde(rename = "s")]
    pub from: usize,
    #[serde(rename = "t")]
    pub to: usize,
    pub kind: Link,
    /// How many times (recalls repeat).
    pub n: u32,
}

#[derive(Serialize, Default)]
pub struct Graph {
    pub made: i64,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    #[serde(skip)]
    pub by_id: HashMap<String, usize>,
}

const TEXT_CHARS: usize = 480;
const LABEL_CHARS: usize = 64;

static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap()
});
static WIKI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([^\]\n|#]{1,120})(?:[|#][^\]\n]*)?\]\]").unwrap());
static PATHQ: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@?["']((?:~|/)[^"'\n]*)["']"#).unwrap());
static UPLOAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{8}-").unwrap());
static MDLINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\]\(([^)\s]{1,200}\.md)\)").unwrap());

impl Graph {
    fn node(&mut self, n: Node) -> usize {
        if let Some(&i) = self.by_id.get(&n.id) {
            return i;
        }
        self.by_id.insert(n.id.clone(), self.nodes.len());
        self.nodes.push(n);
        self.nodes.len() - 1
    }

    fn link(
        &mut self,
        from: usize,
        to: usize,
        kind: Link,
        seen: &mut HashMap<(usize, usize, Link), usize>,
    ) {
        if from == to {
            return;
        }
        if let Some(&e) = seen.get(&(from, to, kind)) {
            self.edges[e].n += 1;
            return;
        }
        seen.insert((from, to, kind), self.edges.len());
        self.edges.push(Edge {
            from,
            to,
            kind,
            n: 1,
        });
    }

    /// The graph without some of its nodes, and their links.
    fn without(self, drop: &[bool]) -> Graph {
        let mut at = vec![usize::MAX; self.nodes.len()];
        let mut g = Graph {
            made: self.made,
            ..Default::default()
        };
        for (i, n) in self.nodes.into_iter().enumerate() {
            if !drop[i] {
                at[i] = g.nodes.len();
                g.by_id.insert(n.id.clone(), g.nodes.len());
                g.nodes.push(n);
            }
        }
        g.edges = self
            .edges
            .into_iter()
            .filter(|e| !drop[e.from] && !drop[e.to])
            .map(|e| Edge {
                from: at[e.from],
                to: at[e.to],
                ..e
            })
            .collect();
        g
    }

    pub fn get(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).copied()
    }

    /// Each node's links, both ways: (other node, edge index).
    pub fn adjacency(&self) -> Vec<Vec<(usize, usize)>> {
        let mut adj = vec![Vec::new(); self.nodes.len()];
        for (i, e) in self.edges.iter().enumerate() {
            adj[e.from].push((e.to, i));
            adj[e.to].push((e.from, i));
        }
        adj
    }

    pub fn count(&self, kind: Kind) -> usize {
        self.nodes.iter().filter(|n| n.kind == kind).count()
    }
}

/// The graph of everything in memory now.
pub fn build(mem: &Memory) -> Result<Graph> {
    let entries = mem.all()?;
    let recalls = mem.recalls()?;
    let names = crate::state::State::load().names;
    Ok(assemble(
        &entries,
        &recalls,
        &names,
        &crate::index::titles(),
        crate::registry::now_ms(),
    ))
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_string(),
    }
}

fn one_line(s: &str, n: usize) -> String {
    clip(&s.split_whitespace().collect::<Vec<_>>().join(" "), n)
}

/// A prompt as a label: its first line with words in it, without the
/// markdown around it (a `# heading`, `> quote`, `**bold**`, a rule).
fn plain(p: &str) -> String {
    let line = p
        .lines()
        .map(|l| {
            l.trim()
                .trim_start_matches(['#', '>', '*', '-', '=', '_', ' '])
        })
        .find(|l| l.chars().any(char::is_alphanumeric))
        .unwrap_or("");
    // An attached file is named by its file name, not its whole path.
    let line = PATHQ.replace_all(line, |c: &regex::Captures| {
        let name = c[1].rsplit('/').next().unwrap_or("");
        UPLOAD.replace(name, "").into_owned()
    });
    line.replace("**", "").replace('`', "")
}

fn project_label(root: &str) -> String {
    let home = crate::config::home().display().to_string();
    if root == home {
        return "~".into();
    }
    if root == "/" || root.is_empty() {
        return "/".into();
    }
    Path::new(root)
        .file_name()
        .map_or_else(|| root.to_string(), |n| n.to_string_lossy().into_owned())
}

/// A memory file's frontmatter name and description, and its body.
fn frontmatter(text: &str) -> (Option<String>, Option<String>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, None, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, None, text);
    };
    let (head, body) = (
        &rest[..end],
        rest[end + 4..].trim_start_matches(['-', '\n']),
    );
    let field = |k: &str| {
        head.lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix(k)
                    .and_then(|v| v.trim_start().strip_prefix(':'))
            })
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    };
    (field("name"), field("description"), body)
}

/// What a turn was asked, from its entry.
fn asked(content: &str) -> &str {
    let c = content.trim_start();
    let c = c
        .strip_prefix("You asked: ")
        .or_else(|| c.find(" was asked: ").map(|i| &c[i + 12..]))
        .unwrap_or(c);
    c.split("\n\nOutcome: ").next().unwrap_or(c)
}

fn outcome(content: &str) -> &str {
    content.split_once("\n\nOutcome: ").map_or("", |(_, o)| o)
}

/// A session id from a `session:<id>…` source or recall label.
fn session_of(label: &str) -> Option<&str> {
    let rest = label.strip_prefix("session:")?;
    let id = rest.split(['/', '#', ' ']).next()?;
    (id.len() >= 8).then_some(id)
}

/// Each session's names in Claude Code, by id: (given, Claude's summary).
pub type Titles = HashMap<String, (Option<String>, Option<String>)>;

pub fn assemble(
    entries: &[Entry],
    recalls: &[(String, String, String, i64)],
    names: &HashMap<String, String>,
    titles: &Titles,
    now: i64,
) -> Graph {
    let mut g = Graph {
        made: now,
        ..Default::default()
    };
    let mut seen = HashMap::new();
    // The node standing for each entry, for recalls.
    let mut of_entry: Vec<(String, usize)> = Vec::new();
    // Memory files for the links pass: node, folder, text.
    let mut files: Vec<(usize, String, String)> = Vec::new();
    // Each session's first prompts, for the brief it continued.
    let mut firsts: HashMap<String, Vec<(u64, String)>> = HashMap::new();
    // What a session is called when nothing names it: its first prompt, or its brief's title.
    let mut briefs: HashMap<String, String> = HashMap::new();

    let project = |g: &mut Graph, scope: &str| -> usize {
        let root = scope.strip_prefix("project:").unwrap_or(scope);
        let label = if scope == "global" {
            "everywhere".to_string()
        } else {
            project_label(root)
        };
        g.node(Node {
            id: format!("p:{root}"),
            kind: Kind::Project,
            label,
            project: None,
            at: i64::MAX,
            last: 0,
            size: 0,
            text: String::new(),
            path: (scope != "global").then(|| root.to_string()),
            entry: None,
        })
    };
    let session = |g: &mut Graph, sid: &str, p: usize, at: i64| -> usize {
        let pid = g.nodes[p].id.clone();
        let i = g.node(Node {
            id: format!("s:{sid}"),
            kind: Kind::Session,
            label: String::new(),
            project: Some(pid),
            at,
            last: at,
            size: 0,
            text: String::new(),
            path: None,
            entry: None,
        });
        let n = &mut g.nodes[i];
        n.at = n.at.min(at);
        n.last = n.last.max(at);
        i
    };

    for e in entries {
        let p = project(&mut g, &e.scope);
        {
            let pn = &mut g.nodes[p];
            pn.size += 1;
            pn.at = pn.at.min(e.created_ms);
            pn.last = pn.last.max(e.created_ms);
        }
        let pid = Some(g.nodes[p].id.clone());
        let short = e.id[..12].to_string();
        if let Some(path) = e.source.strip_prefix("memory: ") {
            let text = e
                .content
                .split_once("\n\n")
                .map_or(e.content.as_str(), |(_, t)| t);
            let (name, desc, body) = frontmatter(text);
            let stem = Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let index = stem == "MEMORY";
            // An index's own name says nothing: call it after its folder's workspace.
            let name = if index {
                Some("MEMORY.md".to_string())
            } else {
                name
            };
            let i = g.node(Node {
                id: format!("f:{path}"),
                kind: if index { Kind::Index } else { Kind::File },
                label: one_line(&name.unwrap_or(stem), LABEL_CHARS),
                project: pid,
                at: e.created_ms,
                last: e.created_ms,
                size: text.len() as u64,
                text: clip(
                    &desc.map_or_else(|| body.to_string(), |d| format!("{d}\n\n{body}")),
                    TEXT_CHARS,
                ),
                path: Some(path.to_string()),
                entry: Some(short),
            });
            g.link(i, p, Link::In, &mut seen);
            let dir = Path::new(path)
                .parent()
                .map(|d| d.display().to_string())
                .unwrap_or_default();
            files.push((i, dir, text.to_string()));
            of_entry.push((e.id.clone(), i));
        } else if let Some(rest) = e.source.strip_prefix("handover:") {
            let (sid, title) = rest.split_once(' ').unwrap_or((rest, ""));
            let s = session(&mut g, sid, p, e.created_ms);
            let heading = e.content.lines().find(|l| l.starts_with('#')).map(|l| {
                l.trim_start_matches(['#', ' '])
                    .trim_start_matches("Handover")
                    .trim_start_matches([':', ' ', '—', '-'])
            });
            let label = heading
                .filter(|h| !h.is_empty())
                .map_or_else(|| title.to_string(), str::to_string);
            let i = g.node(Node {
                id: format!("h:{sid}"),
                kind: Kind::Handover,
                label: one_line(
                    if label.is_empty() { "handover" } else { &label },
                    LABEL_CHARS,
                ),
                project: pid,
                at: e.created_ms,
                last: e.created_ms,
                size: e.content.len() as u64,
                text: clip(&e.content, TEXT_CHARS),
                path: None,
                entry: Some(short),
            });
            g.link(s, i, Link::Wrote, &mut seen);
            if !title.is_empty() {
                briefs.insert(sid.to_string(), title.to_string());
            }
            of_entry.push((e.id.clone(), i));
        } else if let Some(rest) = e.source.strip_prefix("output:") {
            // `output:<id> session:<sid> $ <command>`
            let mut parts = rest.splitn(3, ' ');
            let (oid, sess, cmd) = (
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
            );
            let i = g.node(Node {
                id: format!("o:{oid}"),
                kind: Kind::Output,
                label: one_line(cmd.trim_start_matches("$ "), LABEL_CHARS),
                project: pid,
                at: e.created_ms,
                last: e.created_ms,
                size: e.content.len() as u64,
                text: clip(&e.content, TEXT_CHARS),
                path: None,
                entry: Some(short),
            });
            match session_of(sess) {
                Some(sid) => {
                    let s = session(&mut g, sid, p, e.created_ms);
                    g.link(s, i, Link::Holds, &mut seen);
                }
                None => g.link(i, p, Link::In, &mut seen),
            }
            of_entry.push((e.id.clone(), i));
        } else if let Some(sid) = session_of(&e.source) {
            let s = session(&mut g, sid, p, e.created_ms);
            let agent = e.source.contains("/agent-");
            let n: u64 = e
                .source
                .rsplit('#')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            let q = asked(&e.content);
            let i = g.node(Node {
                id: format!("t:{}", e.id),
                kind: Kind::Turn,
                label: one_line(
                    &format!("{}{}", if agent { "agent: " } else { "" }, plain(q)),
                    LABEL_CHARS,
                ),
                project: pid,
                at: e.created_ms,
                last: e.created_ms,
                size: e.content.len() as u64,
                text: clip(
                    &format!("{}\n\n{}", clip(q, 200), outcome(&e.content)),
                    TEXT_CHARS,
                ),
                path: None,
                entry: Some(short),
            });
            g.link(s, i, Link::Holds, &mut seen);
            if !agent {
                g.nodes[s].size += 1;
                firsts
                    .entry(sid.to_string())
                    .or_default()
                    .push((n, q.to_string()));
            }
            of_entry.push((e.id.clone(), i));
        } else {
            // A saved note, `note: <about>`, or anything else kept.
            let about = e.source.strip_prefix("note: ").unwrap_or(&e.source);
            let i = g.node(Node {
                id: format!("n:{}", e.id),
                kind: Kind::Note,
                label: one_line(about, LABEL_CHARS),
                project: pid,
                at: e.created_ms,
                last: e.created_ms,
                size: e.content.len() as u64,
                text: clip(&e.content, TEXT_CHARS),
                path: None,
                entry: Some(short),
            });
            g.link(i, p, Link::In, &mut seen);
            of_entry.push((e.id.clone(), i));
        }
    }

    // Sessions: their names, their project, the brief each continued.
    for turns in firsts.values_mut() {
        turns.sort_by_key(|t| t.0);
    }
    let sessions: Vec<usize> = (0..g.nodes.len())
        .filter(|&i| g.nodes[i].kind == Kind::Session)
        .collect();
    // A name many sessions share (a window's, or one carried through
    // handovers) says little about any one of them.
    let mut shared: HashMap<&str, usize> = HashMap::new();
    for s in &sessions {
        let sid = &g.nodes[*s].id[2..];
        for n in names
            .get(sid)
            .into_iter()
            .chain(titles.get(sid).and_then(|t| t.0.as_ref()))
        {
            *shared.entry(n.as_str()).or_default() += 1;
        }
    }
    let unshared = |n: &String| shared.get(n.as_str()).copied().unwrap_or(0) < 3;
    for s in sessions {
        let sid = g.nodes[s].id[2..].to_string();
        let first = firsts
            .get(&sid)
            .and_then(|t| t.first())
            .map(|t| t.1.clone());
        // "Go" says little: the first prompt with more to it names it better.
        let title = firsts
            .get(&sid)
            .and_then(|t| {
                t.iter()
                    .map(|t| plain(&t.1))
                    .find(|p| p.chars().count() >= 12)
            })
            .or_else(|| first.as_deref().map(plain));
        // A successor's opening names the session it continued.
        let mut continued = None;
        for (_, prompt) in firsts.get(&sid).into_iter().flatten().take(2) {
            for m in UUID.find_iter(prompt) {
                if m.as_str() != sid
                    && let Some(h) = g.get(&format!("h:{}", m.as_str()))
                {
                    g.link(h, s, Link::Continued, &mut seen);
                    continued.get_or_insert(h);
                }
            }
        }
        let own = names.get(&sid).filter(|n| unshared(n)).cloned();
        // What Claude Code calls it, as toomux's session list does.
        let (given, summary) = titles.get(&sid).cloned().unwrap_or_default();
        let wrote = g
            .get(&format!("h:{sid}"))
            .map(|h| g.nodes[h].label.clone())
            .filter(|l| l != "handover");
        let label = own
            .or(given.filter(|n| unshared(n)))
            .or(summary)
            .or(wrote)
            .or_else(|| continued.map(|h| format!("{} (continued)", g.nodes[h].label)))
            .or_else(|| title.as_deref().map(crate::registry::prompt_title))
            .or_else(|| names.get(&sid).cloned())
            .or_else(|| briefs.get(&sid).cloned())
            .unwrap_or_else(|| sid[..8].to_string());
        let n = &mut g.nodes[s];
        n.label = one_line(&label, LABEL_CHARS);
        n.text = first
            .as_deref()
            .map(|f| clip(f, TEXT_CHARS))
            .unwrap_or_default();
        if let Some(p) = n.project.clone().and_then(|p| g.by_id.get(&p).copied()) {
            g.link(s, p, Link::In, &mut seen);
        }
    }

    // Memory files naming each other.
    let mut by_name: HashMap<(String, String), usize> = HashMap::new();
    let mut anywhere: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, dir, _) in &files {
        let n = &g.nodes[*i];
        let stem = n
            .path
            .as_deref()
            .and_then(|p| Path::new(p).file_stem())
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        for key in [stem, n.label.to_lowercase()] {
            by_name.insert((dir.clone(), key.clone()), *i);
            anywhere.entry(key).or_default().push(*i);
        }
    }
    for (i, dir, text) in &files {
        let mut targets: Vec<String> = WIKI
            .captures_iter(text)
            .map(|c| c[1].trim().to_lowercase())
            .collect();
        targets.extend(
            MDLINK
                .captures_iter(text)
                .filter(|c| !c[1].contains("://"))
                .map(|c| {
                    Path::new(&c[1])
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_lowercase())
                        .unwrap_or_default()
                }),
        );
        for t in targets {
            let hit = by_name.get(&(dir.clone(), t.clone())).copied().or_else(|| {
                let all = anywhere.get(&t)?;
                let mut u = all.clone();
                u.dedup();
                (u.len() == 1).then(|| u[0])
            });
            if let Some(j) = hit {
                g.link(*i, j, Link::Links, &mut seen);
            }
        }
    }

    // Recalls: a session reaching for an entry.
    of_entry.sort();
    for (label, target, how, _at) in recalls {
        let Some(sid) = session_of(label) else {
            continue;
        };
        let Some(&s) = g.by_id.get(&format!("s:{sid}")) else {
            continue;
        };
        let to = if let Some(prefix) = target.strip_prefix("id:") {
            let at = of_entry.partition_point(|(id, _)| id.as_str() < prefix);
            of_entry
                .get(at)
                .filter(|(id, _)| id.starts_with(prefix))
                .map(|(_, n)| *n)
        } else if let Some(path) = target.strip_prefix("file:") {
            g.get(&format!("f:{path}"))
        } else {
            None
        };
        if let Some(t) = to {
            g.link(
                s,
                t,
                if how == "found" {
                    Link::Found
                } else {
                    Link::Read
                },
                &mut seen,
            );
        }
    }
    for n in &mut g.nodes {
        if n.at == i64::MAX {
            n.at = 0;
        }
    }
    // A session of one turn with nothing else to it, over an hour ago (a
    // `claude -p` check, a quick test), is noise in a picture of memory.
    // Search still finds it.
    let mut linked = vec![false; g.nodes.len()];
    for e in g
        .edges
        .iter()
        .filter(|e| !matches!(e.kind, Link::In | Link::Holds))
    {
        linked[e.from] = true;
        linked[e.to] = true;
    }
    for e in g.edges.iter().filter(|e| e.kind == Link::Holds) {
        if linked[e.to] {
            linked[e.from] = true;
        }
    }
    let lone: Vec<bool> = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            n.kind == Kind::Session && n.size <= 1 && !linked[i] && n.last < now - 3_600_000
        })
        .collect();
    let mut drop = lone.clone();
    for e in g
        .edges
        .iter()
        .filter(|e| e.kind == Link::Holds && lone[e.from])
    {
        drop[e.to] = true;
    }
    if drop.contains(&true) {
        g = g.without(&drop);
    }
    // Projects with the same folder name: add the folder above it.
    let mut seen_names: HashMap<String, usize> = HashMap::new();
    for n in g.nodes.iter().filter(|n| n.kind == Kind::Project) {
        *seen_names.entry(n.label.to_lowercase()).or_default() += 1;
    }
    for n in g.nodes.iter_mut().filter(|n| n.kind == Kind::Project) {
        if seen_names
            .get(&n.label.to_lowercase())
            .is_some_and(|&c| c > 1)
            && let Some(parent) = n
                .path
                .as_deref()
                .and_then(|p| Path::new(p).parent())
                .and_then(|p| p.file_name())
        {
            n.label = format!("{}/{}", parent.to_string_lossy(), n.label);
        }
    }
    g
}

/// The browser view: one page, the graph inside it, nothing fetched.
pub fn page(g: &Graph, focus: Option<&str>) -> Result<String> {
    // Inside a <script>, `</` could close it early.
    let data = serde_json::to_string(g)?.replace("</", "<\\/");
    let focus = serde_json::to_string(&focus)?.replace("</", "<\\/");
    // Both places are found in the page before anything goes in: memory can
    // quote the placeholders themselves.
    let page = include_str!("../assets/graph/graph.html");
    let (g_mark, f_mark) = ("/*TOOMUX_GRAPH*/null", "/*TOOMUX_FOCUS*/null");
    let (Some(gi), Some(fi)) = (page.find(g_mark), page.find(f_mark)) else {
        anyhow::bail!("the graph page has lost its placeholders")
    };
    let mut parts = [
        (gi, g_mark.len(), data.as_str()),
        (fi, f_mark.len(), focus.as_str()),
    ];
    parts.sort_by_key(|p| p.0);
    let mut out = String::with_capacity(page.len() + data.len() + focus.len());
    let mut at = 0;
    for (i, len, with) in parts {
        out.push_str(&page[at..i]);
        out.push_str(with);
        at = i + len;
    }
    out.push_str(&page[at..]);
    Ok(out)
}

/// Write the browser view where only you can read it, and open it.
pub fn open_page(g: &Graph, focus: Option<&str>) -> Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = crate::paths::state().join("memory-graph.html");
    let tmp = path.with_extension(format!("html.{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(page(g, focus)?.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(path)
}

/// Recalls in one transcript line: (target, how). `pending` carries the
/// mem_search calls waiting for their results.
pub fn recalls_in(line: &str, pending: &mut Vec<String>) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    let calls = line.contains("mcp__toomux__mem_")
        || (line.contains("\"Read\"") && line.contains("/memory/"));
    let results = !pending.is_empty() && line.contains("tool_result");
    if !calls && !results {
        return out;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return out;
    };
    let Some(items) = v.pointer("/message/content").and_then(|c| c.as_array()) else {
        return out;
    };
    for i in items {
        match i.get("type").and_then(|t| t.as_str()) {
            Some("tool_use") => {
                let name = i.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = i.get("input");
                let arg = |k: &str| {
                    input
                        .and_then(|x| x.get(k))
                        .and_then(|x| x.as_str())
                        .map(str::trim)
                };
                match name {
                    "mcp__toomux__mem_get" => {
                        if let Some(id) = arg("id")
                            .filter(|id| id.len() >= 8 && id.chars().all(|c| c.is_ascii_hexdigit()))
                        {
                            out.push((format!("id:{}", id.to_ascii_lowercase()), "read"));
                        }
                    }
                    "mcp__toomux__mem_search" => {
                        if let Some(id) = i.get("id").and_then(|x| x.as_str()) {
                            pending.push(id.to_string());
                        }
                    }
                    "Read" => {
                        if let Some(p) = arg("file_path")
                            .filter(|p| p.contains("/memory/") && p.ends_with(".md"))
                        {
                            out.push((format!("file:{p}"), "read"));
                        }
                    }
                    _ => {}
                }
            }
            Some("tool_result") => {
                let Some(id) = i.get("tool_use_id").and_then(|x| x.as_str()) else {
                    continue;
                };
                let Some(at) = pending.iter().position(|p| p == id) else {
                    continue;
                };
                pending.remove(at);
                let body = match i.get("content") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                for l in body.lines() {
                    if let Some((id, _)) = l.split_once(" · ")
                        && id.len() == 12
                        && id.chars().all(|c| c.is_ascii_hexdigit())
                    {
                        out.push((format!("id:{id}"), "found"));
                    }
                }
            }
            _ => {}
        }
    }
    // A search whose result never came isn't waited on for ever.
    if pending.len() > 16 {
        pending.drain(..pending.len() - 16);
    }
    out
}

/// Recalls from every transcript, once: those from before recalls were
/// kept. Returns how many were noted.
pub fn backfill(cfg: &crate::config::Config) -> Result<usize> {
    let stamp = crate::paths::state().join("recall.stamp");
    if stamp.exists() {
        return Ok(0);
    }
    let mem = Memory::open()?;
    let mut noted = 0;
    let mut files = Vec::new();
    for root in crate::index::roots(cfg) {
        for p in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(p.path()).into_iter().flatten().flatten() {
                let path = f.path();
                if path.extension().is_some_and(|x| x == "jsonl") {
                    let sid = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    files.push((path, sid));
                } else if path.is_dir() {
                    let sid = f.file_name().to_string_lossy().into_owned();
                    for a in std::fs::read_dir(path.join("subagents"))
                        .into_iter()
                        .flatten()
                        .flatten()
                    {
                        if a.path().extension().is_some_and(|x| x == "jsonl") {
                            files.push((a.path(), sid.clone()));
                        }
                    }
                }
            }
        }
    }
    for (path, sid) in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !text.contains("mcp__toomux__mem_") && !text.contains("/memory/") {
            continue;
        }
        let label = format!("session:{sid}");
        let mut pending = Vec::new();
        mem.batch(|m| {
            for line in text.lines() {
                let found = recalls_in(line, &mut pending);
                if found.is_empty() {
                    continue;
                }
                let at = line_time(line);
                for (target, how) in found {
                    m.recall(&label, &target, how, at)?;
                    noted += 1;
                }
            }
            Ok(())
        })?;
    }
    std::fs::write(&stamp, b"")?;
    Ok(noted)
}

pub fn line_time(line: &str) -> i64 {
    line.find("\"timestamp\":\"")
        .and_then(|i| line.get(i + 13..i + 13 + 40))
        .and_then(|t| t.split('"').next())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map_or(0, |t| t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(scope: &str, source: &str, content: &str, at: i64) -> Entry {
        let id = format!("{at:02x}{}", "0".repeat(62));
        Entry {
            id,
            scope: scope.into(),
            source: source.into(),
            content: content.into(),
            created_ms: at,
            superseded_by: None,
        }
    }

    #[test]
    fn memory_becomes_projects_files_sessions_and_the_links_between_them() {
        let sid = "0ff6405c-c71f-4a6a-8d59-de4d9a0cca4c";
        let next = "65d8f3f2-f608-4ad1-8faa-cadcbc1b2c0e";
        let dir = "/h/.claude/projects/-w/memory";
        let entries = vec![
            entry(
                "project:/w/toomux",
                &format!("memory: {dir}/MEMORY.md"),
                "MEMORY.md\n\n- [Voyage](voyage.md) — the ship\n",
                1,
            ),
            entry(
                "project:/w/toomux",
                &format!("memory: {dir}/voyage.md"),
                "voyage.md\n\n---\nname: toomux-voyage\ndescription: the pirate ship\n---\nSee [[toomux-release]].",
                2,
            ),
            entry(
                "project:/w/toomux",
                &format!("memory: {dir}/release.md"),
                "release.md\n\n---\nname: toomux-release\n---\nGo public when Sam says.",
                3,
            ),
            entry(
                "project:/w/toomux",
                &format!("session:{sid}#1"),
                "You asked: tune the tiers\n\nOutcome: done",
                4,
            ),
            entry(
                "project:/w/toomux",
                &format!("output:abc1234 session:{sid} $ cargo test"),
                "121 passed",
                5,
            ),
            entry(
                "project:/w/toomux",
                &format!("handover:{sid} Voyage tiers"),
                "# Handover: voyage tiers tuned\n\nnext steps",
                6,
            ),
            entry(
                "project:/w/toomux",
                &format!("session:{next}#1"),
                &format!(
                    "You asked: Continue from a handover. The previous session here (id {sid})\n\nOutcome: on it"
                ),
                7,
            ),
            entry("global", "note: judge model", "sonnet for hard", 8),
        ];
        let recalls = vec![
            (
                format!("session:{next}"),
                format!("id:{}", &entries[1].id[..12]),
                "found".to_string(),
                9,
            ),
            (
                format!("session:{next}/agent-x"),
                format!("file:{dir}/release.md"),
                "read".to_string(),
                10,
            ),
        ];
        let mut names = HashMap::new();
        names.insert(sid.to_string(), "Voyage work".to_string());
        // Claude's summary names a session before its first prompt does.
        let titles: Titles = [(next.to_string(), (None, Some("Memory recall".to_string())))].into();
        let g = assemble(&entries, &recalls, &names, &titles, 11);
        let n = |id: &str| g.get(id).unwrap_or_else(|| panic!("no {id}"));
        let has = |a: usize, b: usize, k: Link| {
            g.edges
                .iter()
                .any(|e| e.from == a && e.to == b && e.kind == k)
        };

        assert_eq!(g.nodes[n("p:/w/toomux")].label, "toomux");
        assert_eq!(g.nodes[n("p:global")].label, "everywhere");
        let (index, voyage, release) = (
            n(&format!("f:{dir}/MEMORY.md")),
            n(&format!("f:{dir}/voyage.md")),
            n(&format!("f:{dir}/release.md")),
        );
        assert_eq!(g.nodes[index].kind, Kind::Index);
        assert_eq!(g.nodes[voyage].label, "toomux-voyage");
        assert!(g.nodes[voyage].text.starts_with("the pirate ship"));
        assert!(
            has(index, voyage, Link::Links),
            "a markdown link from the index"
        );
        assert!(
            has(voyage, release, Link::Links),
            "a [[wikilink]] by frontmatter name"
        );
        let (s, s2) = (n(&format!("s:{sid}")), n(&format!("s:{next}")));
        assert_eq!(
            g.nodes[s].label, "Voyage work",
            "a name given in toomux wins"
        );
        assert_eq!(g.nodes[n(&format!("s:{next}"))].label, "Memory recall");
        assert!(has(s, n("o:abc1234"), Link::Holds));
        assert!(has(s, n(&format!("h:{sid}")), Link::Wrote));
        assert_eq!(g.nodes[n(&format!("h:{sid}"))].label, "voyage tiers tuned");
        assert!(
            has(n(&format!("h:{sid}")), s2, Link::Continued),
            "the successor picked up the brief"
        );
        assert!(
            has(s2, voyage, Link::Found),
            "a search hit, by its 12-character id"
        );
        assert!(
            has(s2, release, Link::Read),
            "a subagent's read counts for its session"
        );
        assert!(has(s, n("p:/w/toomux"), Link::In));
        assert_eq!(g.nodes[s].size, 1, "one turn");
        assert_eq!(g.count(Kind::Note), 1);
    }

    #[test]
    fn recalls_are_read_from_transcript_lines() {
        let mut pending = Vec::new();
        let get = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__toomux__mem_get","input":{"id":"093cf9ef3bd9"}},{"type":"tool_use","id":"t2","name":"mcp__toomux__mem_search","input":{"query":"rig"}},{"type":"tool_use","id":"t3","name":"Read","input":{"file_path":"/h/p/memory/toomux.md"}}]}}"#;
        assert_eq!(
            recalls_in(get, &mut pending),
            vec![
                ("id:093cf9ef3bd9".to_string(), "read"),
                ("file:/h/p/memory/toomux.md".to_string(), "read")
            ]
        );
        assert_eq!(pending, ["t2"]);
        let res = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":[{"type":"text","text":"093cf9ef3bd9 · handover:f379 · 1h ago · project:/h\n  …rig…\nd45e3e542808 · session:265f#1 · 2h ago · project:/h\n  …"}]}]}}"#;
        assert_eq!(
            recalls_in(res, &mut pending),
            vec![
                ("id:093cf9ef3bd9".to_string(), "found"),
                ("id:d45e3e542808".to_string(), "found")
            ]
        );
        assert!(pending.is_empty());
        assert!(
            recalls_in(
                r#"{"type":"user","message":{"content":"hello"}}"#,
                &mut pending
            )
            .is_empty()
        );
    }

    #[test]
    fn the_page_fills_its_own_places_even_when_memory_quotes_them() {
        let quoted = "keep const FOCUS = /*TOOMUX_FOCUS*/null; and /*TOOMUX_GRAPH*/null intact";
        let g = assemble(
            &[entry("project:/w/toomux", "note: placeholders", quoted, 1)],
            &[],
            &HashMap::new(),
            &Titles::new(),
            10,
        );
        let html = page(&g, Some("n:x")).unwrap();
        assert!(
            html.contains(r#"const FOCUS = "n:x";"#),
            "the focus goes in the page's own place"
        );
        assert!(html.contains("const GRAPH = {"), "and the graph in its");
        assert!(
            html.contains("/*TOOMUX_FOCUS*/null; and"),
            "what memory says is left as it was"
        );
    }
}
