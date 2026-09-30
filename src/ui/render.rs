//! Stateless text/layout helpers for the terminal UI.

use super::Palette;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) fn sparkline(values: &[f64], cols: usize) -> [String; 2] {
    let n = cols.max(2) * 2;
    let (lo, hi) = values.iter().fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
    let span = (hi - lo).max(1e-9);
    let at = |i: usize| {
        let x = i as f64 / (n - 1) as f64 * (values.len() - 1) as f64;
        let (a, b) = (x.floor() as usize, (x.ceil() as usize).min(values.len() - 1));
        let v = values[a] + (values[b] - values[a]) * (x - a as f64);
        (((v - lo) / span) * 7.0).round() as usize
    };
    // Dot bits by (column side, row from the top) in a braille cell.
    const BITS: [[u32; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    let mut cells = [vec![0u32; cols.max(2)], vec![0u32; cols.max(2)]];
    for i in 0..n {
        let level = at(i);
        let (row, y) = if level >= 4 { (0, 7 - level) } else { (1, 3 - level) };
        cells[row][i / 2] |= BITS[i % 2][y];
    }
    cells.map(|r| r.iter().map(|&b| char::from_u32(0x2800 + b).unwrap_or(' ')).collect())
}

/// A horizontal hairline across `r`, with junctions where it meets other
/// lines: `joins` are (x, glyph) pairs such as a divider's ┬ or the frame's ├.
pub(super) fn hrule(f: &mut Frame, r: Rect, fg: Color, bg: Color, joins: &[(u16, &str)]) {
    let buf = f.buffer_mut();
    for x in r.x..r.x + r.width {
        if let Some(c) = buf.cell_mut((x, r.y)) {
            c.set_symbol("─").set_fg(fg).set_bg(bg);
        }
    }
    for (x, g) in joins {
        if let Some(c) = buf.cell_mut((*x, r.y)) {
            c.set_symbol(g).set_fg(fg).set_bg(bg);
        }
    }
}

/// A vertical hairline between two surfaces.
/// A voyage's scene as spans, cells of one look run together.
pub(super) fn scene_spans(rows: Vec<Vec<crate::scene::Cell>>) -> Vec<Vec<Span<'static>>> {
    let rgb = |c: crate::scene::Rgb| Color::Rgb(c[0], c[1], c[2]);
    rows.into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut run = String::new();
            let mut look: Option<(crate::scene::Rgb, crate::scene::Rgb)> = None;
            for c in row {
                // A blank cell shows only its background: any foreground will do.
                let key = (if c.ch == ' ' { look.map_or(c.fg, |l| l.0) } else { c.fg }, c.bg);
                if look.is_some_and(|l| l != key) {
                    let (fg, bg) = look.unwrap_or(key);
                    spans.push(Span::styled(std::mem::take(&mut run), Style::new().fg(rgb(fg)).bg(rgb(bg))));
                }
                look = Some(key);
                run.push(c.ch);
            }
            if let Some((fg, bg)) = look {
                spans.push(Span::styled(run, Style::new().fg(rgb(fg)).bg(rgb(bg))));
            }
            spans
        })
        .collect()
}

pub(super) fn divider(f: &mut Frame, r: Rect, fg: Color, bg: Color) {
    let buf = f.buffer_mut();
    for y in r.y..r.y + r.height {
        if let Some(c) = buf.cell_mut((r.x, y)) {
            c.set_symbol("│").set_fg(fg).set_bg(bg);
        }
    }
}

pub(super) fn pad(mut spans: Vec<Span<'static>>) -> Line<'static> {
    spans.insert(0, Span::raw(" "));
    Line::from(spans)
}

/// Light markdown for transcript previews: headings and **bold** get weight,
/// code fences and link targets disappear, bullets become dots.
pub(super) fn markdown(src: &str, w: usize, p: &Palette) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut fenced = false;
    let mut table: Vec<Vec<String>> = Vec::new();
    let lines: Vec<&str> = src.lines().chain(std::iter::once("")).collect();
    for raw in lines {
        let t = raw.trim_end();
        let row = t.trim_start();
        if !fenced && row.starts_with('|') {
            if !row.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
                table.push(row.trim_matches('|').split('|').map(|c| c.trim().replace("**", "").replace('`', "")).collect());
            }
            continue;
        }
        if !table.is_empty() {
            out.extend(render_table(std::mem::take(&mut table), w, p));
        }
        if t.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            for chunk in wrap_spans(&[(format!("  {t}"), Style::new().fg(p.dim))], w, w) {
                out.push(Line::from(chunk));
            }
            continue;
        }
        if t.trim().is_empty() {
            if out.last().is_some_and(|l: &Line| !l.spans.is_empty()) {
                out.push(Line::raw(""));
            }
            continue;
        }
        let indent = t.len() - t.trim_start().len();
        let mut body = t.trim_start();
        let mut lead = " ".repeat(indent.min(6));
        let mut base = Style::new().fg(p.text);
        if let Some(h) = body.strip_prefix("### ").or(body.strip_prefix("## ")).or(body.strip_prefix("# ")) {
            body = h;
            base = base.add_modifier(Modifier::BOLD);
        } else if let Some(b) = body.strip_prefix("- ").or(body.strip_prefix("* ")) {
            body = b;
            lead.push_str("· ");
        }
        let mut segs = vec![(lead.clone(), base)];
        segs.extend(inline_md(body, base, p));
        let rest = w.saturating_sub(lead.width());
        for (n, chunk) in wrap_spans(&segs, w, rest).into_iter().enumerate() {
            let mut l = chunk;
            if n > 0 {
                l.insert(0, Span::raw(" ".repeat(lead.width())));
            }
            out.push(Line::from(l));
        }
    }
    while out.last().is_some_and(|l| l.spans.is_empty()) {
        out.pop();
    }
    out
}

