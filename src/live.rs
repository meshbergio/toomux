//! A live, interactive view of one tmux pane. Output arrives over the
//! control connection and is parsed here; the screen is drawn straight into
//! the ratatui buffer; keys go back as tmux key names (tmux encodes them for
//! whatever mode the program is in).

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{self, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, Color as AColor, CursorShape, NamedColor};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

/// The real terminal behind the pane (tmux) answers the program's queries;
/// this copy only watches.
#[derive(Clone, Copy)]
pub struct Quiet;
impl EventListener for Quiet {
    fn send_event(&self, _: TermEvent) {}
}

/// What the pane's program asked for, as tmux reports it.
#[derive(Default, Clone, Copy, Debug)]
pub struct Flags2 {
    pub mouse: bool,
    pub mouse_motion: bool,
    pub sgr: bool,
}

pub struct Live {
    pub pane: String,
    term: Term<Quiet>,
    parser: ansi::Processor,
    pub cols: u16,
    pub lines: u16,
    /// Reply numbers of the capture and the state query that seed the view;
    /// output until both are in is already part of the capture.
    pub seed: Option<(u64, u64)>,
    captured: Option<Vec<String>>,
    pub flags: Flags2,
    /// Output that arrived after the seed was taken, waiting for it.
    backlog: Vec<u8>,
    /// Bumped whenever the screen changes, to know when to draw.
    pub version: u64,
}

pub const HISTORY: usize = 2000;

impl Live {
    pub fn new(pane: &str, cols: u16, lines: u16) -> Self {
        let size = TermSize::new(cols.max(2) as usize, lines.max(1) as usize);
        let config = term::Config { scrolling_history: HISTORY, ..Default::default() };
        Self {
            pane: pane.to_string(),
            term: Term::new(config, &size, Quiet),
            parser: ansi::Processor::new(),
            cols,
            lines,
            seed: None,
            captured: None,
            flags: Flags2::default(),
            backlog: Vec::new(),
            version: 0,
        }
    }

    /// The commands that seed the view, sent together so nothing lands
    /// between the picture and the cursor.
    pub fn seed_commands(&self) -> Vec<String> {
        let p = crate::tmux::bare(&self.pane);
        vec![
            format!("capture-pane -p -e -N -t {p} -S -{HISTORY}"),
            format!(
                "display -p -t {p} '#{{pane_width}} #{{pane_height}} #{{cursor_x}} #{{cursor_y}} #{{cursor_flag}} #{{alternate_on}} #{{mouse_any_flag}} #{{mouse_button_flag}} #{{mouse_sgr_flag}} #{{history_size}}'"
            ),
        ]
    }

    pub fn seeding(&self) -> bool {
        self.seed.is_some()
    }

    /// A reply arrived; returns true once the view is seeded.
    pub fn reply(&mut self, seq: u64, lines: Vec<String>) -> bool {
        let Some((cap, state)) = self.seed else { return false };
        if seq == cap {
            self.captured = Some(lines);
            return false;
        }
        if seq != state {
            return false;
        }
        self.seed = None;
        let captured = self.captured.take().unwrap_or_default();
        let f: Vec<i64> = lines.first().map(|l| l.split(' ').filter_map(|x| x.parse().ok()).collect()).unwrap_or_default();
        let get = |i: usize| f.get(i).copied().unwrap_or(0);
        let (w, h) = (get(0).max(2) as u16, get(1).max(1) as u16);
        self.cols = w;
        self.lines = h;
        let size = TermSize::new(w as usize, h as usize);
        let config = term::Config { scrolling_history: HISTORY, ..Default::default() };
        self.term = Term::new(config, &size, Quiet);
        self.parser = ansi::Processor::new();
        self.flags = Flags2 { mouse: get(6) == 1 || get(7) == 1, mouse_motion: get(7) == 1, sgr: get(8) == 1 };
        let alternate = get(5) == 1;
        let mut seed: Vec<u8> = Vec::new();
        if alternate {
            seed.extend(b"\x1b[?1049h");
        }
        // The capture is history then the visible screen; only the last `h`
        // lines are the screen. Written in order, the rest scrolls into
        // history the way it did in the pane.
        let n = captured.len();
        let start = if alternate { n.saturating_sub(h as usize) } else { 0 };
        for (i, l) in captured[start..].iter().enumerate() {
            if i > 0 {
                seed.extend(b"\r\n");
            }
            seed.extend(l.as_bytes());
            seed.extend(b"\x1b[0m");
        }
        seed.extend(format!("\x1b[{};{}H", get(3) + 1, get(2) + 1).as_bytes());
        if get(4) == 0 {
            seed.extend(b"\x1b[?25l");
        }
        self.parser.advance(&mut self.term, &seed);
        let backlog = std::mem::take(&mut self.backlog);
        self.parser.advance(&mut self.term, &backlog);
        self.version += 1;
        true
    }

