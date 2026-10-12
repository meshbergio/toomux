//! Which conversation each running Claude process holds, without relying on
//! Claude's own session registry (`sessions/<pid>.json`): that file can be
//! missing (deleted, never written, another config folder), and a
//! conversation must never be opened twice because of it.
//!
//! A process's registry record is believed first: a session can `/resume`
//! another conversation in place, and a handover successor may carry its
//! predecessor's id in its environment. Only processes without one are read
//! from their command line (`--resume <id>`, `--session-id <id>`), then from
//! the `TOOMUX_PROVIDER_SESSION_ID` toomux launches them with.

use crate::config::Config;
use std::path::Path;

/// The variable toomux launches Claude with naming the conversation.
pub const ID_ENV: &str = "TOOMUX_PROVIDER_SESSION_ID";

/// A live interactive Claude process and the conversation it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Holder {
    pub pid: i32,
    pub id: String,
    /// Whether Claude's own registry has a record of it.
    pub registered: bool,
}

/// A Claude Code process: the native build (`claude`, or a versioned file
/// under `claude/versions/`), or the npm one run by node.
pub fn is_claude(args: &[String], exe: Option<&Path>) -> bool {
    let named = |p: &str| Path::new(p).file_name().is_some_and(|n| n == "claude");
    args.first().is_some_and(|a| named(a))
        || exe.is_some_and(|e| {
            e.file_name().is_some_and(|n| n == "claude")
                || e.to_string_lossy().contains("/claude/versions/")
        })
        || args
            .iter()
            .take(2)
            .any(|a| a.contains("claude-code/") && a.ends_with("cli.js"))
}

/// A conversation someone works in, not `claude -p`, an SDK run or a
/// subcommand (`claude auth login`, `claude mcp …`).
pub fn interactive(args: &[String]) -> bool {
    const SUBCOMMANDS: &[&str] = &[
        "auth",
        "mcp",
        "config",
        "doctor",
        "update",
        "install",
        "setup-token",
        "plugin",
        "migrate-installer",
    ];
    let skip = args
        .iter()
        .position(|a| a.ends_with("cli.js"))
        .map_or(1, |i| i + 1);
    !args.iter().skip(skip).any(|a| {
        matches!(a.as_str(), "-p" | "--print") || a.starts_with("--print=") || a == "--sdk-url"
    }) && !args
        .get(skip)
        .is_some_and(|a| SUBCOMMANDS.contains(&a.as_str()))
}

/// The conversation a Claude command line opens, if it names one.
pub fn id_in_args(args: &[String]) -> Option<String> {
    // A fork opens a new conversation from the one named.
    if args.iter().any(|a| a == "--fork-session") {
        return None;
    }
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        if matches!(flag, "--resume" | "-r" | "--session-id") {
            let value = inline.or_else(|| it.clone().next().cloned());
            if let Some(v) = value.filter(|v| looks_like_id(v)) {
                return Some(v);
            }
        }
    }
    None
}

/// A conversation id rather than a picker's search words or a placeholder
/// (`launch-…`, given to launches whose conversation isn't known yet).
fn looks_like_id(v: &str) -> bool {
    v.len() >= 8
        && !v.starts_with('-')
        && !v.starts_with("launch-")
        && v.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// The conversation an unregistered Claude process holds: from its command
/// line, else (only when it runs in a terminal, so not a helper process that
/// inherited the variable) from the id toomux launched it with.
pub fn id_of(args: &[String], env: &[String], in_terminal: bool) -> Option<String> {
    id_in_args(args).or_else(|| {
        in_terminal
            .then(|| {
                env.iter()
                    .find_map(|v| v.strip_prefix(ID_ENV)?.strip_prefix('='))
                    .filter(|v| looks_like_id(v))
                    .map(str::to_string)
            })
            .flatten()
    })
}

pub fn is_terminal(tty: &str) -> bool {
    tty.starts_with("/dev/pts/") || tty.starts_with("/dev/tty")
}

/// One process, if it is an interactive Claude whose conversation can be told.
pub fn holder_at(pid: i32) -> Option<Holder> {
    let args = crate::platform::cmdline(pid);
    if !is_claude(&args, crate::platform::exe(pid).as_deref()) || !interactive(&args) {
        return None;
    }
    let tty = crate::platform::stdin(pid).is_some_and(|t| is_terminal(&t.to_string_lossy()));
    let id = id_of(&args, &crate::platform::environ(pid), tty)?;
    Some(Holder {
        pid,
        id,
        registered: false,
    })
}

/// Live pids Claude's registry has a record of, with their conversations.
pub fn registered(cfg: &Config) -> Vec<Holder> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Raw {
        pid: i32,
        session_id: String,
        #[serde(default)]
        proc_start: Option<String>,
        #[serde(default)]
        kind: Option<String>,
    }
    let mut out: Vec<Holder> = Vec::new();
    for dir in cfg.session_dirs() {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let Some(r) = std::fs::read_to_string(e.path())
                .ok()
                .and_then(|raw| serde_json::from_str::<Raw>(&raw).ok())
            else {
                continue;
            };
            // A background job holds its conversation open as much as an
            // interactive session does.
            if r.kind
                .as_deref()
                .is_none_or(|k| k == "interactive" || k == "bg")
                && crate::registry::alive(r.pid, r.proc_start.as_deref())
                && !out.iter().any(|h| h.pid == r.pid)
            {
                out.push(Holder {
                    pid: r.pid,
                    id: r.session_id,
                    registered: true,
                });
            }
        }
    }
    out
}