/// Markdown table rows as aligned columns with faint rules. Columns that
/// don't fit fall back to one "header: value" line per cell.
pub(super) fn render_table(rows: Vec<Vec<String>>, w: usize, p: &Palette) -> Vec<Line<'static>> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> =
        (0..cols).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|x| x.width()).max().unwrap_or(0)).collect();
    let total: usize = widths.iter().sum::<usize>() + 3 * cols.saturating_sub(1);
    let mut out = Vec::new();
    if total <= w {
        for (n, r) in rows.iter().enumerate() {
            let mut spans = Vec::new();
            for (c, width) in widths.iter().enumerate() {
                if c > 0 {
                    spans.push(Span::styled(" │ ", Style::new().fg(p.faint)));
                }
                let cell = r.get(c).cloned().unwrap_or_default();
                let pad = if c + 1 == cols { 0 } else { width - cell.width() };
                let style = if n == 0 { Style::new().fg(p.dim) } else { Style::new().fg(p.text) };
                spans.push(Span::styled(format!("{cell}{}", " ".repeat(pad)), style));
            }
            out.push(Line::from(spans));
            if n == 0 && rows.len() > 1 {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                out.push(Line::from(Span::styled(rule.join("─┼─"), Style::new().fg(p.faint))));
            }
        }
    } else {
        let head = rows.first().cloned().unwrap_or_default();
        for r in rows.iter().skip(1) {
            for (c, cell) in r.iter().enumerate() {
                let segs = [
                    (format!("{}: ", head.get(c).map(String::as_str).unwrap_or("")), Style::new().fg(p.dim)),
                    (cell.clone(), Style::new().fg(p.text)),
                ];
                out.extend(wrap_spans(&segs, w, w).into_iter().map(Line::from));
            }
            out.push(Line::raw(""));
        }
    }
    out
}

pub(super) fn inline_md(s: &str, base: Style, p: &Palette) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut cur = String::new();
    let (mut bold, mut code) = (false, false);
    let style = |bold: bool, code: bool| {
        let mut st = base;
        if bold {
            st = st.add_modifier(Modifier::BOLD);
        }
        if code {
            st = st.fg(p.accent);
        }
        st
    };
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' => {
                out.push((std::mem::take(&mut cur), style(bold, code)));
                code = !code;
            }
            '*' if !code && chars.peek() == Some(&'*') => {
                chars.next();
                out.push((std::mem::take(&mut cur), style(bold, code)));
                bold = !bold;
            }
            '[' if !code => {
                // [text](url) -> text
                let rest: String = chars.clone().collect();
                if let Some(close) = rest.find("](")
                    && let Some(end) = rest[close..].find(')') {
                        cur.push_str(&rest[..close]);
                        for _ in 0..rest[..close + end + 1].chars().count() {
                            chars.next();
                        }
                        continue;
                    }
                cur.push(c);
            }
            _ => cur.push(c),
        }
    }
    out.push((cur, style(bold, code)));
    out.retain(|(t, _)| !t.is_empty());
    out
}

/// Word-wrap styled segments. The first line may be narrower than the rest
/// (it shares its row with a right-aligned column).
pub(super) fn wrap_spans(segs: &[(String, Style)], first: usize, rest: usize) -> Vec<Vec<Span<'static>>> {
    wrap_segments(segs, first, rest, false)
}

/// Like `wrap_spans`, but each segment ("idle 3m", "outside tmux") moves to
/// the next line whole rather than breaking inside, when it can fit on one.
pub(super) fn wrap_chips(segs: &[(String, Style)], first: usize, rest: usize) -> Vec<Vec<Span<'static>>> {
    wrap_segments(segs, first, rest, true)
}