    pub fn output(&mut self, data: &[u8]) {
        if self.seeding() {
            // Only output after the seed was taken matters; the reply for the
            // capture hasn't arrived, so this is newer than it.
            if self.captured.is_some() {
                self.backlog.extend_from_slice(data);
            }
            return;
        }
        self.parser.advance(&mut self.term, data);
        // The program may have switched mouse modes; follow what it asks.
        let mode = *self.term.mode();
        self.flags.mouse = mode.intersects(TermMode::MOUSE_MODE);
        self.flags.mouse_motion = mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG);
        self.flags.sgr = mode.contains(TermMode::SGR_MOUSE);
        self.version += 1;
    }

    /// Scroll our own copy of the history (for programs that don't take the
    /// mouse). Positive is up.
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
        self.version += 1;
    }

    pub fn scrolled(&self) -> bool {
        self.term.grid().display_offset() > 0
    }

    pub fn to_bottom(&mut self) {
        if self.scrolled() {
            self.term.scroll_display(Scroll::Bottom);
            self.version += 1;
        }
    }

    /// Draw into `area`; returns where the cursor is (absolute) and its shape.
    pub fn render(&self, buf: &mut Buffer, area: Rect, fg: Color, bg: Color) -> Option<(u16, u16, CursorShape)> {
        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let c = &mut buf[(x, y)];
                c.reset();
                c.set_bg(bg).set_fg(fg);
            }
        }
        for ic in content.display_iter {
            let cell = &ic.cell;
            let row = ic.point.line.0 + offset;
            let col = ic.point.column.0;
            if row < 0 || row as u16 >= area.height || col as u16 >= area.width {
                continue;
            }
            let (x, y) = (area.x + col as u16, area.y + row as u16);
            let out = &mut buf[(x, y)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                out.set_symbol(" ");
                continue;
            }
            let mut sym = String::new();
            sym.push(if cell.c == '\0' { ' ' } else { cell.c });
            if let Some(z) = cell.zerowidth() {
                sym.extend(z.iter());
            }
            out.set_symbol(&sym);
            out.set_fg(color(cell.fg, fg, bg));
            out.set_bg(color(cell.bg, fg, bg));
            let mut m = Modifier::empty();
            for (f, md) in [
                (Flags::BOLD, Modifier::BOLD),
                (Flags::ITALIC, Modifier::ITALIC),
                (Flags::DIM, Modifier::DIM),
                (Flags::INVERSE, Modifier::REVERSED),
                (Flags::STRIKEOUT, Modifier::CROSSED_OUT),
                (Flags::HIDDEN, Modifier::HIDDEN),
            ] {
                if cell.flags.contains(f) {
                    m |= md;
                }
            }
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                m |= Modifier::UNDERLINED;
            }
            out.modifier = m;
        }
        let cur = content.cursor;
        let row = cur.point.line.0 + offset;
        let visible = content.mode.contains(TermMode::SHOW_CURSOR) && cur.shape != CursorShape::Hidden;
        (visible && row >= 0 && (row as u16) < area.height && (cur.point.column.0 as u16) < area.width)
            .then(|| (area.x + cur.point.column.0 as u16, area.y + row as u16, cur.shape))
    }

    pub fn columns(&self) -> usize {
        self.term.columns()
    }
}

fn color(c: AColor, fg: Color, bg: Color) -> Color {
    match c {
        AColor::Spec(r) => Color::Rgb(r.r, r.g, r.b),
        AColor::Indexed(i) => Color::Indexed(i),
        AColor::Named(n) => match n {
            NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground | NamedColor::Cursor => fg,
            NamedColor::Background => bg,
            n if (n as usize) < 16 => Color::Indexed(n as u8),
            n if (n as usize) >= NamedColor::DimBlack as usize && (n as usize) <= NamedColor::DimWhite as usize => {
                Color::Indexed((n as usize - NamedColor::DimBlack as usize) as u8)
            }
            _ => fg,
        },
    }
}

/// A key as tmux key names (for `send-keys`), or raw text bytes.
pub enum Keys {
    Named(String),
    Text(Vec<u8>),
}

