//! The memory screen (alt-m): what toomux remembers, as a graph. At first a
//! sky of projects, each a disc of its memory files, sessions and briefs,
//! brighter where it was touched lately. Focusing one thing gathers its
//! links around it, grouped by how they link (a session's turns, the brief
//! it wrote, what it read). Arrows move, enter focuses, backspace goes back,
//! / finds, o opens the same place in the browser view.

use super::*;
use crate::graph::{Graph, Kind, Link};
use ratatui::symbols::Marker;
use ratatui::widgets::canvas::{Canvas, Circle, Line as CLine, Points};
use std::collections::HashMap;

const TWEEN_MS: f32 = 340.0;
const GOLDEN: f64 = 2.399_963_229_728_653;

pub(super) struct MemView {
    rx: Option<Receiver<Result<Loaded, String>>>,
    data: Option<Loaded>,
    error: Option<String>,
    started: Instant,
    /// What's gathered in the middle; none for the sky of projects.
    focus: Option<usize>,
    sel: usize,
    back: Vec<(Option<usize>, usize)>,
    placed: Vec<Placed>,
    groups: Vec<Group>,
    /// Where things were before the last move, and the point new ones grow from.
    from: HashMap<usize, (f64, f64)>,
    bloom: (f64, f64),
    tween: Option<Instant>,
    search: Option<Search>,
    /// Where each node's mark was drawn: (node, col, row).
    hits: Vec<(usize, u16, u16)>,
    /// The graph's area when last laid out, and its width to height in pixels.
    area: Rect,
    aspect: f64,
    /// The key to glyphs and colours, in the quietest corner; ? hides it.
    legend: bool,
}

struct Loaded {
    g: Graph,
    adj: Vec<Vec<(usize, usize)>>,
    sky: Vec<Star>,
    /// Links between projects: (star, star, how many).
    lanes: Vec<(usize, usize, u32)>,
    /// Each node's project node.
    home: Vec<Option<usize>>,
}

/// A project in the sky, in the unit disc, with its members around it.
struct Star {
    node: usize,
    x: f64,
    y: f64,
    r: f64,
    dust: Vec<(usize, f64, f64)>,
}

#[derive(Clone)]
struct Placed {
    node: usize,
    x: f64,
    y: f64,
    via: Option<Link>,
    /// Which ring out from the focus: the first is labelled most.
    ring: u8,
    /// Its group, in the order they're drawn.
    group: usize,
}

struct Group {
    text: String,
    x: f64,
    y: f64,
    /// Where its arc starts, and how far it runs (clockwise, radians).
    from: f64,
    span: f64,
    link: Link,
    members: usize,
}

struct Search {
    query: String,
    hits: Vec<usize>,
    sel: usize,
}

fn ease(t: f32) -> f64 {
    let t = t.clamp(0.0, 1.0) as f64;
    1.0 - (1.0 - t).powi(3)
}

fn load() -> Result<Loaded, String> {
    let mem = crate::memory::Memory::open().map_err(|e| e.to_string())?;
    let g = crate::graph::build(&mem).map_err(|e| e.to_string())?;
    Ok(prepare(g))
}

fn prepare(g: Graph) -> Loaded {
    let adj = g.adjacency();
    let home: Vec<Option<usize>> = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            if n.kind == Kind::Project {
                Some(i)
            } else {
                n.project.as_ref().and_then(|p| g.get(p))
            }
        })
        .collect();
    let (sky, lanes) = sky(&g, &home);
    Loaded {
        g,
        adj,
        sky,
        lanes,
        home,
    }
}

/// Lay the projects out: discs sized by what they hold, pulled together by
/// the links between them, pushed apart where they'd overlap. Wider than
/// tall, as terminals are.
fn sky(g: &Graph, home: &[Option<usize>]) -> (Vec<Star>, Vec<(usize, usize, u32)>) {
    let mut projects: Vec<usize> = (0..g.nodes.len())
        .filter(|&i| g.nodes[i].kind == Kind::Project)
        .collect();
    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, n) in g.nodes.iter().enumerate() {
        if n.kind != Kind::Project
            && !n.kind.fine()
            && let Some(p) = home[i]
        {
            members.entry(p).or_default().push(i);
        }
    }
    let weight = |p: usize| members.get(&p).map_or(0, Vec::len) as f64 + 1.0;
    projects.sort_by(|a, b| weight(*b).total_cmp(&weight(*a)).then(a.cmp(b)));
    let at: HashMap<usize, usize> = projects.iter().enumerate().map(|(i, p)| (*p, i)).collect();
    let mut cross: HashMap<(usize, usize), u32> = HashMap::new();
    for e in &g.edges {
        if let (Some(a), Some(b)) = (home[e.from], home[e.to])
            && a != b
            && let (Some(&ia), Some(&ib)) = (at.get(&a), at.get(&b))
        {
            *cross.entry((ia.min(ib), ia.max(ib))).or_default() += e.n;
        }
    }
    let n = projects.len();
    let r: Vec<f64> = projects
        .iter()
        .map(|&p| 0.02 + 0.016 * weight(p).sqrt())
        .collect();
    let mut pos: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let a = i as f64 * GOLDEN;
            let d = 0.06 * (i as f64).sqrt();
            (a.cos() * d * 1.6, a.sin() * d)
        })
        .collect();
    let lanes: Vec<(usize, usize, u32)> = cross.iter().map(|(&(a, b), &w)| (a, b, w)).collect();
    for step in 0..360 {
        let cool = 1.0 - step as f64 / 400.0;
        let mut force = vec![(0.0f64, 0.0f64); n];
        for i in 0..n {
            for j in i + 1..n {
                let (dx, dy) = (pos[j].0 - pos[i].0, pos[j].1 - pos[i].1);
                let d = (dx * dx + dy * dy).sqrt().max(1e-4);
                let room = r[i] + r[j] + 0.02;
                let mut f = 0.00022 / (d * d);
                if d < room {
                    f += (room - d) * 0.5;
                }
                let (ux, uy) = (dx / d, dy / d);
                force[i].0 -= ux * f;
                force[i].1 -= uy * f;
                force[j].0 += ux * f;
                force[j].1 += uy * f;
            }
        }
        for &(a, b, w) in &lanes {
            let (dx, dy) = (pos[b].0 - pos[a].0, pos[b].1 - pos[a].1);
            let d = (dx * dx + dy * dy).sqrt().max(1e-4);
            let target = r[a] + r[b] + 0.04;
            let f = (d - target) * 0.012 * (1.0 + (w as f64).ln());
            let (ux, uy) = (dx / d, dy / d);
            force[a].0 += ux * f;
            force[a].1 += uy * f;
            force[b].0 -= ux * f;
            force[b].1 -= uy * f;
        }
        for i in 0..n {
            // Gravity, stronger up and down: the sky comes out wide.
            force[i].0 -= pos[i].0 * 0.004;
            force[i].1 -= pos[i].1 * 0.009;
            let (fx, fy) = force[i];
            let m = (fx * fx + fy * fy).sqrt();
            let cap = 0.03 * cool;
            let s = if m > cap { cap / m } else { 1.0 };
            pos[i].0 += fx * s;
            pos[i].1 += fy * s;
        }
    }
    let stars = projects
        .iter()
        .enumerate()
        .map(|(i, &p)| {
            let mut ms = members.remove(&p).unwrap_or_default();
            ms.sort_by_key(|&m| (g.nodes[m].kind, std::cmp::Reverse(g.nodes[m].last)));
            let k = ms.len().max(1) as f64;
            let spin = (p as f64 * 0.7) % std::f64::consts::TAU;
            let dust = ms
                .iter()
                .enumerate()
                .map(|(j, &m)| {
                    let rr = r[i] * 0.9 * ((j as f64 + 0.5) / k).sqrt();
                    let a = j as f64 * GOLDEN + spin;
                    (m, pos[i].0 + rr * a.cos(), pos[i].1 + rr * a.sin())
                })
                .collect();
            Star {
                node: p,
                x: pos[i].0,
                y: pos[i].1,
                r: r[i],
                dust,
            }
        })
        .collect();
    (stars, lanes)
}

