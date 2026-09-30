//! A tmux control-mode client (`tmux -C`). tmux streams each pane's raw
//! output as `%output` lines and answers commands in `%begin`/`%end` blocks,
//! all over a pipe: no pty, no second tmux screen, no polling.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::Sender;

#[derive(Debug)]
pub enum Event {
    /// Bytes a pane wrote.
    Output { pane: String, data: Vec<u8> },
    /// The answer to command number `seq` (in the order they were sent).
    Reply {
        seq: u64,
        ok: bool,
        lines: Vec<String>,
    },
    /// Windows or panes changed shape (resize, split, close).
    Layout,
    /// The session our client is attached to changed.
    Session,
    /// tmux went away (server exit, or the client was detached).
    Exit,
}

pub struct Control {
    /// The tmux server this client is attached to.
    pub server: String,
    stdin: ChildStdin,
    child: Child,
    next: u64,
}

impl Control {
    /// Attach in control mode to `server`: to its most recent session, or,
    /// on the default server only, a new one. Every event goes to `tx`,
    /// wrapped by `wrap`.
    pub fn start<M: Send + 'static>(
        server: &str,
        tx: Sender<M>,
        wrap: impl Fn(Event) -> M + Send + 'static,
    ) -> Result<Self> {
        let has_server = crate::tmux::command_on(server)
            .args(["has-session"])
            .env_remove("TMUX")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        let args: &[&str] = match (has_server, server) {
            (true, _) => &["-C", "attach"],
            (false, "default") => &["-C", "new-session", "-s", "main"],
            (false, _) => anyhow::bail!("tmux server {server} isn't running"),
        };
        let mut child = crate::tmux::command_on(server)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting tmux in control mode")?;
        let stdin = child.stdin.take().context("tmux stdin")?;
        let stdout = child.stdout.take().context("tmux stdout")?;
        std::thread::spawn(move || read(BufReader::new(stdout), tx, wrap));
        Ok(Self {
            server: server.to_string(),
            stdin,
            child,
            next: 0,
        })
    }

    /// Send commands as one line: tmux runs them back to back, with nothing
    /// in between. Returns each command's reply number.
    pub fn send(&mut self, cmds: &[String]) -> Vec<u64> {
        if cmds.is_empty() {
            return Vec::new();
        }
        let line = cmds.join(" ; ");
        let seqs: Vec<u64> = (0..cmds.len() as u64).map(|i| self.next + i).collect();
        self.next += cmds.len() as u64;
        let _ = writeln!(self.stdin, "{line}");
        let _ = self.stdin.flush();
        seqs
    }

    pub fn one(&mut self, cmd: impl Into<String>) -> u64 {
        self.send(&[cmd.into()])[0]
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        // An empty line detaches a control client cleanly.
        let _ = writeln!(self.stdin);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read<M>(mut r: impl BufRead, tx: Sender<M>, wrap: impl Fn(Event) -> M) {
    let mut line = Vec::new();
    // Replies to our own commands carry flag 1; the attach itself has flag 0.
    let mut seq = 0u64;
    let mut block: Option<(bool, Vec<String>)> = None;
    loop {
        line.clear();
        match r.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => {
                let _ = tx.send(wrap(Event::Exit));
                return;
            }
            Ok(_) => {}
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if let Some((mine, lines)) = &mut block {
            let end = line.starts_with(b"%end ") || line.starts_with(b"%error ");
            if end {
                let ok = line.starts_with(b"%end ");
                let (mine, lines) = (*mine, std::mem::take(lines));
                block = None;
                if mine {
                    let _ = tx.send(wrap(Event::Reply { seq, ok, lines }));
                    seq += 1;
                }
            } else {
                lines.push(String::from_utf8_lossy(&line).into_owned());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"%begin ") {
            let mine = rest.split(|b| *b == b' ').nth(2) == Some(b"1");
            block = Some((mine, Vec::new()));
        } else if let Some(rest) = line.strip_prefix(b"%output ") {
            let Some(sp) = rest.iter().position(|b| *b == b' ') else {
                continue;
            };
            let pane = String::from_utf8_lossy(&rest[..sp]).into_owned();
            let _ = tx.send(wrap(Event::Output {
                pane,
                data: unescape(&rest[sp + 1..]),
            }));
        } else if line.starts_with(b"%layout-change")
            || line.starts_with(b"%window-")
            || line.starts_with(b"%unlinked-window")
        {
            let _ = tx.send(wrap(Event::Layout));
        } else if line.starts_with(b"%session-changed")
            || line.starts_with(b"%client-session-changed")
        {
            let _ = tx.send(wrap(Event::Session));
        } else if line.starts_with(b"%exit") {
            let _ = tx.send(wrap(Event::Exit));
        }
    }
}

/// `%output` data: bytes below 32 and backslash arrive as `\ooo`.
fn unescape(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\'
            && i + 3 < s.len()
            && s[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((s[i + 1] - b'0') * 64 + (s[i + 2] - b'0') * 8 + (s[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// A string as a tmux double-quoted argument: nothing inside is expanded.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 32 => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_unescapes_octal() {
        assert_eq!(unescape(br"a\015\012b\134c"), b"a\r\nb\\c");
        assert_eq!(unescape(br"\033[31m"), b"\x1b[31m");
        assert_eq!(unescape(br"trailing\01"), br"trailing\01");
    }

    #[test]
    fn quoting_stops_expansion() {
        assert_eq!(quote("it's $HOME \"x\"\n"), r#""it's \$HOME \"x\"\n""#);
    }
}
