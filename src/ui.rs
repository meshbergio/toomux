use crate::actions;
use crate::config::{Config, hex, tilde};
use crate::registry::{self, Session, State as St, ago, now_ms};
use crate::state::State;
use crate::tmux;
use crate::transcript::{self, Who};
use ansi_to_tui::IntoText;
use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// How long a conversation's cached context lasts between calls.
const CACHE_MS: i64 = 3_600_000;
use unicode_width::UnicodeWidthStr;

mod memory;
mod render;
mod shell;

use render::*;

/// `toomux` as the whole terminal, with sessions live beside the list.
pub fn run_shell(cfg: Config, popup: bool) -> Result<()> {
    shell::run(cfg, popup)
}

struct Palette {
    text: Color,
    dim: Color,
    /// Tertiary words: still meant to be read.
    muted: Color,
    /// Hairlines only.
    faint: Color,
    accent: Color,
    working: Color,
    attention: Color,
    finished: Color,
    selection: Color,
    hover: Color,
    base: Color,
    raised: Color,
    well: Color,
    overlay: Color,
    /// Statuses for words: a little softer than for glyphs, because small
    /// coloured text reads louder than a small coloured shape.
    attention_word: Color,
    working_word: Color,
    finished_word: Color,
    /// The frame and divider: a hairline one step above the meter tracks.
    frame: Color,
}

/// The terminal has hung up (its pane or window closed).
pub(crate) fn terminal_gone() -> bool {
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut fd, 1, 0) };
    n > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

/// Leave when the terminal hangs up. SIGHUP normally ends us first, but when
/// it's ignored (a launcher, a tracer) crossterm's poll spins on the dead tty
/// inside its own loop, at full CPU, and never returns to ours.
pub(crate) fn exit_with_terminal() {
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if terminal_gone() {
                std::process::exit(0);
            }
        }
    });
}

fn rgb(s: &str) -> Color {
    let (r, g, b) = hex(s);
    Color::Rgb(r, g, b)
}

/// `a` moved toward `b` by `t` (0 keeps a, 1 gives b). Only RGB mixes; a
/// named or indexed colour becomes `fallback`, or stays when t is small.
fn mix(a: Color, b: Color, t: f32, fallback: Color) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let m = |x: u8, y: u8| {
                (x as f32 + (y as f32 - x as f32) * t)
                    .round()
                    .clamp(0.0, 255.0) as u8
            };
            Color::Rgb(m(r1, r2), m(g1, g2), m(b1, b2))
        }
        _ if t < 0.2 => a,
        _ => fallback,
    }
}

/// Recede everything in `area` toward `ground` (a scrim behind overlays, or
/// the start of a fade-in).
fn recede(buf: &mut ratatui::buffer::Buffer, area: Rect, ground: Color, t: f32, p: &Palette) {
    let area = area.intersection(buf.area);
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let c = &mut buf[(x, y)];
            // Named and indexed colours can't be mixed; treat them as text.
            let fg = if matches!(c.fg, Color::Rgb(..)) {
                c.fg
            } else {
                p.text
            };
            c.fg = mix(fg, ground, t, p.muted);
            if c.bg != Color::Reset {
                c.bg = mix(c.bg, ground, t, ground);
            }
        }
    }
}

/// Gentle breathing for in-flight work: full strength down to about half and
/// back over a few seconds. Never a blink.
fn breathe(c: Color, ground: Color, since: Instant) -> Color {
    let t = since.elapsed().as_secs_f32() / 3.2;
    let k = 0.5 - 0.5 * (t * std::f32::consts::TAU).cos();
    mix(c, ground, 0.45 * k, c)
}

impl Palette {
    fn new(cfg: &Config) -> Self {
        let c = &cfg.colors;
        Self {
            text: rgb(&c.text),
            dim: rgb(&c.dim),
            muted: rgb(&c.muted),
            faint: rgb(&c.faint),
            accent: rgb(&c.accent),
            working: rgb(&c.working),
            attention: rgb(&c.attention),
            finished: rgb(&c.finished),
            selection: rgb(&c.selection),
            hover: rgb(&c.hover),
            base: rgb(&c.base),
            raised: rgb(&c.raised),
            well: rgb(&c.well),
            overlay: rgb(&c.overlay),
            attention_word: mix(rgb(&c.attention), rgb(&c.dim), 0.16, rgb(&c.attention)),
            working_word: mix(rgb(&c.working), rgb(&c.dim), 0.16, rgb(&c.working)),
            finished_word: mix(rgb(&c.finished), rgb(&c.dim), 0.16, rgb(&c.finished)),
            frame: mix(rgb(&c.faint), rgb(&c.muted), 0.4, rgb(&c.faint)),
        }
    }

    /// The colour of a state's word in running text.
    fn word(&self, s: &Session, noticed: bool) -> Color {
        if s.dormant || s.restore.is_some() {
            return self.muted;
        }
        match &s.handover {
            Some(crate::handover::Phase::Failed(_)) => return self.attention_word,
            Some(_) => return self.working_word,
            None => {}
        }
        match s.state {
            St::NeedsYou => self.attention_word,
            St::Working => self.working_word,
            St::Finished => self.finished_word,
            _ if noticed => self.finished_word,
            St::Background => self.dim,
            St::Idle => self.muted,
        }
    }

    /// Usage numbers stay neutral until they matter.
    fn usage_tone(&self, used: f64, limited: bool) -> Color {
        if limited || used >= 95.0 {
            self.attention
        } else if used >= 80.0 {
            self.working
        } else {
            self.dim
        }
    }

    fn state(&self, s: St) -> (&'static str, Color) {
        match s {
            St::NeedsYou => ("◆", self.attention),
            St::Working => ("●", self.working),
            // Idle to you, with tasks ticking over: neutral, so amber means
            // exactly "working now".
            St::Background => ("◐", self.dim),
            St::Finished => ("●", self.finished),
            St::Idle => ("○", self.dim),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Group {
    Attention,
    Account,
    Project,
}

impl Group {
    fn next(self) -> Self {
        match self {
            Group::Attention => Group::Account,
            Group::Account => Group::Project,
            Group::Project => Group::Attention,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Group::Attention => "by attention",
            Group::Account => "by account",
            Group::Project => "by project",
        }
    }
}

enum Row {
    Header(String, usize),
    Item(usize),
}

#[derive(Clone)]
enum Cmd {
    Help,
    RestoreAll,
    New,
    All,
    Sweep,
    Toggle,
    Jump,
    /// In alt-s's popup: close toomux on the session, in tmux itself.
    GoThere,
    Pin,
    Rename,
    JumpPin(usize),
    Account,
    Adopt,
    Close,
    Group,
    Quit,
    Yes,
    No,
    Pick(usize),
    /// Show or hide an account's usage in the preview pane.
    Usage(usize),
    /// ctrl-u: usage for the selected session's account.
    UsageToggle,
    /// alt-m: the memory graph.
    MemoryToggle,
    /// alt-a: account management.
    Accounts,
    AccountAdd,
    AccountLogin,
    AccountShare,
    AccountUnshare,
    AccountRemove,
    /// An entry of the right-click menu.
    MenuPick(usize),
}

enum Pending {
    Switch {
        pid: i32,
        account: usize,
        wait: bool,
        force: bool,
    },
    Adopt(i32),
    Close(i32),
    Unqueue(i32),
    /// Several at once, one after another: (what, pids, account for moves).
    Many(Batch, Vec<i32>),
    Account(AccountPending),
}

#[derive(Clone)]
enum AccountPending {
    Share {
        index: usize,
        group: String,
        force: bool,
    },
    Unshare {
        index: usize,
        force: bool,
    },
    Remove {
        index: usize,
        delete: bool,
        force: bool,
    },
}

#[derive(Clone, Copy)]
enum Batch {
    Move(usize),
    Close,
}

/// ctrl-n: choose a folder, then an account.
struct NewFlow {
    places: Vec<crate::places::Place>,
    query: String,
    sel: usize,
    /// Set once a folder is chosen: (folder, suggested account, why).
    folder: Option<(String, usize, &'static str)>,
}

impl NewFlow {
    /// Places matching the query; a typed path that exists comes first.
    fn shown(&self) -> Vec<crate::places::Place> {
        let q = self.query.trim().to_lowercase();
        let mut out: Vec<crate::places::Place> = Vec::new();
        if q.starts_with('/') || q.starts_with('~') {
            let p = crate::config::expand(self.query.trim())
                .display()
                .to_string();
            let p = p.trim_end_matches('/').to_string();
            if std::path::Path::new(&p).is_dir() && !self.places.iter().any(|x| x.path == p) {
                out.push(crate::places::Place {
                    path: p,
                    score: 0.0,
                    last_ms: 0,
                    prompts: 0,
                    recent: vec![],
                });
            }
        }
        let terms: Vec<&str> = q.split_whitespace().collect();
        out.extend(
            self.places
                .iter()
                .filter(|pl| {
                    let hay = tilde(&pl.path).to_lowercase();
                    terms
                        .iter()
                        .all(|t| hay.contains(t.trim_start_matches('~')))
                })
                .cloned(),
        );
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddStep {
    Name,
    Folder,
    Sharing,
    NewGroup,
    Creating,
    Login,
}

struct AddFlow {
    step: AddStep,
    name: String,
    folder: String,
    share_sel: usize,
    groups: Vec<String>,
    new_group: String,
    hint: Option<String>,
    notes: Vec<String>,
    added: Option<(usize, std::path::PathBuf)>,
    waiting_login: bool,
}

impl AddFlow {
    fn new(cfg: &Config) -> Self {
        Self {
            step: AddStep::Name,
            name: String::new(),
            folder: String::new(),
            share_sel: 0,
            groups: account_groups(cfg),
            new_group: String::new(),
            hint: None,
            notes: Vec::new(),
            added: None,
            waiting_login: false,
        }
    }
}

enum AccountAction {
    Share { groups: Vec<String>, sel: usize },
}

struct LoginWatch {
    index: usize,
    name: String,
    dir: std::path::PathBuf,
}

fn account_groups(cfg: &Config) -> Vec<String> {
    let mut groups = vec![crate::accounts::DEFAULT_GROUP.to_string()];
    for i in 0..cfg.accounts.len() {
        let Some(group) = crate::accounts::group_of(&cfg.account_dir(i)) else {
            continue;
        };
        let Some(name) = group.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let group = if name == ".claude-shared" {
            crate::accounts::DEFAULT_GROUP
        } else if let Some(name) = name.strip_prefix(".claude-shared-") {
            name
        } else {
            continue;
        };
        if !groups.iter().any(|g| g == group) {
            groups.push(group.to_string());
        }
    }
    groups
}

struct AccountsView {
    sel: usize,
    flow: Option<AddFlow>,
    action: Option<AccountAction>,
    login: Vec<bool>,
    login_watch: Option<LoginWatch>,
}

impl AccountsView {
    fn new(cfg: &Config, sel: usize) -> Self {
        Self {
            sel: sel.min(cfg.accounts.len().saturating_sub(1)),
            flow: None,
            action: None,
            login: cfg
                .accounts
                .iter()
                .enumerate()
                .map(|(i, _)| crate::credentials::present(&cfg.account_dir(i)))
                .collect(),
            login_watch: None,
        }
    }
}

enum AccountDone {
    Added(Result<crate::account_ops::Added, String>),
    Changed {
        sel: usize,
        result: Result<Vec<String>, String>,
    },
}

enum Mode {
    Normal,
    Help,
    Rename(String),
    New(NewFlow),
    Confirm(String, Pending),
    /// A yes/no with a second, wider option on `a`: (prompt, one, label, all).
    Offer(String, Pending, String, Pending),
    /// ctrl-s: stale sessions and whether each will be closed.
    Sweep(Vec<(i32, bool)>),
    Choose(Vec<usize>),
    /// Right-click (or long-press) actions for one session.
    Menu(Menu),
    Accounts(AccountsView),
}

struct Menu {
    pid: i32,
    at: (u16, u16),
    sel: usize,
    items: Vec<(String, Cmd)>,
}

/// The key that does the same as a menu entry, to learn it by.
fn key_for(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Jump => "enter",
        Cmd::GoThere => "alt-j",
        Cmd::Account => "^a",
        Cmd::Adopt => "^o",
        Cmd::Pin => "^p",
        Cmd::Rename => "^r",
        Cmd::Usage(_) => "^u",
        Cmd::Close => "^x",
        _ => "",
    }
}

struct Preview {
    pid: i32,
    at: Instant,
    size: (u16, u16),
    text: Text<'static>,
}

pub struct App {
    cfg: Config,
    pal: Palette,
    sessions: Vec<Session>,
    rows: Vec<Row>,
    sel: Option<i32>,
    filter: String,
    group: Group,
    mode: Mode,
    flash: Option<(String, Color, Instant)>,
    working_on: Option<String>,
    preview: Option<Preview>,
    scroll: u16,
    list_hits: Vec<(Rect, i32)>,
    cmd_hits: Vec<(Rect, Cmd)>,
    last_click: Option<(Instant, i32)>,
    loaded_at: Instant,
    done_tx: Sender<Result<String, String>>,
    done_rx: Receiver<Result<String, String>>,
    account_done_tx: Sender<AccountDone>,
    account_done_rx: Receiver<AccountDone>,
    quit: bool,
    attach: Option<Session>,
    attach_pane: Option<String>,
    notices: Vec<crate::watch::Notice>,
    /// Running as the narrow always-on pane rather than the popup.
    sidebar: bool,
    usage: Vec<crate::usage::Usage>,
    plans: Vec<Option<String>>,
    /// Context and model per session id, from the status line.
    info: std::collections::HashMap<String, crate::usage::SessionInfo>,
    /// Open voyages by the session they're in.
    voyages: std::collections::HashMap<String, crate::voyage::Voyage>,
    /// The account whose usage fills the preview pane instead of a session.
    usage_view: Option<usize>,
    hover: Option<(u16, u16)>,
    epoch: Instant,
    /// The preview fades in briefly when the selection changes.
    fade: Option<(i32, Instant)>,
    /// Where the list is drawn from, easing toward `scroll`.
    shown_scroll: f32,
    dirty: bool,
    /// Session files changed since the last reload.
    changed: bool,
    /// Running as the whole terminal, with a live session beside the list.
    shell: Option<shell::Shell>,
    /// The memory graph, over the list and the session (alt-m).
    memory: Option<memory::MemView>,
    /// Fingerprint of what the last reload put on screen.
    seen: u64,
}

pub fn run(cfg: Config, sidebar: bool) -> Result<()> {
    exit_with_terminal();
    let mut term = ratatui::init();
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    let mut app = App::new(cfg);
    app.sidebar = sidebar;
    let res = app.run(&mut term);
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res?;
    if let Some(s) = app.attach.take() {
        actions::jump(&s)?; // outside tmux: execs `tmux attach`
    }
    if let Some(p) = app.attach_pane.take() {
        actions::jump_pane(&p)?;
    }
    Ok(())
}

impl App {
    fn new(cfg: Config) -> Self {
        let sessions = load_all(&cfg);
        let mut app = Self::with(cfg, sessions);
        app.usage = crate::usage::summary(&app.cfg, &app.sessions, now_ms());
        app.info = crate::usage::sessions();
        app
    }

    fn with(cfg: Config, sessions: Vec<Session>) -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        let (account_done_tx, account_done_rx) = mpsc::channel();
        let pal = Palette::new(&cfg);
        let sel = sessions.first().map(|s| s.pid);
        let mut app = Self {
            cfg,
            pal,
            sessions,
            rows: Vec::new(),
            sel,
            filter: String::new(),
            group: Group::Attention,
            mode: Mode::Normal,
            flash: None,
            working_on: None,
            preview: None,
            scroll: 0,
            list_hits: Vec::new(),
            cmd_hits: Vec::new(),
            last_click: None,
            loaded_at: Instant::now(),
            done_tx,
            done_rx,
            account_done_tx,
            account_done_rx,
            quit: false,
            attach: None,
            attach_pane: None,
            notices: Vec::new(),
            sidebar: false,
            usage: Vec::new(),
            plans: Vec::new(),
            info: Default::default(),
            voyages: Default::default(),
            usage_view: None,
            memory: None,
            hover: None,
            epoch: Instant::now(),
            fade: None,
            shown_scroll: 0.0,
            dirty: true,
            changed: false,
            shell: None,
            seen: 0,
        };
        app.plans = (0..app.cfg.accounts.len())
            .map(|i| crate::usage::plan(&app.cfg, i))
            .collect();
        app.rebuild();
        app
    }

    /// Draw only when something changed, or while something is moving; at
    /// rest toomux wakes once a second to reload.
    fn run(&mut self, term: &mut DefaultTerminal) -> Result<()> {
        let (watch_tx, watch_rx) = mpsc::channel::<()>();
        let _watcher = watch(&self.cfg, watch_tx);
        crate::usage::fetch_in_background(&self.cfg, now_ms());
        while !self.quit {
            // Busy sessions rewrite their registry files constantly: a change
            // reloads soon, but never more than a few times a second.
            let since = self.loaded_at.elapsed();
            while watch_rx.try_recv().is_ok() {
                self.changed = true;
            }
            let mut reload = since > Duration::from_secs(1)
                || (self.changed && since > Duration::from_millis(500));
            while let Ok(done) = self.done_rx.try_recv() {
                self.working_on = None;
                match done {
                    Ok(m) => self.say(m, self.pal.finished),
                    Err(e) => self.say(e, self.pal.attention),
                }
                reload = true;
            }
            while let Ok(done) = self.account_done_rx.try_recv() {
                self.working_on = None;
                self.finish_account_done(done);
                reload = true;
            }
            if reload {
                self.reload();
            }
            let tick = self.animating();
            if self.dirty || tick.is_some() {
                term.draw(|f| self.draw(f))?;
                self.dirty = false;
            }
            let next = if self.changed {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(1)
            };
            let rest = next
                .saturating_sub(self.loaded_at.elapsed())
                .max(Duration::from_millis(40));
            let mut wait = tick.unwrap_or(rest).min(rest);
            // Take every pending event before drawing again.
            while event::poll(wait)? {
                wait = Duration::ZERO;
                self.dirty = true;
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => self.key(k),
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::Down(MouseButton::Left) => self.click(m.column, m.row),
                        MouseEventKind::Down(MouseButton::Right) => self.menu_at(m.column, m.row),
                        MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                            self.hover = Some((m.column, m.row))
                        }
                        MouseEventKind::ScrollDown => self.scroll_by(1),
                        MouseEventKind::ScrollUp => self.scroll_by(-1),
                        _ => {}
                    },
                    _ => {}
                }
                if self.quit {
                    break;
                }
            }
        }
        Ok(())
    }

    /// How soon to draw again for motion, if anything is moving.
    fn animating(&self) -> Option<Duration> {
        if let Some(t) = self.memory.as_ref().and_then(|m| m.busy()) {
            return Some(t);
        }
        let fading = self
            .fade
            .is_some_and(|(_, t)| t.elapsed() < Duration::from_millis(260));
        let easing = (self.shown_scroll - self.scroll as f32).abs() > 0.01;
        if fading || easing {
            return Some(Duration::from_millis(30));
        }
        let flash = self
            .flash
            .as_ref()
            .is_some_and(|(_, _, t)| t.elapsed() < Duration::from_millis(5_800));
        // Breathing is for when you're looking at the list: in the full-screen
        // shell, a session with the keyboard keeps the rest perfectly still.
        let list_in_view = self.shell.as_ref().is_none_or(|sh| !sh.focus_live);
        let breathing = self.working_on.is_some()
            || (list_in_view
                && matches!(self.mode, Mode::Normal)
                && self.items().any(|s| s.state == St::Working && !s.dormant));
        (flash || breathing).then_some(Duration::from_millis(200))
    }

    fn reload(&mut self) {
        self.changed = false;
        self.notices = crate::watch::notices();
        self.sessions = load_all(&self.cfg);
        self.usage = crate::usage::summary(&self.cfg, &self.sessions, now_ms());
        self.info = crate::usage::sessions();
        // Open voyages, and ones met a few minutes ago (their landing shows).
        let now = now_ms();
        self.voyages = crate::voyage::landed(now)
            .into_iter()
            .chain(crate::voyage::open())
            .map(|q| (q.session().to_string(), q))
            .collect();
        self.loaded_at = Instant::now();
        self.rebuild();
        self.check_account_login();
        // Redraw only when something you'd see is different.
        let seen = self.fingerprint();
        if seen != self.seen {
            self.seen = seen;
            self.dirty = true;
        }
    }

    fn refresh_config(&mut self) -> Result<()> {
        self.cfg = Config::load()?;
        self.pal = Palette::new(&self.cfg);
        self.usage = crate::usage::summary(&self.cfg, &self.sessions, now_ms());
        self.plans = (0..self.cfg.accounts.len())
            .map(|i| crate::usage::plan(&self.cfg, i))
            .collect();
        Ok(())
    }

    fn finish_account_done(&mut self, done: AccountDone) {
        match done {
            AccountDone::Added(Ok(added)) => {
                let _ = self.refresh_config();
                let mut view = AccountsView::new(&self.cfg, added.index);
                let mut flow = AddFlow::new(&self.cfg);
                flow.step = AddStep::Login;
                flow.name = self
                    .cfg
                    .accounts
                    .get(added.index)
                    .map(|a| a.name.clone())
                    .unwrap_or_default();
                flow.folder = tilde(&added.dir.display().to_string());
                flow.notes = added.notes;
                flow.added = Some((added.index, added.dir));
                view.flow = Some(flow);
                self.mode = Mode::Accounts(view);
            }
            AccountDone::Added(Err(e)) => {
                let attempted = match &self.mode {
                    Mode::Accounts(view) => view.flow.as_ref().map(|f| f.name.clone()),
                    _ => None,
                };
                let _ = self.refresh_config();
                if let Some(index) = attempted
                    .as_deref()
                    .and_then(|name| self.cfg.account_by_name(name))
                {
                    self.mode = Mode::Accounts(AccountsView::new(&self.cfg, index));
                    self.say(
                        format!(
                            "{} was created, but its setup needs attention: {e}",
                            self.cfg.accounts[index].name
                        ),
                        self.pal.attention,
                    );
                } else {
                    if let Mode::Accounts(view) = &mut self.mode
                        && let Some(flow) = &mut view.flow
                    {
                        flow.step = AddStep::Sharing;
                        flow.hint = Some(e.clone());
                    }
                    self.say(e, self.pal.attention);
                }
            }
            AccountDone::Changed { sel, result } => {
                let _ = self.refresh_config();
                self.mode = Mode::Accounts(AccountsView::new(&self.cfg, sel));
                match result {
                    Ok(notes) => {
                        if !notes.is_empty() {
                            self.say(notes.join(" · "), self.pal.finished);
                        }
                    }
                    Err(e) => self.say(e, self.pal.attention),
                }
            }
        }
    }

    fn check_account_login(&mut self) {
        let watch = match &self.mode {
            Mode::Accounts(view) => view
                .login_watch
                .as_ref()
                .map(|w| (w.index, w.name.clone(), w.dir.clone())),
            _ => None,
        };
        let Some((index, name, dir)) = watch else {
            return;
        };
        if !crate::credentials::present(&dir) {
            return;
        }
        if let Mode::Accounts(view) = &mut self.mode {
            if let Some(login) = view.login.get_mut(index) {
                *login = true;
            }
            view.login_watch = None;
            view.flow = None;
        }
        self.say(format!("signed in · {name} is ready"), self.pal.finished);
    }

    /// Everything a reload can change on screen, hashed.
    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let now = now_ms();
        for s in &self.sessions {
            (
                s.pid,
                s.state as u8,
                &s.title,
                &s.topic,
                s.status_text(now),
                s.account,
                s.pin,
                &s.queued,
                &s.limit,
            )
                .hash(&mut h);
            s.pane.as_ref().map(|p| &p.id).hash(&mut h);
        }
        for u in &self.usage {
            for m in [&u.five, &u.week].into_iter().flatten() {
                ((m.used * 10.0) as i64, m.limited, m.resets_ms / 60_000).hash(&mut h);
            }
            (u.at_ms / 60_000, &u.problem).hash(&mut h);
        }
        self.notices.len().hash(&mut h);
        let mut voyages: Vec<_> = self
            .voyages
            .values()
            .map(|q| (&q.id, q.session(), q.status.word(), q.at_limit, q.chip(now)))
            .collect();
        voyages.sort();
        voyages.hash(&mut h);
        // The detail pane's scene moves once a second.
        if self.cfg.voyage_scene
            && !self.sidebar
            && self
                .selected()
                .is_some_and(|s| self.voyages.contains_key(&s.id))
        {
            (now / 1000).hash(&mut h);
        }
        h.finish()
    }