/// Whether Claude's registry has a record of this pid, of any kind.
pub fn has_record(cfg: &Config, pid: i32) -> bool {
    cfg.session_dirs()
        .iter()
        .any(|d| d.join(format!("{pid}.json")).is_file())
}

/// Every live interactive Claude process whose conversation can be told: the
/// registry's, then every other process on the machine. Reads each process's
/// command line, so it's for actions and for doubting a loss, not for every
/// redraw.
pub fn all(cfg: &Config) -> Vec<Holder> {
    let mut out = registered(cfg);
    let me = std::process::id() as i32;
    let unregistered: Vec<Holder> = crate::platform::pids()
        .into_iter()
        .filter(|p| *p != me && !out.iter().any(|h| h.pid == *p))
        .filter_map(holder_at)
        .filter(|h| !has_record(cfg, h.pid))
        .collect();
    out.extend(unregistered);
    out
}

/// A live process holding conversation `id`, other than `except`.
pub fn holding(cfg: &Config, id: &str, except: Option<i32>) -> Option<Holder> {
    all(cfg)
        .into_iter()
        .find(|h| h.id == id && Some(h.pid) != except)
}

/// Where a process runs: its tmux pane (qualified), if it's in one.
pub fn pane_of(pid: i32) -> Option<String> {
    let tty = crate::platform::stdin(pid)?.display().to_string();
    if let Some(p) = crate::tmux::panes_by_tty().get(&tty) {
        return Some(p.id.clone());
    }
    let server = crate::platform::environ(pid)
        .iter()
        .find_map(|v| v.strip_prefix("TMUX="))
        .and_then(crate::tmux::server_in)?;
    crate::tmux::panes_on(&server, 0)
        .get(&tty)
        .map(|p| p.id.clone())
}

/// Refused: the conversation is already open. Callers that can jump to it
/// downcast to this.
#[derive(Debug)]
pub struct AlreadyRunning {
    pub pid: i32,
    pub pane: Option<String>,
}

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.pane {
            Some(p) => write!(
                f,
                "it's already running in {p} (pid {}) · jump to it instead",
                self.pid
            ),
            None => write!(
                f,
                "it's already running outside tmux (pid {}) · find it there instead",
                self.pid
            ),
        }
    }
}

impl std::error::Error for AlreadyRunning {}

