//! `toomux` as the whole terminal: header, the session list, and the chosen
//! session live and interactive beside it. The live view comes from tmux
//! control mode, so there is no second tmux screen and nothing is polled:
//! the loop sleeps until a key, pane output or a registry change arrives.

use super::*;
use crate::control::{self, Control};
use crate::live::{self, Live};
use alacritty_terminal::vte::ansi::CursorShape;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, MouseEvent};

pub(super) struct Shell {
    /// In alt-s's popup, over the tmux window you were in.
    pub(super) popup: bool,
    /// The terminal (tmux client) the popup opened in, which alt-j moves.
    pub(super) origin: Option<String>,
    control: Control,
    /// Bumped with each control client, so a replaced one's last words
    /// (its exit) aren't taken for the current one's.
    generation: u64,
    tx: mpsc::Sender<Msg>,
    live: Option<Live>,
    /// Keys go to the live session; otherwise to the list.
    pub(super) focus_live: bool,
    pub(super) show_list: bool,
    /// Where the live view was last drawn.
    area: Rect,
    /// The client size last asked of tmux.
    sized: (u16, u16),
    /// Show this pane once the selection has rested on it briefly.
    want: Option<(String, Instant)>,
    /// Re-seed after tmux reshaped panes (resizes settle first).
    reseed_at: Option<Instant>,
    /// Pane commands waiting to be sent together.
    outbox: Vec<String>,
    cursor: Option<CursorShape>,
    applied_cursor: Option<CursorShape>,
    drawn_version: u64,
    gone: bool,
    /// The pane listing asked of tmux for the next reload.
    listing: Option<u64>,
    /// `toomux status`, run as a status line would run it: with only this
    /// control client attached, no status line draws, and nothing else runs
    /// the tick (the index, upkeep, handovers, voyages).
    ticker: Option<std::process::Child>,
    ticked: Instant,
}

enum Msg {
    Input(Event),
    Tmux(u64, control::Event),
    Changed,
    /// The terminal hung up.
    Gone,
}

pub fn run(cfg: Config, popup: bool) -> Result<()> {
    // Each session has a tmux server of its own, which exits with it: a
    // write to its control client can meet a closed pipe. That's an error
    // to shrug off (the client is replaced), not a reason to die.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    crate::ui::exit_with_terminal();
    let mut term = ratatui::init();
    crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let res = run_inner(cfg, popup, &mut term);
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        SetCursorStyle::DefaultUserShape
    );
    ratatui::restore();
    res
}