pub fn key(k: KeyEvent) -> Option<Keys> {
    let m = k.modifiers;
    let prefix = |name: &str| {
        let mut s = String::new();
        if m.contains(KeyModifiers::CONTROL) {
            s.push_str("C-");
        }
        if m.contains(KeyModifiers::ALT) {
            s.push_str("M-");
        }
        if m.contains(KeyModifiers::SHIFT) {
            s.push_str("S-");
        }
        s.push_str(name);
        Keys::Named(s)
    };
    Some(match k.code {
        KeyCode::Char(c) if m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            let mut s = String::new();
            if m.contains(KeyModifiers::CONTROL) {
                s.push_str("C-");
            }
            if m.contains(KeyModifiers::ALT) {
                s.push_str("M-");
            }
            match c {
                ' ' => s.push_str("Space"),
                c if m.contains(KeyModifiers::CONTROL) => s.push(c.to_ascii_lowercase()),
                c => s.push(c),
            }
            Keys::Named(s)
        }
        KeyCode::Char(c) => Keys::Text(c.to_string().into_bytes()),
        KeyCode::Enter => prefix("Enter"),
        KeyCode::Tab => prefix("Tab"),
        KeyCode::BackTab => Keys::Named("BTab".into()),
        KeyCode::Backspace => prefix("BSpace"),
        KeyCode::Esc => prefix("Escape"),
        KeyCode::Left => prefix("Left"),
        KeyCode::Right => prefix("Right"),
        KeyCode::Up => prefix("Up"),
        KeyCode::Down => prefix("Down"),
        KeyCode::Home => prefix("Home"),
        KeyCode::End => prefix("End"),
        KeyCode::PageUp => prefix("PPage"),
        KeyCode::PageDown => prefix("NPage"),
        KeyCode::Insert => prefix("IC"),
        KeyCode::Delete => prefix("DC"),
        KeyCode::F(n) if (1..=12).contains(&n) => prefix(&format!("F{n}")),
        _ => return None,
    })
}

/// A mouse event as the bytes the program asked for (SGR or legacy), with
/// coordinates relative to the pane.
pub fn mouse(ev: &MouseEvent, col: u16, row: u16, flags: Flags2) -> Option<Vec<u8>> {
    if !flags.mouse {
        return None;
    }
    let (b, pressed, motion) = match ev.kind {
        MouseEventKind::Down(b) => (button(b), true, false),
        MouseEventKind::Up(b) => (button(b), false, false),
        MouseEventKind::Drag(b) if flags.mouse_motion => (button(b), true, true),
        MouseEventKind::ScrollUp => (64, true, false),
        MouseEventKind::ScrollDown => (65, true, false),
        _ => return None,
    };
    let mut code = b + if motion { 32 } else { 0 };
    code += 4 * u8::from(ev.modifiers.contains(KeyModifiers::SHIFT))
        + 8 * u8::from(ev.modifiers.contains(KeyModifiers::ALT))
        + 16 * u8::from(ev.modifiers.contains(KeyModifiers::CONTROL));
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    if flags.sgr {
        return Some(format!("\x1b[<{code};{x};{y}{}", if pressed { 'M' } else { 'm' }).into_bytes());
    }
    let code = if pressed { code } else { 3 + (code & !3) };
    (x <= 223 && y <= 223).then(|| vec![0x1b, b'[', b'M', 32 + code, 32 + x as u8, 32 + y as u8])
}

fn button(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Hex arguments for `send-keys -H`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_become_tmux_names() {
        let k = |c, m| key(KeyEvent::new(c, m));
        let name = |x: Option<Keys>| match x {
            Some(Keys::Named(s)) => s,
            Some(Keys::Text(t)) => format!("text:{}", String::from_utf8_lossy(&t)),
            None => "none".into(),
        };
        assert_eq!(name(k(KeyCode::Char('c'), KeyModifiers::CONTROL)), "C-c");
        assert_eq!(name(k(KeyCode::Char('b'), KeyModifiers::ALT)), "M-b");
        assert_eq!(name(k(KeyCode::Char('é'), KeyModifiers::NONE)), "text:é");
        assert_eq!(name(k(KeyCode::Up, KeyModifiers::SHIFT)), "S-Up");
        assert_eq!(name(k(KeyCode::Enter, KeyModifiers::NONE)), "Enter");
        assert_eq!(name(k(KeyCode::PageUp, KeyModifiers::NONE)), "PPage");
    }

    #[test]
    fn seeding_places_history_screen_and_cursor() {
        let mut live = Live::new("%1", 10, 3);
        live.seed = Some((0, 1));
        live.output(b"ignored: already in the capture");
        assert!(!live.reply(0, vec!["old".into(), "line1".into(), "line2".into(), "\x1b[31mred\x1b[39m".into()]));
        live.output(b"!");
        assert!(live.reply(1, vec!["10 3 3 2 1 0 0 0 0 1".into()]));
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
        let cur = live.render(&mut buf, Rect::new(0, 0, 10, 3), Color::White, Color::Black);
        let row = |y| (0..10).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>();
        assert_eq!(row(0).trim_end(), "line1");
        assert_eq!(row(2).trim_end(), "red!", "later output lands after the seed, at the cursor");
        assert_eq!(buf[(0, 2)].fg, Color::Indexed(1));
        assert_eq!(cur.map(|c| (c.0, c.1)), Some((4, 2)));
        live.scroll(1);
        assert!(live.scrolled());
    }
}