/// How a neighbour is linked, from the focus: the group it's shown in, and
/// its order around the circle.
fn group_of(g: &Graph, other: usize, link: Link, out: bool) -> (u8, &'static str) {
    let k = g.nodes[other].kind;
    match (link, out, k) {
        (Link::In, true, _) => (0, "project"),
        (Link::In, false, Kind::Session) => (4, "sessions"),
        (Link::In, false, Kind::Index) => (1, "memory index"),
        (Link::In, false, Kind::File) => (2, "memory files"),
        (Link::In, false, Kind::Note) => (3, "notes"),
        (Link::In, false, _) => (5, "in it"),
        (Link::Links, true, _) => (2, "links to"),
        (Link::Links, false, _) => (3, "linked from"),
        (Link::Holds, true, Kind::Turn) => (6, "turns"),
        (Link::Holds, true, _) => (7, "kept outputs"),
        (Link::Holds, false, _) => (1, "session"),
        (Link::Wrote, true, _) => (8, "brief it wrote"),
        (Link::Wrote, false, _) => (1, "written by"),
        (Link::Continued, true, _) => (9, "continued by"),
        (Link::Continued, false, _) => (1, "continued from"),
        (Link::Read, true, _) => (10, "read"),
        (Link::Found, true, _) => (11, "found"),
        (Link::Read, false, _) => (10, "read by"),
        (Link::Found, false, _) => (11, "found by"),
    }
}

/// A group's heading, the same in the graph and the panel: its name and
/// count ("turns 6", "found 3"), or for one, just the name ("turn", "found").
fn group_text(name: &str, n: usize) -> String {
    match (n, name) {
        (1, "memory index") => name.to_string(),
        (1, "sessions" | "memory files" | "notes" | "turns" | "kept outputs") => {
            name.trim_end_matches('s').to_string()
        }
        (1, _) => name.to_string(),
        (_, "memory index") => format!("memory indexes {n}"),
        _ => format!("{name} {n}"),
    }
}

fn plural(n: usize, one: &str) -> String {
    match (n, one) {
        (1, _) => format!("1 {one}"),
        (_, "memory index") => format!("{n} memory indexes"),
        (_, w) if w.ends_with('s') => format!("{n} {w}"),
        _ => format!("{n} {one}s"),
    }
}

impl MemView {
    pub(super) fn open() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(load());
        });
        MemView {
            rx: Some(rx),
            data: None,
            error: None,
            started: Instant::now(),
            focus: None,
            sel: 0,
            back: Vec::new(),
            placed: Vec::new(),
            groups: Vec::new(),
            from: HashMap::new(),
            bloom: (0.0, 0.0),
            tween: None,
            search: None,
            hits: Vec::new(),
            area: Rect::default(),
            aspect: 2.0,
            legend: true,
        }
    }

    #[cfg(test)]
    fn with(g: Graph) -> Self {
        let mut v = Self::open_empty();
        v.arrive(prepare(g));
        v
    }

    #[cfg(test)]
    fn open_empty() -> Self {
        let mut v = Self::open();
        v.rx = None;
        v
    }

    /// Take the graph once it's read.
    pub(super) fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        match rx.try_recv() {
            Ok(Ok(d)) => {
                self.rx = None;
                self.arrive(d);
                true
            }
            Ok(Err(e)) => {
                self.rx = None;
                self.error = Some(e);
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                self.error = Some("reading memory stopped".into());
                true
            }
        }
    }

    fn arrive(&mut self, d: Loaded) {
        // Start on the project touched last.
        self.sel = d
            .sky
            .iter()
            .max_by_key(|s| d.g.nodes[s.node].last)
            .map_or(0, |s| s.node);
        self.data = Some(d);
        self.layout();
    }

    pub(super) fn busy(&self) -> Option<Duration> {
        if self.rx.is_some() {
            return Some(Duration::from_millis(120));
        }
        self.tween
            .filter(|t| t.elapsed().as_secs_f32() * 1000.0 < TWEEN_MS)
            .map(|_| Duration::from_millis(16))
    }

    fn shown(&self) -> Vec<(usize, f64, f64)> {
        let Some(d) = &self.data else {
            return Vec::new();
        };
        match self.focus {
            None => d.sky.iter().map(|s| (s.node, s.x, s.y)).collect(),
            Some(_) => self.placed.iter().map(|p| (p.node, p.x, p.y)).collect(),
        }
    }

    /// Gather the focus's links around it, or lay out the sky.
    fn layout(&mut self) {
        let old: HashMap<usize, (f64, f64)> = self.current_positions();
        self.bloom = old
            .get(&self.focus.unwrap_or(self.sel))
            .copied()
            .unwrap_or((0.0, 0.0));
        self.placed.clear();
        self.groups.clear();
        let Some(d) = &self.data else { return };
        if let Some(f) = self.focus {
            let (placed, groups) = gather(d, f, self.aspect);
            self.placed = placed;
            self.groups = groups;
        }
        self.from = old;
        self.tween = Some(Instant::now());
    }

    /// Where each shown node is now, mid-move included.
    fn current_positions(&self) -> HashMap<usize, (f64, f64)> {
        let t = self.progress();
        self.shown()
            .into_iter()
            .map(|(n, x, y)| {
                let (fx, fy) = self.from.get(&n).copied().unwrap_or(self.bloom);
                (n, (fx + (x - fx) * t, fy + (y - fy) * t))
            })
            .collect()
    }

    fn progress(&self) -> f64 {
        self.tween
            .map_or(1.0, |t| ease(t.elapsed().as_secs_f32() * 1000.0 / TWEEN_MS))
    }

    fn go(&mut self, to: usize) {
        if self.data.is_none() {
            return;
        }
        self.back.push((self.focus, self.sel));
        self.focus = Some(to);
        self.sel = to;
        self.layout();
    }

    fn back(&mut self) {
        let Some((f, s)) = self.back.pop() else {
            if self.focus.is_some() {
                self.back_to_sky();
            }
            return;
        };
        self.focus = f;
        self.sel = s;
        self.layout();
    }

    fn back_to_sky(&mut self) {
        let Some(d) = &self.data else { return };
        if let Some(f) = self.focus {
            self.sel = d.home[f].unwrap_or(self.sel);
        }
        self.focus = None;
        self.back.clear();
        self.layout();
    }

    /// Move the selection to the nearest thing in a direction on screen.
    fn step(&mut self, dx: f64, dy: f64) {
        let shown = self.shown();
        let Some(&(_, sx, sy)) = shown.iter().find(|s| s.0 == self.sel) else {
            if let Some(s) = shown.first() {
                self.sel = s.0;
            }
            return;
        };
        let best = shown
            .iter()
            .filter(|c| c.0 != self.sel)
            .filter_map(|&(n, x, y)| {
                let (vx, vy) = (x - sx, y - sy);
                let along = vx * dx + vy * dy;
                (along > 1e-6).then(|| (n, along + 2.2 * (vx * dy - vy * dx).abs()))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((n, _)) = best {
            self.sel = n;
        }
    }

    fn cycle(&mut self, by: isize) {
        let shown = self.shown();
        if shown.is_empty() {
            return;
        }
        let i = shown.iter().position(|s| s.0 == self.sel).unwrap_or(0) as isize;
        self.sel = shown[(i + by).rem_euclid(shown.len() as isize) as usize].0;
    }

    fn find(&mut self) {
        let Some(d) = &self.data else { return };
        let Some(s) = self.search.as_mut() else {
            return;
        };
        let q = s.query.to_lowercase();
        s.sel = 0;
        if q.trim().is_empty() {
            s.hits.clear();
            return;
        }
        let mut scored: Vec<(i64, usize)> =
            d.g.nodes
                .iter()
                .enumerate()
                .filter_map(|(i, n)| {
                    let label = n.label.to_lowercase();
                    let base = if n.kind.fine() { 0 } else { 40 };
                    let score = if let Some(at) = label.find(&q) {
                        200 - at as i64 - (label.len() as i64 / 8) + if at == 0 { 60 } else { 0 }
                    } else if n
                        .path
                        .as_deref()
                        .is_some_and(|p| p.to_lowercase().contains(&q))
                    {
                        90
                    } else if n.text.to_lowercase().contains(&q) {
                        30
                    } else {
                        return None;
                    };
                    Some((score + base, i))
                })
                .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(d.g.nodes[b.1].last.cmp(&d.g.nodes[a.1].last))
        });
        s.hits = scored.into_iter().take(60).map(|x| x.1).collect();
    }

    /// A key for the memory screen. Returns false when it should close.
    pub(super) fn key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(s) = self.search.as_mut() {
            match k.code {
                KeyCode::Esc => self.search = None,
                KeyCode::Enter => {
                    let hit = s.hits.get(s.sel).copied();
                    self.search = None;
                    if let Some(h) = hit {
                        self.go(h);
                    }
                }
                KeyCode::Down => s.sel = (s.sel + 1).min(s.hits.len().saturating_sub(1)),
                KeyCode::Up => s.sel = s.sel.saturating_sub(1),
                KeyCode::Backspace => {
                    s.query.pop();
                    self.find();
                }
                KeyCode::Char('u') if ctrl => {
                    s.query.clear();
                    self.find();
                }
                KeyCode::Char(c) if !ctrl => {
                    s.query.push(c);
                    self.find();
                }
                _ => {}
            }
            return true;
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.focus.is_some() {
                    self.back_to_sky();
                } else {
                    return false;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => self.step(-1.0, 0.0),
            KeyCode::Right | KeyCode::Char('l') => self.step(1.0, 0.0),
            KeyCode::Up | KeyCode::Char('k') => self.step(0.0, 1.0),
            KeyCode::Down | KeyCode::Char('j') => self.step(0.0, -1.0),
            KeyCode::Tab => self.cycle(1),
            KeyCode::BackTab => self.cycle(-1),
            KeyCode::Enter => {
                if Some(self.sel) != self.focus {
                    self.go(self.sel);
                }
            }
            KeyCode::Backspace => self.back(),
            KeyCode::Char('/') => {
                self.search = Some(Search {
                    query: String::new(),
                    hits: Vec::new(),
                    sel: 0,
                })
            }
            KeyCode::Char('o') => self.open_browser(),
            KeyCode::Char('r') => {
                let keep = (self.focus, self.sel);
                let legend = self.legend;
                *self = MemView::open();
                self.back.push(keep);
                self.legend = legend;
            }
            KeyCode::Char('?') => self.legend = !self.legend,
            _ => {}
        }
        true
    }

    fn open_browser(&self) {
        let Some(d) = &self.data else { return };
        let id = d.g.nodes.get(self.sel).map(|n| n.id.clone());
        let _ = crate::graph::open_page(&d.g, id.as_deref());
    }

    /// A click: select what's under it, focus it if it was selected.
    pub(super) fn click(&mut self, col: u16, row: u16) {
        let near = self
            .hits
            .iter()
            .map(|&(n, c, r)| {
                (
                    n,
                    (c as i32 - col as i32).abs() + 2 * (r as i32 - row as i32).abs(),
                )
            })
            .filter(|h| h.1 <= 3)
            .min_by_key(|h| h.1);
        if let Some((n, _)) = near {
            if n == self.sel && Some(n) != self.focus {
                self.go(n);
            } else {
                self.sel = n;
            }
        }
    }

    pub(super) fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.search.is_some() {
            return vec![("enter", "go"), ("↑↓", "choose"), ("esc", "cancel")];
        }
        let mut h = vec![
            ("enter", "focus"),
            ("arrows", "move"),
            ("/", "find"),
            ("o", "browser"),
        ];
        if !self.back.is_empty() {
            h.push(("bksp", "back"));
        }
        h.push(("esc", if self.focus.is_some() { "sky" } else { "close" }));
        h.push(("?", if self.legend { "hide legend" } else { "legend" }));
        h
    }
}