fn run_inner(cfg: Config, popup: bool, term: &mut DefaultTerminal) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            loop {
                if crate::ui::terminal_gone() {
                    let _ = tx.send(Msg::Gone);
                    break;
                }
                let ev = match event::poll(Duration::from_millis(500)) {
                    Ok(true) => event::read(),
                    Ok(false) => continue,
                    Err(e) => Err(e),
                };
                let msg = match ev {
                    Ok(ev) => Msg::Input(ev),
                    Err(_) => Msg::Gone,
                };
                let gone = matches!(msg, Msg::Gone);
                if tx.send(msg).is_err() || gone {
                    break;
                }
            }
        });
    }
    let _watcher = {
        let tx = tx.clone();
        use notify::{RecursiveMode, Watcher};
        let mut w = notify::recommended_watcher(move |_res| {
            let _ = tx.send(Msg::Changed);
        })
        .ok();
        if let Some(w) = w.as_mut() {
            for d in cfg.session_dirs() {
                let _ = w.watch(&d, RecursiveMode::NonRecursive);
            }
        }
        w
    };
    // Ask which terminal opened the popup before our control client joins:
    // tmux answers with the client that last had a key, and that is it.
    let origin = if popup {
        crate::tmux::run(&["display-message", "-p", "#{client_name}"])
            .ok()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
    } else {
        None
    };
    let mut app = App::new(cfg);
    // Attach to the server of the session that most wants attention.
    let server = app.selected_server();
    let control = Control::start(&server, tx.clone(), |e| Msg::Tmux(0, e))
        .or_else(|_| Control::start("default", tx.clone(), |e| Msg::Tmux(0, e)))?;
    app.shell = Some(Shell {
        popup,
        origin,
        control,
        generation: 0,
        tx: tx.clone(),
        live: None,
        focus_live: false,
        show_list: true,
        area: Rect::default(),
        sized: (0, 0),
        want: None,
        reseed_at: None,
        outbox: Vec::new(),
        cursor: None,
        applied_cursor: None,
        drawn_version: 0,
        gone: false,
        listing: None,
        ticker: None,
        ticked: Instant::now() - Duration::from_secs(10),
    });
    crate::usage::fetch_in_background(&app.cfg, now_ms());
    // Start on the session that most wants attention.
    if let Some(p) = app
        .selected()
        .and_then(|s| s.pane.as_ref())
        .map(|p| p.id.clone())
    {
        app.shell_mut().want = Some((p, Instant::now()));
    }
    let mut last_sel = app.sel;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let frame = Duration::from_millis(16);
    while !app.quit {
        let now = Instant::now();
        // Sleep until something happens, or the next thing that's due.
        let mut due = app.loaded_at + Duration::from_secs(if app.changed { 0 } else { 2 });
        if app.changed {
            due = due.max(app.loaded_at + Duration::from_millis(500));
        }
        let sh = app.shell_ref();
        if let Some((_, t)) = &sh.want {
            due = due.min(*t + Duration::from_millis(90));
        }
        if let Some(t) = sh.reseed_at {
            due = due.min(t);
        }
        if app.dirty {
            due = due.min(last_draw + frame);
        }
        if let Some(tick) = app.animating() {
            due = due.min(last_draw + tick);
        }
        match rx.recv_timeout(due.saturating_duration_since(now)) {
            Ok(m) => app.handle(m),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(m) = rx.try_recv() {
            app.handle(m);
        }
        app.flush();
        let now = Instant::now();
        let reload_due = now >= app.loaded_at + Duration::from_secs(2)
            || (app.changed && now >= app.loaded_at + Duration::from_millis(500));
        if reload_due && app.shell_ref().listing.is_none() {
            if app.shell_ref().gone {
                app.reload();
                let server = app.selected_server();
                app.restart_control(&server);
            } else {
                // The reload happens when tmux answers with its panes.
                let sh = app.shell_mut();
                let seq = sh.control.one(format!(
                    "list-panes -a -F {}",
                    control::quote(crate::tmux::PANES_FORMAT)
                ));
                sh.listing = Some(seq);
                app.loaded_at = now;
            }
        }
        // The live view follows the list only while you're moving through it;
        // a reload that shifts the selection never takes the session away.
        if app.sel != last_sel && app.shell_ref().focus_live {
            last_sel = app.sel;
        }
        if app.sel != last_sel {
            last_sel = app.sel;
            let pane = app
                .selected()
                .and_then(|s| s.pane.as_ref())
                .map(|p| p.id.clone());
            let sh = app.shell_mut();
            match pane {
                Some(p) if sh.live.as_ref().is_none_or(|l| l.pane != p) => sh.want = Some((p, now)),
                _ => sh.want = None,
            }
            app.dirty = true;
        }
        if let Some((p, t)) = app.shell_ref().want.clone()
            && now >= t + Duration::from_millis(90)
        {
            app.shell_mut().want = None;
            app.show_live(&p);
        }
        if app.shell_ref().reseed_at.is_some_and(|t| now >= t) {
            app.shell_mut().reseed_at = None;
            app.reseed();
        }
        let version = app.shell_ref().live.as_ref().map_or(0, |l| l.version);
        if version != app.shell_ref().drawn_version {
            app.dirty = true;
        }
        app.tick_status();
        if app.memory.as_mut().is_some_and(|m| m.poll()) {
            app.dirty = true;
        }
        let tick = app.animating().is_some_and(|t| last_draw.elapsed() >= t);
        if (app.dirty && last_draw.elapsed() >= frame) || tick {
            app.shell_mut().drawn_version = version;
            term.draw(|f| app.draw_shell(f))?;
            app.set_cursor_style();
            app.dirty = false;
            last_draw = Instant::now();
        }
    }
    Ok(())
}

impl App {
    /// Every 2 seconds, the tick a status line would run; one at a time.
    fn tick_status(&mut self) {
        let sh = self.shell_mut();
        if let Some(c) = sh.ticker.as_mut() {
            if matches!(c.try_wait(), Ok(None)) {
                return;
            }
            sh.ticker = None;
        }
        if sh.ticked.elapsed() < Duration::from_secs(2) {
            return;
        }
        sh.ticked = Instant::now();
        let Ok(me) = std::env::current_exe() else {
            return;
        };
        sh.ticker = std::process::Command::new(me)
            .arg("status")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok();
    }