pub(super) fn wrap_segments(segs: &[(String, Style)], first: usize, rest: usize, atomic: bool) -> Vec<Vec<Span<'static>>> {
    let mut out: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0usize;
    let width = |n: usize| if n == 0 { first.max(8) } else { rest.max(8) };
    for (text, style) in segs {
        // Split into words, keeping each word's leading spaces with it.
        let mut words: Vec<String> = Vec::new();
        let mut cur = String::new();
        let whole = atomic && text.width() <= first.min(rest).max(8);
        for ch in text.chars() {
            if whole {
                cur.push(ch);
                continue;
            }
            if ch == ' ' && !cur.is_empty() && !cur.ends_with(' ') {
                words.push(std::mem::take(&mut cur));
            }
            cur.push(ch);
        }
        if !cur.is_empty() {
            words.push(cur);
        }
        for word in words {
            let wlen = word.width();
            let limit = width(out.len() - 1);
            let word = if used > 0 && used + wlen > limit {
                out.push(Vec::new());
                used = 0;
                word.trim_start().to_string()
            } else {
                word
            };
            let mut word = word;
            // Words longer than a whole line are split.
            while word.width() > width(out.len() - 1) - used {
                let room = width(out.len() - 1) - used;
                let (head, tail) = split_at_width(&word, room);
                if head.is_empty() {
                    break;
                }
                out.last_mut().unwrap().push(Span::styled(head, *style));
                out.push(Vec::new());
                used = 0;
                word = tail;
            }
            used += word.width();
            out.last_mut().unwrap().push(Span::styled(word, *style));
        }
    }
    // A line break replaces a separator: never end on " ·" or start on spaces.
    for l in out.iter_mut() {
        while l.last().is_some_and(|s| matches!(s.content.trim(), "" | "·")) {
            l.pop();
        }
    }
    for l in out.iter_mut().skip(1) {
        while l.first().is_some_and(|s| matches!(s.content.trim(), "" | "·")) {
            l.remove(0);
        }
        if let Some(first) = l.first_mut() {
            let t = first.content.trim_start().to_string();
            first.content = t.into();
        }
    }
    out.retain(|l| !l.is_empty());
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

pub(super) fn split_at_width(s: &str, w: usize) -> (String, String) {
    let mut acc = 0;
    for (i, ch) in s.char_indices() {
        let cw = ch.width().unwrap_or(0);
        if acc + cw > w {
            return (s[..i].to_string(), s[i..].to_string());
        }
        acc += cw;
    }
    (s.to_string(), String::new())
}