    fn say(&mut self, msg: String, color: Color) {
        self.flash = Some((msg, color, Instant::now()));
    }

    // ---- model -------------------------------------------------------------

    fn matches(&self, s: &Session) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let hay = format!(
            "{} {} {} {} {}",
            s.title,
            s.place(),
            s.account_name(&self.cfg),
            s.state.section(),
            if s.pane.is_some() { "tmux" } else { "outside" }
        );
        let hay = (hay
            + if s.pin.is_some() { " pinned" } else { "" }
            + if s.dormant { " not running" } else { "" })
        .to_lowercase();
        self.filter
            .to_lowercase()
            .split_whitespace()
            .all(|t| hay.contains(t))
    }

    fn rebuild(&mut self) {
        let sweeping: Option<Vec<i32>> = match &self.mode {
            Mode::Sweep(list) => Some(list.iter().map(|(p, _)| *p).collect()),
            _ => None,
        };
        let mut idx: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| match &sweeping {
                Some(pids) => pids.contains(&self.sessions[i].pid),
                None => self.matches(&self.sessions[i]),
            })
            .collect();
        let key = |s: &Session| -> String {
            match self.group {
                Group::Attention if s.restore.is_some() => "before the restart".to_string(),
                Group::Attention if s.dormant => "pinned · not running".to_string(),
                Group::Attention => s.state.section().to_string(),
                Group::Account => s.account_name(&self.cfg).to_string(),
                Group::Project => s.place().split('/').take(2).collect::<Vec<_>>().join("/"),
            }
        };
        if self.group != Group::Attention {
            idx.sort_by(|&a, &b| {
                let (sa, sb) = (&self.sessions[a], &self.sessions[b]);
                key(sa)
                    .cmp(&key(sb))
                    .then(sa.state.cmp(&sb.state))
                    .then(sb.since_ms.cmp(&sa.since_ms))
            });
        }
        let mut rows = Vec::new();
        let mut current: Option<String> = None;
        for &i in &idx {
            let k = key(&self.sessions[i]);
            if current.as_ref() != Some(&k) {
                let n = idx.iter().filter(|&&j| key(&self.sessions[j]) == k).count();
                rows.push(Row::Header(k.clone(), n));
                current = Some(k);
            }
            rows.push(Row::Item(i));
        }
        self.rows = rows;
        let visible: Vec<i32> = self.items().map(|s| s.pid).collect();
        if !self.sel.is_some_and(|p| visible.contains(&p)) {
            self.sel = visible.first().copied();
        }
    }

    fn items(&self) -> impl Iterator<Item = &Session> {
        self.rows.iter().filter_map(|r| match r {
            Row::Item(i) => Some(&self.sessions[*i]),
            Row::Header(..) => None,
        })
    }

    fn selected(&self) -> Option<&Session> {
        let pid = self.sel?;
        self.sessions.iter().find(|s| s.pid == pid)
    }

    fn scroll_by(&mut self, d: i32) {
        if let Mode::New(flow) = &mut self.mode {
            let n = flow.shown().len() as i32;
            flow.sel = (flow.sel as i32 + d).clamp(0, (n - 1).max(0)) as usize;
        } else {
            self.step(d);
        }
    }

    fn step(&mut self, d: i32) {
        let pids: Vec<i32> = self.items().map(|s| s.pid).collect();
        if pids.is_empty() {
            return;
        }
        let cur = self
            .sel
            .and_then(|p| pids.iter().position(|&x| x == p))
            .unwrap_or(0) as i32;
        let next = (cur + d).clamp(0, pids.len() as i32 - 1) as usize;
        self.select(pids[next]);
    }

    /// Move the selection; the preview fades in on the new session and the
    /// usage view gives way to it.
    fn select(&mut self, pid: i32) {
        if self.sel != Some(pid) {
            self.fade = Some((pid, Instant::now()));
            self.preview = None;
        }
        self.sel = Some(pid);
        if !matches!(self.mode, Mode::Sweep(_)) {
            self.usage_view = None;
        }
    }

    // ---- input -------------------------------------------------------------

    fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if matches!(self.mode, Mode::Accounts(_)) {
            self.account_key(k);
            return;
        }
        match &self.mode {
            Mode::Help => self.mode = Mode::Normal,
            Mode::Menu(m) => {
                let n = m.items.len();
                let sel = m.sel;
                match k.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => self.exec(Cmd::MenuPick(sel)),
                    KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                        if let Mode::Menu(m) = &mut self.mode {
                            m.sel = (sel + 1) % n.max(1);
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                        if let Mode::Menu(m) = &mut self.mode {
                            m.sel = (sel + n.max(1) - 1) % n.max(1);
                        }
                    }
                    _ => {}
                }
            }
            Mode::Offer(..) => match k.code {
                KeyCode::Enter | KeyCode::Char('y') => self.exec(Cmd::Yes),
                KeyCode::Char('a') => self.exec(Cmd::All),
                KeyCode::Esc | KeyCode::Char('n') => self.exec(Cmd::No),
                _ => {}
            },
            Mode::Sweep(_) => match k.code {
                KeyCode::Char(' ') => self.exec(Cmd::Toggle),
                KeyCode::Char('a') => self.exec(Cmd::All),
                KeyCode::Enter => self.exec(Cmd::Yes),
                KeyCode::Esc => self.exec(Cmd::No),
                KeyCode::Down | KeyCode::Char('j') => self.step(1),
                KeyCode::Up | KeyCode::Char('k') => self.step(-1),
                _ => {}
            },
            Mode::Confirm(..) => match k.code {
                KeyCode::Enter | KeyCode::Char('y') => self.exec(Cmd::Yes),
                KeyCode::Esc | KeyCode::Char('n') => self.exec(Cmd::No),
                _ => {}
            },
            Mode::Rename(buf) => {
                let mut buf = buf.clone();
                match k.code {
                    KeyCode::Enter => {
                        self.exec(Cmd::Yes);
                        return;
                    }
                    KeyCode::Esc => {
                        self.mode = Mode::Normal;
                        return;
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char('u') if ctrl => buf.clear(),
                    KeyCode::Char('w') if ctrl => {
                        let t = buf
                            .trim_end()
                            .rsplit_once(' ')
                            .map(|(a, _)| format!("{a} "))
                            .unwrap_or_default();
                        buf = t;
                    }
                    KeyCode::Char(c) if !ctrl => buf.push(c),
                    _ => {}
                }
                self.mode = Mode::Rename(buf);
            }
            Mode::New(flow) if flow.folder.is_some() => {
                let n_accounts = self.cfg.accounts.len();
                match k.code {
                    KeyCode::Enter => self.exec(Cmd::Yes),
                    KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < n_accounts => {
                        self.exec(Cmd::Pick(c as usize - '1' as usize))
                    }
                    KeyCode::Esc => {
                        if let Mode::New(f) = &mut self.mode {
                            f.folder = None;
                        }
                    }
                    _ => {}
                }
            }
            Mode::New(_) => {
                let Mode::New(flow) = &mut self.mode else {
                    return;
                };
                let n = flow.shown().len();
                match k.code {
                    KeyCode::Esc if !flow.query.is_empty() => {
                        flow.query.clear();
                        flow.sel = 0;
                    }
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => self.exec(Cmd::Yes),
                    KeyCode::Down => flow.sel = (flow.sel + 1).min(n.saturating_sub(1)),
                    KeyCode::Up => flow.sel = flow.sel.saturating_sub(1),
                    KeyCode::Char('j') if ctrl => {
                        flow.sel = (flow.sel + 1).min(n.saturating_sub(1))
                    }
                    KeyCode::Char('k') if ctrl => flow.sel = flow.sel.saturating_sub(1),
                    KeyCode::PageDown => flow.sel = (flow.sel + 8).min(n.saturating_sub(1)),
                    KeyCode::PageUp => flow.sel = flow.sel.saturating_sub(8),
                    KeyCode::Backspace => {
                        flow.query.pop();
                        flow.sel = 0;
                    }
                    KeyCode::Char('u') if ctrl => {
                        flow.query.clear();
                        flow.sel = 0;
                    }
                    KeyCode::Char(c) if !ctrl => {
                        flow.query.push(c);
                        flow.sel = 0;
                    }
                    _ => {}
                }
            }
            Mode::Choose(opts) => match k.code {
                KeyCode::Char(c @ '1'..='9') => {
                    let n = c as usize - '1' as usize;
                    if n < opts.len() {
                        self.exec(Cmd::Pick(n));
                    }
                }
                KeyCode::Esc => self.exec(Cmd::No),
                _ => {}
            },
            Mode::Normal => match k.code {
                KeyCode::Enter => self.exec(Cmd::Jump),
                KeyCode::Esc if !self.filter.is_empty() => {
                    self.filter.clear();
                    self.rebuild();
                }
                KeyCode::Esc if self.usage_view.is_some() => self.usage_view = None,
                KeyCode::Esc if self.sidebar => {}
                KeyCode::Esc => self.exec(Cmd::Quit),
                KeyCode::Down => self.step(1),
                KeyCode::Up => self.step(-1),
                KeyCode::Char('j') if ctrl => self.step(1),
                KeyCode::Char('k') if ctrl => self.step(-1),
                KeyCode::PageDown => self.step(8),
                KeyCode::PageUp => self.step(-8),
                KeyCode::Home => self.step(-10_000),
                KeyCode::End => self.step(10_000),
                KeyCode::Tab => self.exec(Cmd::Group),
                KeyCode::Char('a') if ctrl => self.exec(Cmd::Account),
                KeyCode::Char('o') if ctrl => self.exec(Cmd::Adopt),
                KeyCode::Char('x') if ctrl => self.exec(Cmd::Close),
                KeyCode::Char('p') if ctrl => self.exec(Cmd::Pin),
                KeyCode::Char('n') if ctrl => self.exec(Cmd::New),
                KeyCode::Char('s') if ctrl => self.exec(Cmd::Sweep),
                KeyCode::Char('e') if ctrl => self.exec(Cmd::RestoreAll),
                KeyCode::Char('?') if self.filter.is_empty() => self.mode = Mode::Help,
                KeyCode::Char('r') if ctrl => self.exec(Cmd::Rename),
                KeyCode::Char(c @ '1'..='9') if k.modifiers.contains(KeyModifiers::ALT) => {
                    self.exec(Cmd::JumpPin(c as usize - '1' as usize))
                }
                KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::ALT) => {
                    self.exec(Cmd::Accounts)
                }
                // ctrl-u clears a half-typed filter, as in a shell; otherwise usage.
                KeyCode::Char('u') if ctrl && !self.filter.is_empty() => {
                    self.filter.clear();
                    self.rebuild();
                }
                KeyCode::Char('u') if ctrl => self.exec(Cmd::UsageToggle),
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.rebuild();
                }
                KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => {
                    self.filter.push(c);
                    self.rebuild();
                }
                _ => {}
            },
            Mode::Accounts(_) => unreachable!(),
        }
    }

    fn account_key(&mut self, k: KeyEvent) {
        let flow = matches!(&self.mode, Mode::Accounts(v) if v.flow.is_some());
        if flow {
            self.account_flow_key(k);
            return;
        }
        let action = matches!(&self.mode, Mode::Accounts(v) if v.action.is_some());
        if action {
            let mut choose: Option<(usize, String)> = None;
            if let Mode::Accounts(view) = &mut self.mode
                && let Some(AccountAction::Share { groups, sel }) = &mut view.action
            {
                match k.code {
                    KeyCode::Esc => view.action = None,
                    KeyCode::Down | KeyCode::Char('j') => {
                        *sel = (*sel + 1).min(groups.len().saturating_sub(1))
                    }
                    KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                    KeyCode::Char(c @ '1'..='9') => {
                        let n = c as usize - '1' as usize;
                        if n < groups.len() {
                            *sel = n;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(group) = groups.get(*sel).cloned() {
                            choose = Some((view.sel, group));
                            view.action = None;
                        }
                    }
                    _ => {}
                }
            }
            if let Some((index, group)) = choose {
                self.request_account_share(index, group);
            }
            return;
        }

        enum Do {
            None,
            Login(usize),
            Share(usize),
            Unshare(usize),
            Remove(usize),
        }
        let mut do_ = Do::None;
        if let Mode::Accounts(view) = &mut self.mode {
            match k.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Down | KeyCode::Char('j') => {
                    view.sel = (view.sel + 1).min(self.cfg.accounts.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => view.sel = view.sel.saturating_sub(1),
                KeyCode::Char('a') => view.flow = Some(AddFlow::new(&self.cfg)),
                KeyCode::Char('l') => do_ = Do::Login(view.sel),
                KeyCode::Char('s') => do_ = Do::Share(view.sel),
                KeyCode::Char('u') => do_ = Do::Unshare(view.sel),
                KeyCode::Char('x') => do_ = Do::Remove(view.sel),
                _ => {}
            }
        }
        match do_ {
            Do::None => {}
            Do::Login(i) => self.start_account_login(i),
            Do::Share(i) => self.open_account_share(i),
            Do::Unshare(i) => self.request_account_unshare(i),
            Do::Remove(i) => self.request_account_remove(i),
        }
    }

    fn account_flow_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let step = match &self.mode {
            Mode::Accounts(view) => view.flow.as_ref().map(|f| f.step),
            _ => None,
        };
        let Some(step) = step else { return };
        match step {
            AddStep::Name => {
                if k.code == KeyCode::Esc {
                    if let Mode::Accounts(view) = &mut self.mode {
                        view.flow = None;
                    }
                    return;
                }
                let mut advance = false;
                if let Mode::Accounts(view) = &mut self.mode
                    && let Some(flow) = &mut view.flow
                {
                    match k.code {
                        KeyCode::Backspace => {
                            flow.name.pop();
                        }
                        KeyCode::Char('u') if ctrl => flow.name.clear(),
                        KeyCode::Char(c) if !ctrl => flow.name.push(c),
                        KeyCode::Enter => advance = true,
                        _ => {}
                    }
                    if !flow.name.is_empty() {
                        flow.hint = crate::accounts::valid_name(&flow.name)
                            .err()
                            .map(|e| e.to_string())
                            .or_else(|| {
                                self.cfg
                                    .account_by_name(&flow.name)
                                    .is_some()
                                    .then(|| "there's already an account with that name".into())
                            });
                    } else {
                        flow.hint = None;
                    }
                }
                if advance {
                    let (name, invalid) = match &self.mode {
                        Mode::Accounts(view) => {
                            let flow = view.flow.as_ref().unwrap();
                            let duplicate = self.cfg.account_by_name(&flow.name).is_some();
                            let invalid = crate::accounts::valid_name(&flow.name)
                                .err()
                                .map(|e| e.to_string())
                                .or_else(|| {
                                    duplicate
                                        .then(|| "there's already an account with that name".into())
                                });
                            (flow.name.clone(), invalid)
                        }
                        _ => return,
                    };
                    if let Mode::Accounts(view) = &mut self.mode
                        && let Some(flow) = &mut view.flow
                    {
                        if let Some(e) = invalid {
                            flow.hint = Some(e);
                        } else {
                            flow.folder = format!("~/.claude-{name}");
                            flow.step = AddStep::Folder;
                        }
                    }
                }
            }
            AddStep::Folder => {
                let mut advance = false;
                if let Mode::Accounts(view) = &mut self.mode
                    && let Some(flow) = &mut view.flow
                {
                    flow.hint = None;
                    match k.code {
                        KeyCode::Esc => flow.step = AddStep::Name,
                        KeyCode::Backspace => {
                            flow.folder.pop();
                        }
                        KeyCode::Char('u') if ctrl => flow.folder.clear(),
                        KeyCode::Char(c) if !ctrl => flow.folder.push(c),
                        KeyCode::Enter => advance = true,
                        _ => {}
                    }
                }
                if advance {
                    let (name, folder) = match &self.mode {
                        Mode::Accounts(view) => {
                            let f = view.flow.as_ref().unwrap();
                            (f.name.clone(), f.folder.clone())
                        }
                        _ => return,
                    };
                    let checked = crate::account_ops::validate_add(
                        &self.cfg,
                        &name,
                        &crate::config::expand(folder.trim()),
                    );
                    if let Mode::Accounts(view) = &mut self.mode
                        && let Some(flow) = &mut view.flow
                    {
                        match checked {
                            Ok(_) => flow.step = AddStep::Sharing,
                            Err(e) => flow.hint = Some(e.to_string()),
                        }
                    }
                }
            }
            AddStep::Sharing => {
                let mut create: Option<Option<String>> = None;
                if let Mode::Accounts(view) = &mut self.mode
                    && let Some(flow) = &mut view.flow
                {
                    let choices = flow.groups.len() + 2;
                    match k.code {
                        KeyCode::Esc => flow.step = AddStep::Folder,
                        KeyCode::Down | KeyCode::Char('j') => {
                            flow.share_sel = (flow.share_sel + 1).min(choices - 1)
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            flow.share_sel = flow.share_sel.saturating_sub(1)
                        }
                        KeyCode::Char(c @ '1'..='9') => {
                            let n = c as usize - '1' as usize;
                            if n < choices {
                                flow.share_sel = n;
                            }
                        }
                        KeyCode::Enter => {
                            if flow.share_sel == choices - 1 {
                                flow.new_group.clear();
                                flow.step = AddStep::NewGroup;
                            } else if flow.share_sel == 0 {
                                create = Some(None);
                            } else {
                                create = Some(flow.groups.get(flow.share_sel - 1).cloned());
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(group) = create {
                    self.start_account_add(group);
                }
            }
            AddStep::NewGroup => {
                let mut create: Option<String> = None;
                if let Mode::Accounts(view) = &mut self.mode
                    && let Some(flow) = &mut view.flow
                {
                    flow.hint = None;
                    match k.code {
                        KeyCode::Esc => flow.step = AddStep::Sharing,
                        KeyCode::Backspace => {
                            flow.new_group.pop();
                        }
                        KeyCode::Char('u') if ctrl => flow.new_group.clear(),
                        KeyCode::Char(c) if !ctrl => flow.new_group.push(c),
                        KeyCode::Enter => {
                            if let Err(e) = crate::accounts::valid_name(&flow.new_group) {
                                flow.hint = Some(e.to_string());
                            } else {
                                create = Some(flow.new_group.clone());
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(group) = create {
                    self.start_account_add(Some(group));
                }
            }
            AddStep::Creating => {}
            AddStep::Login => match k.code {
                KeyCode::Esc => {
                    if let Mode::Accounts(view) = &mut self.mode {
                        view.flow = None;
                    }
                    self.say("log in later with l".into(), self.pal.dim);
                }
                KeyCode::Enter => {
                    let index = match &self.mode {
                        Mode::Accounts(view) => view
                            .flow
                            .as_ref()
                            .and_then(|f| f.added.as_ref().map(|(i, _)| *i)),
                        _ => None,
                    };
                    if let Some(i) = index {
                        self.start_account_login(i);
                    }
                }
                _ => {}
            },
        }
    }

    fn start_account_add(&mut self, group: Option<String>) {
        let (name, folder) = match &mut self.mode {
            Mode::Accounts(view) => {
                let Some(flow) = &mut view.flow else { return };
                flow.step = AddStep::Creating;
                flow.hint = None;
                (flow.name.clone(), flow.folder.clone())
            }
            _ => return,
        };
        let (mut cfg, tx) = (self.cfg.clone(), self.account_done_tx.clone());
        self.working_on = Some(format!("adding {name}"));
        std::thread::spawn(move || {
            let result = (|| -> Result<crate::account_ops::Added> {
                let mut added =
                    crate::account_ops::add(&mut cfg, &name, Some(crate::config::expand(&folder)))?;
                if let Some(group) = group {
                    match crate::account_ops::share(&cfg, &[added.index], &group, false, false) {
                        Ok(notes) => added.notes.extend(notes),
                        Err(e) => added
                            .notes
                            .push(format!("{name}: account is ready, but sharing failed: {e}")),
                    }
                }
                Ok(added)
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(AccountDone::Added(result));
        });
    }

    fn start_account_login(&mut self, index: usize) {
        let Some(account) = self.cfg.accounts.get(index) else {
            return;
        };
        let name = account.name.clone();
        let dir = self.cfg.account_dir(index);
        let cwd = dir.display().to_string();
        match actions::start(&self.cfg, &cwd, index, &[]) {
            Ok(pane) => {
                if let Mode::Accounts(view) = &mut self.mode {
                    view.login_watch = Some(LoginWatch {
                        index,
                        name: name.clone(),
                        dir: dir.clone(),
                    });
                    if let Some(flow) = &mut view.flow {
                        flow.waiting_login = true;
                    }
                }
                std::thread::spawn(move || {
                    for _ in 0..12 {
                        std::thread::sleep(Duration::from_millis(350));
                        if actions::prompt_empty(&pane) {
                            let _ = actions::type_prompt(&pane, "/login");
                            break;
                        }
                    }
                });
                self.say(format!("waiting for {name} to sign in…"), self.pal.working);
            }
            Err(e) => self.say(e.to_string(), self.pal.attention),
        }
    }

    fn open_account_share(&mut self, index: usize) {
        if index >= self.cfg.accounts.len() {
            return;
        }
        if let Mode::Accounts(view) = &mut self.mode {
            view.action = Some(AccountAction::Share {
                groups: account_groups(&self.cfg),
                sel: 0,
            });
        }
    }

    fn request_account_share(&mut self, index: usize, group: String) {
        let n = self
            .sessions
            .iter()
            .filter(|s| !s.dormant && s.account == Some(index))
            .count();
        if n > 0 {
            let name = self.cfg.accounts[index].name.clone();
            self.mode = Mode::Confirm(
                format!(
                    "{n} session{} run on {name} · share anyway?",
                    if n == 1 { "" } else { "s" }
                ),
                Pending::Account(AccountPending::Share {
                    index,
                    group,
                    force: true,
                }),
            );
        } else {
            self.run_account_pending(AccountPending::Share {
                index,
                group,
                force: false,
            });
        }
    }

    fn request_account_unshare(&mut self, index: usize) {
        if index >= self.cfg.accounts.len() {
            return;
        }
        let n = self
            .sessions
            .iter()
            .filter(|s| !s.dormant && s.account == Some(index))
            .count();
        if n > 0 {
            let name = self.cfg.accounts[index].name.clone();
            self.mode = Mode::Confirm(
                format!(
                    "{n} session{} run on {name} · unshare anyway?",
                    if n == 1 { "" } else { "s" }
                ),
                Pending::Account(AccountPending::Unshare { index, force: true }),
            );
        } else {
            self.run_account_pending(AccountPending::Unshare {
                index,
                force: false,
            });
        }
    }

    fn request_account_remove(&mut self, index: usize) {
        if index >= self.cfg.accounts.len() {
            return;
        }
        let n = self
            .sessions
            .iter()
            .filter(|s| !s.dormant && s.account == Some(index))
            .count();
        let name = self.cfg.accounts[index].name.clone();
        let busy = if n == 0 {
            String::new()
        } else {
            format!(" · {n} session{} run on it", if n == 1 { "" } else { "s" })
        };
        self.mode = Mode::Offer(
            format!("remove {name} from toomux?{busy}"),
            Pending::Account(AccountPending::Remove {
                index,
                delete: false,
                force: n > 0,
            }),
            "delete folder".into(),
            Pending::Account(AccountPending::Remove {
                index,
                delete: true,
                force: n > 0,
            }),
        );
    }

    fn run_account_pending(&mut self, op: AccountPending) {
        let (sel, label) = match &op {
            AccountPending::Share { index, group, .. } => (
                *index,
                format!("sharing {} in {group}", self.cfg.accounts[*index].name),
            ),
            AccountPending::Unshare { index, .. } => (
                *index,
                format!("unsharing {}", self.cfg.accounts[*index].name),
            ),
            AccountPending::Remove { index, delete, .. } => (
                *index,
                format!(
                    "removing {}{}",
                    self.cfg.accounts[*index].name,
                    if *delete { " and its folder" } else { "" }
                ),
            ),
        };
        self.mode = Mode::Accounts(AccountsView::new(&self.cfg, sel));
        self.working_on = Some(label);
        let (mut cfg, tx) = (self.cfg.clone(), self.account_done_tx.clone());
        std::thread::spawn(move || {
            let result = match op {
                AccountPending::Share {
                    index,
                    group,
                    force,
                } => crate::account_ops::share(&cfg, &[index], &group, false, force),
                AccountPending::Unshare { index, force } => {
                    crate::account_ops::unshare(&cfg, &[index], false, false, force)
                }
                AccountPending::Remove {
                    index,
                    delete,
                    force,
                } => crate::account_ops::remove(&mut cfg, index, delete, force),
            }
            .map_err(|e| e.to_string());
            let _ = tx.send(AccountDone::Changed { sel, result });
        });
    }

    fn click(&mut self, x: u16, y: u16) {
        let inside = |r: &Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        if let Some((_, cmd)) = self.cmd_hits.iter().find(|(r, _)| inside(r)).cloned() {
            self.exec(cmd);
            return;
        }
        if matches!(self.mode, Mode::Menu(_) | Mode::Help) {
            self.mode = Mode::Normal;
            return;
        }
        if let Mode::Accounts(view) = &mut self.mode {
            if view.flow.is_none()
                && view.action.is_none()
                && let Some(&(_, index)) = self.list_hits.iter().find(|(r, _)| inside(r))
            {
                view.sel = (index as usize).min(self.cfg.accounts.len().saturating_sub(1));
            }
            return;
        }
        if let Mode::Sweep(_) = self.mode {
            if let Some(&(_, pid)) = self.list_hits.iter().find(|(r, _)| inside(r)) {
                self.select(pid);
                self.exec(Cmd::Toggle);
            }
            return;
        }
        if let Mode::New(flow) = &mut self.mode {
            if let Some(&(_, i)) = self.list_hits.iter().find(|(r, _)| inside(r)) {
                let double = flow.sel == i as usize
                    && self
                        .last_click
                        .is_some_and(|(t, p)| p == i && t.elapsed() < Duration::from_millis(450));
                flow.sel = i as usize;
                self.last_click = Some((Instant::now(), i));
                if double {
                    self.exec(Cmd::Yes);
                }
            }
            return;
        }
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        if let Some(&(_, pid)) = self.list_hits.iter().find(|(r, _)| inside(r)) {
            let double = self.sidebar
                || self
                    .last_click
                    .is_some_and(|(t, p)| p == pid && t.elapsed() < Duration::from_millis(450));
            self.select(pid);
            self.last_click = Some((Instant::now(), pid));
            if double {
                self.exec(Cmd::Jump);
            }
        }
    }

    /// Right-click, or a long-press on a tablet: what can be done with it.
    fn menu_at(&mut self, x: u16, y: u16) {
        if !matches!(self.mode, Mode::Normal) {
            self.mode = Mode::Normal;
            return;
        }
        let inside = |r: &Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        let Some(&(_, pid)) = self.list_hits.iter().find(|(r, _)| inside(r)) else {
            return;
        };
        self.select(pid);
        let Some(s) = self.selected().cloned() else {
            return;
        };
        let mut items: Vec<(String, Cmd)> = Vec::new();
        items.push((
            if s.dormant {
                "reopen"
            } else if self.shell.is_some() {
                "open"
            } else {
                "jump to it"
            }
            .into(),
            Cmd::Jump,
        ));
        if s.pane.is_some() && self.shell.as_ref().is_some_and(|sh| sh.popup) {
            items.push(("go there in tmux".into(), Cmd::GoThere));
        }
        if !s.dormant {
            let label = match &s.queued {
                Some(to) => format!("stay here, don't move to {to}"),
                None if self.cfg.accounts.len() == 2 => {
                    let other = self
                        .cfg
                        .accounts
                        .iter()
                        .enumerate()
                        .find(|(i, _)| Some(*i) != s.account);
                    format!(
                        "move to {}",
                        other
                            .map(|(_, a)| a.name.as_str())
                            .unwrap_or("the other account")
                    )
                }
                None => "move to another account".into(),
            };
            items.push((label, Cmd::Account));
        }
        if s.pane.is_none() && !s.dormant {
            items.push(("bring into tmux".into(), Cmd::Adopt));
        }
        items.push((
            if s.pin.is_some() {
                "unpin"
            } else {
                "pin to alt-1..9"
            }
            .into(),
            Cmd::Pin,
        ));
        if !s.dormant {
            items.push(("rename".into(), Cmd::Rename));
        }
        if let Some(a) = s.account {
            items.push((
                format!("{} usage", self.cfg.accounts[a].name),
                Cmd::Usage(a),
            ));
        }
        if s.restore.is_some() {
            items.push(("don't offer again".into(), Cmd::Close));
        } else if !s.dormant {
            items.push(("close".into(), Cmd::Close));
        }
        self.mode = Mode::Menu(Menu {
            pid,
            at: (x, y),
            sel: 0,
            items,
        });
    }

    fn exec(&mut self, cmd: Cmd) {
        if matches!(self.mode, Mode::Accounts(_)) {
            let key = match cmd {
                Cmd::Yes => Some(KeyCode::Enter),
                Cmd::No => Some(KeyCode::Esc),
                Cmd::Pick(n) if n < 9 => Some(KeyCode::Char(char::from(b'1' + n as u8))),
                _ => None,
            };
            if let Some(code) = key {
                self.account_key(KeyEvent::new(code, KeyModifiers::NONE));
                return;
            }
        }
        match cmd {
            Cmd::MenuPick(n) => {
                let Mode::Menu(m) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return;
                };
                if let Some((_, c)) = m.items.get(n).cloned() {
                    self.sel = Some(m.pid);
                    self.exec(c);
                }
                return;
            }
            Cmd::Usage(i) => {
                self.mode = Mode::Normal;
                self.usage_view = if self.usage_view == Some(i) {
                    None
                } else {
                    Some(i)
                };
                return;
            }
            Cmd::UsageToggle => {
                let a = self.selected().and_then(|s| s.account).unwrap_or(0);
                self.usage_view = if self.usage_view.is_some() {
                    None
                } else {
                    Some(a)
                };
                return;
            }
            Cmd::MemoryToggle => {
                self.mode = Mode::Normal;
                self.memory = if self.memory.is_some() {
                    None
                } else {
                    Some(memory::MemView::open())
                };
                self.dirty = true;
                return;
            }
            _ => {}
        }
        if self.working_on.is_some() && !matches!(cmd, Cmd::Quit | Cmd::Group) {
            return;
        }
        if matches!(cmd, Cmd::New) {
            let places = crate::places::frecent(&self.cfg, now_ms());
            self.mode = Mode::New(NewFlow {
                places,
                query: String::new(),
                sel: 0,
                folder: None,
            });
            return;
        }
        if matches!(cmd, Cmd::Help) {
            self.mode = Mode::Help;
            return;
        }
        if matches!(self.mode, Mode::Help) {
            self.mode = Mode::Normal;
            return;
        }
        if matches!(cmd, Cmd::Accounts) {
            self.mode = Mode::Accounts(AccountsView::new(&self.cfg, 0));
            return;
        }
        if matches!(
            cmd,
            Cmd::AccountAdd
                | Cmd::AccountLogin
                | Cmd::AccountShare
                | Cmd::AccountUnshare
                | Cmd::AccountRemove
        ) {
            let index = match &self.mode {
                Mode::Accounts(view) => view.sel,
                _ => return,
            };
            match cmd {
                Cmd::AccountAdd => {
                    if let Mode::Accounts(view) = &mut self.mode {
                        view.flow = Some(AddFlow::new(&self.cfg));
                    }
                }
                Cmd::AccountLogin => self.start_account_login(index),
                Cmd::AccountShare => self.open_account_share(index),
                Cmd::AccountUnshare => self.request_account_unshare(index),
                Cmd::AccountRemove => self.request_account_remove(index),
                _ => {}
            }
            return;
        }
        if matches!(cmd, Cmd::Sweep) {
            let stale: Vec<(i32, bool)> = self.stale().into_iter().map(|pid| (pid, true)).collect();
            if stale.is_empty() {
                self.say(
                    format!("nothing has been idle for {}+ days", self.cfg.stale_days),
                    self.pal.dim,
                );
            } else {
                self.mode = Mode::Sweep(stale);
                self.rebuild();
            }
            return;
        }
        if let Mode::Sweep(list) = &mut self.mode {
            match cmd {
                Cmd::Toggle => {
                    if let Some(e) = list.iter_mut().find(|(pid, _)| Some(*pid) == self.sel) {
                        e.1 = !e.1;
                    }
                }
                Cmd::All => {
                    let all = list.iter().all(|(_, on)| *on);
                    list.iter_mut().for_each(|e| e.1 = !all);
                }
                Cmd::No => {
                    self.mode = Mode::Normal;
                    self.rebuild();
                }
                Cmd::Yes => {
                    let pids: Vec<i32> =
                        list.iter().filter(|(_, on)| *on).map(|(p, _)| *p).collect();
                    self.mode = Mode::Normal;
                    self.rebuild();
                    if !pids.is_empty() {
                        let pinned = pids
                            .iter()
                            .filter(|p| {
                                self.sessions
                                    .iter()
                                    .any(|s| s.pid == **p && s.pin.is_some())
                            })
                            .count();
                        let note = if pinned > 0 {
                            " pinned ones stay pinned and can be reopened."
                        } else {
                            ""
                        };
                        let n = pids.len();
                        let prompt = format!(
                            "close {n} idle session{}?{note}",
                            if n == 1 { "" } else { "s" }
                        );
                        self.mode = Mode::Confirm(prompt, Pending::Many(Batch::Close, pids));
                    }
                }
                Cmd::Jump => {}
                _ => {}
            }
            return;
        }
        if let Mode::Offer(..) = self.mode {
            let Mode::Offer(_, one, _, all) = std::mem::replace(&mut self.mode, Mode::Normal)
            else {
                return;
            };
            let account_sel = match (&one, &all) {
                (Pending::Account(AccountPending::Remove { index, .. }), _)
                | (_, Pending::Account(AccountPending::Remove { index, .. })) => Some(*index),
                _ => None,
            };
            match cmd {
                Cmd::Yes => self.run_pending(one),
                Cmd::All => self.run_pending(all),
                _ => {
                    if let Some(sel) = account_sel {
                        self.mode = Mode::Accounts(AccountsView::new(&self.cfg, sel));
                    }
                }
            }
            return;
        }
        if matches!(self.mode, Mode::Confirm(_, Pending::Account(_))) {
            let Mode::Confirm(_, pending) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                return;
            };
            let sel = match &pending {
                Pending::Account(AccountPending::Share { index, .. })
                | Pending::Account(AccountPending::Unshare { index, .. })
                | Pending::Account(AccountPending::Remove { index, .. }) => *index,
                _ => 0,
            };
            match cmd {
                Cmd::Yes => self.run_pending(pending),
                _ => self.mode = Mode::Accounts(AccountsView::new(&self.cfg, sel)),
            }
            return;
        }
        if let Mode::New(flow) = &mut self.mode {
            match (cmd, flow.folder.clone()) {
                (Cmd::No, Some(_)) => flow.folder = None,
                (Cmd::No, None) => self.mode = Mode::Normal,
                (Cmd::Yes, None) => {
                    if let Some(place) = flow.shown().get(flow.sel) {
                        let (acct, why) =
                            crate::places::account_for(&self.cfg, &place.path, &self.sessions);
                        flow.folder = Some((place.path.clone(), acct, why));
                    }
                }
                (Cmd::Yes, Some((folder, acct, _))) => self.start_in(folder, acct),
                (Cmd::Pick(n), Some((folder, _, _))) => self.start_in(folder, n),
                _ => {}
            }
            return;
        }
        let Some(s) = self.selected().cloned() else {
            if matches!(cmd, Cmd::Quit) {
                self.quit = true;
            }
            if matches!(cmd, Cmd::Group) {
                self.group = self.group.next();
                self.rebuild();
            }
            return;
        };
        match cmd {
            Cmd::New
            | Cmd::All
            | Cmd::Sweep
            | Cmd::Toggle
            | Cmd::Usage(_)
            | Cmd::UsageToggle
            | Cmd::MemoryToggle
            | Cmd::Accounts
            | Cmd::AccountAdd
            | Cmd::AccountLogin
            | Cmd::AccountShare
            | Cmd::AccountUnshare
            | Cmd::AccountRemove
            | Cmd::MenuPick(_) => {}
            Cmd::Help => self.mode = Mode::Help,
            Cmd::RestoreAll => {
                let all: Vec<Session> = self
                    .sessions
                    .iter()
                    .filter(|x| x.restore.is_some())
                    .cloned()
                    .collect();
                if all.is_empty() {
                    return;
                }
                let mut ok = Vec::new();
                let mut failed = Vec::new();
                for x in &all {
                    match actions::revive(&self.cfg, x) {
                        Ok(_) => ok.push(x.id.clone()),
                        Err(e) => failed.push(format!("{}: {e}", x.title)),
                    }
                }
                crate::snapshot::forget(&ok);
                if failed.is_empty() {
                    self.say(
                        format!("reopened {} sessions where they were", ok.len()),
                        self.pal.finished,
                    );
                } else {
                    self.say(
                        format!("reopened {} · {}", ok.len(), failed.join(" · ")),
                        self.pal.attention,
                    );
                }
                self.reload();
            }
            Cmd::Close if s.restore.is_some() => {
                crate::snapshot::forget(std::slice::from_ref(&s.id));
                self.say(format!("won't offer {} again", s.title), self.pal.dim);
                self.reload();
            }
            Cmd::Quit => self.quit = true,
            Cmd::Group => {
                self.group = self.group.next();
                self.rebuild();
            }
            Cmd::JumpPin(n) => {
                let Some(t) = self.sessions.iter().find(|x| x.pin == Some(n)).cloned() else {
                    self.say(format!("nothing is pinned to alt-{}", n + 1), self.pal.dim);
                    return;
                };
                self.sel = Some(t.pid);
                self.exec(Cmd::Jump);
            }
            Cmd::Pin => {
                if s.pin.is_some() {
                    match State::update(|st| st.unpin(&s.id)) {
                        Ok(_) => self.say(format!("unpinned {}", s.title), self.pal.dim),
                        Err(e) => {
                            self.say(format!("couldn't update pins: {e}"), self.pal.attention)
                        }
                    }
                } else {
                    match State::update(|st| st.pin(s.pin_record(&self.cfg))) {
                        Ok(Some(n)) => self.say(
                            format!(
                                "pinned {} · alt-{} jumps to it from anywhere",
                                s.title,
                                n + 1
                            ),
                            self.pal.dim,
                        ),
                        Ok(None) => self.say(
                            "all nine pins are taken · ctrl-p on one to free it".into(),
                            self.pal.dim,
                        ),
                        Err(e) => {
                            self.say(format!("couldn't update pins: {e}"), self.pal.attention)
                        }
                    }
                }
                self.reload();
            }
            Cmd::Rename => {
                if s.dormant {
                    self.say("reopen it first (enter), then rename".into(), self.pal.dim);
                } else {
                    self.mode = Mode::Rename(s.title.clone());
                }
            }
            Cmd::Jump if s.dormant => {
                let revived = actions::revive(&self.cfg, &s);
                if revived.is_ok() && s.restore.is_some() {
                    crate::snapshot::forget(std::slice::from_ref(&s.id));
                }
                match revived {
                    Ok(pane) if tmux::inside() || self.shell.is_some() => self.go(&pane),
                    Ok(pane) => {
                        self.attach_pane = Some(pane);
                        self.quit = true;
                    }
                    Err(e) => self.say(
                        format!("couldn't reopen {}: {e}", s.title),
                        self.pal.attention,
                    ),
                }
            }
            Cmd::Jump => {
                if s.pane.is_none() {
                    self.say(
                        format!("{} runs outside tmux · ctrl-o brings it in", s.title),
                        self.pal.dim,
                    );
                } else if tmux::inside() || self.shell.is_some() {
                    let pane = s.pane.as_ref().map(|p| p.id.clone()).unwrap_or_default();
                    self.go(&pane);
                } else {
                    self.attach = Some(s);
                    self.quit = true;
                }
            }
            Cmd::GoThere => match &s.pane {
                _ if s.dormant => self.say(
                    format!("{} isn't running · enter reopens it", s.title),
                    self.pal.dim,
                ),
                None => self.say(
                    format!("{} runs outside tmux · ctrl-o brings it in", s.title),
                    self.pal.dim,
                ),
                Some(p) => match actions::jump_client(
                    &p.id,
                    self.shell.as_ref().and_then(|sh| sh.origin.as_deref()),
                ) {
                    Ok(()) => self.quit = true,
                    Err(e) => self.say(e.to_string(), self.pal.attention),
                },
            },
            Cmd::Account if s.queued.is_some() => {
                let to = s.queued.clone().unwrap_or_default();
                self.mode = Mode::Confirm(
                    format!("{} is waiting to move to {to}. cancel that?", s.title),
                    Pending::Unqueue(s.pid),
                );
            }
            Cmd::Account => {
                let opts: Vec<usize> = (0..self.cfg.accounts.len())
                    .filter(|&i| Some(i) != s.account)
                    .collect();
                match opts.as_slice() {
                    [] => self.say("only one account is configured".into(), self.pal.dim),
                    [one] => self.confirm_switch(&s, *one),
                    _ => self.mode = Mode::Choose(opts),
                }
            }
            Cmd::Pick(n) => {
                if let Mode::Choose(opts) = &self.mode {
                    let account = opts[n];
                    self.confirm_switch(&s, account);
                }
            }
            Cmd::Adopt => {
                if s.pane.is_some() {
                    self.say(format!("{} is already in tmux", s.title), self.pal.dim);
                } else if !s.can_move() && s.state != St::Background {
                    self.say(
                        format!(
                            "{} is {} · bring it in once it's idle",
                            s.title,
                            s.state.section()
                        ),
                        self.pal.dim,
                    );
                } else {
                    let bg = if s.state == St::Background {
                        " its background tasks will stop."
                    } else {
                        ""
                    };
                    let p = format!(
                        "bring {} into tmux? it restarts on the same conversation in a tmux server of its own.{bg}",
                        s.title
                    );
                    self.mode = Mode::Confirm(p, Pending::Adopt(s.pid));
                }
            }
            Cmd::Close => {
                let warn = if s.is_idle() {
                    ""
                } else {
                    " it's still working and will be interrupted."
                };
                self.mode =
                    Mode::Confirm(format!("close {}?{warn}", s.title), Pending::Close(s.pid));
            }
            Cmd::No => {
                self.mode = Mode::Normal;
                if let Some(sh) = self.shell.as_mut().filter(|sh| sh.has_live()) {
                    sh.focus_live = true;
                }
            }
            Cmd::Yes if matches!(self.mode, Mode::Rename(_)) => {
                let Mode::Rename(buf) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return;
                };
                match actions::rename(&self.cfg, &s, &buf, false) {
                    Ok(m) => self.say(m, self.pal.dim),
                    Err(e) => self.say(e.to_string(), self.pal.attention),
                }
                self.reload();
            }
            Cmd::Yes => {
                let Mode::Confirm(_, pending) = std::mem::replace(&mut self.mode, Mode::Normal)
                else {
                    return;
                };
                self.run_pending(pending);
            }
        }
    }

    fn start_in(&mut self, folder: String, account: usize) {
        self.mode = Mode::Normal;
        let args = crate::places::new_args(&self.cfg, &self.sessions);
        match actions::start(&self.cfg, &folder, account, &args) {
            Ok(pane) if tmux::inside() || self.shell.is_some() => self.go(&pane),
            Ok(pane) => {
                self.attach_pane = Some(pane);
                self.quit = true;
            }
            Err(e) => self.say(format!("couldn't start a session: {e}"), self.pal.attention),
        }
    }

    /// Jump to a pane. The popup closes; the sidebar stays and comes along;
    /// the shell shows it live and hands it the keyboard.
    fn go(&mut self, pane: &str) {
        if self.shell.is_some() {
            self.show_live(pane);
            let sh = self.shell_mut();
            sh.focus_live = true;
            self.usage_view = None;
            if !self.filter.is_empty() {
                self.filter.clear();
                self.rebuild();
            }
            return;
        }
        let r = if self.sidebar {
            actions::follow(pane)
        } else {
            actions::jump_pane(pane)
        };
        match r {
            Ok(()) if !self.sidebar => self.quit = true,
            Ok(()) => {}
            Err(e) => self.say(e.to_string(), self.pal.attention),
        }
    }

    fn confirm_switch(&mut self, s: &Session, account: usize) {
        let to = &self.cfg.accounts[account].name;
        let (prompt, wait, force) = if let Some(l) = &s.limit {
            (
                format!(
                    "move {} to {to}? it hit {l} on {}",
                    s.title,
                    s.account_name(&self.cfg)
                ),
                false,
                false,
            )
        } else if s.is_idle() {
            (
                format!(
                    "move {} to {to}? it restarts on the same conversation",
                    s.title
                ),
                false,
                false,
            )
        } else if s.state == St::Background {
            (
                format!(
                    "{} has background tasks running · moving it to {to} stops them. move anyway?",
                    s.title
                ),
                false,
                true,
            )
        } else {
            (
                format!(
                    "{} is {} · move it to {to} as soon as it goes idle?",
                    s.title,
                    s.state.section()
                ),
                true,
                false,
            )
        };
        let one = Pending::Switch {
            pid: s.pid,
            account,
            wait,
            force,
        };
        let limited: Vec<i32> = self
            .sessions
            .iter()
            .filter(|x| {
                !x.dormant
                    && x.limit.is_some()
                    && x.account == s.account
                    && x.account != Some(account)
            })
            .map(|x| x.pid)
            .collect();
        if s.limit.is_some() && limited.len() > 1 {
            let label = format!("all {} limited", limited.len());
            self.mode = Mode::Offer(
                prompt,
                one,
                label,
                Pending::Many(Batch::Move(account), limited),
            );
        } else {
            self.mode = Mode::Confirm(prompt, one);
        }
    }

    /// Live sessions idle for at least stale_days.
    fn stale(&self) -> Vec<i32> {
        let cutoff = now_ms() - self.cfg.stale_days as i64 * 86_400_000;
        self.sessions
            .iter()
            .filter(|s| !s.dormant && s.is_idle() && s.since_ms < cutoff)
            .map(|s| s.pid)
            .collect()
    }

    fn run_pending(&mut self, p: Pending) {
        if let Pending::Account(op) = p {
            self.run_account_pending(op);
            return;
        }
        if let Pending::Many(batch, pids) = p {
            let targets: Vec<Session> = pids
                .iter()
                .filter_map(|p| self.sessions.iter().find(|s| s.pid == *p).cloned())
                .collect();
            let (cfg, tx) = (self.cfg.clone(), self.done_tx.clone());
            let n = targets.len();
            self.working_on = Some(match batch {
                Batch::Move(a) => format!("moving {n} sessions to {}", self.cfg.accounts[a].name),
                Batch::Close => format!("closing {n} sessions"),
            });
            std::thread::spawn(move || {
                let mut failed = Vec::new();
                for s in &targets {
                    let r = match batch {
                        Batch::Move(a) => actions::switch(&cfg, s, a, false, false),
                        Batch::Close => actions::close(&cfg, s),
                    };
                    if let Err(e) = r {
                        failed.push(format!("{}: {e}", s.title));
                    }
                }
                let done = n - failed.len();
                let verb = match batch {
                    Batch::Move(a) => format!("moved to {}", cfg.accounts[a].name),
                    Batch::Close => "closed".to_string(),
                };
                let _ = tx.send(if failed.is_empty() {
                    Ok(format!("{done} sessions {verb}"))
                } else {
                    Err(format!("{done} of {n} {verb} · {}", failed.join(" · ")))
                });
            });
            return;
        }
        let pid = match &p {
            Pending::Switch { pid, .. }
            | Pending::Adopt(pid)
            | Pending::Close(pid)
            | Pending::Unqueue(pid) => *pid,
            Pending::Many(..) => unreachable!(),
            Pending::Account(_) => unreachable!(),
        };
        let Some(s) = self.sessions.iter().find(|x| x.pid == pid).cloned() else {
            return;
        };
        if let Pending::Unqueue(_) = p {
            if crate::queue::cancel(s.pid, s.proc_start.as_deref()) {
                self.say(
                    format!("{} stays on {}", s.title, s.account_name(&self.cfg)),
                    self.pal.dim,
                );
            }
            self.reload();
            return;
        }
        if let Pending::Switch {
            account,
            wait: true,
            ..
        } = p
        {
            let to = self.cfg.accounts[account].name.clone();
            match actions::switch_later(&s, &to) {
                Ok(()) => self.say(
                    format!("{} will move to {to} when it goes idle", s.title),
                    self.pal.dim,
                ),
                Err(e) => self.say(e.to_string(), self.pal.attention),
            }
            return;
        }
        let (label, cfg, tx) = (s.title.clone(), self.cfg.clone(), self.done_tx.clone());
        self.working_on = Some(match &p {
            Pending::Switch { account, .. } => {
                format!("moving {label} to {}", self.cfg.accounts[*account].name)
            }
            Pending::Adopt(_) => format!("bringing {label} into tmux"),
            Pending::Close(_) => format!("closing {label}"),
            Pending::Unqueue(_) | Pending::Many(..) | Pending::Account(_) => unreachable!(),
        });
        std::thread::spawn(move || {
            let r = match p {
                Pending::Switch { account, force, .. } => {
                    actions::switch(&cfg, &s, account, false, force)
                }
                Pending::Adopt(_) => actions::adopt(&cfg, &s),
                Pending::Close(_) => actions::close(&cfg, &s),
                Pending::Unqueue(_) | Pending::Many(..) | Pending::Account(_) => unreachable!(),
            };
            let _ = tx.send(r.map_err(|e| e.to_string()));
        });
    }

    // ---- view --------------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        self.cmd_hits.clear();
        self.list_hits.clear();
        let full = f.area();
        let (base, raised, well) = (self.pal.base, self.pal.raised, self.pal.well);
        f.render_widget(
            Block::new().style(Style::new().bg(base).fg(self.pal.text)),
            full,
        );
        let (foot_lines, foot_hits) = self.foot(full.width);
        let fh = (foot_lines.len() as u16)
            .min(full.height.saturating_sub(2))
            .max(1);
        let head_rows = self.head_lines(full.width);
        let hh = (head_rows.len() as u16)
            .min(full.height.saturating_sub(fh + 2))
            .max(1);
        let [head, rule_top, body, rule_bottom, foot] = Layout::vertical([
            Constraint::Length(hh),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(fh),
        ])
        .areas(full);
        // The header and footer are bands of their own; the list sits on the
        // base, and the preview is a recessed well beside it.
        f.render_widget(Block::new().style(Style::new().bg(raised)), head);
        f.render_widget(Block::new().style(Style::new().bg(raised)), foot);
        for (row, (line, hits)) in head_rows.into_iter().enumerate().take(hh as usize) {
            let y = head.y + row as u16;
            f.render_widget(
                Paragraph::new(line),
                Rect {
                    y,
                    height: 1,
                    ..head
                },
            );
            for (x, w, cmd) in hits {
                self.cmd_hits.push((
                    Rect {
                        x: head.x + x,
                        y,
                        width: w,
                        height: 1,
                    },
                    cmd,
                ));
            }
        }
        // Hairlines close off the header and footer, meeting the divider.
        let mut divider_x: Option<u16> = None;
        if matches!(self.mode, Mode::Accounts(_)) {
            self.draw_accounts(f, body);
        } else if matches!(self.mode, Mode::New(_)) {
            self.draw_new_head(f, head);
            if self.sidebar {
                self.draw_places(f, body);
            } else {
                let [list, _, preview] = Layout::horizontal([
                    Constraint::Percentage(42),
                    Constraint::Length(2),
                    Constraint::Fill(1),
                ])
                .areas(body);
                self.draw_places(f, list);
                self.draw_place_preview(f, preview);
            }
        } else if self.sidebar {
            match self.usage_view {
                Some(i) => self.draw_usage(f, body, i),
                None => self.draw_list(f, body),
            }
        } else {
            let [list, gap, preview] = Layout::horizontal([
                Constraint::Percentage(42),
                Constraint::Length(2),
                Constraint::Fill(1),
            ])
            .areas(body);
            divider(f, Rect { width: 1, ..gap }, self.pal.frame, self.pal.base);
            divider_x = Some(gap.x);
            self.draw_list(f, list);
            match self.usage_view {
                Some(i) => self.draw_usage(f, preview, i),
                None => self.draw_preview(f, preview),
            }
        }
        let (fc, bc) = (self.pal.frame, self.pal.base);
        if rule_top.height > 0 {
            let j: Vec<(u16, &str)> = divider_x.map(|x| (x, "┬")).into_iter().collect();
            hrule(f, rule_top, fc, bc, &j);
        }
        if rule_bottom.height > 0 {
            let j: Vec<(u16, &str)> = divider_x.map(|x| (x, "┴")).into_iter().collect();
            hrule(f, rule_bottom, fc, bc, &j);
        }
        let hint_y = foot.y + foot.height.saturating_sub(1);
        for (x, w, cmd) in foot_hits {
            self.cmd_hits.push((
                Rect {
                    x: foot.x + x,
                    y: hint_y,
                    width: w,
                    height: 1,
                },
                cmd,
            ));
        }
        let skip = foot_lines.len().saturating_sub(foot.height as usize);
        f.render_widget(
            Paragraph::new(Text::from(
                foot_lines.into_iter().skip(skip).collect::<Vec<_>>(),
            )),
            foot,
        );

        // Overlays: everything behind them recedes, and only they take clicks.
        if matches!(self.mode, Mode::Help | Mode::Menu(_)) {
            recede(f.buffer_mut(), full, well, 0.62, &self.pal);
            self.cmd_hits.clear();
            self.list_hits.clear();
            match self.mode {
                Mode::Help => self.draw_help(f, body),
                _ => self.draw_menu(f, full),
            }
        }
        // Hover: whatever the pointer rests on that can be clicked lights up.
        if let Some((hx, hy)) = self.hover {
            let hit = self
                .cmd_hits
                .iter()
                .find(|(r, _)| hx >= r.x && hx < r.x + r.width && hy >= r.y && hy < r.y + r.height);
            if let Some((r, cmd)) = hit {
                let menu = matches!(cmd, Cmd::MenuPick(_));
                let buf = f.buffer_mut();
                for x in r.x..(r.x + r.width).min(buf.area.width) {
                    let c = &mut buf[(x, r.y)];
                    if menu {
                        c.bg = self.pal.hover;
                    } else if !c.symbol().trim().is_empty() {
                        c.fg = self.pal.accent;
                    }
                }
            }
        }
    }

    fn draw_new_head(&mut self, f: &mut Frame, area: Rect) {
        let p = &self.pal;
        let Mode::New(flow) = &self.mode else { return };
        let mut spans = vec![Span::raw(" ")];
        if !self.sidebar {
            spans.push(Span::styled(
                "toomux",
                Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("   new session", Style::new().fg(p.text)));
            spans.push(Span::raw("   "));
        }
        if flow.query.is_empty() {
            spans.push(Span::styled(
                "type to filter, or a path",
                Style::new().fg(p.muted),
            ));
        } else {
            spans.extend([
                Span::styled("folder ", Style::new().fg(p.dim)),
                Span::styled(flow.query.clone(), Style::new().fg(p.text)),
                Span::styled("▏", Style::new().fg(p.accent)),
            ]);
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_places(&mut self, f: &mut Frame, area: Rect) {
        let p = &self.pal;
        let Mode::New(flow) = &self.mode else { return };
        let now = now_ms();
        let w = area.width as usize;
        let shown = flow.shown();
        if shown.is_empty() {
            let msg = if flow.query.is_empty() {
                " no folders in your history yet · type a path"
            } else {
                " no folder matches"
            };
            f.render_widget(
                Paragraph::new(Span::styled(msg, Style::new().fg(p.dim))),
                area,
            );
            return;
        }
        let chosen = flow.folder.as_ref().map(|(f, _, _)| f.clone());
        let mut items: Vec<(usize, Vec<Line<'static>>)> = Vec::new();
        for (i, pl) in shown.iter().enumerate() {
            let selected = i == flow.sel;
            let bar = if selected {
                Span::styled("▎", Style::new().fg(p.accent))
            } else {
                Span::raw(" ")
            };
            let mut style = Style::new().fg(p.text);
            if selected {
                style = style.add_modifier(Modifier::BOLD);
            }
            let mut lines = Vec::new();
            for chunk in wrap_chips(
                &path_segments(&tilde(&pl.path), style),
                w.saturating_sub(4),
                w.saturating_sub(4),
            ) {
                let mut l = vec![bar.clone(), Span::raw("   ")];
                l.extend(chunk);
                lines.push(Line::from(l));
            }
            let here = self
                .sessions
                .iter()
                .filter(|s| !s.dormant && s.cwd == pl.path)
                .count();
            let mut detail = Vec::new();
            if pl.last_ms > 0 {
                detail.push((
                    format!("last used {} ago", ago(now - pl.last_ms)),
                    Style::new().fg(p.dim),
                ));
                detail.push((" · ".to_string(), Style::new().fg(p.muted)));
                detail.push((format!("{} prompts", pl.prompts), Style::new().fg(p.muted)));
            } else {
                detail.push(("new to claude".to_string(), Style::new().fg(p.dim)));
            }
            if here > 0 {
                detail.push((" · ".to_string(), Style::new().fg(p.muted)));
                detail.push((format!("{here} running here"), Style::new().fg(p.working)));
            }
            if chosen.as_deref() == Some(&pl.path) {
                detail.push((" · ".to_string(), Style::new().fg(p.muted)));
                detail.push(("chosen".to_string(), Style::new().fg(p.accent)));
            }
            for chunk in wrap_chips(&detail, w.saturating_sub(4), w.saturating_sub(4)) {
                let mut l = vec![bar.clone(), Span::raw("   ")];
                l.extend(chunk);
                lines.push(Line::from(l));
            }
            items.push((i, lines));
        }
        // Keep the selection in view.
        let heights: Vec<u16> = items.iter().map(|(_, l)| l.len() as u16 + 1).collect();
        let top_of = |i: usize| heights[..i].iter().sum::<u16>();
        let (top, bottom) = (top_of(flow.sel), top_of(flow.sel) + heights[flow.sel]);
        let mut scroll = 0u16;
        if bottom > area.height {
            scroll = bottom - area.height;
        }
        let _ = top;
        let mut y = 0u16;
        for (i, lines) in items {
            let h = lines.len() as u16;
            if y + h > scroll && y < scroll + area.height {
                let top = y.max(scroll);
                let vis = (y + h).min(scroll + area.height) - top;
                let r = Rect {
                    x: area.x,
                    y: area.y + top - scroll,
                    width: area.width,
                    height: vis,
                };
                let mut para = Paragraph::new(Text::from(lines)).scroll((top - y, 0));
                if i == flow.sel {
                    para = para.style(Style::new().bg(p.selection));
                }
                f.render_widget(para, r);
                self.list_hits.push((r, i as i32));
            }
            y += h + 1;
        }
    }

    fn draw_place_preview(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(Block::new().style(Style::new().bg(self.pal.well)), area);
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(1),
        };
        let p = &self.pal;
        let Mode::New(flow) = &self.mode else { return };
        let shown = flow.shown();
        let Some(pl) = shown.get(flow.sel) else {
            return;
        };
        let w = inner.width as usize;
        let now = now_ms();
        let mut lines: Vec<Line<'static>> = Vec::new();
        for chunk in wrap_chips(
            &path_segments(
                &tilde(&pl.path),
                Style::new().fg(p.text).add_modifier(Modifier::BOLD),
            ),
            w,
            w,
        ) {
            lines.push(Line::from(chunk));
        }
        let (acct, why) = crate::places::account_for(&self.cfg, &pl.path, &self.sessions);
        let args = crate::places::new_args(&self.cfg, &self.sessions);
        lines.push(Line::from(vec![
            Span::styled(
                format!("starts on {}", self.cfg.accounts[acct].name),
                Style::new().fg(p.dim),
            ),
            Span::styled(format!(" · {why}"), Style::new().fg(p.muted)),
        ]));
        if !args.is_empty() {
            for chunk in wrap_spans(
                &[(
                    format!("claude {}", args.join(" ")),
                    Style::new().fg(p.muted),
                )],
                w,
                w,
            ) {
                lines.push(Line::from(chunk));
            }
        }
        lines.push(Line::raw(""));
        let here: Vec<&Session> = self
            .sessions
            .iter()
            .filter(|s| !s.dormant && s.cwd == pl.path)
            .collect();
        if !here.is_empty() {
            lines.push(Line::from(Span::styled(
                "running here",
                Style::new().fg(p.dim),
            )));
            for s in here {
                let (g, c) = p.state(s.state);
                let mut segs = vec![
                    (format!("{g} "), Style::new().fg(c)),
                    (s.title.clone(), Style::new().fg(p.text)),
                ];
                segs.push((format!("  {}", s.status_text(now)), Style::new().fg(p.dim)));
                lines.extend(
                    wrap_spans(&segs, w, w.saturating_sub(2))
                        .into_iter()
                        .map(Line::from),
                );
            }
            lines.push(Line::raw(""));
        }
        if !pl.recent.is_empty() {
            lines.push(Line::from(Span::styled(
                "recent prompts here",
                Style::new().fg(p.dim),
            )));
            for r in &pl.recent {
                let first = r
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let chunks = wrap_spans(
                    &[(first, Style::new().fg(p.text))],
                    w.saturating_sub(2),
                    w.saturating_sub(2),
                );
                let n = chunks.len();
                for (i, chunk) in chunks.into_iter().take(3).enumerate() {
                    let mut l = vec![Span::styled(
                        if i == 0 { "· " } else { "  " },
                        Style::new().fg(p.muted),
                    )];
                    l.extend(chunk);
                    if i == 2 && n > 3 {
                        l.push(Span::styled(" …", Style::new().fg(p.muted)));
                    }
                    lines.push(Line::from(l));
                }
            }
        }
        f.render_widget(Paragraph::new(Text::from(lines)), inner);
    }

    fn draw_help(&mut self, f: &mut Frame, area: Rect) {
        let p = &self.pal;
        let groups: [(&str, &[(&str, &str)]); 3] = [
            (
                "in toomux",
                &[
                    ("type", "filter by title, folder, account or state"),
                    ("enter", "jump to it · reopen it if it isn't running"),
                    ("^a", "move to another account, same conversation"),
                    ("^o", "bring a plain-terminal session into tmux"),
                    ("^x", "close it"),
                    ("^r", "rename it (here, in tmux and in claude)"),
                    ("^p", "pin it to alt-1..9 · again to unpin"),
                    ("^n", "start a new session in a recent folder"),
                    ("^u", "usage: each account's 5-hour and weekly limits"),
                    ("alt-a", "accounts: sign in, add, share, unshare or remove"),
                    ("^s", "clean up sessions idle for days"),
                    ("^e", "reopen what was running before a restart"),
                    ("tab", "group by attention, account or project"),
                    ("right-click", "everything you can do with a session"),
                ],
            ),
            if self.shell.as_ref().is_some_and(|sh| sh.popup) {
                (
                    "full screen",
                    &[
                        ("alt-s", "between the session and the list"),
                        ("alt-j", "go there: close toomux on that session"),
                        ("alt-b", "hide or show the list"),
                        ("alt-u", "usage in place of the session"),
                        ("alt-1..9", "a pin"),
                        ("shift-drag", "select text in your terminal"),
                        ("^c", "close toomux (in the list); sessions keep running"),
                    ],
                )
            } else if self.shell.is_some() {
                (
                    "full screen",
                    &[
                        ("alt-s", "between the session and the list"),
                        ("alt-b", "hide or show the list"),
                        ("alt-u", "usage in place of the session"),
                        ("alt-1..9", "a pin"),
                        ("shift-drag", "select text in your terminal"),
                        ("^c", "leave toomux (in the list); sessions keep running"),
                    ],
                )
            } else {
                (
                    "anywhere in tmux",
                    &[
                        ("alt-s", "open toomux"),
                        ("alt-b", "show or hide the sidebar"),
                        ("alt-1..9", "jump to a pin"),
                    ],
                )
            },
            ("", &[("any key", "close this")]),
        ];
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (title, keys) in groups {
            if !title.is_empty() {
                lines.push(Line::from(Span::styled(
                    title.to_string(),
                    Style::new().fg(p.muted),
                )));
            }
            for (k, what) in keys {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {k:<13}"), Style::new().fg(p.text)),
                    Span::styled(what.to_string(), Style::new().fg(p.dim)),
                ]));
            }
            lines.push(Line::raw(""));
        }
        lines.pop();
        let h = (lines.len() as u16 + 4).min(area.height);
        let w = (lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16 + 8).min(area.width);
        let r = Rect {
            x: area.x + (area.width - w) / 2,
            y: area.y + (area.height - h) / 2,
            width: w,
            height: h,
        };
        f.render_widget(ratatui::widgets::Clear, r);
        let block = Block::bordered()
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::new().fg(p.muted).bg(p.overlay))
            .style(Style::new().bg(p.overlay))
            .padding(ratatui::widgets::Padding::new(3, 3, 1, 1));
        let inner = block.inner(r);
        f.render_widget(block, r);
        f.render_widget(Paragraph::new(Text::from(lines)), inner);
    }

    /// The right-click menu, beside the pointer and kept on screen.
    fn draw_menu(&mut self, f: &mut Frame, full: Rect) {
        let p = &self.pal;
        let Mode::Menu(m) = &self.mode else { return };
        let title = self
            .sessions
            .iter()
            .find(|s| s.pid == m.pid)
            .map(|s| s.title.clone())
            .unwrap_or_default();
        let inner_w = m
            .items
            .iter()
            .map(|(l, c)| l.width() + key_for(c).width() + 3)
            .max()
            .unwrap_or(0)
            .max(20)
            + 3;
        let title_lines = wrap_spans(&[(title, Style::new().fg(p.dim))], inner_w, inner_w);
        let w = (inner_w as u16 + 2).min(full.width);
        let h = (m.items.len() as u16 + title_lines.len() as u16 + 3).min(full.height);
        let x = m.at.0.saturating_add(1).min(full.width.saturating_sub(w));
        let y = m.at.1.min(full.height.saturating_sub(h));
        let r = Rect {
            x,
            y,
            width: w,
            height: h,
        };
        f.render_widget(ratatui::widgets::Clear, r);
        let block = Block::bordered()
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::new().fg(p.muted).bg(p.overlay))
            .style(Style::new().bg(p.overlay));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let mut lines: Vec<Line<'static>> = title_lines
            .into_iter()
            .map(|l| {
                let mut l = l;
                l.insert(0, Span::raw(" "));
                Line::from(l)
            })
            .collect();
        lines.push(Line::from(Span::styled(
            format!(" {}", "─".repeat(inner_w.saturating_sub(1))),
            Style::new().fg(p.faint),
        )));
        let top = inner.y + lines.len() as u16;
        let mut hits = Vec::new();
        for (n, (label, cmd)) in m.items.iter().enumerate() {
            let on = n == m.sel;
            let row = Rect {
                x: inner.x,
                y: top + n as u16,
                width: inner.width,
                height: 1,
            };
            let mut style = Style::new().fg(if on { p.text } else { p.dim });
            let mut key_style = Style::new().fg(p.muted);
            if on {
                style = style.bg(p.selection);
                key_style = key_style.bg(p.selection);
            }
            let bar = if on {
                Span::styled("▎", Style::new().fg(p.accent).bg(p.selection))
            } else {
                Span::raw(" ")
            };
            let key = key_for(cmd);
            let fill = (inner.width as usize).saturating_sub(label.width() + key.width() + 3);
            lines.push(Line::from(vec![
                bar,
                Span::styled(format!(" {label}{}", " ".repeat(fill)), style),
                Span::styled(format!("{key} "), key_style),
            ]));
            if row.y < inner.y + inner.height {
                hits.push((row, Cmd::MenuPick(n)));
            }
        }
        f.render_widget(Paragraph::new(Text::from(lines)), inner);
        self.cmd_hits.extend(hits);
    }

    /// Header rows with their clickable parts as (x, width, command).
    /// The header carries usage and nothing else: state counts are the list's
    /// sections, and the filter lives on the list.
    fn head_lines(&self, width: u16) -> Vec<(Line<'static>, Vec<(u16, u16, Cmd)>)> {
        if matches!(self.mode, Mode::New(_)) {
            return vec![(Line::default(), Vec::new())];
        }
        if self.sidebar {
            return self.head_compact(width);
        }
        for bar in [10, 6, 0] {
            let (spans, hits) = self.head_chips(bar);
            let w: usize = spans.iter().map(|s| s.width()).sum();
            if w <= width as usize || bar == 0 {
                return vec![(Line::from(spans), hits)];
            }
        }
        unreachable!()
    }

    /// "toomux     northwind  5h ━━╾─────── 20%  wk ━───────── 10%     personal  wk at limit · back sat 5pm •"
    fn head_chips(&self, bar: usize) -> (Vec<Span<'static>>, Vec<(u16, u16, Cmd)>) {
        let p = &self.pal;
        let now = now_ms();
        let mut spans = vec![
            Span::styled(" toomux", Style::new().fg(p.accent)),
            Span::raw("      "),
        ];
        let mut hits = Vec::new();
        let mut first = true;
        for (i, a) in self.cfg.accounts.iter().enumerate() {
            let Some(u) = self
                .usage
                .get(i)
                .filter(|u| u.five.is_some() || u.week.is_some())
            else {
                continue;
            };
            if !first {
                spans.push(Span::raw("       "));
            }
            first = false;
            let x0: usize = spans.iter().map(|s| s.width()).sum();
            let viewing = self.usage_view == Some(i);
            spans.push(Span::styled(
                a.name.clone(),
                Style::new().fg(if viewing { p.accent } else { p.text }),
            ));
            let fresh = self.fresh_limit(i, now);
            // At a limit, only that window matters.
            let limited = [&u.five, &u.week].into_iter().flatten().any(|m| m.limited);
            for (label, m) in [("5h", &u.five), ("wk", &u.week)] {
                let Some(m) = m else { continue };
                if limited && !m.limited {
                    continue;
                }
                spans.push(Span::raw("  "));
                spans.extend(self.meter_spans(label, m, bar, u.stale(now), fresh, now));
            }
            let x1: usize = spans.iter().map(|s| s.width()).sum();
            hits.push((x0 as u16, (x1 - x0) as u16, Cmd::Usage(i)));
        }
        let unknown = self.sessions.iter().filter(|s| s.account.is_none()).count();
        if unknown > 0 {
            spans.push(Span::styled(
                format!("       other ○{unknown}"),
                Style::new().fg(p.muted),
            ));
        }
        (spans, hits)
    }

    /// A limit that hit in the last few minutes is news; after that it's a
    /// settled fact, drawn quietly.
    fn fresh_limit(&self, account: usize, now: i64) -> bool {
        self.sessions.iter().any(|s| {
            s.account == Some(account)
                && s.limit.is_some()
                && s.since_ms > 0
                && now - s.since_ms < 10 * 60_000
        })
    }

    /// One meter: "5h ━━━━╾───── 42%". At a limit: "wk at limit · back sat
    /// 5pm •", quiet, with one rose dot; rose throughout only when it's news.
    fn meter_spans(
        &self,
        label: &str,
        m: &crate::usage::Meter,
        bar: usize,
        stale: bool,
        fresh: bool,
        now: i64,
    ) -> Vec<Span<'static>> {
        let p = &self.pal;
        let mut out = vec![Span::styled(format!("{label} "), Style::new().fg(p.muted))];
        if m.limited && !fresh {
            out.push(Span::styled("at limit", Style::new().fg(p.dim)));
            if m.resets_ms > now {
                out.push(Span::styled(
                    format!(" · {}", registry::back_at(m.resets_ms, now)),
                    Style::new().fg(p.muted),
                ));
            }
            out.push(Span::styled(" •", Style::new().fg(p.attention)));
            return out;
        }
        let tone = if stale {
            p.muted
        } else {
            p.usage_tone(m.used, m.limited)
        };
        if bar > 0 {
            out.extend(self.bar_spans(if m.limited { 100.0 } else { m.used }, bar, tone));
            out.push(Span::raw(" "));
        }
        if m.limited {
            out.push(Span::styled("limit", Style::new().fg(p.attention_word)));
            if m.resets_ms > now {
                out.push(Span::styled(
                    format!(" · {}", registry::back_at(m.resets_ms, now)),
                    Style::new().fg(p.muted),
                ));
            }
        } else {
            out.push(Span::styled(
                format!("{:.0}%", m.used),
                Style::new().fg(tone),
            ));
        }
        out
    }

    /// A hairline meter: heavy rule for what's used, light rule for the rest,
    /// with half-cell precision.
    fn bar_spans(&self, used: f64, w: usize, color: Color) -> Vec<Span<'static>> {
        let p = &self.pal;
        let cells = (used.clamp(0.0, 100.0) / 100.0 * w as f64 * 2.0).round() as usize;
        let (full, half) = (cells / 2, cells % 2 == 1);
        let rest = w - full - usize::from(half);
        let track = mix(p.faint, p.muted, 0.3, p.faint);
        let mut out = vec![Span::styled("━".repeat(full), Style::new().fg(color))];
        if half {
            out.push(Span::styled("╾", Style::new().fg(color)));
        }
        out.push(Span::styled("─".repeat(rest), Style::new().fg(track)));
        out
    }

    /// Sidebar header: a usage row per account, then the tally (or the filter).
    fn head_compact(&self, width: u16) -> Vec<(Line<'static>, Vec<(u16, u16, Cmd)>)> {
        let p = &self.pal;
        let now = now_ms();
        let w = width as usize;
        let mut rows = Vec::new();
        let nw = self
            .cfg
            .accounts
            .iter()
            .map(|a| a.name.width())
            .max()
            .unwrap_or(0);
        for (i, a) in self.cfg.accounts.iter().enumerate() {
            let Some(u) = self
                .usage
                .get(i)
                .filter(|u| u.five.is_some() || u.week.is_some())
            else {
                continue;
            };
            let style = if self.usage_view == Some(i) {
                Style::new().fg(p.accent)
            } else {
                Style::new().fg(p.dim)
            };
            let mut line = Vec::new();
            for (bar, tight) in [(4, false), (0, false), (0, true)] {
                line = vec![
                    Span::raw(" "),
                    Span::styled(format!("{:<nw$}", a.name), style),
                ];
                // At the limit, only that window matters.
                let limited = [&u.five, &u.week].into_iter().flatten().find(|m| m.limited);
                let shown: Vec<(&str, &crate::usage::Meter)> = match limited {
                    Some(m) => vec![(
                        if u.week.as_ref().is_some_and(|w| w.limited) {
                            "wk"
                        } else {
                            "5h"
                        },
                        m,
                    )],
                    None => [("5h", &u.five), ("wk", &u.week)]
                        .into_iter()
                        .filter_map(|(l, m)| m.as_ref().map(|m| (l, m)))
                        .collect(),
                };
                let fresh = self.fresh_limit(i, now);
                for (label, m) in shown {
                    line.push(Span::raw("  "));
                    let mut spans = self.meter_spans(label, m, bar, u.stale(now), fresh, now);
                    if tight {
                        // Too narrow for when it lifts: the list says that.
                        spans.retain(|s| !s.content.starts_with(" · back"));
                    }
                    line.extend(spans);
                }
                if line.iter().map(|s| s.width()).sum::<usize>() <= w {
                    break;
                }
            }
            rows.push((Line::from(line), vec![(0, width, Cmd::Usage(i))]));
        }
        if rows.is_empty() {
            rows.push((
                Line::from(Span::styled(" toomux", Style::new().fg(p.accent))),
                Vec::new(),
            ));
        }
        rows
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        // The list's own line: the filter (or how to start one) and how it's
        // grouped, where filtering actually applies.
        let top = Rect {
            height: 1.min(area.height),
            ..area
        };
        self.draw_list_top(f, top);
        let area = Rect {
            y: area.y + 2,
            height: area.height.saturating_sub(2),
            ..area
        };
        let now = now_ms();
        let w = area.width.max(10) as usize;
        let sel = self.sel;

        // Lay every row out, then scroll so the selection is fully visible.
        struct Placed {
            y: u16,
            lines: Vec<Line<'static>>,
            pid: Option<i32>,
        }
        let mut placed: Vec<Placed> = Vec::new();
        let mut y: u16 = 0;
        for (n, row) in self.rows.iter().enumerate() {
            match row {
                Row::Header(title, count) => {
                    if n > 0 {
                        y += 1;
                    }
                    // Space, not rules, separates sections.
                    placed.push(Placed {
                        y,
                        lines: vec![Line::from(vec![
                            Span::styled(format!(" {title}"), Style::new().fg(self.pal.dim)),
                            Span::styled(format!("  {count}"), Style::new().fg(self.pal.muted)),
                        ])],
                        pid: None,
                    });
                    y += 1;
                }
                Row::Item(i) => {
                    let lines = self.item_lines(
                        &self.sessions[*i],
                        w,
                        now,
                        Some(self.sessions[*i].pid) == sel,
                    );
                    let h = lines.len() as u16;
                    placed.push(Placed {
                        y,
                        lines,
                        pid: Some(self.sessions[*i].pid),
                    });
                    y += h;
                }
            }
        }
        if let Some(p) = placed.iter().find(|p| p.pid.is_some() && p.pid == sel) {
            let (top, bottom) = (p.y, p.y + p.lines.len() as u16);
            if top < self.scroll {
                self.scroll = top.saturating_sub(1);
            } else if bottom > self.scroll + area.height {
                self.scroll = bottom - area.height;
            }
        }
        if y <= area.height {
            self.scroll = 0;
        }
        // Ease toward the new position rather than jumping; far moves snap
        // most of the way first so they never feel slow.
        let target = self.scroll as f32;
        let gap = target - self.shown_scroll;
        if gap.abs() > 8.0 {
            self.shown_scroll = target - gap.signum() * 8.0;
        }
        self.shown_scroll += (target - self.shown_scroll) * 0.45;
        if (target - self.shown_scroll).abs() < 0.5 {
            self.shown_scroll = target;
        }
        let scroll = self.shown_scroll.round().max(0.0) as u16;

        if placed.is_empty() {
            let p = &self.pal;
            let lines: Vec<Line> = if self.filter.is_empty() {
                vec![
                    Line::from(Span::styled(
                        " no claude sessions running",
                        Style::new().fg(p.dim),
                    )),
                    Line::raw(""),
                    Line::from(vec![
                        Span::styled(" ^n", Style::new().fg(p.text)),
                        Span::styled(" starts one in a recent folder", Style::new().fg(p.muted)),
                    ]),
                    Line::from(vec![
                        Span::styled(" alt-b", Style::new().fg(p.text)),
                        Span::styled(
                            " keeps this list beside your work",
                            Style::new().fg(p.muted),
                        ),
                    ]),
                ]
            } else {
                vec![Line::from(Span::styled(
                    " nothing matches · esc clears the filter",
                    Style::new().fg(p.dim),
                ))]
            };
            f.render_widget(Paragraph::new(Text::from(lines)), area);
            return;
        }
        let hover = self
            .hover
            .filter(|_| matches!(self.mode, Mode::Normal | Mode::Sweep(_)));
        for p in placed {
            let h = p.lines.len() as u16;
            if p.y + h <= scroll || p.y >= scroll + area.height {
                continue;
            }
            let top = p.y.max(scroll);
            let skip = top - p.y;
            let vis_h = (p.y + h).min(scroll + area.height) - top;
            let r = Rect {
                x: area.x,
                y: area.y + top - scroll,
                width: area.width,
                height: vis_h,
            };
            let mut para = Paragraph::new(Text::from(p.lines)).scroll((skip, 0));
            let hovered = hover.is_some_and(|(x, y)| {
                x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
            });
            if p.pid.is_some() && p.pid == sel {
                para = para.style(Style::new().bg(self.pal.selection));
            } else if p.pid.is_some() && hovered {
                para = para.style(Style::new().bg(self.pal.hover));
            }
            f.render_widget(para, r);
            if let Some(pid) = p.pid {
                self.list_hits.push((r, pid));
            }
        }
    }

    /// Two lines: the state dot, title and time in state; then the state
    /// word (the only coloured text) and the account. A long title wraps
    /// rather than being cut. Everything else is on the selected session's
    /// band, one step away.
    /// "keys → Hello from toomux" in the full-screen shell.
    fn keys_note(&self) -> Option<String> {
        let sh = self.shell.as_ref()?;
        if !matches!(self.mode, Mode::Normal) {
            return None;
        }
        if sh.focus_live && self.usage_view.is_none() {
            let title = sh
                .live_title(&self.sessions)
                .unwrap_or_else(|| "the session".into());
            Some(format!("keys → {title}"))
        } else {
            Some("keys → the list".into())
        }
    }

    fn draw_list_top(&mut self, f: &mut Frame, r: Rect) {
        let p = &self.pal;
        let mut left: Vec<Span<'static>> = vec![Span::raw(" ")];
        let mut right: Option<(String, Cmd)> = None;
        if let Mode::Sweep(list) = &self.mode {
            let n = list.iter().filter(|(_, on)| *on).count();
            left.push(Span::styled(
                format!("sweep · closing {n} of {}", list.len()),
                Style::new().fg(p.text),
            ));
        } else if self.filter.is_empty() {
            let typing_here = self.shell.as_ref().is_none_or(|sh| !sh.focus_live);
            let hint = if typing_here {
                "type to filter"
            } else {
                "alt-s to filter"
            };
            left.push(Span::styled(hint, Style::new().fg(p.muted)));
            right = Some((self.group.label().to_string(), Cmd::Group));
        } else {
            left.extend([
                Span::styled("filter ", Style::new().fg(p.dim)),
                Span::styled(self.filter.clone(), Style::new().fg(p.text)),
                Span::styled("▏", Style::new().fg(p.accent)),
            ]);
            right = Some((self.group.label().to_string(), Cmd::Group));
        }
        let lw: usize = left.iter().map(|s| s.width()).sum();
        if let Some((label, cmd)) = right {
            let gw = label.width();
            if lw + gw + 3 <= r.width as usize {
                let x = r.x + r.width - gw as u16 - 1;
                self.cmd_hits.push((
                    Rect {
                        x,
                        y: r.y,
                        width: gw as u16,
                        height: 1,
                    },
                    cmd,
                ));
                left.push(Span::raw(" ".repeat(r.width as usize - lw - gw - 1)));
                left.push(Span::styled(label, Style::new().fg(p.muted)));
            }
        }
        f.render_widget(Paragraph::new(Line::from(left)), r);
    }

    fn item_lines(&self, s: &Session, w: usize, now: i64, selected: bool) -> Vec<Line<'static>> {
        let p = &self.pal;
        let sweep = match &self.mode {
            Mode::Sweep(list) => list
                .iter()
                .find(|(pid, _)| *pid == s.pid)
                .map(|(_, on)| *on),
            _ => None,
        };
        let live_focus = self.shell.as_ref().is_some_and(|sh| sh.focus_live);
        let (glyph, color) = match sweep {
            Some(true) => ("✕", p.attention),
            Some(false) => ("○", p.dim),
            None if s.dormant => ("○", p.muted),
            // In-flight work breathes, gently, while you're looking at the list.
            None if s.state == St::Working && live_focus => ("●", p.working),
            None if s.state == St::Working => (
                "●",
                breathe(
                    p.working,
                    if selected { p.selection } else { p.base },
                    self.epoch,
                ),
            ),
            None => p.state(s.state),
        };
        let noticed = self.notices.iter().any(|n| n.pid == s.pid);
        let settled = matches!(s.state, St::Idle | St::Background) && s.limit.is_none() && !noticed;
        let mut title_style = Style::new().fg(if settled && !selected { p.dim } else { p.text });
        // Bold marks one thing: the selection, unless a session has the keys.
        if selected && !live_focus {
            title_style = title_style.add_modifier(Modifier::BOLD);
        }
        let (word, rest, time) = s.state_parts(now);
        let time = time.unwrap_or_default();
        let right_w = if time.is_empty() { 0 } else { time.width() + 1 };
        let first_w = w.saturating_sub(5 + right_w + 1).max(12);
        let head_lines = wrap_spans(
            &[(s.title.clone(), title_style)],
            first_w,
            w.saturating_sub(5),
        );

        // The detail line: the state word, then parts that give way, least
        // important first, when the line would otherwise wrap. A limit keeps
        // when it lifts; a queued move always shows.
        // A limit keeps when it lifts, a failed handover keeps why.
        let limited =
            s.limit.is_some() || matches!(s.handover, Some(crate::handover::Phase::Failed(_)));
        let mut parts: Vec<(u8, String, Style)> = Vec::new();
        if let Some(r) = rest {
            parts.push((if limited { 1 } else { 3 }, r, Style::new().fg(p.muted)));
        }
        parts.push((
            2,
            s.account_name(&self.cfg).to_string(),
            Style::new().fg(p.muted),
        ));
        // The folder only when it tells two same-named sessions apart.
        let twins = self
            .sessions
            .iter()
            .any(|x| x.pid != s.pid && x.title.eq_ignore_ascii_case(&s.title) && x.cwd != s.cwd);
        if twins {
            parts.push((4, short_place(&s.place()), Style::new().fg(p.muted)));
        }
        if let Some(q) = &s.queued {
            parts.push((0, format!("moves to {q}"), Style::new().fg(p.accent)));
        }
        // A voyage stays in view: the one thing this session is for.
        if let Some(q) = self.voyages.get(&s.id) {
            parts.push((
                0,
                format!("◎ {}", q.chip(now)),
                Style::new().fg(self.voyage_tone(q)),
            ));
            // Where the ship would be: the judge's estimate, until it's met.
            if let Some(pc) = q.progress.filter(|_| q.status.open()) {
                parts.push((0, format!("{pc}% there"), Style::new().fg(p.muted)));
            }
        }
        let room = w.saturating_sub(5);
        let assemble = |parts: &[(u8, String, Style)]| {
            let mut d = vec![(word.clone(), Style::new().fg(p.word(s, noticed)))];
            for (_, t, st) in parts {
                d.push((" · ".to_string(), Style::new().fg(p.muted)));
                d.push((t.clone(), *st));
            }
            d
        };
        let fits = |d: &[(String, Style)]| d.iter().map(|(t, _)| t.width()).sum::<usize>() <= room;
        while !fits(&assemble(&parts)) {
            let Some(worst) = parts
                .iter()
                .enumerate()
                .filter(|(_, x)| x.0 > 0)
                .max_by_key(|(i, x)| (x.0, *i))
                .map(|(i, _)| i)
            else {
                break;
            };
            parts.remove(worst);
        }
        let detail = assemble(&parts);
        let detail_lines = wrap_chips(&detail, w.saturating_sub(5), w.saturating_sub(5));

        let bar = if selected {
            Span::styled("▎", Style::new().fg(p.accent))
        } else {
            Span::raw(" ")
        };
        let mut lines = Vec::new();
        for (n, spans) in head_lines.into_iter().enumerate() {
            let mut l = if n == 0 {
                let slot = match s.pin {
                    Some(n) => Span::styled((n + 1).to_string(), Style::new().fg(p.accent)),
                    None => Span::raw(" "),
                };
                vec![
                    bar.clone(),
                    slot,
                    Span::raw(" "),
                    Span::styled(glyph, Style::new().fg(color)),
                    Span::raw(" "),
                ]
            } else {
                vec![bar.clone(), Span::raw("    ")]
            };
            let used: usize = 5 + spans.iter().map(|s| s.width()).sum::<usize>();
            l.extend(spans);
            if n == 0 && right_w > 0 && used + right_w <= w {
                l.push(Span::raw(" ".repeat(w - used - right_w)));
                l.push(Span::styled(format!("{time} "), Style::new().fg(p.muted)));
            }
            lines.push(Line::from(l));
        }
        for spans in detail_lines {
            let mut l = vec![bar.clone(), Span::raw("    ")];
            l.extend(spans);
            lines.push(Line::from(l));
        }
        lines
    }

    fn voyage_tone(&self, q: &crate::voyage::Voyage) -> Color {
        use crate::voyage::Status;
        match q.status {
            Status::Paused => self.pal.attention,
            Status::Met => self.pal.finished,
            _ if q.at_limit => self.pal.attention,
            _ => self.pal.accent,
        }
    }

    /// The selected session's voyage, for the detail pane: its chip, the
    /// outcome, the last check, and its scene.
    fn voyage_block(
        &self,
        q: &crate::voyage::Voyage,
        w: usize,
        now: i64,
    ) -> Vec<Vec<Span<'static>>> {
        let p = &self.pal;
        let mut out: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        let mut chip = vec![(
            format!("◎ {}", q.chip(now)),
            Style::new().fg(self.voyage_tone(q)),
        )];
        let sep = || (" · ".to_string(), Style::new().fg(p.muted));
        if q.status == crate::voyage::Status::Met {
            chip.extend([
                sep(),
                (
                    format!("after {}", crate::voyage::turns_taken(q)),
                    Style::new().fg(p.muted),
                ),
            ]);
        } else if let Some(pc) = q.progress {
            chip.extend([
                sep(),
                (
                    format!("about {pc}% there, by the judge"),
                    Style::new().fg(p.muted),
                ),
            ]);
        }
        if q.spent_usd > 0.0 {
            let spent = match q.budget_usd {
                Some(b) => format!("${:.2} of ${b:.0}", q.spent_usd),
                None => format!("${:.2}", q.spent_usd),
            };
            chip.extend([sep(), (spent, Style::new().fg(p.muted))]);
        }
        out.extend(wrap_chips(&chip, w, w));
        out.extend(
            wrap_spans(&[(q.outcome.clone(), Style::new().fg(p.text))], w, w)
                .into_iter()
                .take(3),
        );
        let said = match (&q.why, &q.last) {
            (Some(w), _) if q.status == crate::voyage::Status::Paused => {
                Some(("waiting on you: ", w))
            }
            (_, Some(l)) => Some(("last check: ", l)),
            _ => None,
        };
        if let Some((label, l)) = said {
            out.extend(
                wrap_spans(
                    &[
                        (label.to_string(), Style::new().fg(p.muted)),
                        (l.clone(), Style::new().fg(p.dim)),
                    ],
                    w,
                    w,
                )
                .into_iter()
                .take(2),
            );
        }
        if self.cfg.voyage_scene && w >= crate::scene::MIN_WIDTH {
            out.push(Vec::new());
            out.extend(scene_spans(crate::scene::cells(w, &q.scene(now))));
        }
        out
    }

    /// Waiting on you, a conversation's cached context lasts an hour from its
    /// last call; after that the next prompt writes all of it again.
    fn cache_chip(&self, s: &Session, now: i64) -> Option<(String, Color)> {
        if matches!(s.state, St::Working) || s.dormant || s.limit.is_some() || s.since_ms <= 0 {
            return None;
        }
        let info = self
            .info
            .get(&s.id)
            .filter(|i| now - i.at_ms < 6 * 3_600_000)?;
        let t = info.tokens?;
        let left = CACHE_MS - (now - s.since_ms);
        Some(if left > 0 {
            (
                format!("cache warm · {} left", ago(left)),
                if left < 10 * 60_000 {
                    self.pal.working
                } else {
                    self.pal.muted
                },
            )
        } else {
            let price = crate::price::of(info.model_id.as_deref().unwrap_or(""), false);
            (
                format!(
                    "cache cold · the next prompt writes {}k again (${:.2})",
                    t / 1000,
                    t as f64 * price.write_1h()
                ),
                self.pal.muted,
            )
        })
    }

    /// What the conversation has cost so far, at API prices.
    fn cost_so_far(&self, s: &Session) -> Option<String> {
        let c = self.info.get(&s.id)?.cost.filter(|c| *c >= 0.01)?;
        Some(format!("${c:.2} so far"))
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let p = &self.pal;
        f.render_widget(Block::new().style(Style::new().bg(p.well)), area);
        let Some(s) = self.selected().cloned() else {
            return;
        };
        let now = now_ms();
        let (_, color) = p.state(s.state);
        let inner = Rect {
            x: area.x + 2,
            width: area.width.saturating_sub(4),
            ..area
        };
        let w = inner.width as usize;
        let sep = || (" · ".to_string(), Style::new().fg(p.muted));

        let mut head: Vec<Vec<Span<'static>>> = Vec::new();
        // Bold stays on the list's selection; the preview title is plain.
        head.extend(wrap_spans(
            &[(s.title.clone(), Style::new().fg(p.text))],
            w,
            w,
        ));
        if let Some(t) = &s.topic {
            head.extend(wrap_spans(&[(t.clone(), Style::new().fg(p.dim))], w, w));
        }
        let mut place = path_segments(&tilde(&s.cwd), Style::new().fg(p.dim));
        place.extend([
            sep(),
            (
                s.account_name(&self.cfg).to_string(),
                Style::new().fg(p.dim),
            ),
            sep(),
            (
                format!("started {} ago", ago(now - s.started_ms)),
                Style::new().fg(p.muted),
            ),
        ]);
        head.extend(wrap_chips(&place, w, w));
        let noticed = self.notices.iter().any(|n| n.pid == s.pid);
        let (word, rest, time) = s.state_parts(now);
        let _ = color;
        let mut status = vec![(word, Style::new().fg(p.word(&s, noticed)))];
        for part in [rest, time].into_iter().flatten() {
            status.extend([sep(), (part, Style::new().fg(p.muted))]);
        }
        if let Some(q) = &s.queued {
            status.extend([
                sep(),
                (format!("moves to {q} when idle"), Style::new().fg(p.accent)),
            ]);
        }
        if let Some((chip, tone)) = self.cache_chip(&s, now) {
            status.extend([sep(), (chip, Style::new().fg(tone))]);
        }
        let info = self
            .info
            .get(&s.id)
            .filter(|i| now - i.at_ms < 6 * 3_600_000);
        head.extend(wrap_chips(&status, w, w));
        if let Some(hint) = self.room_elsewhere(&s) {
            head.extend(wrap_chips(&hint, w, w));
        }
        let mut whereabouts = match (&s.pane, &s.tty) {
            (Some(pane), _) => vec![(
                format!("tmux {}:{}", pane.session, pane.window_index),
                Style::new().fg(p.muted),
            )],
            (None, Some(t)) => vec![(
                format!("outside tmux on {}", t.trim_start_matches("/dev/")),
                Style::new().fg(p.muted),
            )],
            (None, None) => vec![("no terminal".to_string(), Style::new().fg(p.muted))],
        };
        if let Some(info) = info {
            if let Some(m) = &info.model {
                whereabouts.extend([sep(), (m.to_lowercase(), Style::new().fg(p.muted))]);
            }
            let near =
                |t: u64| self.cfg.handover_tokens > 0 && t * 4 >= self.cfg.handover_tokens * 3;
            match (info.tokens, info.context) {
                (Some(t), _) => whereabouts.extend([
                    sep(),
                    (
                        format!("{}k context", t / 1000),
                        Style::new().fg(if near(t) { p.working } else { p.muted }),
                    ),
                ]),
                (None, Some(c)) => whereabouts.extend([
                    sep(),
                    (format!("context {c:.0}%"), Style::new().fg(p.muted)),
                ]),
                _ => {}
            }
            if let Some(c) = self.cost_so_far(&s) {
                whereabouts.extend([sep(), (c, Style::new().fg(p.muted))]);
            }
        }
        if let Some((repo, n)) = &s.pr {
            whereabouts.extend([
                sep(),
                (
                    format!("pr {}#{n}", repo.rsplit('/').next().unwrap_or(repo)),
                    Style::new().fg(p.muted),
                ),
            ]);
        }
        whereabouts.extend([
            sep(),
            (
                s.id[..8.min(s.id.len())].to_string(),
                Style::new().fg(p.muted),
            ),
        ]);
        head.extend(wrap_chips(&whereabouts, w, w));
        if let Some(q) = self.voyages.get(&s.id) {
            head.extend(self.voyage_block(q, w, now));
        }

        let head: Vec<Line> = head.into_iter().map(Line::from).collect();
        let head_h = head.len() as u16;
        // The session's particulars on a raised band; its live terminal below.
        let band = Rect {
            height: (head_h + 2).min(area.height),
            ..area
        };
        f.render_widget(Block::new().style(Style::new().bg(p.raised)), band);
        f.render_widget(
            Paragraph::new(Text::from(head)),
            Rect {
                y: inner.y + 1,
                height: head_h.min(inner.height.saturating_sub(1)),
                ..inner
            },
        );
        if area.height <= band.height + 3 {
            return;
        }
        let body = Rect {
            y: band.y + band.height + 1,
            height: area.height - band.height - 1,
            ..inner
        };
        let text = self.preview_text(&s, body);
        f.render_widget(Paragraph::new(text), body);
        if let Some((pid, t)) = self.fade {
            let e = t.elapsed().as_secs_f32() / 0.26;
            if pid == s.pid && e < 1.0 {
                let ease = 1.0 - e * e;
                recede(f.buffer_mut(), body, self.pal.well, 0.85 * ease, &self.pal);
            }
        }
    }

    /// For a session stopped by a limit: which other account has room.
    fn room_elsewhere(&self, s: &Session) -> Option<Vec<(String, Style)>> {
        s.limit.as_ref()?;
        let p = &self.pal;
        let (j, left) = (0..self.cfg.accounts.len())
            .filter(|&j| Some(j) != s.account)
            .filter_map(|j| {
                let u = self.usage.get(j)?;
                let (_, m) = u.tightest()?;
                (!m.limited && m.used < 90.0).then_some((j, 100.0 - m.used))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        let limited = self
            .sessions
            .iter()
            .filter(|x| !x.dormant && x.limit.is_some() && x.account == s.account)
            .count();
        let u = &self.usage[j];
        let figures: Vec<String> = [("5h", &u.five), ("wk", &u.week)]
            .into_iter()
            .filter_map(|(l, m)| m.as_ref().map(|m| format!("{l} {:.0}%", m.used)))
            .collect();
        let _ = left;
        let mut out = vec![(
            format!("{} has room", self.cfg.accounts[j].name),
            Style::new().fg(p.accent),
        )];
        if !figures.is_empty() {
            out.push((
                format!(" ({})", figures.join(" · ")),
                Style::new().fg(p.muted),
            ));
        }
        out.push((" · ".to_string(), Style::new().fg(p.muted)));
        out.push(("^a moves it".to_string(), Style::new().fg(p.dim)));
        if limited > 1 {
            out.push((
                format!(", then a moves all {limited}"),
                Style::new().fg(p.dim),
            ));
        }
        Some(out)
    }

    /// Plan usage for every account; `focus` is marked.
    fn draw_usage(&mut self, f: &mut Frame, area: Rect, focus: usize) {
        let p = &self.pal;
        let now = now_ms();
        f.render_widget(Block::new().style(Style::new().bg(p.well)), area);
        let pad = if self.sidebar { 1 } else { 2 };
        let inner = Rect {
            x: area.x + pad,
            width: area.width.saturating_sub(pad * 2),
            ..area
        };
        let w = inner.width as usize;
        let mut title = vec![Line::from(Span::styled(
            "usage",
            Style::new().fg(p.text).add_modifier(Modifier::BOLD),
        ))];
        title.extend(
            wrap_spans(
                &[(
                    "each account's 5-hour and weekly limits".to_string(),
                    Style::new().fg(p.muted),
                )],
                w,
                w,
            )
            .into_iter()
            .map(Line::from),
        );
        let band = Rect {
            height: (title.len() as u16 + 2).min(area.height),
            ..area
        };
        f.render_widget(Block::new().style(Style::new().bg(p.raised)), band);
        f.render_widget(
            Paragraph::new(Text::from(title)),
            Rect {
                y: inner.y + 1,
                height: band.height.saturating_sub(2),
                ..inner
            },
        );

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut marks: Vec<(usize, usize)> = Vec::new(); // (first line, last line) of the focused account
        for (i, a) in self.cfg.accounts.iter().enumerate() {
            let Some(u) = self.usage.get(i) else { continue };
            let start = lines.len();
            if i > 0 {
                lines.push(Line::raw(""));
            }
            let when = match (u.source, u.at_ms) {
                (None, _) | (_, 0) => "no report yet".to_string(),
                (Some(src), t) => {
                    let how = match src {
                        crate::usage::Source::Live => "from its sessions",
                        crate::usage::Source::Fetched => "looked up",
                        crate::usage::Source::Transcript => "from a session that hit it",
                    };
                    if now - t < 90_000 {
                        format!("{how}, just now")
                    } else {
                        format!("{how}, {} ago", ago(now - t))
                    }
                }
            };
            let mut head = vec![(
                a.name.clone(),
                Style::new().fg(p.text).add_modifier(Modifier::BOLD),
            )];
            if let Some(plan) = self.plans.get(i).cloned().flatten() {
                head.push((format!("  {plan}"), Style::new().fg(p.muted)));
            }
            head.push(("   ".into(), Style::new()));
            head.push((when, Style::new().fg(p.muted)));
            lines.extend(wrap_chips(&head, w, w).into_iter().map(Line::from));
            if let Some(problem) = &u.problem {
                lines.extend(
                    wrap_spans(&[(problem.clone(), Style::new().fg(p.muted))], w, w)
                        .into_iter()
                        .map(Line::from),
                );
            }
            for (label, m) in [("5-hour", &u.five), ("weekly", &u.week)] {
                lines.push(Line::raw(""));
                let Some(m) = m else {
                    lines.push(Line::from(vec![
                        Span::styled(label, Style::new().fg(p.dim)),
                        Span::styled("  not reported", Style::new().fg(p.muted)),
                    ]));
                    continue;
                };
                let tone = if u.stale(now) {
                    p.muted
                } else {
                    p.usage_tone(m.used, m.limited)
                };
                let figure = if m.limited {
                    "at the limit".to_string()
                } else {
                    format!("{:.0}%", m.used)
                };
                let gap = w.saturating_sub(label.width() + figure.width());
                lines.push(Line::from(vec![
                    Span::styled(label, Style::new().fg(p.dim)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(figure, Style::new().fg(tone).add_modifier(Modifier::BOLD)),
                ]));
                lines.push(Line::from(self.bar_spans(
                    if m.limited { 100.0 } else { m.used },
                    w,
                    tone,
                )));
                let mut detail: Vec<(String, Style)> = Vec::new();
                if m.resets_ms > now {
                    let at = chrono::DateTime::from_timestamp_millis(m.resets_ms)
                        .map(|t| t.with_timezone(&chrono::Local));
                    let clock = at.map(|t| {
                        let fmt = if m.resets_ms - now < 20 * 3_600_000 {
                            "%-I:%M %P"
                        } else {
                            "%a %-I:%M %P"
                        };
                        t.format(fmt).to_string().to_lowercase()
                    });
                    detail.push((
                        format!("resets in {}", registry::duration(m.resets_ms - now)),
                        Style::new().fg(p.dim),
                    ));
                    if let Some(c) = clock {
                        detail.push((format!(", {c}"), Style::new().fg(p.muted)));
                    }
                } else {
                    detail.push((
                        "a fresh window starts with the next message".into(),
                        Style::new().fg(p.muted),
                    ));
                }
                match m.pace {
                    Some(crate::usage::Pace::LimitIn(ms)) => {
                        detail.push((" · ".into(), Style::new().fg(p.muted)));
                        detail.push((
                            format!("at this pace the limit comes in {}", registry::duration(ms)),
                            Style::new().fg(p.working),
                        ));
                    }
                    Some(crate::usage::Pace::EndsAt(x)) if x >= m.used + 1.0 => {
                        detail.push((" · ".into(), Style::new().fg(p.muted)));
                        detail.push((
                            format!("on pace to end near {x:.0}%"),
                            Style::new().fg(p.muted),
                        ));
                    }
                    _ => {}
                }
                lines.extend(wrap_chips(&detail, w, w).into_iter().map(Line::from));
            }
            if u.trend.len() > 1 {
                lines.push(Line::raw(""));
                let (lo, hi) = u
                    .trend
                    .iter()
                    .fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
                let label = format!(
                    "week, past day  {lo:.0}% → {:.0}%",
                    u.trend.last().copied().unwrap_or(hi)
                );
                lines.push(Line::from(Span::styled(label, Style::new().fg(p.muted))));
                for row in sparkline(&u.trend, w.min(72)) {
                    lines.push(Line::from(Span::styled(row, Style::new().fg(p.dim))));
                }
            }
            let mine: Vec<&Session> = self
                .sessions
                .iter()
                .filter(|s| s.account == Some(i) && !s.dormant)
                .collect();
            if !mine.is_empty() {
                lines.push(Line::raw(""));
                let count = |st: &[St]| mine.iter().filter(|s| st.contains(&s.state)).count();
                let mut segs: Vec<(String, Style)> = Vec::new();
                for (n, what, color) in [
                    (count(&[St::NeedsYou]), "needs you", p.attention),
                    (count(&[St::Working]), "working", p.working),
                    (count(&[St::Background]), "in the background", p.working),
                    (count(&[St::Finished, St::Idle]), "idle", p.muted),
                ] {
                    if n > 0 {
                        if !segs.is_empty() {
                            segs.push((" · ".into(), Style::new().fg(p.muted)));
                        }
                        segs.push((format!("{n} {what}"), Style::new().fg(color)));
                    }
                }
                lines.extend(wrap_chips(&segs, w, w).into_iter().map(Line::from));
                let stuck = mine.iter().filter(|s| s.limit.is_some()).count();
                if let Some(hint) = mine
                    .iter()
                    .find(|s| s.limit.is_some())
                    .and_then(|s| self.room_elsewhere(s))
                {
                    let mut hint = hint;
                    if stuck > 1 {
                        hint.insert(
                            0,
                            (
                                format!("{stuck} stopped at the limit · "),
                                Style::new().fg(p.dim),
                            ),
                        );
                    }
                    lines.extend(wrap_chips(&hint, w, w).into_iter().map(Line::from));
                }
            }
            if i == focus {
                marks.push((start + usize::from(i > 0), lines.len()));
            }
        }
        let body = Rect {
            y: band.y + band.height + 1,
            height: area.height.saturating_sub(band.height + 1),
            ..inner
        };
        f.render_widget(Paragraph::new(Text::from(lines)), body);
        // A hairline at the edge marks the account you came from.
        if pad > 0 && self.cfg.accounts.len() > 1 {
            for (a, b) in marks {
                for y in a..b {
                    let y = body.y + y as u16;
                    if y < body.y + body.height {
                        f.buffer_mut()[(area.x, y)].set_symbol("▎").set_fg(p.accent);
                    }
                }
            }
        }
    }

    fn draw_accounts(&mut self, f: &mut Frame, area: Rect) {
        let p = &self.pal;
        f.render_widget(Block::new().style(Style::new().bg(p.well)), area);
        let title_h = 4.min(area.height);
        let title = Rect {
            height: title_h,
            ..area
        };
        f.render_widget(Block::new().style(Style::new().bg(p.raised)), title);
        if title_h > 1 {
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        "accounts",
                        Style::new().fg(p.text).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        "Claude Code identities, their folders, login and shared history",
                        Style::new().fg(p.muted),
                    )),
                ]),
                Rect {
                    x: area.x + 2,
                    y: area.y + 1,
                    width: area.width.saturating_sub(4),
                    height: title_h.saturating_sub(1),
                },
            );
        }
        let body = Rect {
            x: area.x + 2,
            y: area.y + title_h,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(title_h),
        };
        let (sel, login) = match &self.mode {
            Mode::Accounts(view) => (view.sel, view.login.clone()),
            _ => return,
        };
        if self.cfg.accounts.is_empty() {
            f.render_widget(
                Paragraph::new(vec![
                    Line::raw(""),
                    Line::from(Span::styled(
                        "no accounts yet",
                        Style::new().fg(p.text).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        "press a to add your first Claude Code account",
                        Style::new().fg(p.dim),
                    )),
                ]),
                body,
            );
        } else {
            let rows_fit = usize::from(body.height.saturating_add(1) / 4).max(1);
            let first = sel.saturating_add(1).saturating_sub(rows_fit);
            let mut y = body.y;
            for (i, account) in self.cfg.accounts.iter().enumerate().skip(first) {
                if y >= body.y + body.height {
                    break;
                }
                let dir = self.cfg.account_dir(i);
                let group = crate::accounts::group_of(&dir);
                let sharing = match group.as_ref() {
                    None => "stands alone".to_string(),
                    Some(group) => {
                        let with: Vec<&str> = (0..self.cfg.accounts.len())
                            .filter(|&j| {
                                j != i
                                    && crate::accounts::group_of(&self.cfg.account_dir(j)).as_ref()
                                        == Some(group)
                            })
                            .map(|j| self.cfg.accounts[j].name.as_str())
                            .collect();
                        let (_, own) = crate::accounts::describe(&dir, group);
                        let own = if own.is_empty() {
                            String::new()
                        } else {
                            format!(" · own: {}", own.join(", "))
                        };
                        if with.is_empty() {
                            format!("shares in {}{own}", tilde(&group.display().to_string()))
                        } else {
                            format!(
                                "shares with {} in {}{own}",
                                with.join(", "),
                                tilde(&group.display().to_string())
                            )
                        }
                    }
                };
                let running = self
                    .sessions
                    .iter()
                    .filter(|s| !s.dormant && s.account == Some(i))
                    .count();
                let login = if login.get(i).copied().unwrap_or(false) {
                    "signed in"
                } else {
                    "not logged in"
                };
                let selected = i == sel;
                let h = 3.min(body.y + body.height - y);
                let row = Rect {
                    x: body.x,
                    y,
                    width: body.width,
                    height: h,
                };
                if selected {
                    f.render_widget(Block::new().style(Style::new().bg(p.selection)), row);
                }
                let mark = if selected { "› " } else { "  " };
                let running = if running == 0 {
                    "no running sessions".to_string()
                } else {
                    format!(
                        "{running} running session{}",
                        if running == 1 { "" } else { "s" }
                    )
                };
                let lines = vec![
                    Line::from(vec![
                        Span::styled(mark, Style::new().fg(p.accent)),
                        Span::styled(
                            account.name.clone(),
                            Style::new().fg(p.text).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("   {login}"), Style::new().fg(p.dim)),
                        Span::styled(format!("   {running}"), Style::new().fg(p.muted)),
                    ]),
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(tilde(&dir.display().to_string()), Style::new().fg(p.dim)),
                    ]),
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(sharing, Style::new().fg(p.muted)),
                    ]),
                ];
                f.render_widget(Paragraph::new(lines), row);
                self.list_hits.push((row, i as i32));
                y = y.saturating_add(h + 1);
            }
        }
        self.draw_account_overlay(f, area);
    }

    fn draw_account_overlay(&mut self, f: &mut Frame, area: Rect) {
        let p = &self.pal;
        let Mode::Accounts(view) = &self.mode else {
            return;
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(flow) = &view.flow {
            lines.push(Line::from(Span::styled(
                "add account",
                Style::new().fg(p.text).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            match flow.step {
                AddStep::Name => {
                    lines.push(Line::from(vec![
                        Span::styled("name  ", Style::new().fg(p.muted)),
                        Span::styled(format!("{}▏", flow.name), Style::new().fg(p.text)),
                    ]));
                    lines.push(Line::from(Span::styled(
                        "letters, digits, - and _",
                        Style::new().fg(p.muted),
                    )));
                }
                AddStep::Folder => {
                    lines.push(Line::from(vec![
                        Span::styled("folder  ", Style::new().fg(p.muted)),
                        Span::styled(format!("{}▏", flow.folder), Style::new().fg(p.text)),
                    ]));
                    let dir = crate::config::expand(&flow.folder);
                    let what = if dir.is_dir() {
                        format!("adopt {} as it is", tilde(&dir.display().to_string()))
                    } else {
                        "a new Claude Code config folder".into()
                    };
                    lines.push(Line::from(Span::styled(what, Style::new().fg(p.muted))));
                }
                AddStep::Sharing => {
                    lines.push(Line::from(Span::styled(
                        "history",
                        Style::new().fg(p.muted),
                    )));
                    let mut choices = vec!["stand alone".to_string()];
                    choices.extend(flow.groups.iter().map(|g| format!("join {g}")));
                    choices.push("new group…".into());
                    for (i, choice) in choices.iter().enumerate() {
                        let selected = i == flow.share_sel;
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("{} {}  ", if selected { "›" } else { " " }, i + 1),
                                Style::new().fg(if selected { p.accent } else { p.muted }),
                            ),
                            Span::styled(
                                choice.clone(),
                                Style::new().fg(if selected { p.text } else { p.dim }),
                            ),
                        ]));
                    }
                }
                AddStep::NewGroup => {
                    lines.push(Line::from(vec![
                        Span::styled("group  ", Style::new().fg(p.muted)),
                        Span::styled(format!("{}▏", flow.new_group), Style::new().fg(p.text)),
                    ]));
                }
                AddStep::Creating => {
                    lines.push(Line::from(Span::styled(
                        "creating the account and configuring toomux…",
                        Style::new().fg(p.working),
                    )));
                }
                AddStep::Login => {
                    lines.push(Line::from(Span::styled(
                        format!("{} is ready", flow.name),
                        Style::new().fg(p.finished),
                    )));
                    for note in &flow.notes {
                        lines.extend(
                            wrap_spans(&[(note.clone(), Style::new().fg(p.dim))], 72, 72)
                                .into_iter()
                                .map(Line::from),
                        );
                    }
                    lines.push(Line::raw(""));
                    lines.push(Line::from(Span::styled(
                        if flow.waiting_login {
                            "waiting for login…"
                        } else {
                            "log in now, or do it later from the Accounts page"
                        },
                        Style::new().fg(if flow.waiting_login {
                            p.working
                        } else {
                            p.text
                        }),
                    )));
                }
            }
            if let Some(hint) = &flow.hint {
                lines.push(Line::raw(""));
                lines.extend(
                    wrap_spans(&[(hint.clone(), Style::new().fg(p.attention))], 72, 72)
                        .into_iter()
                        .map(Line::from),
                );
            }
        } else if let Some(AccountAction::Share { groups, sel }) = &view.action {
            lines.push(Line::from(Span::styled(
                "share history with",
                Style::new().fg(p.text).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            for (i, group) in groups.iter().enumerate() {
                let selected = i == *sel;
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} {}  ", if selected { "›" } else { " " }, i + 1),
                        Style::new().fg(if selected { p.accent } else { p.muted }),
                    ),
                    Span::styled(
                        group.clone(),
                        Style::new().fg(if selected { p.text } else { p.dim }),
                    ),
                ]));
            }
        } else {
            return;
        }
        let w =
            (lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16 + 8).clamp(34, area.width);
        let h = (lines.len() as u16 + 4).min(area.height);
        let r = Rect {
            x: area.x + area.width.saturating_sub(w) / 2,
            y: area.y + area.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        };
        f.render_widget(ratatui::widgets::Clear, r);
        let block = Block::bordered()
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::new().fg(p.muted).bg(p.overlay))
            .style(Style::new().bg(p.overlay))
            .padding(ratatui::widgets::Padding::new(3, 3, 1, 1));
        let inner = block.inner(r);
        f.render_widget(block, r);
        f.render_widget(Paragraph::new(Text::from(lines)), inner);
    }

    fn preview_text(&mut self, s: &Session, area: Rect) -> Text<'static> {
        // A working pane changes constantly; a resting one rarely does.
        let fresh = match &s.pane {
            Some(_) if matches!(s.state, St::Working | St::NeedsYou) => Duration::from_millis(400),
            Some(_) => Duration::from_millis(1500),
            None => Duration::from_secs(2),
        };
        if let Some(pv) = &self.preview
            && pv.pid == s.pid
            && pv.at.elapsed() < fresh
            && pv.size == (area.width, area.height)
        {
            return pv.text.clone();
        }
        let h = area.height as usize;
        let w = area.width as usize;
        let text = match &s.pane {
            Some(pane) => {
                let raw =
                    tmux::capture(&pane.id, area.height.saturating_mul(2)).unwrap_or_default();
                let t = raw.into_text().unwrap_or_default();
                let mut lines: Vec<Line<'static>> =
                    t.lines.into_iter().flat_map(|l| reflow(l, w)).collect();
                while lines
                    .last()
                    .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
                {
                    lines.pop();
                }
                let skip = lines.len().saturating_sub(h);
                Text::from(lines.into_iter().skip(skip).collect::<Vec<_>>())
            }
            None => self.transcript_text(s, w, h),
        };
        self.preview = Some(Preview {
            pid: s.pid,
            at: Instant::now(),
            size: (area.width, area.height),
            text: text.clone(),
        });
        text
    }

    fn transcript_text(&self, s: &Session, w: usize, h: usize) -> Text<'static> {
        let p = &self.pal;
        let Some(path) = s.transcript(&self.cfg) else {
            return Text::from(Span::styled("no conversation yet", Style::new().fg(p.dim)));
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        for t in transcript::tail(&path, 6) {
            let (label, color) = match t.who {
                Who::You => ("you", p.accent),
                Who::Claude => ("claude", p.dim),
            };
            lines.push(Line::from(Span::styled(label, Style::new().fg(color))));
            lines.extend(markdown(&t.text, w.saturating_sub(1), p));
            lines.push(Line::raw(""));
        }
        let note = Line::from(Span::styled(
            "last exchanges · ctrl-o brings it into tmux for a live view",
            Style::new().fg(p.muted),
        ));
        let skip = lines.len().saturating_sub(h.saturating_sub(2));
        let mut out = vec![note, Line::raw("")];
        out.extend(lines.into_iter().skip(skip));
        Text::from(out)
    }

    /// Footer lines (a wrapped prompt, then the hints line) and the hints'
    /// clickable spans as (x, width, command) on the last line.
    fn foot(&self, width: u16) -> (Vec<Line<'static>>, Vec<(u16, u16, Cmd)>) {
        let p = &self.pal;
        let w = width as usize;
        if let Some((msg, color, at)) = &self.flash {
            let age = at.elapsed().as_secs_f32();
            if age < 5.8 && matches!(self.mode, Mode::Normal) {
                // Settles into the band over its last moments instead of vanishing.
                let color = mix(
                    *color,
                    p.raised,
                    ((age - 4.8) / 1.0).clamp(0.0, 1.0),
                    *color,
                );
                let lines = wrap_spans(
                    &[(msg.clone(), Style::new().fg(color))],
                    w.saturating_sub(2),
                    w.saturating_sub(2),
                );
                return (lines.into_iter().map(|l| pad(l)).collect(), Vec::new());
            }
        }
        if let Some(job) = &self.working_on {
            let color = breathe(p.working, p.raised, self.epoch);
            let lines = wrap_spans(
                &[(format!("{job}…"), Style::new().fg(color))],
                w.saturating_sub(2),
                w.saturating_sub(2),
            );
            return (lines.into_iter().map(|l| pad(l)).collect(), Vec::new());
        }
        let (lead, hints): (Option<String>, Vec<(String, String, Cmd)>) = match &self.mode {
            Mode::Help => (None, vec![("esc".into(), "close".into(), Cmd::No)]),
            Mode::Menu(_) => (
                None,
                vec![
                    ("enter".into(), "choose".into(), Cmd::No),
                    ("esc".into(), "close".into(), Cmd::No),
                ],
            ),
            Mode::Rename(buf) => (
                Some(format!("name  {buf}▏")),
                vec![
                    ("enter".into(), "save".into(), Cmd::Yes),
                    ("esc".into(), "cancel".into(), Cmd::No),
                ],
            ),
            Mode::Confirm(prompt, _) => (
                Some(prompt.clone()),
                vec![
                    ("enter".into(), "yes".into(), Cmd::Yes),
                    ("esc".into(), "no".into(), Cmd::No),
                ],
            ),
            Mode::Offer(prompt, Pending::Account(AccountPending::Remove { .. }), label, _) => (
                Some(prompt.clone()),
                vec![
                    ("enter".into(), "keep folder".into(), Cmd::Yes),
                    ("a".into(), label.clone(), Cmd::All),
                    ("esc".into(), "back".into(), Cmd::No),
                ],
            ),
            Mode::Offer(prompt, _, label, _) => (
                Some(prompt.clone()),
                vec![
                    ("enter".into(), "this one".into(), Cmd::Yes),
                    ("a".into(), label.clone(), Cmd::All),
                    ("esc".into(), "no".into(), Cmd::No),
                ],
            ),
            Mode::Sweep(list) => {
                let n = list.iter().filter(|(_, on)| *on).count();
                (
                    Some(format!("idle {}+ days", self.cfg.stale_days)),
                    vec![
                        ("space".into(), "keep / close".into(), Cmd::Toggle),
                        ("a".into(), "all".into(), Cmd::All),
                        ("enter".into(), format!("close {n}"), Cmd::Yes),
                        ("esc".into(), "cancel".into(), Cmd::No),
                    ],
                )
            }
            Mode::Choose(opts) => (
                Some("move to".into()),
                opts.iter()
                    .enumerate()
                    .map(|(n, &a)| {
                        (
                            (n + 1).to_string(),
                            self.cfg.accounts[a].name.clone(),
                            Cmd::Pick(n),
                        )
                    })
                    .chain(std::iter::once((
                        "esc".to_string(),
                        "cancel".to_string(),
                        Cmd::No,
                    )))
                    .collect(),
            ),
            Mode::New(flow) => match &flow.folder {
                None => (
                    None,
                    vec![
                        ("enter".into(), "choose".into(), Cmd::Yes),
                        ("esc".into(), "cancel".into(), Cmd::No),
                    ],
                ),
                Some((folder, suggested, why)) => {
                    let leaf = folder.rsplit('/').find(|x| !x.is_empty()).unwrap_or(folder);
                    let mut h: Vec<(String, String, Cmd)> = self
                        .cfg
                        .accounts
                        .iter()
                        .enumerate()
                        .map(|(n, a)| {
                            let label = if n == *suggested {
                                format!("{} ({why})", a.name)
                            } else {
                                a.name.clone()
                            };
                            ((n + 1).to_string(), label, Cmd::Pick(n))
                        })
                        .collect();
                    h.push((
                        "enter".into(),
                        self.cfg.accounts[*suggested].name.clone(),
                        Cmd::Yes,
                    ));
                    h.push(("esc".into(), "back".into(), Cmd::No));
                    (Some(format!("start in {leaf} on")), h)
                }
            },
            Mode::Accounts(view) => {
                if let Some(flow) = &view.flow {
                    let lead = match flow.step {
                        AddStep::Name => Some(format!("name  {}▏", flow.name)),
                        AddStep::Folder => Some(format!("folder  {}▏", flow.folder)),
                        AddStep::Sharing => Some("choose history".into()),
                        AddStep::NewGroup => Some(format!("group  {}▏", flow.new_group)),
                        AddStep::Creating => Some("creating account".into()),
                        AddStep::Login if flow.waiting_login => Some("waiting for login…".into()),
                        AddStep::Login => Some(format!("{} is ready", flow.name)),
                    };
                    let hints = match flow.step {
                        AddStep::Creating => vec![],
                        AddStep::Login => vec![
                            ("enter".into(), "log in now".into(), Cmd::Yes),
                            ("esc".into(), "later".into(), Cmd::No),
                        ],
                        _ => vec![
                            ("enter".into(), "next".into(), Cmd::Yes),
                            ("esc".into(), "back".into(), Cmd::No),
                        ],
                    };
                    (lead, hints)
                } else if view.action.is_some() {
                    (
                        Some("share history with".into()),
                        vec![
                            ("enter".into(), "choose".into(), Cmd::Yes),
                            ("esc".into(), "back".into(), Cmd::No),
                        ],
                    )
                } else {
                    (
                        Some("↑↓ select".into()),
                        vec![
                            ("a".into(), "add".into(), Cmd::AccountAdd),
                            ("l".into(), "login".into(), Cmd::AccountLogin),
                            ("s".into(), "share".into(), Cmd::AccountShare),
                            ("u".into(), "unshare".into(), Cmd::AccountUnshare),
                            ("x".into(), "remove".into(), Cmd::AccountRemove),
                            ("esc".into(), "close".into(), Cmd::No),
                        ],
                    )
                }
            }
            Mode::Normal => {
                let sel = self.selected();
                let dormant = sel.is_some_and(|s| s.dormant);
                let outside = sel.is_some_and(|s| s.pane.is_none() && !s.dormant);
                let pinned = sel.is_some_and(|s| s.pin.is_some());
                let queued = sel.is_some_and(|s| s.queued.is_some());
                let restoring = sel.is_some_and(|s| s.restore.is_some());
                let any_restore = self.sessions.iter().any(|s| s.restore.is_some());
                // (key, label, command, priority: lower survives narrower widths)
                // Only what this moment needs; everything else is under `?`
                // and on right-click.
                let mut h: Vec<(String, &str, Cmd, u8)> = Vec::new();
                let shell = self.shell.is_some();
                let open = match (dormant, shell) {
                    (true, _) => "reopen",
                    (false, true) => "open",
                    (false, false) => "jump",
                };
                h.push(("enter".into(), open, Cmd::Jump, 0));
                if !dormant && self.shell.as_ref().is_some_and(|sh| sh.popup) {
                    h.push(("alt-j".into(), "go there", Cmd::GoThere, 1));
                }
                if !dormant {
                    h.push((
                        "^a".into(),
                        if queued { "cancel move" } else { "move" },
                        Cmd::Account,
                        1,
                    ));
                }
                h.push(("alt-a".into(), "accounts", Cmd::Accounts, 1));
                if outside {
                    h.push(("^o".into(), "into tmux", Cmd::Adopt, 1));
                }
                if restoring {
                    h.push(("^x".into(), "dismiss", Cmd::Close, 2));
                }
                if any_restore {
                    h.push(("^e".into(), "reopen all from before", Cmd::RestoreAll, 1));
                }
                let _ = pinned;
                if shell {
                    h.push(("esc".into(), "back", Cmd::No, 0));
                }
                h.push(("?".into(), "keys", Cmd::Help, 1));
                if !shell {
                    h.push(("esc".into(), "quit", Cmd::Quit, 0));
                }
                let fits = |h: &[(String, &str, Cmd, u8)]| {
                    h.iter()
                        .map(|(k, l, _, _)| k.chars().count() + l.chars().count() + 4)
                        .sum::<usize>()
                        < width as usize
                };
                while !fits(&h) {
                    let Some(worst) = h
                        .iter()
                        .enumerate()
                        .max_by_key(|(i, x)| (x.3, *i))
                        .map(|(i, _)| i)
                    else {
                        break;
                    };
                    if h[worst].3 == 0 {
                        break;
                    }
                    h.remove(worst);
                }
                (
                    None,
                    h.into_iter()
                        .map(|(k, l, c, _)| (k, l.to_string(), c))
                        .collect(),
                )
            }
        };
        let hints: Vec<(String, String, Cmd)> = if self.sidebar && matches!(self.mode, Mode::Normal)
        {
            hints
                .into_iter()
                .filter(|(_, _, c)| !matches!(c, Cmd::Quit | Cmd::Group))
                .collect()
        } else {
            hints
        };
        let mut lines = Vec::new();
        let hint_w: usize = hints
            .iter()
            .map(|(k, l, _)| k.chars().count() + l.chars().count() + 4)
            .sum();
        let mut spans = vec![Span::raw(" ")];
        let mut x = 1u16;
        if let Some(l) = lead {
            let lw = l.width();
            if 1 + lw + 3 + hint_w <= w {
                spans.push(Span::styled(l, Style::new().fg(p.text)));
                spans.push(Span::raw("   "));
                x += lw as u16 + 3;
            } else {
                // Too long to share a line with the hints: wrap it above them.
                for chunk in wrap_spans(
                    &[(l, Style::new().fg(p.text))],
                    w.saturating_sub(2),
                    w.saturating_sub(2),
                ) {
                    lines.push(pad(chunk));
                }
            }
        }
        let mut hits = Vec::new();
        for (key, label, cmd) in hints {
            let hw = (key.chars().count() + 1 + label.chars().count()) as u16;
            hits.push((x, hw, cmd));
            spans.push(Span::styled(key, Style::new().fg(p.text)));
            spans.push(Span::styled(format!(" {label}"), Style::new().fg(p.dim)));
            spans.push(Span::raw("   "));
            x += hw + 3;
        }
        // Full screen: say quietly where the keys are.
        if let Some(where_) = self.keys_note() {
            let nw = where_.width() + 1;
            if (x as usize) + nw + 4 <= w {
                spans.push(Span::raw(" ".repeat(w - x as usize - nw)));
                spans.push(Span::styled(where_, Style::new().fg(p.muted)));
            }
        }
        lines.push(Line::from(spans));
        (lines, hits)
    }
}