/// Lay legend items out in rows no wider than `width`, three spaces apart.
fn pack(items: Vec<Vec<Span<'static>>>, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut used = 0;
    for item in items {
        let w: usize = item.iter().map(|s| s.width()).sum();
        match rows.last_mut() {
            Some(row) if used + 3 + w <= width => {
                row.push(Span::raw("   "));
                row.extend(item);
                used += 3 + w;
            }
            _ => {
                rows.push(item);
                used = w;
            }
        }
    }
    rows
}

/// The focus in the middle and its links around it, in groups.
/// The focus's neighbours in groups, each in one only (by its strongest
/// link), in their order around the circle.
fn groups_of(d: &Loaded, f: usize) -> Vec<(u8, &'static str, Vec<usize>, Link)> {
    let g = &d.g;
    let fine_ok = matches!(g.nodes[f].kind, Kind::Session | Kind::Turn | Kind::Output);
    // One place per neighbour, by its strongest link.
    let mut best: HashMap<usize, (u8, &'static str, Link)> = HashMap::new();
    for &(o, e) in &d.adj[f] {
        if o == f || (g.nodes[o].kind.fine() && !fine_ok) {
            continue;
        }
        let edge = &g.edges[e];
        let out = edge.from == f;
        let (order, name) = group_of(g, o, edge.kind, out);
        let cur = best.entry(o).or_insert((order, name, edge.kind));
        if order < cur.0 {
            *cur = (order, name, edge.kind);
        }
    }
    let mut groups: Vec<(u8, &'static str, Vec<usize>, Link)> = Vec::new();
    for (o, (order, name, link)) in best {
        match groups.iter_mut().find(|x| x.1 == name) {
            Some(x) => x.2.push(o),
            None => groups.push((order, name, vec![o], link)),
        }
    }
    groups.sort_by_key(|x| x.0);
    for (_, name, members, _) in &mut groups {
        if *name == "turns" {
            members.sort_by_key(|&m| g.nodes[m].at);
        } else {
            members.sort_by_key(|&m| std::cmp::Reverse(g.nodes[m].last));
        }
    }
    groups
}

fn gather(d: &Loaded, f: usize, aspect: f64) -> (Vec<Placed>, Vec<Group>) {
    let groups = groups_of(d, f);
    let mut placed = vec![Placed {
        node: f,
        x: 0.0,
        y: 0.0,
        via: None,
        ring: 0,
        group: usize::MAX,
    }];
    let mut labels = Vec::new();
    let total: f64 = groups.iter().map(|x| (x.2.len() as f64).max(3.0)).sum();
    if total == 0.0 {
        return (placed, labels);
    }
    let gap = if groups.len() > 1 { 0.08 } else { 0.0 };
    let usable = std::f64::consts::TAU - gap * groups.len() as f64;
    // From the top, clockwise.
    let mut angle = std::f64::consts::FRAC_PI_2;
    let rx = aspect * 0.9;
    for (gi, (_, name, members, link)) in groups.iter().enumerate() {
        let span = usable * (members.len() as f64).max(3.0) / total;
        // Rings outward until everyone has a seat; seats about 0.045 apart.
        let mut left: &[usize] = members;
        let mut ring = 0;
        let mut outer = 0.5;
        while !left.is_empty() {
            let r = 0.5 + 0.17 * ring as f64;
            outer = r;
            let seats = ((span * r / 0.045).floor() as usize).max(1);
            let take = if ring >= 2 {
                left.len()
            } else {
                seats.min(left.len())
            };
            let (now, rest) = left.split_at(take);
            for (j, &m) in now.iter().enumerate() {
                let a = angle - span * (j as f64 + 0.5) / take as f64;
                let rr = if ring >= 2 {
                    r + 0.05 * (j % 3) as f64
                } else {
                    r
                };
                placed.push(Placed {
                    node: m,
                    x: a.cos() * rr * rx,
                    y: a.sin() * rr * 0.9,
                    via: Some(*link),
                    ring: ring as u8,
                    group: gi,
                });
            }
            left = rest;
            ring += 1;
        }
        let mid = angle - span / 2.0;
        let lr = (outer + 0.16).min(1.0);
        let text = group_text(name, members.len());
        labels.push(Group {
            text,
            x: mid.cos() * lr * rx,
            y: mid.sin() * lr * 0.9,
            from: angle,
            span,
            link: *link,
            members: members.len(),
        });
        angle -= span + gap;
    }
    (placed, labels)
}

impl App {
    /// The memory screen's keys, in the footer's style.
    pub(super) fn hint_line(&self, hints: &[(&str, &str)], width: u16) -> Line<'static> {
        let p = &self.pal;
        let mut spans = vec![Span::raw(" ")];
        let mut used = 1;
        for (k, l) in hints {
            let w = k.width() + 1 + l.width() + 3;
            if used + w > width as usize {
                break;
            }
            used += w;
            spans.push(Span::styled(k.to_string(), Style::new().fg(p.text)));
            spans.push(Span::styled(format!(" {l}"), Style::new().fg(p.dim)));
            spans.push(Span::raw("   "));
        }
        Line::from(spans)
    }

    fn kind_color(&self, k: Kind) -> Color {
        let p = &self.pal;
        match k {
            // As the browser view draws them: memory neutral, sessions
            // and their turns blue, briefs amber.
            Kind::Project | Kind::Index => p.text,
            Kind::File => mix(p.dim, p.text, 0.5, p.text),
            Kind::Note => p.dim,
            Kind::Session => p.accent,
            Kind::Handover => p.working,
            Kind::Turn => mix(p.base, p.accent, 0.6, p.accent),
            Kind::Output => p.muted,
        }
    }

    fn link_color(&self, l: Link) -> Color {
        let p = &self.pal;
        match l {
            Link::In | Link::Holds => p.muted,
            Link::Links => p.dim,
            Link::Wrote | Link::Continued => p.working,
            Link::Read | Link::Found => p.finished,
        }
    }

    /// The legend's rows: the kinds of thing shown, then the kinds of link,
    /// and in the sky what brightness and size mean. Only what's on screen.
    fn memory_legend(&self, d: &Loaded, v: &MemView, width: usize) -> Vec<Vec<Span<'static>>> {
        let p = &self.pal;
        let g = &d.g;
        let order = [
            Kind::Project,
            Kind::Index,
            Kind::File,
            Kind::Note,
            Kind::Session,
            Kind::Handover,
            Kind::Turn,
            Kind::Output,
        ];
        let mut kinds: Vec<Kind> = Vec::new();
        let mut links: Vec<Link> = Vec::new();
        match v.focus {
            None => {
                kinds.push(Kind::Project);
                for s in &d.sky {
                    for &(m, _, _) in &s.dust {
                        let k = g.nodes[m].kind;
                        if !kinds.contains(&k) {
                            kinds.push(k);
                        }
                    }
                }
            }
            Some(fo) => {
                for pl in v.placed.iter().filter(|pl| pl.node != fo) {
                    let k = g.nodes[pl.node].kind;
                    if !kinds.contains(&k) {
                        kinds.push(k);
                    }
                }
                for gr in &v.groups {
                    if !links.contains(&gr.link) {
                        links.push(gr.link);
                    }
                }
                // The selection's own links to the others shown.
                if v.sel != fo && v.placed.iter().any(|pl| pl.node == v.sel) {
                    for &(o, e) in &d.adj[v.sel] {
                        let l = g.edges[e].kind;
                        if o != fo && v.placed.iter().any(|x| x.node == o) && !links.contains(&l) {
                            links.push(l);
                        }
                    }
                }
            }
        }
        kinds.sort_by_key(|k| order.iter().position(|o| o == k));
        let word = |k: Kind| match k {
            Kind::Index => "memory index",
            Kind::Handover => "brief",
            Kind::Output => "output",
            k => k.word(),
        };
        let mut items: Vec<Vec<Span<'static>>> = kinds
            .iter()
            .map(|&k| {
                vec![
                    Span::styled(Self::glyph(k), Style::new().fg(self.kind_color(k))),
                    Span::styled(format!(" {}", word(k)), Style::new().fg(p.muted)),
                ]
            })
            .collect();
        let mut rows = pack(std::mem::take(&mut items), width);
        // One swatch per colour, naming the links it stands for.
        let classes: [&[Link]; 4] = [
            &[Link::In, Link::Holds],
            &[Link::Links],
            &[Link::Wrote, Link::Continued],
            &[Link::Read, Link::Found],
        ];
        for members in classes {
            let here: Vec<&str> = members
                .iter()
                .filter(|l| links.contains(l))
                .map(|l| match l {
                    Link::In => "in project",
                    Link::Holds => "holds",
                    Link::Links => "links",
                    Link::Wrote => "wrote",
                    Link::Continued => "continued",
                    Link::Read => "read",
                    Link::Found => "found",
                })
                .collect();
            if let Some(&first) = members.first()
                && !here.is_empty()
            {
                items.push(vec![
                    Span::styled("──", Style::new().fg(self.link_color(first))),
                    Span::styled(format!(" {}", here.join(" · ")), Style::new().fg(p.muted)),
                ]);
            }
        }
        if v.focus.is_none() {
            items.push(vec![
                Span::styled("──", Style::new().fg(p.dim)),
                Span::styled(" shared links".to_string(), Style::new().fg(p.muted)),
            ]);
        }
        rows.extend(pack(items, width));
        if v.focus.is_none() {
            let (muted, file) = (Style::new().fg(p.muted), self.kind_color(Kind::File));
            rows.extend(pack(
                vec![
                    vec![
                        Span::styled("⣿", Style::new().fg(mix(p.base, file, 0.3, file))),
                        Span::styled("⣿", Style::new().fg(file)),
                        Span::styled(" brighter is newer", muted),
                    ],
                    vec![
                        Span::styled("∙◉", Style::new().fg(p.text)),
                        Span::styled(" bigger holds more", muted),
                    ],
                ],
                width,
            ));
        }
        rows
    }

    fn glyph(k: Kind) -> &'static str {
        match k {
            Kind::Project => "◉",
            Kind::Index => "◎",
            Kind::File => "•",
            Kind::Note => "▪",
            Kind::Session => "●",
            Kind::Handover => "◆",
            Kind::Turn => "·",
            Kind::Output => "▫",
        }
    }

    pub(super) fn draw_memory(&mut self, f: &mut Frame, area: Rect) {
        let p = &self.pal;
        let (base, raised) = (p.base, p.raised);
        f.render_widget(Block::new().style(Style::new().bg(base)), area);
        let Some(mut v) = self.memory.take() else {
            return;
        };
        let panel_w = if area.width >= 100 {
            (area.width * 34 / 100).clamp(34, 56)
        } else {
            0
        };
        let [graph, gap, panel] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(if panel_w > 0 { 1 } else { 0 }),
            Constraint::Length(panel_w),
        ])
        .areas(area);
        if panel_w > 0 {
            divider(f, gap, self.pal.frame, base);
            f.render_widget(Block::new().style(Style::new().bg(raised)), panel);
        }
        let [head, canvas] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(graph);
        // Pixels are about square in braille: two across, four down a cell.
        let aspect = (canvas.width as f64 * 2.0) / (canvas.height.max(1) as f64 * 4.0);
        if canvas != v.area {
            v.area = canvas;
            if (aspect - v.aspect).abs() > 0.01 {
                v.aspect = aspect;
                let t = v.tween;
                v.layout();
                v.tween = t.or(Some(Instant::now() - Duration::from_secs(1)));
            }
        }
        self.draw_memory_head(f, head, &v);
        match (&v.data, &v.error) {
            (_, Some(e)) => {
                let msg = format!("couldn't read memory: {e}");
                f.render_widget(
                    Paragraph::new(msg)
                        .style(Style::new().fg(self.pal.attention_word))
                        .alignment(ratatui::layout::Alignment::Center),
                    Rect {
                        y: canvas.y + canvas.height / 2,
                        height: 1,
                        ..canvas
                    },
                );
            }
            (None, _) => {
                let dots = ".".repeat(1 + (v.started.elapsed().as_millis() / 300 % 3) as usize);
                let msg = format!("reading memory{dots:<3}");
                f.render_widget(
                    Paragraph::new(msg)
                        .style(Style::new().fg(self.pal.muted))
                        .alignment(ratatui::layout::Alignment::Center),
                    Rect {
                        y: canvas.y + canvas.height / 2,
                        height: 1,
                        ..canvas
                    },
                );
            }
            (Some(_), None) => {
                self.draw_memory_graph(f, canvas, &mut v);
                if panel_w > 0 {
                    self.draw_memory_panel(
                        f,
                        Rect {
                            x: panel.x + 2,
                            width: panel.width.saturating_sub(3),
                            y: panel.y + 1,
                            height: panel.height.saturating_sub(1),
                        },
                        &v,
                    );
                }
            }
        }
        self.memory = Some(v);
    }

    fn draw_memory_head(&self, f: &mut Frame, r: Rect, v: &MemView) {
        let p = &self.pal;
        let mut spans = vec![Span::styled("  memory", Style::new().fg(p.accent))];
        if let Some(s) = &v.search {
            spans.push(Span::styled("   find ", Style::new().fg(p.muted)));
            spans.push(Span::styled(
                format!("{}▏", s.query),
                Style::new().fg(p.text),
            ));
        } else if let Some(d) = &v.data {
            let mut trail = Vec::new();
            if let Some(fo) = v.focus {
                if let Some(h) = d.home[fo].filter(|&h| h != fo) {
                    trail.push(d.g.nodes[h].label.clone());
                }
                trail.push(d.g.nodes[fo].label.clone());
            } else {
                trail.push("every project".into());
            }
            for t in trail {
                spans.push(Span::styled(" › ", Style::new().fg(p.faint)));
                spans.push(Span::styled(t, Style::new().fg(p.dim)));
            }
        }
        let used: usize = spans.iter().map(|s| s.width()).sum();
        if let Some(d) = &v.data {
            let c = |k| d.g.count(k);
            let counts = format!(
                "{} memory files · {} sessions · {} briefs · {} projects  ",
                c(Kind::File) + c(Kind::Index),
                c(Kind::Session),
                c(Kind::Handover),
                c(Kind::Project)
            );
            if used + counts.width() + 2 <= r.width as usize {
                spans.push(Span::raw(
                    " ".repeat(r.width as usize - used - counts.width()),
                ));
                spans.push(Span::styled(counts, Style::new().fg(p.muted)));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), r);
    }

    fn draw_memory_graph(&self, f: &mut Frame, area: Rect, v: &mut MemView) {
        let Some(d) = &v.data else { return };
        let p = &self.pal;
        let g = &d.g;
        let t = v.progress();
        let now = now_ms();
        let (wpx, hpx) = (area.width as f64 * 2.0, area.height as f64 * 4.0);
        let a = v.aspect;
        // The legend's rows along the top, kept clear (the sky spreads to the
        // edges); `?` gives them back.
        let band = if v.legend && v.focus.is_none() {
            let rows = self
                .memory_legend(d, v, (area.width as usize).saturating_sub(4).min(64))
                .len();
            if rows == 0 {
                0.0
            } else {
                2.0 * (rows + 2) as f64 / area.height.max(1) as f64
            }
        } else {
            0.0
        };
        // The sky is fitted to the area; a focus is laid out in it already.
        let fit = if v.focus.is_none() {
            let (mut lo_x, mut hi_x, mut lo_y, mut hi_y) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
            for s in &d.sky {
                lo_x = lo_x.min(s.x - s.r);
                hi_x = hi_x.max(s.x + s.r);
                lo_y = lo_y.min(s.y - s.r);
                hi_y = hi_y.max(s.y + s.r);
            }
            let (cx, cy) = ((lo_x + hi_x) / 2.0, (lo_y + hi_y) / 2.0);
            let s =
                (2.0 * a * 0.92 / (hi_x - lo_x).max(1e-3)).min(2.0 * 0.9 / (hi_y - lo_y).max(1e-3));
            // Fitted whole it fills one way only (on a 132x40 screen, half the
            // height), so the projects spread apart to fill the other too.
            // Their discs keep their size and shape.
            let spread = |half: f64, off: &dyn Fn(&Star) -> f64| {
                d.sky
                    .iter()
                    .filter(|st| off(st).abs() > 1e-6)
                    .map(|st| (half - st.r * s) / off(st).abs())
                    .fold(f64::MAX, f64::min)
                    .max(s)
            };
            let kx = spread(a * 0.92, &|st| st.x - cx);
            let ky = spread(0.9 - band / 2.0, &|st| st.y - cy);
            Some((
                cx,
                cy,
                kx.min(ky * 1.8).min(s * 2.5),
                ky.min(kx * 1.8).min(s * 2.5),
                s,
            ))
        } else {
            None
        };
        let world = |x: f64, y: f64| -> (f64, f64) {
            match fit {
                Some((cx, cy, kx, ky, _)) => ((x - cx) * kx, (y - cy) * ky - band / 2.0),
                None => (x, y),
            }
        };
        // A disc's member, around its project wherever that went.
        let member = |st: &Star, x: f64, y: f64| -> (f64, f64) {
            let (ox, oy) = world(st.x, st.y);
            let s = fit.map_or(1.0, |f| f.4);
            (ox + (x - st.x) * s, oy + (y - st.y) * s)
        };
        let px =
            |x: f64, y: f64| -> (f64, f64) { ((x + a) / (2.0 * a) * wpx, (y + 1.0) / 2.0 * hpx) };
        let cell = |x: f64, y: f64| -> (u16, u16) {
            let (cx, cy) = px(x, y);
            let col = ((cx / wpx) * area.width as f64)
                .floor()
                .clamp(0.0, area.width as f64 - 1.0) as u16;
            let row = ((1.0 - cy / hpx) * area.height as f64)
                .floor()
                .clamp(0.0, area.height as f64 - 1.0) as u16;
            (area.x + col, area.y + row)
        };
        // Where each shown node is this frame.
        let at: HashMap<usize, (f64, f64)> = v
            .shown()
            .into_iter()
            .map(|(n, x, y)| {
                let (x, y) = world(x, y);
                let (fx, fy) = v
                    .from
                    .get(&n)
                    .map(|&(x, y)| world(x, y))
                    .unwrap_or_else(|| world(v.bloom.0, v.bloom.1));
                // The sky doesn't move: it's fitted, and fades in.
                if v.focus.is_none() {
                    (n, (x, y))
                } else {
                    (n, (fx + (x - fx) * t, fy + (y - fy) * t))
                }
            })
            .collect();
        let fade = |c: Color, k: f64| mix(p.base, c, k.clamp(0.0, 1.0) as f32, c);
        let recency = |last: i64| {
            let days = (now - last) as f64 / 86_400_000.0;
            (1.0 - (days / 45.0)).clamp(0.28, 1.0)
        };
        let sel = v.sel;
        let focus = v.focus;
        let sky = focus.is_none();
        let reveal = if sky { t } else { 1.0 };
        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .background_color(p.base)
            .x_bounds([0.0, wpx])
            .y_bounds([0.0, hpx])
            .paint(|ctx| {
                if sky {
                    for &(ia, ib, w) in &d.lanes {
                        let (sa, sb) = (&d.sky[ia], &d.sky[ib]);
                        let hot = sa.node == sel || sb.node == sel;
                        // One link between two projects is noise until you look at one.
                        if w < 2 && !hot {
                            continue;
                        }
                        // Edge to edge, not through the discs.
                        let (x1, y1) = px(world(sa.x, sa.y).0, world(sa.x, sa.y).1);
                        let (x2, y2) = px(world(sb.x, sb.y).0, world(sb.x, sb.y).1);
                        let disc = |r: f64| r * fit.map_or(1.0, |f| f.4) / (2.0 * a) * wpx + 2.0;
                        let (dx, dy) = (x2 - x1, y2 - y1);
                        let len = (dx * dx + dy * dy).sqrt();
                        let (ra, rb) = (disc(sa.r), disc(sb.r));
                        if len <= ra + rb {
                            continue;
                        }
                        let (ux, uy) = (dx / len, dy / len);
                        let (x1, y1, x2, y2) =
                            (x1 + ux * ra, y1 + uy * ra, x2 - ux * rb, y2 - uy * rb);
                        let k = if hot {
                            0.75
                        } else {
                            (0.06 + 0.04 * (w as f64).ln()).min(0.22)
                        } * reveal;
                        ctx.draw(&CLine {
                            x1,
                            y1,
                            x2,
                            y2,
                            color: fade(if hot { p.accent } else { p.dim }, k),
                        });
                    }
                    ctx.layer();
                    // Each project's disc of members, by kind, bright where recent.
                    let mut by_color: HashMap<(u8, u8, u8), Vec<(f64, f64)>> = HashMap::new();
                    for s in &d.sky {
                        let hot = s.node == sel;
                        for &(m, x, y) in &s.dust {
                            let (wx, wy) = member(s, x, y);
                            let k = recency(g.nodes[m].last) * if hot { 1.0 } else { 0.8 } * reveal;
                            if let Color::Rgb(r, gg, b) = fade(self.kind_color(g.nodes[m].kind), k)
                            {
                                by_color.entry((r, gg, b)).or_default().push(px(wx, wy));
                            }
                        }
                        if hot {
                            let (cx, cy) = world(s.x, s.y);
                            let (x, y) = px(cx, cy);
                            let r = s.r * fit.map_or(1.0, |f| f.4) / (2.0 * a) * wpx;
                            ctx.draw(&Circle {
                                x,
                                y,
                                radius: r + 3.0,
                                color: fade(p.accent, 0.8),
                            });
                        }
                    }
                    for ((r, gg, b), pts) in &by_color {
                        ctx.draw(&Points {
                            coords: pts,
                            color: Color::Rgb(*r, *gg, *b),
                        });
                    }
                } else if let Some(fo) = focus {
                    let &(cx, cy) = at.get(&fo).unwrap_or(&(0.0, 0.0));
                    let (x0, y0) = px(cx, cy);
                    let rx = a * 0.9;
                    // Each group: one spoke out to an arc its members sit around.
                    for (gi, gr) in v.groups.iter().enumerate() {
                        let c = self.link_color(gr.link);
                        // A few: a line to each.
                        if gr.members <= 4 {
                            for pl in v.placed.iter().filter(|p| p.group == gi) {
                                if let Some(&(x, y)) = at.get(&pl.node) {
                                    let (x1, y1) = px(x, y);
                                    ctx.draw(&CLine {
                                        x1: x0,
                                        y1: y0,
                                        x2: x1,
                                        y2: y1,
                                        color: fade(c, 0.4),
                                    });
                                }
                            }
                            continue;
                        }
                        let arc = 0.36;
                        let steps = ((gr.span * 40.0).ceil() as usize).max(2);
                        let point = |th: f64| {
                            px(cx + th.cos() * arc * rx * t, cy + th.sin() * arc * 0.9 * t)
                        };
                        let mid = gr.from - gr.span / 2.0;
                        let (mx, my) = point(mid);
                        // A small group's arc hugs its members.
                        let span = gr.span.min(0.3 + 0.04 * gr.members as f64);
                        let start = mid + span / 2.0;
                        ctx.draw(&CLine {
                            x1: x0,
                            y1: y0,
                            x2: mx,
                            y2: my,
                            color: fade(c, 0.32),
                        });
                        let mut last = point(start);
                        for i in 1..=steps {
                            let next = point(start - span * i as f64 / steps as f64);
                            ctx.draw(&CLine {
                                x1: last.0,
                                y1: last.1,
                                x2: next.0,
                                y2: next.1,
                                color: fade(c, 0.45),
                            });
                            last = next;
                        }
                    }
                    // The selection: its own line in, and its links to the others shown.
                    if let Some(pl) = v.placed.iter().find(|p| p.node == sel && p.node != fo)
                        && let Some(&(x, y)) = at.get(&sel)
                    {
                        let (x1, y1) = px(x, y);
                        let th = y.atan2(x / rx);
                        let (ax, ay) = px(cx + th.cos() * 0.36 * rx, cy + th.sin() * 0.36 * 0.9);
                        let c = self.link_color(pl.via.unwrap_or(Link::In));
                        ctx.draw(&CLine {
                            x1: x0,
                            y1: y0,
                            x2: ax,
                            y2: ay,
                            color: c,
                        });
                        ctx.draw(&CLine {
                            x1: ax,
                            y1: ay,
                            x2: x1,
                            y2: y1,
                            color: c,
                        });
                        for &(o, e) in &d.adj[sel] {
                            if o != fo
                                && let Some(&(ox, oy)) = at.get(&o)
                            {
                                let (x2, y2) = px(ox, oy);
                                ctx.draw(&CLine {
                                    x1,
                                    y1,
                                    x2,
                                    y2,
                                    color: fade(self.link_color(g.edges[e].kind), 0.7),
                                });
                            }
                        }
                    }
                    ctx.layer();
                    // Faint rings of dust for the members, so a crowd reads as a shape.
                    for pl in &v.placed {
                        if pl.node == fo {
                            continue;
                        }
                        if let Some(&(x, y)) = at.get(&pl.node) {
                            let pt = [px(x, y)];
                            ctx.draw(&Points {
                                coords: &pt,
                                color: fade(self.kind_color(g.nodes[pl.node].kind), 0.5),
                            });
                        }
                    }
                }
            });
        f.render_widget(canvas, area);

        // Marks and labels, straight into the cells, over the lines.
        let buf = f.buffer_mut();
        let mut taken = vec![false; area.width as usize * area.height as usize];
        let free = |taken: &Vec<bool>, x: u16, y: u16, w: u16| -> bool {
            if x < area.x || y < area.y || x + w > area.x + area.width || y >= area.y + area.height
            {
                return false;
            }
            (0..w).all(|i| {
                !taken[(y - area.y) as usize * area.width as usize + (x - area.x + i) as usize]
            })
        };
        let take = |taken: &mut Vec<bool>, x: u16, y: u16, w: u16| {
            for i in 0..w {
                let (cx, cy) = (x + i, y);
                if cx >= area.x
                    && cx < area.x + area.width
                    && cy >= area.y
                    && cy < area.y + area.height
                {
                    taken[(cy - area.y) as usize * area.width as usize + (cx - area.x) as usize] =
                        true;
                }
            }
        };
        let mut hits = Vec::new();
        let mut marks: Vec<(usize, u16, u16)> = Vec::new();
        if sky {
            for s in &d.sky {
                let (x, y) = world(s.x, s.y);
                let (c, r) = cell(x, y);
                marks.push((s.node, c, r));
            }
        } else {
            for pl in &v.placed {
                if let Some(&(x, y)) = at.get(&pl.node) {
                    let (c, r) = cell(x, y);
                    marks.push((pl.node, c, r));
                }
            }
        }
        for &(n, c, r) in &marks {
            let k = g.nodes[n].kind;
            let hot = n == sel;
            let centre = Some(n) == focus;
            let glyph = if sky {
                if d.sky
                    .iter()
                    .find(|s| s.node == n)
                    .is_some_and(|s| s.dust.len() >= 12)
                {
                    "◉"
                } else {
                    "∙"
                }
            } else if centre {
                "◉"
            } else {
                Self::glyph(k)
            };
            let color = if sky {
                fade(p.text, 0.55 + 0.45 * recency(g.nodes[n].last))
            } else {
                self.kind_color(k)
            };
            let cellv = &mut buf[(c, r)];
            cellv.set_symbol(glyph);
            cellv.set_fg(if hot { p.text } else { color });
            if hot {
                cellv.set_bg(p.selection);
            }
            take(&mut taken, c, r, 1);
            hits.push((n, c, r));
        }
        // Labels: the focus and the selection first, then the groups, then
        // what fits, biggest and latest first.
        let mut order: Vec<usize> = marks.iter().map(|m| m.0).collect();
        order.sort_by_key(|&n| {
            let node = &g.nodes[n];
            let pri = if n == sel {
                0
            } else if Some(n) == focus {
                1
            } else {
                2
            };
            (
                pri,
                std::cmp::Reverse(if sky {
                    node.size
                } else {
                    node.last.max(0) as u64
                }),
            )
        });
        let pos: HashMap<usize, (u16, u16)> = marks.iter().map(|m| (m.0, (m.1, m.2))).collect();
        let place = |buf: &mut ratatui::buffer::Buffer,
                     taken: &mut Vec<bool>,
                     text: &str,
                     c: u16,
                     r: u16,
                     right: bool,
                     style: Style|
         -> bool {
            // Up to 28 cells, fewer where the edge is nearer; under 8 isn't worth it.
            let room = if right {
                (area.x + area.width).saturating_sub(c + 2)
            } else {
                c.saturating_sub(area.x + 1)
            } as usize;
            let max = room.min(28);
            if max < 8 && text.width() > max {
                return false;
            }
            let text: String = if text.width() > max {
                let mut t = String::new();
                for ch in text.chars() {
                    if t.width() + ch.to_string().width() + 1 > max {
                        break;
                    }
                    t.push(ch);
                }
                format!("{t}…")
            } else {
                text.to_string()
            };
            let w = text.width() as u16;
            let x = if right {
                c + 2
            } else {
                c.saturating_sub(w + 1)
            };
            if !free(taken, x, r, w) {
                return false;
            }
            buf.set_string(x, r, &text, style);
            take(taken, x.saturating_sub(1), r, w + 2);
            true
        };
        // The cells the selection's ring passes through: no other label
        // crosses it.
        let ring: Vec<(u16, u16)> = d
            .sky
            .iter()
            .filter(|s| sky && s.node == sel)
            .flat_map(|s| {
                let (x, y) = px(world(s.x, s.y).0, world(s.x, s.y).1);
                let r = s.r * fit.map_or(1.0, |f| f.4) / (2.0 * a) * wpx + 3.0;
                (0..72).map(move |i| {
                    let th = i as f64 * std::f64::consts::TAU / 72.0;
                    let (qx, qy) = (x + r * th.cos(), y + r * th.sin());
                    let col = (qx / wpx * area.width as f64)
                        .floor()
                        .clamp(0.0, area.width as f64 - 1.0) as u16;
                    let row = ((1.0 - qy / hpx) * area.height as f64)
                        .floor()
                        .clamp(0.0, area.height as f64 - 1.0) as u16;
                    (area.x + col, area.y + row)
                })
            })
            .collect();
        let mut labelled = 0;
        let budget = if sky { 24 } else { 26 };
        let outer: std::collections::HashSet<usize> = v
            .placed
            .iter()
            .filter(|p| p.ring > 0)
            .map(|p| p.node)
            .collect();
        // Group names sit in the outer ring, before the nodes claim room.
        if !sky {
            for gr in &v.groups {
                let (c, r) = cell(gr.x, gr.y);
                let w = gr.text.width() as u16;
                let x = c.saturating_sub(w / 2).max(area.x);
                if free(&taken, x, r, w) {
                    buf.set_string(
                        x,
                        r,
                        &gr.text,
                        Style::new()
                            .fg(fade(p.muted, t))
                            .add_modifier(Modifier::ITALIC),
                    );
                    take(&mut taken, x, r, w);
                }
            }
        }
        for n in order {
            let Some(&(c, r)) = pos.get(&n) else { continue };
            let hot = n == sel;
            let centre = Some(n) == focus;
            if !hot && !centre && (labelled >= budget || outer.contains(&n)) {
                continue;
            }
            let node = &g.nodes[n];
            let style = if hot {
                Style::new()
                    .fg(p.text)
                    .bg(p.selection)
                    .add_modifier(Modifier::BOLD)
            } else if centre {
                Style::new().fg(p.text).add_modifier(Modifier::BOLD)
            } else if sky {
                Style::new().fg(fade(p.dim, 0.6 + 0.4 * recency(node.last)))
            } else {
                Style::new().fg(fade(p.dim, 0.9))
            };
            let right = c >= area.x + area.width / 2 || centre;
            // Outward only, so labels never crowd the middle; the selection may go either way.
            if place(buf, &mut taken, &node.label, c, r, right, style)
                || ((hot || sky) && place(buf, &mut taken, &node.label, c, r, !right, style))
            {
                labelled += 1;
            }
            if hot {
                for &(rc, rr) in &ring {
                    take(&mut taken, rc, rr, 1);
                }
            }
        }
        if v.legend {
            let rows = self.memory_legend(d, v, (area.width as usize).saturating_sub(4).min(64));
            let cw = rows
                .iter()
                .map(|r| r.iter().map(|s| s.width()).sum::<usize>())
                .max()
                .unwrap_or(0) as u16;
            // A blank row either side keeps it off the header and the footer.
            let (bw, bh) = (cw + 2, rows.len() as u16 + 2);
            if !rows.is_empty() && bw + 4 <= area.width && bh + 4 <= area.height {
                let (left, right) = (area.x, area.x + area.width - bw);
                let (top, bottom) = (area.y, area.y + area.height - bh);
                // The quietest corner, bottom left when they're alike.
                let busy = |&(x, y): &(u16, u16)| {
                    (y..y + bh)
                        .flat_map(|r| (x..x + bw).map(move |c| (c, r)))
                        .filter(|&(c, r)| {
                            taken[(r - area.y) as usize * area.width as usize
                                + (c - area.x) as usize]
                        })
                        .count()
                };
                let corners = [(left, bottom), (right, bottom), (left, top), (right, top)];
                let &(x, y) = corners
                    .iter()
                    .min_by_key(|c| busy(c))
                    .unwrap_or(&corners[0]);
                for r in y..y + bh {
                    for c in x..x + bw {
                        buf[(c, r)].reset();
                        buf[(c, r)].set_bg(p.base);
                    }
                }
                for (i, row) in rows.into_iter().enumerate() {
                    buf.set_line(x + 1, y + 1 + i as u16, &Line::from(row), cw);
                }
                hits.retain(|&(_, c, r)| !(c >= x && c < x + bw && r >= y && r < y + bh));
            }
        }
        v.hits = hits;
    }

    fn draw_memory_panel(&self, f: &mut Frame, r: Rect, v: &MemView) {
        let Some(d) = &v.data else { return };
        let p = &self.pal;
        let g = &d.g;
        let w = r.width as usize;
        let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
        if let Some(s) = &v.search {
            lines.push(vec![Span::styled(
                if s.query.is_empty() {
                    "type to find a project, file, session or turn".to_string()
                } else {
                    format!("{} found", s.hits.len())
                },
                Style::new().fg(p.muted),
            )]);
            lines.push(Vec::new());
            for (i, &h) in s
                .hits
                .iter()
                .enumerate()
                .take(r.height.saturating_sub(2) as usize)
            {
                let n = &g.nodes[h];
                let hot = i == s.sel;
                let bg = if hot { p.selection } else { p.raised };
                let mut l = vec![Span::styled(
                    format!("{} ", Self::glyph(n.kind)),
                    Style::new().fg(self.kind_color(n.kind)).bg(bg),
                )];
                let room = w.saturating_sub(2);
                let label: String = if n.label.width() > room {
                    format!(
                        "{}…",
                        n.label
                            .chars()
                            .take(room.saturating_sub(1))
                            .collect::<String>()
                    )
                } else {
                    n.label.clone()
                };
                l.push(Span::styled(
                    format!("{label:<room$}"),
                    Style::new().fg(if hot { p.text } else { p.dim }).bg(bg),
                ));
                lines.push(l);
            }
            f.render_widget(
                Paragraph::new(Text::from(
                    lines.into_iter().map(Line::from).collect::<Vec<_>>(),
                )),
                r,
            );
            return;
        }
        let Some(n) = g.nodes.get(v.sel) else { return };
        let now = now_ms();
        lines.push(vec![
            Span::styled(
                format!("{} ", Self::glyph(n.kind)),
                Style::new().fg(self.kind_color(n.kind)),
            ),
            Span::styled(n.kind.word().to_string(), Style::new().fg(p.muted)),
        ]);
        lines.extend(
            wrap_spans(
                &[(
                    n.label.clone(),
                    Style::new().fg(p.text).add_modifier(Modifier::BOLD),
                )],
                w,
                w,
            )
            .into_iter()
            .take(3),
        );
        let mut meta: Vec<(String, Style)> = Vec::new();
        let sep = || (" · ".to_string(), Style::new().fg(p.muted));
        if let Some(h) = d.home[v.sel].filter(|&h| h != v.sel) {
            meta.push((g.nodes[h].label.clone(), Style::new().fg(p.dim)));
        }
        let when = |ms: i64| match ago(now - ms) {
            a if a == "now" => "just now".to_string(),
            a => format!("{a} ago"),
        };
        match n.kind {
            Kind::Session => {
                meta.extend([
                    sep(),
                    (plural(n.size as usize, "turn"), Style::new().fg(p.dim)),
                ]);
                if n.at > 0 {
                    // When it started, and for how long it ran.
                    let ran = ago(n.last - n.at);
                    let span = if ran == "now" {
                        format!("started {}", when(n.at))
                    } else {
                        format!("started {}, ran {ran}", when(n.at))
                    };
                    meta.extend([sep(), (span, Style::new().fg(p.muted))]);
                }
            }
            Kind::Project => {
                meta.push((
                    plural(n.size as usize, "entry").replace("entrys", "entries"),
                    Style::new().fg(p.dim),
                ));
                meta.extend([
                    sep(),
                    (format!("last {}", when(n.last)), Style::new().fg(p.muted)),
                ]);
            }
            _ if n.at > 0 => meta.extend([sep(), (when(n.at), Style::new().fg(p.muted))]),
            _ => {}
        }
        if meta.first().is_some_and(|m| m.0 == " · ") {
            meta.remove(0);
        }
        lines.extend(wrap_chips(&meta, w, w));
        lines.push(Vec::new());
        // What it's linked to, grouped and counted as the graph does.
        let groups = groups_of(d, v.sel);
        let text_room = (r.height as usize)
            .saturating_sub(lines.len() + groups.len().min(8) * 4 + 3)
            .min(10);
        if !n.text.is_empty() && text_room >= 2 {
            let body: Vec<Vec<Span<'static>>> = n
                .text
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("---"))
                .flat_map(|l| wrap_spans(&[(l.trim().to_string(), Style::new().fg(p.dim))], w, w))
                .take(text_room)
                .collect();
            lines.extend(body);
            lines.push(Vec::new());
        }
        // Each group: its name and count, then its latest few, one a line.
        let room = (r.height as usize).saturating_sub(lines.len() + 3);
        let heads = groups.len().min(8);
        let per = room
            .saturating_sub(heads * 2)
            .checked_div(heads)
            .map_or(0, |p| p.clamp(1, 3));
        for (_, name, members, _) in groups.iter().take(8) {
            let mut members = members.clone();
            members.sort_by_key(|&m| std::cmp::Reverse(g.nodes[m].last));
            lines.push(vec![Span::styled(
                group_text(name, members.len()),
                Style::new().fg(p.muted),
            )]);
            for &m in members.iter().take(per) {
                let o = &g.nodes[m];
                let room = w.saturating_sub(2);
                let label: String = if o.label.width() > room {
                    format!(
                        "{}…",
                        o.label
                            .chars()
                            .take(room.saturating_sub(1))
                            .collect::<String>()
                    )
                } else {
                    o.label.clone()
                };
                lines.push(vec![
                    Span::styled(
                        format!("{} ", Self::glyph(o.kind)),
                        Style::new().fg(self.kind_color(o.kind)),
                    ),
                    Span::styled(label, Style::new().fg(p.dim)),
                ]);
            }
            if members.len() > per {
                lines.push(vec![Span::styled(
                    format!("  and {} more", members.len() - per),
                    Style::new().fg(p.muted),
                )]);
            }
            lines.push(Vec::new());
        }
        lines.push(Vec::new());
        let mut id: Vec<(String, Style)> = Vec::new();
        if let Some(e) = &n.entry {
            id.push((format!("id {e}"), Style::new().fg(p.muted)));
        }
        if n.kind == Kind::Session {
            if !id.is_empty() {
                id.push(sep());
            }
            id.push((
                format!("toomux jump {}", &n.id[2..10.min(n.id.len())]),
                Style::new().fg(p.muted),
            ));
        }
        if let Some(path) = &n.path {
            if !id.is_empty() {
                id.push(sep());
            }
            id.push((tilde(path), Style::new().fg(p.muted)));
        }
        lines.extend(wrap_chips(&id, w, w));
        let lines: Vec<Line> = lines
            .into_iter()
            .take(r.height as usize)
            .map(Line::from)
            .collect();
        f.render_widget(Paragraph::new(Text::from(lines)), r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Entry;

    fn graph() -> Graph {
        let e = |i: i64, scope: &str, source: &str, content: &str| Entry {
            id: format!("{i:02x}{}", "0".repeat(62)),
            scope: scope.into(),
            source: source.into(),
            content: content.into(),
            created_ms: i,
            superseded_by: None,
        };
        let sid = "0ff6405c-c71f-4a6a-8d59-de4d9a0cca4c";
        let entries = vec![
            e(
                1,
                "project:/w/toomux",
                "memory: /m/voyage.md",
                "voyage.md\n\n---\nname: voyage\n---\nsee [[release]]",
            ),
            e(
                2,
                "project:/w/toomux",
                "memory: /m/release.md",
                "release.md\n\ngo public later",
            ),
            e(
                3,
                "project:/w/toomux",
                &format!("session:{sid}#1"),
                "You asked: tune tiers\n\nOutcome: done",
            ),
            e(
                4,
                "project:/w/toomux",
                &format!("session:{sid}#2"),
                "You asked: the ship\n\nOutcome: drawn",
            ),
            e(
                0,
                "project:/w/atlas",
                "memory: /c/icons.md",
                "icons.md\n\niconoir",
            ),
        ];
        crate::graph::assemble(
            &entries,
            &[],
            &HashMap::new(),
            &crate::graph::Titles::new(),
            10,
        )
    }

    #[test]
    fn the_sky_opens_on_projects_and_focusing_gathers_links_in_groups() {
        let mut v = MemView::with(graph());
        v.tween = None;
        let d = v.data.as_ref().unwrap();
        assert_eq!(d.sky.len(), 2, "one star per project");
        let toomux = d.g.get("p:/w/toomux").unwrap();
        assert_eq!(v.sel, toomux, "starts on the project touched last");
        assert_eq!(
            d.sky.iter().find(|s| s.node == toomux).unwrap().dust.len(),
            3,
            "two files and a session, no turns"
        );
        v.key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(v.focus, Some(toomux));
        let names: Vec<&str> = v.groups.iter().map(|g| g.text.as_str()).collect();
        assert!(
            names.contains(&"memory files 2") && names.contains(&"session"),
            "{names:?}"
        );
        // Into the session: its turns come with it.
        let s = v
            .data
            .as_ref()
            .unwrap()
            .g
            .get("s:0ff6405c-c71f-4a6a-8d59-de4d9a0cca4c")
            .unwrap();
        v.sel = s;
        v.key(KeyEvent::from(KeyCode::Enter));
        let names: Vec<&str> = v.groups.iter().map(|g| g.text.as_str()).collect();
        assert!(
            names.contains(&"turns 2") && names.contains(&"project"),
            "{names:?}"
        );
        v.key(KeyEvent::from(KeyCode::Backspace));
        assert_eq!(v.focus, Some(toomux), "back to the project");
        assert!(
            v.key(KeyEvent::from(KeyCode::Esc)),
            "esc goes to the sky first"
        );
        assert_eq!(v.focus, None);
        assert!(!v.key(KeyEvent::from(KeyCode::Esc)), "then closes");
        assert!(v.legend, "the legend shows at first");
        v.key(KeyEvent::from(KeyCode::Char('?')));
        assert!(!v.legend, "? hides it");
    }

    #[test]
    fn find_ranks_names_first() {
        let mut v = MemView::with(graph());
        v.key(KeyEvent::from(KeyCode::Char('/')));
        for c in "relea".chars() {
            v.key(KeyEvent::from(KeyCode::Char(c)));
        }
        let d = v.data.as_ref().unwrap();
        let first = v.search.as_ref().unwrap().hits[0];
        assert_eq!(d.g.nodes[first].label, "release");
        v.key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(v.focus, Some(first));
        let names: Vec<&str> = v.groups.iter().map(|g| g.text.as_str()).collect();
        assert!(names.contains(&"linked from"), "{names:?}");
        // One way of saying it, the same in the panel.
        assert_eq!(
            [
                group_text("found", 3),
                group_text("turns", 6),
                group_text("turns", 1),
                group_text("memory index", 2)
            ],
            ["found 3", "turns 6", "turn", "memory indexes 2"]
        );
    }
}