/// Reflow a captured terminal line to a narrower width, the way a narrower
/// terminal would. Horizontal rules are clipped rather than wrapped.
pub(super) fn reflow(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut spans = line.spans;
    while spans.last().is_some_and(|s| s.content.trim_end().is_empty()) {
        spans.pop();
    }
    if let Some(last) = spans.last_mut() {
        let t = last.content.trim_end().to_string();
        last.content = t.into();
    }
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    if text.width() <= width {
        return vec![Line::from(spans)];
    }
    // Left and right-aligned parts (status bars, hints): close the gap
    // between them instead of wrapping, if that's enough to fit.
    if let Some(squeezed) = squeeze_gap(&spans, text.width() - width) {
        return vec![Line::from(squeezed)];
    }
    let is_rule = |c: char| matches!(c, '─' | '━' | '═' | '-' | '╌' | '┄');
    let rule = text.trim().chars().all(is_rule);
    let cells: Vec<(char, Style)> = spans.iter().flat_map(|s| s.content.chars().map(move |c| (c, s.style))).collect();
    let clip = |cells: &[(char, Style)], width: usize| -> Vec<(char, Style)> {
        let mut w = 0;
        cells
            .iter()
            .copied()
            .take_while(|(c, _)| {
                w += c.width().unwrap_or(0);
                w <= width
            })
            .collect()
    };
    if rule {
        return vec![Line::from(regroup(&clip(&cells, width)))];
    }
    // A box (a dialog, a prompt frame) is drawn narrower, as a narrower
    // terminal would: its borders clipped to fit, its text wrapped inside.
    let lead = text.chars().take_while(|c| *c == ' ').count();
    let body = &cells[lead..];
    if let (Some(&(open, os)), Some(&(close, cs))) = (body.first(), body.last())
        && body.len() > 2
        && "╭╰┌└├╔╚│┃║".contains(open)
        && "╮╯┐┘┤╗╝│┃║".contains(close)
        && width > lead + 6
    {
        let inner = &body[1..body.len() - 1];
        if inner.iter().all(|(c, _)| is_rule(*c)) {
            let mut kept = cells[..lead].to_vec();
            kept.push((open, os));
            kept.extend(clip(inner, width - lead - 2));
            kept.push((close, cs));
            return vec![Line::from(regroup(&kept))];
        }
        if matches!(open, '│' | '┃' | '║') && open == close {
            let room = width - lead - 4;
            let words = Line::from(regroup(inner.strip_prefix(&[(' ', inner[0].1)][..]).unwrap_or(inner)));
            return reflow(words, room)
                .into_iter()
                .map(|l| {
                    let pad = room.saturating_sub(l.width());
                    let mut v = vec![Span::raw(" ".repeat(lead)), Span::styled(format!("{open} "), os)];
                    v.extend(l.spans);
                    v.push(Span::raw(" ".repeat(pad)));
                    v.push(Span::styled(format!(" {close}"), cs));
                    Line::from(v)
                })
                .collect();
        }
    }
    // Continuation lines hang under the text, past any indent and bullet.
    let bullet = text.trim_start().chars().next().is_some_and(|c| "●○◐◆⎿∗✻*-·•›❯⏺".contains(c))
        && text.trim_start().chars().nth(1) == Some(' ');
    let hang = (lead + if bullet { 2 } else { 0 }).min(width / 3);
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        let room = if out.is_empty() { width } else { width - hang };
        let (mut end, mut used) = (start, 0);
        while end < cells.len() {
            let cw = cells[end].0.width().unwrap_or(0);
            if used + cw > room {
                break;
            }
            used += cw;
            end += 1;
        }
        let mut next = end;
        if end < cells.len() {
            // Break at the last space that keeps a reasonable line length.
            if let Some(b) = (start..=end).rev().find(|&i| cells[i].0 == ' ').filter(|&b| b > start + (end - start) / 3) {
                end = b;
                next = b + 1;
            }
        }
        if end == start {
            end = start + 1;
            next = end;
        }
        let mut line = if out.is_empty() { Vec::new() } else { vec![Span::raw(" ".repeat(hang))] };
        line.extend(regroup(&cells[start..end]));
        out.push(line);
        start = next;
        while !out.is_empty() && start < cells.len() && cells[start].0 == ' ' && end != start {
            start += 1;
        }
    }
    out.into_iter().map(Line::from).collect()
}

/// Runs of same-styled characters back into spans.
pub(super) fn regroup(cells: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut style = None;
    for &(c, st) in cells {
        if style != Some(st) && !cur.is_empty() {
            out.push(Span::styled(std::mem::take(&mut cur), style.unwrap_or_default()));
        }
        style = Some(st);
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(Span::styled(cur, style.unwrap_or_default()));
    }
    out
}

/// A path as wrap segments that break after each "/", never inside a name.
pub(super) fn path_segments(path: &str, style: Style) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    for part in path.split_inclusive('/') {
        out.push((part.to_string(), style));
    }
    out
}

/// A list-sized path: first and last parts of a deep path. The preview shows
/// it in full.
pub(super) fn short_place(place: &str) -> String {
    let parts: Vec<&str> = place.split('/').collect();
    if parts.len() <= 3 || place.chars().count() <= 32 {
        return place.to_string();
    }
    let first = if parts[0].is_empty() { format!("/{}", parts[1]) } else { parts[0].to_string() };
    format!("{first}/…/{}", parts[parts.len() - 1])
}

/// Remove `excess` columns from the widest run of spaces inside a line,
/// keeping at least two. None if the line has no gap that wide.
pub(super) fn squeeze_gap(spans: &[Span<'static>], excess: usize) -> Option<Vec<Span<'static>>> {
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let (mut best, mut run_start, mut run) = ((0usize, 0usize), 0usize, 0usize);
    for (i, ch) in text.char_indices() {
        if ch == ' ' {
            if run == 0 {
                run_start = i;
            }
            run += 1;
            if run > best.1 {
                best = (run_start, run);
            }
        } else {
            run = 0;
        }
    }
    let (start, len) = best;
    if len < excess + 2 {
        return None;
    }
    // Cut `excess` spaces from the byte range [start, start + excess).
    let (cut_from, cut_to) = (start, start + excess);
    let mut out = Vec::new();
    let mut at = 0usize;
    for sp in spans {
        let c = sp.content.as_ref();
        let (s0, s1) = (at, at + c.len());
        at = s1;
        let (a, b) = (cut_from.clamp(s0, s1) - s0, cut_to.clamp(s0, s1) - s0);
        let kept = format!("{}{}", &c[..a], &c[b..]);
        if !kept.is_empty() {
            out.push(Span::styled(kept, sp.style));
        }
    }
    Some(out)
}