    pub(super) fn shell_ref(&self) -> &Shell {
        self.shell.as_ref().expect("shell mode")
    }

    pub(super) fn shell_mut(&mut self) -> &mut Shell {
        self.shell.as_mut().expect("shell mode")
    }

    /// The server of the selected session's pane, else the default one.
    fn selected_server(&self) -> String {
        self.selected()
            .and_then(|s| s.pane.as_ref())
            .and_then(|p| crate::tmux::server_of(&p.id).map(str::to_string))
            .unwrap_or_else(|| "default".into())
    }

    /// A fresh control client on `server` (each session has a server of its
    /// own, so following another session can mean another server).
    fn restart_control(&mut self, server: &str) -> bool {
        let sh = self.shell_mut();
        let g = sh.generation + 1;
        let started = Control::start(server, sh.tx.clone(), move |e| Msg::Tmux(g, e))
            .or_else(|_| Control::start("default", sh.tx.clone(), move |e| Msg::Tmux(g, e)));
        let Ok(c) = started else { return false };
        sh.generation = g;
        sh.control = c;
        sh.gone = false;
        sh.sized = (0, 0);
        sh.live = None;
        sh.listing = None;
        sh.outbox.clear();
        self.dirty = true;
        true
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Changed => self.changed = true,
            Msg::Gone => self.quit = true,
            Msg::Tmux(g, ev) if g == self.shell_ref().generation => self.tmux_event(ev),
            Msg::Tmux(..) => {}
            Msg::Input(ev) => {
                self.dirty = true;
                match ev {
                    Event::Key(k) if k.kind != KeyEventKind::Release => self.shell_key(k),
                    Event::Mouse(m) => self.shell_mouse(m),
                    Event::Paste(text) => self.shell_paste(text),
                    _ => {}
                }
            }
        }
    }

    fn tmux_event(&mut self, ev: control::Event) {
        let sh = self.shell_mut();
        match ev {
            control::Event::Output { pane, data } => {
                if let Some(l) = sh
                    .live
                    .as_mut()
                    .filter(|l| crate::tmux::bare(&l.pane) == pane)
                {
                    l.output(&data);
                }
            }
            control::Event::Reply { seq, lines, .. } if sh.listing == Some(seq) => {
                sh.listing = None;
                crate::tmux::give_panes(&sh.control.server, &lines.join("\n"));
                self.reload();
                crate::tmux::forget_panes();
            }
            control::Event::Reply { seq, lines, .. } => {
                if let Some(l) = sh.live.as_mut()
                    && l.reply(seq, lines)
                {
                    self.dirty = true;
                }
            }
            control::Event::Layout => {
                if sh.live.is_some() {
                    sh.reseed_at = Some(Instant::now() + Duration::from_millis(120));
                }
            }
            control::Event::Session => {}
            control::Event::Exit => {
                sh.gone = true;
                sh.listing = None;
                sh.live = None;
                self.dirty = true;
            }
        }
    }

    /// Point our control client at a pane and start following it.
    pub(super) fn show_live(&mut self, pane: &str) {
        let (w, h) = self.shell_ref().area_size();
        let server = crate::tmux::server_of(pane)
            .unwrap_or("default")
            .to_string();
        if server != self.shell_ref().control.server && !self.restart_control(&server) {
            return;
        }
        let sh = self.shell_mut();
        if sh.gone {
            return;
        }
        let live = Live::new(pane, w, h);
        let pane = crate::tmux::bare(pane);
        let mut cmds = vec![
            format!("switch-client -t {pane}"),
            format!("select-window -t {pane}"),
            format!("select-pane -t {pane}"),
        ];
        cmds.extend(live.seed_commands());
        let seqs = sh.control.send(&cmds);
        let mut live = live;
        live.seed = Some((seqs[3], seqs[4]));
        sh.live = Some(live);
        self.dirty = true;
    }

    fn reseed(&mut self) {
        let sh = self.shell_mut();
        let Some(l) = sh.live.as_ref() else { return };
        let pane = l.pane.clone();
        let (w, h) = (l.cols, l.lines);
        let mut fresh = Live::new(&pane, w, h);
        let seqs = sh.control.send(&fresh.seed_commands());
        fresh.seed = Some((seqs[0], seqs[1]));
        // Keep drawing the old picture until the new one is in.
        let old = sh.live.replace(fresh);
        let _ = old;
    }

    /// Queued keys and clicks for the pane, as one line to tmux.
    fn flush(&mut self) {
        let sh = self.shell_mut();
        if sh.outbox.is_empty() {
            return;
        }
        let cmds = std::mem::take(&mut sh.outbox);
        sh.control.send(&cmds);
    }

    /// The live pane, qualified with its server.
    fn live_pane(&self) -> Option<String> {
        self.shell_ref().live.as_ref().map(|l| l.pane.clone())
    }

    /// The live pane as its own server (our control client's) names it.
    fn live_target(&self) -> Option<String> {
        self.live_pane().map(|p| crate::tmux::bare(&p).to_string())
    }

    fn shell_key(&mut self, k: KeyEvent) {
        let alt = k.modifiers == KeyModifiers::ALT;
        let focus_live = self.shell_ref().focus_live;
        // Keys toomux keeps, wherever the keyboard is.
        match k.code {
            // Android's immersive TUI uses alt-? as a focus-independent way
            // to open Toomux's own help overlay. Plain '?' still belongs to
            // the live terminal when that pane has the keyboard.
            KeyCode::Char('?') if alt => {
                self.mode = Mode::Help;
                return;
            }
            KeyCode::Char('s') if alt => {
                let sh = self.shell_mut();
                sh.show_list = true;
                sh.focus_live = !sh.focus_live && sh.live.is_some();
                if !focus_live {
                    // alt-s from the list goes back to the session.
                } else {
                    self.mode = Mode::Normal;
                }
                return;
            }
            KeyCode::Char('b') if alt => {
                let sh = self.shell_mut();
                sh.show_list = !sh.show_list;
                if !sh.show_list && sh.live.is_some() {
                    sh.focus_live = true;
                }
                return;
            }
            KeyCode::Char('u') if alt => {
                self.exec(Cmd::UsageToggle);
                return;
            }
            KeyCode::Char('m') if alt => {
                self.exec(Cmd::MemoryToggle);
                return;
            }
            KeyCode::Char('a') if alt => {
                let sh = self.shell_mut();
                sh.show_list = true;
                sh.focus_live = false;
                self.exec(Cmd::Accounts);
                return;
            }
            KeyCode::Char(c @ '1'..='9') if alt => {
                self.exec(Cmd::JumpPin(c as usize - '1' as usize));
                return;
            }
            // In the popup: close toomux and be in that session's tmux
            // window, the session with the keyboard or the one selected.
            KeyCode::Char('j') if alt && self.shell_ref().popup => {
                self.mode = Mode::Normal;
                self.exec(Cmd::GoThere);
                return;
            }
            _ => {}
        }
        // The memory graph has the keyboard while it's up.
        if let Some(m) = self.memory.as_mut() {
            if !m.key(k) {
                self.memory = None;
            }
            self.dirty = true;
            return;
        }
        if focus_live && self.usage_view.is_none() {
            let Some(pane) = self.live_target() else {
                self.shell_mut().focus_live = false;
                return;
            };
            let sh = self.shell_mut();
            if let Some(l) = sh.live.as_mut() {
                l.to_bottom();
            }
            match live::key(k) {
                Some(live::Keys::Text(b)) => {
                    // Merge with a text command just before it.
                    let prefix = format!("send-keys -t {pane} -H ");
                    match sh.outbox.last_mut() {
                        Some(last) if last.starts_with(&prefix) => {
                            last.push(' ');
                            last.push_str(&live::hex(&b));
                        }
                        _ => sh.outbox.push(format!("{prefix}{}", live::hex(&b))),
                    }
                }
                Some(live::Keys::Named(n)) => sh.outbox.push(format!("send-keys -t {pane} {n}")),
                None => {}
            }
            return;
        }
        // The list has the keyboard: everything toomux already does, except
        // that esc goes back to the session rather than quitting.
        let plain_esc = k.code == KeyCode::Esc
            && matches!(self.mode, Mode::Normal)
            && self.filter.is_empty()
            && self.usage_view.is_none();
        if plain_esc {
            let sh = self.shell_mut();
            if sh.live.is_some() {
                sh.focus_live = true;
            }
            return;
        }
        self.key(k);
    }

    fn shell_paste(&mut self, text: String) {
        let sh = self.shell_ref();
        if !sh.focus_live {
            if matches!(self.mode, Mode::Normal) {
                self.filter.push_str(text.lines().next().unwrap_or(""));
                self.rebuild();
            }
            return;
        }
        let Some(pane) = self.live_target() else {
            return;
        };
        // paste-buffer -p brackets the text if the program asked for that.
        let sh = self.shell_mut();
        sh.outbox.push(format!(
            "set-buffer -b toomux-paste -- {}",
            control::quote(&text)
        ));
        sh.outbox
            .push(format!("paste-buffer -p -d -b toomux-paste -t {pane}"));
    }

    fn shell_mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        if let Some(v) = self.memory.as_mut() {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                v.click(x, y);
                self.dirty = true;
            }
            return;
        }
        let area = self.shell_ref().area;
        let in_live =
            x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height;
        if in_live && self.usage_view.is_none() && self.shell_ref().live.is_some() {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.shell_mut().focus_live = true;
                self.mode = Mode::Normal;
            }
            let pane = self.live_target().unwrap_or_default();
            let sh = self.shell_mut();
            let Some(l) = sh.live.as_mut() else { return };
            let (col, row) = (x - area.x, y - area.y);
            if let Some(bytes) = live::mouse(&m, col, row, l.flags) {
                sh.outbox
                    .push(format!("send-keys -t {pane} -H {}", live::hex(&bytes)));
            } else {
                match m.kind {
                    MouseEventKind::ScrollUp => l.scroll(3),
                    MouseEventKind::ScrollDown => l.scroll(-3),
                    _ => {}
                }
            }
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.click(x, y);
                // A session clicked in the list opens live, ready for keys:
                // that session, not the one that was showing.
                if matches!(self.mode, Mode::Normal)
                    && self.list_hits.iter().any(|(r, _)| {
                        x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
                    })
                {
                    let pane = self
                        .selected()
                        .and_then(|s| s.pane.as_ref())
                        .map(|p| p.id.clone());
                    match pane {
                        Some(p) if self.live_pane().as_ref() != Some(&p) => self.go(&p),
                        Some(_) => self.shell_mut().focus_live = true,
                        None => {}
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => self.menu_at(x, y),
            MouseEventKind::Moved => self.hover = Some((x, y)),
            MouseEventKind::ScrollDown => self.scroll_by(1),
            MouseEventKind::ScrollUp => self.scroll_by(-1),
            _ => {}
        }
    }

    /// Match the outer cursor's shape to the program's (bar in a prompt,
    /// block elsewhere), telling the terminal only when it changes.
    fn set_cursor_style(&mut self) {
        let sh = self.shell_mut();
        let shape = sh.cursor.filter(|_| sh.focus_live);
        if shape != sh.applied_cursor {
            sh.applied_cursor = shape;
            let _ = crossterm::execute!(std::io::stdout(), cursor_style(shape));
        }
    }

    // ---- drawing ------------------------------------------------------------

    fn draw_shell(&mut self, f: &mut Frame) {
        // With the keyboard in a session, the list marks that session.
        if self.shell_ref().focus_live
            && let Some(pane) = self.live_pane()
            && let Some(pid) = self
                .sessions
                .iter()
                .find(|s| s.pane.as_ref().is_some_and(|p| p.id == pane))
                .map(|s| s.pid)
            && self.items().any(|s| s.pid == pid)
        {
            self.sel = Some(pid);
        }
        self.cmd_hits.clear();
        self.list_hits.clear();
        // A clean margin, where your terminal shows through, and one rounded
        // hairline frame. Small terminals keep the frame and lose the margin.
        let screen = f.area();
        let p_base = self.pal.base;
        let roomy = screen.width >= 90 && screen.height >= 24;
        let (mx, my) = if roomy { (2, 1) } else { (0, 0) };
        let framed = Rect {
            x: screen.x + mx,
            y: screen.y + my,
            width: screen.width.saturating_sub(2 * mx),
            height: screen.height.saturating_sub(2 * my),
        };
        f.render_widget(ratatui::widgets::Clear, screen);
        let border = Block::bordered()
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::new().fg(self.pal.frame).bg(p_base))
            .style(Style::new().bg(p_base).fg(self.pal.text));
        let full = border.inner(framed);
        f.render_widget(border, framed);
        let accounts = matches!(self.mode, Mode::Accounts(_));
        let focus_live = self.shell_ref().focus_live
            && matches!(self.mode, Mode::Normal)
            && self.usage_view.is_none()
            && self.memory.is_none();
        let (foot_lines, foot_hits) = if let Some(m) = &self.memory {
            (vec![self.hint_line(&m.hints(), full.width)], Vec::new())
        } else if focus_live && matches!(self.mode, Mode::Normal) {
            self.foot_live(full.width)
        } else {
            self.foot(full.width)
        };
        let fh = (foot_lines.len() as u16)
            .min(full.height.saturating_sub(2))
            .max(1);
        let head_rows = self.head_lines(full.width);
        let hh = (head_rows.len() as u16)
            .min(full.height.saturating_sub(fh + 3))
            .max(1);
        let [head, rule_top, body, rule_bottom, foot] = Layout::vertical([
            Constraint::Length(hh),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(fh),
        ])
        .areas(full);
        f.render_widget(Block::new().style(Style::new().bg(self.pal.raised)), head);
        f.render_widget(Block::new().style(Style::new().bg(self.pal.raised)), foot);
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
        if matches!(self.mode, Mode::New(_)) {
            self.draw_new_head(f, head);
        }
        let show_list = !accounts && self.shell_ref().show_list && self.memory.is_none();
        let list_w = if show_list {
            (body.width * 32 / 100).clamp(34.min(body.width), 58)
        } else {
            0
        };
        let [list, gap, right] = Layout::horizontal([
            Constraint::Length(list_w),
            Constraint::Length(if show_list { 1 } else { 0 }),
            Constraint::Fill(1),
        ])
        .areas(body);
        // Hairlines close off the header and footer and meet the frame (├ ┤)
        // and the divider (┬ ┴): one drawn structure.
        let fc = self.pal.frame;
        let edge = |left: &'static str, right: &'static str, mid: &'static str| {
            let mut j = vec![
                (framed.x, left),
                (framed.x + framed.width.saturating_sub(1), right),
            ];
            if show_list {
                j.push((gap.x, mid));
            }
            j
        };
        let bordered = framed.x < full.x;
        let widen = |r: Rect| {
            if bordered {
                Rect {
                    x: framed.x,
                    width: framed.width,
                    ..r
                }
            } else {
                r
            }
        };
        let (top_j, bottom_j) = if bordered {
            (edge("├", "┤", "┬"), edge("├", "┤", "┴"))
        } else {
            (
                if show_list {
                    vec![(gap.x, "┬")]
                } else {
                    vec![]
                },
                if show_list {
                    vec![(gap.x, "┴")]
                } else {
                    vec![]
                },
            )
        };
        hrule(f, widen(rule_top), fc, p_base, &top_j);
        hrule(f, widen(rule_bottom), fc, p_base, &bottom_j);
        if show_list {
            divider(f, gap, fc, p_base);
        }
        if accounts {
            self.draw_accounts(f, body);
        } else {
            if show_list {
                let list_area = list;
                if matches!(self.mode, Mode::New(_)) {
                    self.draw_places(f, list_area);
                } else {
                    self.draw_list(f, list_area);
                }
                // The desktop shell can subtly recede the list while the live
                // session owns the keyboard. On the Android remote that looked
                // like the whole app had become disabled, so keep the list at
                // full contrast and make the focus state explicit in the footer.
                if focus_live && std::env::var_os("TOOMUX_REMOTE_ANDROID").is_none() {
                    recede(f.buffer_mut(), list_area, self.pal.base, 0.42, &self.pal);
                }
            }
            if self.memory.is_some() {
                self.draw_memory(f, right);
            } else if matches!(self.mode, Mode::New(_)) {
                self.draw_place_preview(f, right);
            } else if let Some(i) = self.usage_view {
                self.draw_usage(f, right, i);
            } else if self.showing_live() {
                self.draw_live(f, right, focus_live);
            } else {
                self.draw_preview(f, right);
            }
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
        if matches!(self.mode, Mode::Help | Mode::Menu(_)) {
            recede(f.buffer_mut(), framed, self.pal.well, 0.62, &self.pal);
            self.cmd_hits.clear();
            self.list_hits.clear();
            if matches!(self.mode, Mode::Help) {
                self.draw_help(f, body);
            } else {
                self.draw_menu(f, full);
            }
        }
        if let Some((hx, hy)) = self.hover
            && let Some((r, cmd)) = self
                .cmd_hits
                .iter()
                .find(|(r, _)| hx >= r.x && hx < r.x + r.width && hy >= r.y && hy < r.y + r.height)
        {
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

    /// Whether the right side shows the live session (vs. a transcript).
    /// With the keyboard on the session it always does; from the list, when
    /// the selection is that session (or on its way to one).
    fn showing_live(&self) -> bool {
        let sh = self.shell_ref();
        let Some(l) = &sh.live else { return false };
        if sh.focus_live {
            return true;
        }
        let sel_pane = self
            .selected()
            .and_then(|s| s.pane.as_ref())
            .map(|p| p.id.clone());
        sel_pane.as_ref().is_some_and(|p| *p == l.pane) || (sh.want.is_some() && sel_pane.is_some())
    }

    fn draw_live(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        let p = &self.pal;
        let (well, raised, text, muted, accent) = (p.well, p.raised, p.text, p.muted, p.accent);
        f.render_widget(Block::new().style(Style::new().bg(well)), area);
        // One quiet line of particulars above the session.
        let band = Rect { height: 1, ..area };
        f.render_widget(Block::new().style(Style::new().bg(raised)), band);
        let now = now_ms();
        let live_pane = self.live_pane();
        let s = self
            .sessions
            .iter()
            .find(|s| s.pane.as_ref().map(|x| &x.id) == live_pane.as_ref())
            .cloned();
        // Focus shows as the other side stepping back; no mark needed here.
        let _ = accent;
        let mut spans = vec![Span::raw("  ")];
        if let Some(s) = &s {
            let (g, c) = self.pal.state(s.state);
            let mut title = Style::new().fg(if focused { text } else { self.pal.dim });
            if focused {
                title = title.add_modifier(Modifier::BOLD);
            }
            let noticed = self.notices.iter().any(|n| n.pid == s.pid);
            let (word, rest, time) = s.state_parts(now);
            spans.push(Span::styled(format!("{g} "), Style::new().fg(c)));
            spans.push(Span::styled(s.title.clone(), title));
            spans.push(Span::styled(
                format!("   {}", s.account_name(&self.cfg)),
                Style::new().fg(muted),
            ));
            spans.push(Span::styled(" · ", Style::new().fg(muted)));
            spans.push(Span::styled(
                word,
                Style::new().fg(self.pal.word(s, noticed)),
            ));
            for part in [rest, time].into_iter().flatten() {
                spans.push(Span::styled(format!(" · {part}"), Style::new().fg(muted)));
            }
            // Then the particulars, each that fits whole; none is cut. A
            // voyage leads: the session's own status line under it draws the
            // scene, so the band only needs the chip. Next the number handover
            // acts on: amber as it nears the threshold.
            let mut extra: Vec<(String, Color)> = Vec::new();
            if let Some(q) = self.voyages.get(&s.id) {
                extra.push((format!("◎ {}", q.chip(now)), self.voyage_tone(q)));
            }
            if let Some(tokens) = self.info.get(&s.id).and_then(|i| i.tokens) {
                let near =
                    self.cfg.handover_tokens > 0 && tokens * 4 >= self.cfg.handover_tokens * 3;
                extra.push((
                    format!("{}k context", tokens / 1000),
                    if near { self.pal.working_word } else { muted },
                ));
            }
            if let Some(chip) = self.cache_chip(s, now) {
                extra.push(chip);
            }
            extra.extend(self.cost_so_far(s).map(|c| (c, muted)));
            extra.push((short_place(&s.place()), muted));
            if let Some((repo, n)) = &s.pr {
                extra.push((
                    format!("{}#{n}", repo.rsplit('/').next().unwrap_or(repo)),
                    muted,
                ));
            }
            let used: usize = spans.iter().map(|x| x.width()).sum();
            let mut room = (band.width as usize).saturating_sub(used + 1);
            for (e, tone) in extra {
                if e.width() + 3 > room {
                    continue;
                }
                room -= e.width() + 3;
                spans.push(Span::styled(" · ", Style::new().fg(muted)));
                spans.push(Span::styled(e, Style::new().fg(tone)));
            }
        }
        let scrolled = self.shell_ref().live.as_ref().is_some_and(|l| l.scrolled());
        if scrolled {
            spans.push(Span::styled(
                "   scrolled back · any key returns",
                Style::new().fg(self.pal.working_word),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), band);
        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(1),
            height: area.height.saturating_sub(1),
        };
        let sh = self.shell_mut();
        sh.area = inner;
        let want = (inner.width, inner.height);
        if sh.sized != want && inner.width > 4 && inner.height > 2 {
            sh.sized = want;
            sh.control
                .one(format!("refresh-client -C {}x{}", want.0, want.1));
            if sh.live.is_some() {
                sh.reseed_at = Some(Instant::now() + Duration::from_millis(150));
            }
        }
        let Some(l) = sh.live.as_ref() else { return };
        if l.seeding() && l.version == 0 {
            return;
        }
        let cursor = l.render(f.buffer_mut(), inner, text, well);
        let sh = self.shell_mut();
        sh.cursor = cursor.map(|c| c.2);
        if focused {
            if let Some((x, y, _)) = cursor {
                f.set_cursor_position((x, y));
            }
        } else {
            // The list has the keys: the session steps back, its cursor
            // waiting as a soft block.
            recede(f.buffer_mut(), inner, well, 0.42, &self.pal);
            if let Some((x, y, _)) = cursor {
                f.buffer_mut()[(x, y)].set_bg(self.pal.faint);
            }
        }
    }

    fn foot_live(&self, width: u16) -> (Vec<Line<'static>>, Vec<(u16, u16, Cmd)>) {
        let p = &self.pal;
        let w = width as usize;
        if let Some(t) = &self.flash
            && t.2.elapsed() < Duration::from_secs(5)
        {
            return (
                vec![Line::from(vec![
                    Span::raw(" "),
                    Span::styled(t.0.clone(), Style::new().fg(t.1)),
                ])],
                Vec::new(),
            );
        }
        let android_remote = std::env::var_os("TOOMUX_REMOTE_ANDROID").is_some();
        let mut hints: Vec<(&str, &str, Option<Cmd>)> = if android_remote {
            vec![
                ("esc", "sessions", None),
                ("alt-s", "sessions", None),
                ("alt-u", "usage", Some(Cmd::UsageToggle)),
                ("alt-m", "memory", Some(Cmd::MemoryToggle)),
                ("alt-b", "list", None),
            ]
        } else {
            vec![
                ("alt-s", "sessions", None),
                ("alt-u", "usage", Some(Cmd::UsageToggle)),
                ("alt-m", "memory", Some(Cmd::MemoryToggle)),
                ("alt-b", "list", None),
            ]
        };
        if self.shell_ref().popup {
            hints.push(("alt-j", "go there", Some(Cmd::GoThere)));
        }
        let mut spans = vec![Span::raw(" ")];
        if android_remote {
            spans.push(Span::styled(
                "session keys",
                Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw("   "));
        }
        let mut hits = Vec::new();
        let mut x = if android_remote { 16u16 } else { 1u16 };
        for (k, l, cmd) in hints {
            let hw = (k.width() + 1 + l.width()) as u16;
            if (x + hw) as usize > w {
                break;
            }
            spans.push(Span::styled(k, Style::new().fg(p.text)));
            spans.push(Span::styled(format!(" {l}"), Style::new().fg(p.dim)));
            spans.push(Span::raw("   "));
            if let Some(c) = cmd {
                hits.push((x, hw, c));
            }
            x += hw + 3;
        }
        if let Some(note) = self.keys_note() {
            let nw = note.width() + 1;
            if x as usize + nw + 4 <= w {
                spans.push(Span::raw(" ".repeat(w - x as usize - nw)));
                spans.push(Span::styled(note, Style::new().fg(p.muted)));
            }
        }
        (vec![Line::from(spans)], hits)
    }
}

impl Shell {
    pub(super) fn live_title(&self, sessions: &[Session]) -> Option<String> {
        let pane = &self.live.as_ref()?.pane;
        sessions
            .iter()
            .find(|s| s.pane.as_ref().is_some_and(|p| &p.id == pane))
            .map(|s| s.title.clone())
    }

    pub(super) fn has_live(&self) -> bool {
        self.live.is_some()
    }

    fn area_size(&self) -> (u16, u16) {
        if self.area.width > 0 {
            (self.area.width, self.area.height)
        } else {
            (80, 24)
        }
    }
}

/// Cursor shape for the outer terminal, following the program's.
pub(super) fn cursor_style(shape: Option<CursorShape>) -> SetCursorStyle {
    match shape {
        Some(CursorShape::Beam) => SetCursorStyle::SteadyBar,
        Some(CursorShape::Underline) => SetCursorStyle::SteadyUnderScore,
        _ => SetCursorStyle::SteadyBlock,
    }
}