/// Ok unless a live process other than `except` holds conversation `id`.
pub fn ensure_free(cfg: &Config, id: &str, except: Option<i32>) -> anyhow::Result<()> {
    match holding(cfg, id, except) {
        Some(h) => Err(AlreadyRunning {
            pid: h.pid,
            pane: pane_of(h.pid),
        }
        .into()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    const ID: &str = "849b8a1e-6212-4f60-9ff3-c17d6033a59f";

    #[test]
    fn claude_is_recognised_by_name_install_path_or_npm_entry() {
        assert!(is_claude(&v(&["claude", "--resume", ID]), None));
        assert!(is_claude(
            &v(&["/home/x/.local/share/mise/installs/claude/latest/claude"]),
            None
        ));
        assert!(is_claude(
            &v(&["2.1.3"]),
            Some(Path::new("/home/x/.local/share/claude/versions/2.1.3"))
        ));
        assert!(is_claude(
            &v(&[
                "node",
                "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
            ]),
            None
        ));
        assert!(!is_claude(
            &v(&["/usr/bin/bash", "-c", "claude --resume x"]),
            Some(Path::new("/usr/bin/bash"))
        ));
    }

    #[test]
    fn print_runs_and_subcommands_hold_no_conversation_open() {
        assert!(interactive(&v(&["claude", "--resume", ID])));
        assert!(!interactive(&v(&["claude", "-p", "--resume", ID, "hi"])));
        assert!(!interactive(&v(&["claude", "--print", "hi"])));
        assert!(!interactive(&v(&["claude", "auth", "login"])));
        assert!(!interactive(&v(&["node", "/x/claude-code/cli.js", "mcp"])));
        // A prompt that mentions a subcommand word isn't one.
        assert!(interactive(&v(&["claude", "--model", "x", "auth"])));
    }

    #[test]
    fn the_conversation_comes_from_selector_flags_never_from_a_prompt() {
        assert_eq!(
            id_in_args(&v(&["claude", "--remote-control", "--resume", ID])).as_deref(),
            Some(ID)
        );
        assert_eq!(
            id_in_args(&v(&["claude", &format!("--session-id={ID}")])).as_deref(),
            Some(ID)
        );
        assert_eq!(id_in_args(&v(&["claude", "-r", ID])).as_deref(), Some(ID));
        // A handover's opening prompt names the predecessor: not this one.
        let prompt = format!("Continue from a handover. The previous session (id {ID})");
        assert_eq!(id_in_args(&v(&["claude", &prompt])), None);
        // The picker, its search words, and forks open no known conversation.
        assert_eq!(id_in_args(&v(&["claude", "--resume"])), None);
        assert_eq!(id_in_args(&v(&["claude", "--resume", "fix bug"])), None);
        assert_eq!(
            id_in_args(&v(&["claude", "--resume", ID, "--fork-session"])),
            None
        );
    }

    #[test]
    fn the_launch_variable_counts_only_for_a_process_in_a_terminal() {
        let env = v(&[&format!("{ID_ENV}={ID}"), "HOME=/h"]);
        let args = v(&["claude", "--model", "opus"]);
        assert_eq!(id_of(&args, &env, true).as_deref(), Some(ID));
        assert_eq!(id_of(&args, &env, false), None, "a helper inherited it");
        let placeholder = v(&[&format!("{ID_ENV}=launch-12-1700000000000")]);
        assert_eq!(id_of(&args, &placeholder, true), None);
        // The command line wins over an inherited variable.
        let other = "11111111-2222-3333-4444-555555555555";
        assert_eq!(
            id_of(&v(&["claude", "--resume", other]), &env, true).as_deref(),
            Some(other)
        );
    }

    #[test]
    fn an_unregistered_claude_process_is_found_holding_its_conversation() {
        // A stand-in named claude, resuming a conversation, with no registry
        // record anywhere: found by the process scan, and only that one.
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-holder-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Pane lookups list only this test's (empty) tmux socket folder.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &tmp);
            std::env::set_var("TMUX_TMPDIR", &tmp);
        }
        let id = "0f0f0f0f-1234-4abc-8def-000000000001";
        let mut child = std::process::Command::new("sh")
            .arg0("claude")
            .args(["-c", "sleep 30; :", "--resume", id])
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let mut cfg = Config::default();
        cfg.accounts.clear();
        let t = std::time::Instant::now();
        let mut found = None;
        while found.is_none() && t.elapsed() < std::time::Duration::from_secs(5) {
            found = holding(&cfg, id, None);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(
            found,
            Some(Holder {
                pid,
                id: id.into(),
                registered: false
            })
        );
        assert_eq!(holding(&cfg, id, Some(pid)), None, "except itself");
        let err = ensure_free(&cfg, id, None).unwrap_err();
        assert!(err.downcast_ref::<AlreadyRunning>().is_some());
        let _ = child.kill();
        let _ = child.wait();
        assert!(ensure_free(&cfg, id, None).is_ok(), "gone: free again");
        unsafe { std::env::remove_var("TMUX_TMPDIR") };
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