/// A hairline graph two rows tall in braille dots, scaled to its own range.
/// Running sessions plus pinned conversations that aren't running.
fn load_all(cfg: &Config) -> Vec<Session> {
    let mut all = registry::load(cfg);
    registry::sync_pins(cfg, &all);
    let dormant = registry::dormant(cfg, &all);
    all.extend(dormant);
    let restorable = registry::restorable(cfg, &all);
    all.extend(restorable);
    all
}

fn watch(cfg: &Config, tx: Sender<()>) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let mut w = notify::recommended_watcher(move |_res| {
        let _ = tx.send(());
    })
    .ok()?;
    for d in cfg.session_dirs() {
        let _ = w.watch(&d, RecursiveMode::NonRecursive);
    }
    Some(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn session(pid: i32, title: &str, state: St, account: usize) -> Session {
        let now = now_ms();
        Session {
            pid,
            proc_start: None,
            id: format!("{pid:08x}-0000-0000-0000-000000000000"),
            cwd: "/nonexistent/toomux-test/project".into(),
            name: format!("handle-{pid}"),
            title: title.into(),
            topic: None,
            pr: None,
            queued: None,
            pin: None,
            dormant: false,
            restore: None,
            state,
            waiting_for: None,
            limit: None,
            handover: None,
            since_ms: now - 5 * 60_000,
            started_ms: now - 3 * 3_600_000,
            account: Some(account),
            config_dir: None,
            args: vec![],
            env: vec![],
            tty: None,
            pane: None,
        }
    }

    fn cfg() -> Config {
        let mut c = Config::default();
        c.accounts = vec![
            crate::config::Account {
                name: "work".into(),
                config_dir: "/nonexistent/a".into(),
            },
            crate::config::Account {
                name: "home".into(),
                config_dir: "/nonexistent/b".into(),
            },
        ];
        c
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn sample() -> Vec<Session> {
        let mut limited = session(1, "Northwind cloud AWS cost optimization", St::NeedsYou, 1);
        limited.limit = Some("weekly limit · resets Oct 3, 5pm".into());
        let mut named = session(2, "Oli & Studio", St::Working, 0);
        named.topic = Some("OLI runs token efficiency".into());
        named.pr = Some(("org/northwind-studio".into(), 536));
        let mut queued = session(3, "Harbor operator card UX polish", St::Working, 0);
        queued.queued = Some("home".into());
        let mut pinned = session(5, "Resume session with Opus 5.5", St::Idle, 1);
        pinned.pin = Some(2);
        let mut dormant = session(-1, "Rotate Watson MySQL root", St::Idle, 0);
        dormant.dormant = true;
        dormant.pin = Some(0);
        vec![
            limited,
            named,
            queued,
            session(4, "Add to list", St::Idle, 1),
            pinned,
            dormant,
        ]
    }

    /// The buffer as truecolor ANSI, for rendering review screenshots.
    fn ansi(buf: &ratatui::buffer::Buffer) -> String {
        let code = |c: Color, bg: bool| match c {
            Color::Rgb(r, g, b) => format!("\x1b[{};2;{r};{g};{b}m", if bg { 48 } else { 38 }),
            _ => format!("\x1b[{}m", if bg { 49 } else { 39 }),
        };
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let c = &buf[(x, y)];
                let bold = if c.modifier.contains(Modifier::BOLD) {
                    "\x1b[1m"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "\x1b[0m{}{}{bold}{}",
                    code(c.fg, false),
                    code(c.bg, true),
                    c.symbol()
                ));
            }
            out.push_str("\x1b[0m\n");
        }
        out
    }

    fn meter(
        used: f64,
        hours: i64,
        pace: Option<crate::usage::Pace>,
    ) -> Option<crate::usage::Meter> {
        Some(crate::usage::Meter {
            used,
            resets_ms: now_ms() + hours * 3_600_000,
            limited: used >= 100.0,
            pace,
        })
    }

    /// Writes review renders to $TOOMUX_SHOTS: cargo test shots -- --ignored
    #[test]
    #[ignore]
    fn shots() {
        let Some(dir) = std::env::var_os("TOOMUX_SHOTS") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let now = now_ms();
        let usage = || {
            vec![
                crate::usage::Usage {
                    five: meter(91.0, 1, Some(crate::usage::Pace::LimitIn(38 * 60_000))),
                    week: meter(64.0, 80, Some(crate::usage::Pace::EndsAt(88.0))),
                    at_ms: now - 20_000,
                    source: Some(crate::usage::Source::Live),
                    problem: None,
                    trend: (0..24).map(|h| 40.0 + h as f64).collect(),
                },
                crate::usage::Usage {
                    five: None,
                    week: meter(100.0, 95, None),
                    at_ms: now - 4 * 60_000,
                    source: Some(crate::usage::Source::Fetched),
                    problem: None,
                    trend: vec![],
                },
            ]
        };
        let shoot = |name: &str, w: u16, h: u16, setup: &dyn Fn(&mut App)| {
            let mut app = App::with(cfg(), sample());
            app.usage = usage();
            app.plans = vec![Some("max 20x".into()), Some("max 5x".into())];
            setup(&mut app);
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| app.draw(f)).unwrap();
            std::fs::write(
                dir.join(format!("{name}.ansi")),
                ansi(term.backend().buffer()),
            )
            .unwrap();
        };
        shoot("usage", 170, 44, &|a| a.usage_view = Some(0));
        shoot("menu", 170, 44, &|a| {
            let mut term = Terminal::new(TestBackend::new(170, 44)).unwrap();
            term.draw(|f| a.draw(f)).unwrap();
            a.menu_at(20, 9);
        });
        shoot("list", 170, 44, &|a| a.hover = Some((20, 13)));
        shoot("narrow", 96, 40, &|_| {});
        shoot("handover", 170, 44, &|a| {
            use crate::handover::Phase;
            for s in a.sessions.iter_mut() {
                s.handover = match s.title.as_str() {
                    "Oli & Studio" => Some(Phase::Failed("the fresh session didn't start".into())),
                    "Northwind cloud AWS cost optimization" => Some(Phase::Asked { written: true }),
                    _ => s.handover.clone(),
                };
            }
        });
        shoot("sidebar", 36, 48, &|a| a.sidebar = true);
        shoot("voyage", 170, 44, &|a| {
            sail(a, "Oli & Studio", "hard", 58, 0)
        });
        shoot("sidebar-voyage", 36, 48, &|a| {
            a.sidebar = true;
            sail(a, "Oli & Studio", "relentless", 74, 1);
        });
        shoot("sidebar-usage", 36, 48, &|a| {
            a.sidebar = true;
            a.usage_view = Some(1);
        });
    }

    #[test]
    fn list_leads_with_titles_and_status() {
        let mut app = App::with(cfg(), sample());
        let out = render(&mut app, 230, 30);
        assert!(out.contains("needs you  1"), "{out}");
        assert!(
            !out.contains("needs you  1 ─"),
            "sections are separated by space, not rules:\n{out}"
        );
        assert!(out.contains("Northwind cloud AWS cost optimization"));
        assert!(
            out.contains("limit · back"),
            "a limit reads as its word and when it lifts:\n{out}"
        );
        assert!(out.contains("moves to home"));
        assert!(
            !out.contains("outside tmux") && !out.contains("northwind-studio#536"),
            "particulars live on the band, not the list:\n{out}"
        );
        assert!(
            !out.contains("handle-"),
            "registry handles must not leak into the list:\n{out}"
        );
        assert!(
            out.contains("3 ○ Resume session with Opus 5.5"),
            "pin slot shows in the gutter:\n{out}"
        );
        assert!(out.contains("pinned · not running"));
        assert!(out.contains("1 ○ Rotate Watson MySQL root"));
    }

    #[test]
    fn a_session_is_two_lines_and_only_its_word_is_coloured() {
        let mut app = App::with(cfg(), sample());
        let mut term = Terminal::new(TestBackend::new(200, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let row = |y: u16| {
            (0..200)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        let y = (0..30)
            .find(|&y| row(y).contains("Oli & Studio"))
            .expect("listed");
        assert!(
            row(y + 1).trim_start().starts_with("working · work"),
            "{}",
            row(y + 1)
        );
        assert!(
            !row(y + 2).contains("/nonexistent"),
            "no third line: {}",
            row(y + 2)
        );
        let x = row(y + 1).find("working").unwrap() as u16;
        let word = buf[(x, y + 1)].fg;
        let rest = buf[(x + 10, y + 1)].fg;
        assert_eq!(word, app.pal.working_word);
        assert_eq!(
            rest, app.pal.muted,
            "the account after the word stays neutral"
        );
        assert!(
            !row(y).contains("OLI runs token efficiency"),
            "the topic is on the band, not the list"
        );
    }

    #[test]
    fn a_limited_session_shows_its_limit_before_a_failed_handover() {
        let mut sessions = sample();
        let i = sessions
            .iter()
            .position(|s| s.title == "Oli & Studio")
            .unwrap();
        sessions[i].handover = Some(crate::handover::Phase::Failed(
            "waiting for the limit to reset".into(),
        ));
        sessions[i].limit = Some("You've hit your limit · resets 5pm".into());
        let (word, rest, _) = sessions[i].state_parts(crate::registry::now_ms());
        assert_eq!(word, "limit");
        assert!(
            rest.as_deref()
                .is_some_and(|r| r.starts_with("back ") && r.ends_with(" · hands over then")),
            "{rest:?}"
        );
    }

    #[test]
    fn a_failed_handover_says_why_in_the_attention_colour() {
        let mut sessions = sample();
        let i = sessions
            .iter()
            .position(|s| s.title == "Oli & Studio")
            .unwrap();
        sessions[i].handover = Some(crate::handover::Phase::Failed(
            "your prompt had text".into(),
        ));
        let mut app = App::with(cfg(), sessions);
        let mut term = Terminal::new(TestBackend::new(200, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let row = |y: u16| {
            (0..200)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        let y = (0..30)
            .find(|&y| row(y).contains("Oli & Studio"))
            .expect("listed");
        assert!(
            row(y + 1)
                .trim_start()
                .starts_with("handover failed · your prompt had text"),
            "{}",
            row(y + 1)
        );
        let x = row(y + 1).find("handover").unwrap() as u16;
        assert_eq!(buf[(x, y + 1)].fg, app.pal.attention_word);
    }

    /// Puts a voyage on the session titled `title`, and selects it.
    fn sail(app: &mut App, title: &str, tier: &str, progress: u8, laps: u32) {
        let s = app
            .sessions
            .iter()
            .find(|s| s.title == title)
            .unwrap()
            .clone();
        let q: crate::voyage::Voyage = serde_json::from_value(serde_json::json!({
            "id": "ab12cd34", "outcome": "the studio build passes, with every page rendering", "cwd": "/w", "sessions": [s.id],
            "started_ms": crate::registry::now_ms() - 80 * 60_000, "status": "active", "persistence": tier,
            "progress": progress, "laps": laps, "spent_usd": 3.2, "budget_usd": 20.0,
            "last": "the gallery page still 404s"
        }))
        .unwrap();
        app.voyages.insert(s.id.clone(), q);
        app.select(s.pid);
    }

    /// How many rows hold a scene: rows of sextant blocks.
    fn scene_rows(buf: &ratatui::buffer::Buffer) -> usize {
        let sextant = |s: &str| {
            s.chars()
                .next()
                .is_some_and(|c| ('\u{1FB00}'..='\u{1FB3B}').contains(&c))
        };
        (0..buf.area.height)
            .filter(|&y| {
                (0..buf.area.width)
                    .filter(|&x| sextant(buf[(x, y)].symbol()))
                    .count()
                    > 10
            })
            .count()
    }

    #[test]
    fn a_voyage_sails_in_the_detail_pane_and_keeps_to_its_chip_in_the_sidebar() {
        let mut app = App::with(cfg(), sample());
        sail(&mut app, "Oli & Studio", "hard", 58, 0);
        let out = render(&mut app, 170, 44);
        assert!(
            out.contains("◎ hard voyage 1h 20m · about 58% there, by the judge · $3.20 of $20"),
            "{out}"
        );
        assert!(
            out.contains("last check: the gallery page still 404s"),
            "{out}"
        );
        let mut term = Terminal::new(TestBackend::new(170, 44)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(
            scene_rows(term.backend().buffer()) >= crate::scene::ROWS,
            "the full scene in the detail pane"
        );

        let mut app = App::with(cfg(), sample());
        app.sidebar = true;
        sail(&mut app, "Oli & Studio", "relentless", 74, 1);
        let out = render(&mut app, 36, 48);
        assert!(
            out.contains("◎ relentless voyage,") && out.contains("proof lap 1 of 2"),
            "{out}"
        );
        assert!(
            out.contains("74% there"),
            "the judge's estimate stands in for the ship: {out}"
        );
        let mut term = Terminal::new(TestBackend::new(36, 48)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert_eq!(
            scene_rows(term.backend().buffer()),
            0,
            "no scene in the sidebar"
        );

        let mut app = App::with(cfg(), sample());
        app.cfg.voyage_scene = false;
        sail(&mut app, "Oli & Studio", "hard", 58, 0);
        let mut term = Terminal::new(TestBackend::new(170, 44)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert_eq!(
            scene_rows(term.backend().buffer()),
            0,
            "voyage_scene = false keeps it to the chip"
        );
    }

    #[test]
    fn a_voyage_shows_beside_its_session_and_keeps_its_place() {
        let sessions = sample();
        let s = sessions.iter().find(|s| s.title == "Oli & Studio").unwrap();
        let q: crate::voyage::Voyage = serde_json::from_value(serde_json::json!({
            "id": "ab12cd34", "outcome": "the studio build passes", "cwd": "/w", "sessions": [s.id],
            "started_ms": crate::registry::now_ms() - 80 * 60_000, "status": "active"
        }))
        .unwrap();
        let mut app = App::with(cfg(), sessions.clone());
        app.voyages.insert(q.session().to_string(), q);
        // Narrow: the chip outlasts the account and the rest.
        let mut term = Terminal::new(TestBackend::new(70, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let row = |y: u16| {
            (0..70)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        let y = (0..30)
            .find(|&y| row(y).contains("Oli & Studio"))
            .expect("listed");
        let detail = (y + 1..y + 3)
            .map(row)
            .find(|r| r.contains('◎'))
            .unwrap_or_else(|| panic!("{}\n{}", row(y + 1), row(y + 2)));
        assert!(detail.contains("◎ steady voyage 1h 20m"), "{detail}");
        let (x, yy) = (
            detail
                .find('◎')
                .map(|b| detail[..b].chars().count())
                .unwrap() as u16,
            (y + 1..y + 3).find(|&r| row(r).contains('◎')).unwrap(),
        );
        assert_eq!(buf[(x, yy)].fg, app.pal.accent);
    }

    #[test]
    fn narrow_terminal_wraps_instead_of_clipping() {
        let mut app = App::with(cfg(), sample());
        let out = render(&mut app, 70, 30);
        // Every word of the long title survives somewhere on screen.
        for word in ["Northwind", "cloud", "AWS", "cost", "optimization"] {
            assert!(out.contains(word), "{word} missing:\n{out}");
        }
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let mut app = App::with(cfg(), sample());
        for (w, h) in [(20, 5), (1, 1), (40, 3), (200, 4)] {
            render(&mut app, w, h);
        }
    }

    #[test]
    fn empty_and_filtered_states() {
        let mut app = App::with(cfg(), vec![]);
        assert!(render(&mut app, 100, 10).contains("no claude sessions running"));
        let mut app = App::with(cfg(), sample());
        app.filter = "zzz".into();
        app.rebuild();
        assert!(render(&mut app, 100, 10).contains("nothing matches"));
        app.filter = "oli".into();
        app.rebuild();
        let out = render(&mut app, 100, 10);
        assert!(out.contains("Oli & Studio") && !out.contains("Add to list"));
    }

    #[test]
    fn footer_fits_narrow_widths_and_follows_selection() {
        let mut app = App::with(cfg(), sample());
        let wide = render(&mut app, 200, 20);
        let foot = wide.lines().last().unwrap();
        assert!(
            foot.contains("enter jump")
                && foot.contains("^a move")
                && foot.contains("? keys")
                && foot.contains("esc quit"),
            "{foot}"
        );
        assert!(
            !foot.contains("rename") && !foot.contains("sweep") && !foot.contains("tab group"),
            "the rest is under ?: {foot}"
        );
        let narrow = render(&mut app, 40, 20);
        let foot = narrow.lines().last().unwrap();
        assert!(
            foot.contains("enter jump") && foot.contains("esc quit"),
            "{foot}"
        );
        app.sel = Some(-1);
        let out = render(&mut app, 200, 20);
        let foot = out.lines().last().unwrap();
        assert!(
            foot.contains("enter reopen") && !foot.contains("^a"),
            "{foot}"
        );
    }

    #[test]
    fn alt_a_opens_accounts_with_status_and_closes_with_escape() {
        let mut app = App::with(cfg(), sample());
        app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
        assert!(matches!(app.mode, Mode::Accounts(_)));
        let out = render(&mut app, 120, 32);
        assert!(out.contains("accounts"), "{out}");
        assert!(out.contains("work") && out.contains("home"), "{out}");
        assert!(out.contains("not logged in"), "{out}");
        assert!(out.contains("running session"), "{out}");
        let foot = out.lines().last().unwrap();
        for hint in [
            "a add",
            "l login",
            "s share",
            "u unshare",
            "x remove",
            "esc close",
        ] {
            assert!(foot.contains(hint), "missing {hint}: {foot}");
        }
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn add_account_wizard_validates_name_folder_and_sharing_before_mutation() {
        let mut app = App::with(cfg(), sample());
        app.exec(Cmd::Accounts);
        app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        for c in "work".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let Mode::Accounts(view) = &app.mode else {
            panic!("accounts should stay open");
        };
        assert!(
            view.flow
                .as_ref()
                .and_then(|f| f.hint.as_deref())
                .is_some_and(|h| h.contains("already an account")),
            "duplicate names should be rejected while typing"
        );
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Mode::Accounts(view) = &app.mode else {
            panic!("accounts should stay open");
        };
        let flow = view.flow.as_ref().unwrap();
        assert_eq!(flow.step, AddStep::Name);
        assert!(
            flow.hint
                .as_deref()
                .is_some_and(|h| h.contains("already an account"))
        );

        app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "wiztest".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        if let Mode::Accounts(view) = &mut app.mode {
            let flow = view.flow.as_mut().unwrap();
            assert_eq!(flow.step, AddStep::Folder);
            flow.folder = format!("/tmp/toomux-ui-wiztest-{}", std::process::id());
        } else {
            panic!("accounts should stay open");
        }
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Mode::Accounts(view) = &app.mode else {
            panic!("accounts should stay open");
        };
        assert_eq!(view.flow.as_ref().unwrap().step, AddStep::Sharing);
        let out = render(&mut app, 120, 32);
        assert!(out.contains("stand alone"), "{out}");
        assert!(out.contains("join shared"), "{out}");
        assert!(out.contains("new group"), "{out}");

        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            app.mode,
            Mode::Accounts(AccountsView {
                flow: Some(AddFlow {
                    step: AddStep::Folder,
                    ..
                }),
                ..
            })
        ));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            app.mode,
            Mode::Accounts(AccountsView { flow: None, .. })
        ));
    }

    #[test]
    fn accounts_rows_are_touch_selectable() {
        let mut app = App::with(cfg(), sample());
        app.exec(Cmd::Accounts);
        let _ = render(&mut app, 120, 32);
        let row = app
            .list_hits
            .iter()
            .find(|(_, i)| *i == 1)
            .map(|(r, _)| *r)
            .expect("second account row");
        app.click(row.x + 1, row.y + 1);
        let Mode::Accounts(view) = &app.mode else {
            panic!("accounts should stay open");
        };
        assert_eq!(view.sel, 1);
    }

    #[test]
    fn help_advertises_accounts_shortcut() {
        let mut app = App::with(cfg(), sample());
        app.exec(Cmd::Help);
        let out = render(&mut app, 120, 40);
        assert!(
            out.contains("alt-a") && out.contains("accounts: sign in, add, share"),
            "{out}"
        );
    }

    #[test]
    fn account_remove_offer_names_keep_and_delete_folder_choices() {
        let mut app = App::with(cfg(), sample());
        app.exec(Cmd::Accounts);
        app.exec(Cmd::AccountRemove);
        let out = render(&mut app, 140, 24);
        assert!(out.contains("remove work from toomux?"), "{out}");
        let foot = out.lines().last().unwrap();
        assert!(foot.contains("enter keep folder"), "{foot}");
        assert!(foot.contains("a delete folder"), "{foot}");
        assert!(foot.contains("esc back"), "{foot}");
    }

    #[test]
    fn selection_background_covers_the_whole_item() {
        for sidebar in [false, true] {
            let mut app = App::with(cfg(), sample());
            app.sidebar = sidebar;
            let mut term =
                Terminal::new(TestBackend::new(if sidebar { 36 } else { 160 }, 30)).unwrap();
            term.draw(|f| app.draw(f)).unwrap();
            let buf = term.backend().buffer().clone();
            let sel_bg = app.pal.selection;
            let rows: Vec<u16> = (0..30).filter(|&y| buf[(2, y)].bg == sel_bg).collect();
            assert!(
                rows.len() >= 2,
                "sidebar={sidebar}: highlighted rows {rows:?}"
            );
            assert_eq!(
                rows.last().unwrap() - rows[0] + 1,
                rows.len() as u16,
                "contiguous"
            );
        }
    }

    #[test]
    fn sidebar_is_compact() {
        let mut app = App::with(cfg(), sample());
        app.sidebar = true;
        let out = render(&mut app, 36, 40);
        assert!(
            !out.contains("outside tmux") && !out.contains("esc quit"),
            "{out}"
        );
        assert!(out.lines().all(|l| l.chars().count() <= 36));
        assert!(out.contains("Northwind cloud AWS cost"));
        assert!(
            out.contains("type to filter"),
            "the list carries its own filter line:\n{out}"
        );
    }

    #[test]
    fn narrow_detail_gives_way_instead_of_wrapping() {
        let mut sessions = sample();
        let mut asking = session(8, "Fork agent", St::NeedsYou, 1);
        asking.waiting_for = Some("permission to run a command".into());
        sessions.push(asking);
        let mut app = App::with(cfg(), sessions);
        app.sidebar = true;
        let out = render(&mut app, 36, 40);
        let lines: Vec<&str> = out.lines().collect();
        let i = lines.iter().position(|l| l.contains("Fork agent")).unwrap();
        assert!(
            lines[i + 1].contains("needs you · home"),
            "account kept, the generic detail gave way: {}",
            lines[i + 1]
        );
        assert!(
            !lines[i + 2].trim().starts_with("home"),
            "no third line: {}",
            lines[i + 2]
        );
        let j = lines
            .iter()
            .position(|l| l.contains("Northwind cloud AWS"))
            .unwrap();
        assert!(
            lines[j..j + 3].iter().any(|l| l.contains("limit · back")),
            "a limit keeps when it lifts:\n{out}"
        );
    }

    #[test]
    fn rename_edits_in_the_footer() {
        let mut app = App::with(cfg(), sample());
        app.exec(Cmd::Rename);
        let out = render(&mut app, 160, 20);
        assert!(
            out.lines()
                .last()
                .unwrap()
                .contains("name  Northwind cloud AWS cost optimization▏"),
            "{out}"
        );
    }

    #[test]
    fn limited_sessions_can_move_together() {
        let mut sessions = sample();
        let mut second = session(9, "Fork agent", St::NeedsYou, 1);
        second.limit = Some("weekly limit · resets Oct 3, 5pm".into());
        sessions.push(second);
        let mut app = App::with(cfg(), sessions);
        app.sel = Some(1);
        app.exec(Cmd::Account);
        let out = render(&mut app, 200, 20);
        let foot = out.lines().last().unwrap();
        assert!(
            foot.contains("enter this one") && foot.contains("a all 2 limited"),
            "{foot}"
        );
    }

    #[test]
    fn sweep_lists_only_stale_sessions() {
        let mut sessions = sample();
        let mut old = session(7, "Mouse button workspace navigation", St::Idle, 0);
        old.since_ms = now_ms() - 6 * 86_400_000;
        sessions.push(old);
        let mut app = App::with(cfg(), sessions);
        app.exec(Cmd::Sweep);
        let out = render(&mut app, 200, 20);
        assert!(out.contains("✕ Mouse button workspace navigation"), "{out}");
        assert!(
            !out.contains("Oli & Studio"),
            "working sessions are never swept:\n{out}"
        );
        assert!(out.contains("sweep · closing 1 of 1"));
        app.exec(Cmd::Toggle);
        let out = render(&mut app, 200, 20);
        assert!(
            out.lines().last().unwrap().contains("enter close 0"),
            "{out}"
        );
        app.exec(Cmd::No);
        assert!(render(&mut app, 200, 50).contains("Oli & Studio"));
    }

    #[test]
    fn confirm_prompt_replaces_footer() {
        let mut app = App::with(cfg(), sample());
        app.exec(Cmd::Close);
        let out = render(&mut app, 160, 20);
        assert!(
            out.contains("close Northwind cloud AWS cost optimization?"),
            "{out}"
        );
        assert!(out.contains("enter yes"));
    }

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wrap_drops_separators_at_breaks() {
        let st = Style::new();
        let segs = [
            ("idle 3m".to_string(), st),
            (" · ".to_string(), st),
            ("tasks running".to_string(), st),
        ];
        let lines = wrap_spans(&segs, 10, 10);
        let got: Vec<String> = lines.iter().map(|l| text(l)).collect();
        assert_eq!(got, vec!["idle 3m", "tasks", "running"]);
    }

    #[test]
    fn wrap_splits_words_longer_than_a_line() {
        let lines = wrap_spans(&[("abcdefghijklmnop".to_string(), Style::new())], 8, 8);
        let got: Vec<String> = lines.iter().map(|l| text(l)).collect();
        assert_eq!(got, vec!["abcdefgh", "ijklmnop"]);
    }

    #[test]
    fn reflow_closes_gaps_clips_rules_and_wraps_prose() {
        let status = Line::from(format!("left part{}right part", " ".repeat(40)));
        let out = reflow(status, 30);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].width(), 30);
        assert!(
            text(&out[0].spans).starts_with("left part ")
                && text(&out[0].spans).ends_with(" right part")
        );

        let rule = Line::from("─".repeat(200));
        let out = reflow(rule, 30);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].width(), 30);

        let prose = Line::from("word ".repeat(20));
        let out = reflow(prose, 30);
        assert!(out.len() > 1 && out.iter().all(|l| l.width() <= 30));

        // A dialog's box stays a box: borders clipped, text wrapped inside.
        let top = Line::from(format!("╭{}╮", "─".repeat(78)));
        let out = reflow(top, 30);
        assert_eq!(out.len(), 1);
        assert_eq!(text(&out[0].spans), format!("╭{}╮", "─".repeat(28)));
        let side = Line::from(format!(
            "│ Do you want to make this edit to invoice.rs?{} │",
            " ".repeat(33)
        ));
        let out: Vec<String> = reflow(side, 30).iter().map(|l| text(&l.spans)).collect();
        assert!(out.len() > 1, "{out:?}");
        assert!(
            out.iter()
                .all(|l| l.starts_with("│ ") && l.ends_with(" │") && l.width() == 30),
            "{out:?}"
        );
        assert!(
            out[0].contains("Do you want") && out[1].contains("invoice.rs?"),
            "{out:?}"
        );
    }

    fn with_usage() -> App {
        let mut app = App::with(cfg(), sample());
        app.usage = vec![
            crate::usage::Usage {
                five: meter(91.0, 1, Some(crate::usage::Pace::LimitIn(38 * 60_000))),
                week: meter(64.0, 80, None),
                at_ms: now_ms(),
                source: Some(crate::usage::Source::Live),
                ..Default::default()
            },
            crate::usage::Usage {
                week: meter(100.0, 95, None),
                at_ms: now_ms(),
                source: Some(crate::usage::Source::Fetched),
                ..Default::default()
            },
        ];
        app
    }

    #[test]
    fn header_carries_usage_meters_and_narrows_gracefully() {
        let mut app = with_usage();
        // A limit that just hit is news: rose, with its bar.
        let fresh = render(&mut app, 200, 20);
        assert!(
            fresh
                .lines()
                .next()
                .unwrap()
                .contains("home  wk ━━━━━━━━━━ limit · back"),
            "{fresh}"
        );
        // Hours later it's a settled fact.
        app.sessions
            .iter_mut()
            .filter(|s| s.limit.is_some())
            .for_each(|s| s.since_ms = now_ms() - 2 * 3_600_000);
        let wide = render(&mut app, 200, 20);
        let head = wide.lines().next().unwrap();
        assert!(
            head.contains("work  5h ━") && head.contains("91%") && head.contains("wk ━"),
            "{head}"
        );
        assert!(
            head.contains("home  wk at limit · back"),
            "a settled limit is quiet: {head}"
        );
        assert!(
            !head.contains("◆") && !head.contains("by attention"),
            "counts are the list's, the filter is the list's: {head}"
        );
        let narrow = render(&mut app, 70, 20);
        let head = narrow.lines().next().unwrap();
        assert!(
            head.contains("5h 91%") && head.contains("at limit"),
            "numbers survive without bars: {head}"
        );
    }

    #[test]
    fn usage_view_explains_pace_and_room() {
        let mut app = with_usage();
        app.exec(Cmd::UsageToggle);
        let out = render(&mut app, 180, 44);
        assert!(out.contains("at this pace the limit comes in 38m"), "{out}");
        assert!(out.contains("at the limit"));
        app.usage[0].five = meter(30.0, 3, None);
        app.usage[0].week = meter(40.0, 80, None);
        app.usage_view = None;
        app.sel = Some(1);
        let out = render(&mut app, 180, 44);
        assert!(
            out.contains("work has room (5h 30% · wk 40%) · ^a moves it"),
            "limited session points at the account with room:\n{out}"
        );
        app.exec(Cmd::UsageToggle);
        app.step(1);
        assert!(
            app.usage_view.is_none(),
            "moving through sessions leaves the usage view"
        );
    }

    #[test]
    fn right_click_offers_the_session_actions() {
        let mut app = App::with(cfg(), sample());
        render(&mut app, 160, 30);
        let (r, pid) = app
            .list_hits
            .iter()
            .find(|(_, p)| *p == 2)
            .copied()
            .unwrap();
        app.menu_at(r.x + 3, r.y);
        let out = render(&mut app, 160, 30);
        assert!(
            out.contains("jump to it") && out.contains("move to home") && out.contains("^x"),
            "{out}"
        );
        let Mode::Menu(m) = &app.mode else {
            panic!("menu should be open")
        };
        let pin = m
            .items
            .iter()
            .position(|(_, c)| matches!(c, Cmd::Pin))
            .unwrap();
        assert_eq!(m.pid, pid);
        let _ = pin; // executing would write real pin state; the mapping is what matters here
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn reflow_breaks_at_words_and_hangs_under_bullets() {
        let line = Line::from("● Agent terminated early due to an API error while working on it");
        let out: Vec<String> = reflow(line, 24).iter().map(|l| text(&l.spans)).collect();
        assert_eq!(out[0], "● Agent terminated early");
        assert!(
            out[1..]
                .iter()
                .all(|l| l.starts_with("  ") && !l.starts_with("   ")),
            "{out:?}"
        );
        assert!(out.iter().all(|l| l.width() <= 24));
        assert_eq!(
            out.join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            "● Agent terminated early due to an API error while working on it"
        );
    }

    #[test]
    fn palette_contrast_holds_on_every_surface() {
        fn lum(c: Color) -> f64 {
            let Color::Rgb(r, g, b) = c else { panic!() };
            let f = |x: u8| {
                let x = x as f64 / 255.0;
                if x <= 0.03928 {
                    x / 12.92
                } else {
                    ((x + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
        }
        let ratio = |a: Color, b: Color| {
            let (x, y) = (lum(a), lum(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        };
        let p = Palette::new(&cfg());
        for (name, ground) in [
            ("base", p.base),
            ("raised", p.raised),
            ("well", p.well),
            ("overlay", p.overlay),
            ("selection", p.selection),
        ] {
            assert!(ratio(p.text, ground) >= 7.0, "text on {name}");
            assert!(ratio(p.dim, ground) >= 4.5, "dim on {name}");
            assert!(ratio(p.muted, ground) >= 3.0, "muted on {name}");
            for (s, c) in [
                ("accent", p.accent),
                ("working", p.working),
                ("attention", p.attention),
                ("finished", p.finished),
            ] {
                assert!(ratio(c, ground) >= 4.5, "{s} on {name}");
            }
        }
        assert!(
            ratio(p.muted, p.base) >= 4.0,
            "tertiary words stay readable on the list"
        );
    }

    #[test]
    fn markdown_tables_align() {
        let p = Palette::new(&cfg());
        let md =
            "| Check | Result |\n|---|---|\n| Full suite | 5,008 / 5,008 |\n| Fuzz | 2 failing |";
        let lines: Vec<String> = markdown(md, 80, &p)
            .iter()
            .map(|l| text(&l.spans))
            .collect();
        assert_eq!(lines[0], "Check      │ Result");
        assert!(lines[1].starts_with("───"));
        assert_eq!(lines[2], "Full suite │ 5,008 / 5,008");
    }
}
