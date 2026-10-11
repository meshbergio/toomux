use crate::config::{Config, expand};
use crate::registry::{self, Session, State};
use crate::tmux;
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Flags that take a value, so the value is kept alongside the flag.
const VALUE_FLAGS: &[&str] = &[
    "--model",
    "--permission-mode",
    "--add-dir",
    "--append-system-prompt",
    "--system-prompt",
    "--settings",
    "--mcp-config",
    "--agent",
    "--agents",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--fallback-model",
    "--plugin-dir",
    "--setting-sources",
    "--effort",
    "--name",
    "-n",
    "--autocompact",
    "--betas",
    "--tools",
    "--file",
    "--permission-prompts",
    "--plugin-url",
    "--debug-file",
    "--max-budget-usd",
    "--json-schema",
    "--input-format",
    "--output-format",
    "--client-data-url",
    "--environment",
    "--system-prompt-snapshot",
    "--remote-control-session-name-prefix",
];
/// Value flags that take every following word (`--add-dir a b`), as Claude's
/// own parser does. A prompt placed after one would be swallowed too.
const VARIADIC_FLAGS: &[&str] = &[
    "--add-dir",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--mcp-config",
    "--betas",
    "--tools",
    "--file",
];
/// Flags that pick which conversation to open; replaced by our own --resume.
const SESSION_FLAGS_WITH_VALUE: &[&str] = &["--resume", "-r", "--session-id", "--from-pr"];
const SESSION_FLAGS: &[&str] = &["--continue", "-c", "--fork-session"];

/// The original launch flags, minus any conversation selector or initial prompt.
pub fn carried_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter().skip(1).peekable();
    while let Some(a) = it.next() {
        let flag = a.split('=').next().unwrap_or(a);
        if SESSION_FLAGS.contains(&flag) {
            continue;
        }
        if SESSION_FLAGS_WITH_VALUE.contains(&flag) {
            if !a.contains('=') && it.peek().is_some_and(|n| !n.starts_with('-')) {
                it.next();
            }
            continue;
        }
        if !a.starts_with('-') {
            continue; // positional initial prompt: never replay it
        }
        out.push(a.clone());
        // An optional value: the worktree's name.
        if matches!(flag, "-w" | "--worktree") && !a.contains('=') {
            if let Some(v) = it.next_if(|n| !n.starts_with('-')) {
                out.push(v.clone());
            }
            continue;
        }
        if VALUE_FLAGS.contains(&flag) && !a.contains('=') {
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
            if VARIADIC_FLAGS.contains(&flag) {
                while let Some(v) = it.next_if(|n| !n.starts_with('-')) {
                    out.push(v.clone());
                }
            }
        }
    }
    out
}

/// The account's CLAUDE_CONFIG_DIR for a launch: set in its environment,
/// or (where it has to be unset) cleared by the `env` in front of claude.
fn config_dir(cfg: &Config, account: usize, env: &mut Vec<String>, argv: &mut Vec<String>) {
    match crate::credentials::config_dir_var(&cfg.account_dir(account)) {
        Some(v) => env.push(format!("CLAUDE_CONFIG_DIR={v}")),
        None => argv.extend(["-u".to_string(), "CLAUDE_CONFIG_DIR".to_string()]),
    }
}

pub struct Launch {
    pub env: Vec<String>,
    pub cmd: String,
}

// Claude's prompt suggestions must retain their dim attribute: the prompt
// reader uses it to distinguish suggestions from the user's unsent input.
// Automation commonly exports NO_COLOR; a tmux server can retain it even
// after the process that created the server has exited. Sanitize each Claude
// launch, including handovers and respawns on an existing server.
fn interactive_env_argv() -> Vec<String> {
    vec!["env".into(), "-u".into(), "NO_COLOR".into()]
}

pub fn launch_for(cfg: &Config, s: &Session, account: usize) -> Launch {
    launch_with(cfg, s, account, None)
}

/// A launch for this session's account and flags: resuming its conversation,
/// or (with `fresh`) a new conversation that opens with that prompt.
pub fn launch_with(cfg: &Config, s: &Session, account: usize, fresh: Option<&str>) -> Launch {
    let same_account = s.account == Some(account);
    let inherited: Vec<(String, String)> = s
        .env
        .iter()
        .filter(|(k, _)| same_account || !k.starts_with("TOOMUX_CONTEXT_"))
        .cloned()
        .collect();
    let mut env: Vec<String> = inherited.iter().map(|(k, v)| format!("{k}={v}")).collect();
    if !env
        .iter()
        .any(|v| v.starts_with("TOOMUX_PROVIDER_SESSION_ID="))
    {
        env.push(format!("TOOMUX_PROVIDER_SESSION_ID={}", s.id));
    }
    // Panes inherit the tmux server's environment, which may carry another
    // session's runtime markers; strip them so the relaunch starts clean.
    let mut argv = interactive_env_argv();
    for v in registry::RUNTIME_VARS {
        argv.extend(["-u".to_string(), v.to_string()]);
    }
    config_dir(cfg, account, &mut env, &mut argv);
    argv.push(expand(&cfg.claude_bin).display().to_string());
    // An opening prompt goes first: after a variadic flag it would be read
    // as one of that flag's values.
    if let Some(prompt) = fresh {
        argv.push(prompt.to_string());
    }
    let mut carried = carried_args(&s.args);
    if fresh.is_some() {
        // A fresh conversation would otherwise be titled after its opening
        // prompt (the handover boilerplate): it keeps the old one's name.
        carried = without_name(carried);
        let name = s.title.trim();
        if !name.is_empty() {
            carried.extend(["--name".to_string(), name.to_string()]);
        }
    }
    let prepared = crate::context_policy::prepare_launch(
        cfg,
        account,
        Path::new(&s.cwd),
        &carried,
        if same_account { &inherited } else { &[] },
        Some(&s.id),
    );
    carried = prepared.args;
    for (key, value) in prepared.env {
        env.retain(|entry| !entry.starts_with(&format!("{key}=")));
        env.push(format!("{key}={value}"));
    }
    argv.extend(carried);
    if fresh.is_none() {
        argv.push("--resume".into());
        argv.push(s.id.clone());
    }
    Launch {
        env,
        cmd: shell_words::join(&argv),
    }
}

fn without_name(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--name" || a == "-n" {
            it.next();
        } else if !a.starts_with("--name=") {
            out.push(a);
        }
    }
    out
}

/// Restart a session in its own pane as a new conversation opening with
/// `prompt` (same account, same flags). Used by handover.
pub fn relaunch_fresh(cfg: &Config, s: &Session, prompt: &str) -> Result<()> {
    let account = s.account.context("its account isn't known")?;
    let pane = s.pane.clone().context("it isn't in tmux")?;
    // Claude started from a shell in the pane: keep the shell, start the
    // fresh session from it (respawning would replace the shell).
    let pane_pid = tmux::run(&["display-message", "-p", "-t", &pane.id, "#{pane_pid}"])
        .ok()
        .and_then(|p| p.trim().parse::<i32>().ok());
    if pane_pid.is_some_and(|p| p != s.pid) {
        return relaunch_from_shell(cfg, s, &pane.id, account, prompt);
    }
    let launch = launch_with(cfg, s, account, Some(prompt));
    tmux::run(&["set-option", "-p", "-t", &pane.id, "remain-on-exit", "on"])?;
    let result = (|| {
        stop(s)?;
        let t = Instant::now();
        while tmux::pane_dead(&pane.id) == Some(false) && t.elapsed() < Duration::from_secs(5) {
            sleep(Duration::from_millis(100));
        }
        respawn(&pane.id, &s.cwd, &launch)?;
        // A launch that dies at once (a bad flag, say) must not leave a dead
        // pane: bring the old conversation back instead.
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(4) {
            sleep(Duration::from_millis(250));
            if tmux::pane_dead(&pane.id) == Some(true) {
                respawn(&pane.id, &s.cwd, &launch_for(cfg, s, account))?;
                bail!(
                    "the fresh session wouldn't start, so the old conversation is back in its pane"
                );
            }
        }
        Ok(())
    })();
    let _ = tmux::run(&["set-option", "-p", "-u", "-t", &pane.id, "remain-on-exit"]);
    result
}

fn relaunch_from_shell(
    cfg: &Config,
    s: &Session,
    pane: &str,
    account: usize,
    prompt: &str,
) -> Result<()> {
    // The shell has its own environment; only the account and Claude's own
    // settings need saying.
    let mut plain = s.clone();
    plain.env = s
        .env
        .iter()
        .filter(|(k, _)| registry::carry_env(k))
        .cloned()
        .collect();
    let launch = launch_with(cfg, &plain, account, Some(prompt));
    stop(s)?;
    let shell = |c: &str| matches!(c, "bash" | "zsh" | "fish" | "sh" | "dash" | "ksh");
    let t = Instant::now();
    while !tmux::run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{pane_current_command}",
    ])
    .is_ok_and(|c| shell(c.trim()))
    {
        if t.elapsed() > Duration::from_secs(5) {
            bail!("the pane's shell didn't come back after the old session stopped");
        }
        sleep(Duration::from_millis(100));
    }
    // A leading space keeps it out of the shell's history (ignorespace).
    let line = format!(
        " cd {} && env {} {}",
        shell_words::quote(&s.cwd),
        launch
            .env
            .iter()
            .map(|e| shell_words::quote(e).into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        launch.cmd
    );
    tmux::run(&["send-keys", "-t", pane, "-l", &line])?;
    tmux::run(&["send-keys", "-t", pane, "Enter"])?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(8) {
        sleep(Duration::from_millis(250));
        if tmux::run(&[
            "display-message",
            "-p",
            "-t",
            pane,
            "#{pane_current_command}",
        ])
        .is_ok_and(|c| !shell(c.trim()))
        {
            return Ok(());
        }
    }
    bail!("the fresh session wouldn't start from the pane's shell")
}

/// A session outside tmux can't be restarted where it runs: stop it and start
/// the fresh conversation in a tmux window of its own (same account, same
/// flags). Returns the new pane.
pub fn relaunch_fresh_in_tmux(cfg: &Config, s: &Session, prompt: &str) -> Result<String> {
    let account = s.account.context("its account isn't known")?;
    let launch = launch_with(cfg, s, account, Some(prompt));
    let pane = place_in_tmux(cfg, s, &launch)?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(4) {
        sleep(Duration::from_millis(250));
        if tmux::pane_dead(&pane) != Some(false) {
            // It died at once: bring the old conversation back beside it.
            let back = place_in_tmux(cfg, s, &launch_for(cfg, s, account))?;
            bail!(
                "the fresh session wouldn't start, so the old conversation is back in tmux ({back})"
            );
        }
    }
    Ok(pane)
}

fn respawn(pane: &str, cwd: &str, launch: &Launch) -> Result<()> {
    let mut args: Vec<&str> = vec!["respawn-pane", "-k", "-t", pane, "-c", cwd];
    for e in &launch.env {
        args.extend(["-e", e]);
    }
    args.push(&launch.cmd);
    tmux::run(&args).map(|_| ())
}

pub fn jump(s: &Session) -> Result<()> {
    let Some(p) = &s.pane else {
        bail!("{} runs outside tmux · ctrl-o brings it in", s.title)
    };
    jump_pane(&p.id)
}

pub fn jump_pane(pane: &str) -> Result<()> {
    jump_client(pane, None)
}

/// Jump, moving a named terminal (tmux client) rather than tmux's guess.
pub fn jump_client(pane: &str, client: Option<&str>) -> Result<()> {
    tmux::run(&["select-window", "-t", pane])?;
    tmux::run(&["select-pane", "-t", pane])?;
    tmux::attach_here(pane, client).context("attaching to tmux")
}

/// Jump from the sidebar: bring the sidebar pane along into the target's
/// window (left edge, full height, same width), then jump.
pub fn follow(pane: &str) -> Result<()> {
    let me = std::env::var("TMUX_PANE").context("the sidebar isn't running in tmux")?;
    let get = |p: &str, f: &str| {
        tmux::run(&["display-message", "-p", "-t", p, f]).map(|s| s.trim().to_string())
    };
    let same_server = tmux::current_server().as_deref() == tmux::server_of(pane);
    if same_server && get(&me, "#{window_id}")? != get(pane, "#{window_id}")? {
        let width = get(&me, "#{pane_width}")?;
        tmux::run(&[
            "join-pane",
            "-d",
            "-h",
            "-b",
            "-f",
            "-l",
            &width,
            "-s",
            &me,
            "-t",
            pane,
        ])?;
    }
    jump_pane(pane)
}

pub const SIDEBAR_WIDTH: &str = "36";

/// alt-b: show the sidebar in the current window, or hide it if it's here.
/// There is only ever one; showing it elsewhere moves it.
pub fn toggle_sidebar() -> Result<()> {
    let here = tmux::run(&["display-message", "-p", "#{window_id}\t#{pane_id}"])?;
    let (window, pane) = here
        .trim()
        .split_once('\t')
        .context("no current tmux pane")?;
    let panes = tmux::run(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}\t#{window_id}\t#{@toomux_sidebar}",
    ])?;
    let existing = panes.lines().filter_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        (f.len() == 3 && f[2] == "1").then(|| (f[0].to_string(), f[1].to_string()))
    });
    let mut found = false;
    for (id, win) in existing {
        found = true;
        if win == window {
            tmux::run(&["kill-pane", "-t", &id])?;
        } else {
            tmux::run(&[
                "join-pane",
                "-d",
                "-h",
                "-b",
                "-f",
                "-l",
                SIDEBAR_WIDTH,
                "-s",
                &id,
                "-t",
                pane,
            ])?;
        }
    }
    if !found {
        let exe = std::env::current_exe()?.display().to_string();
        let cmd = shell_words::join([exe.as_str(), "sidebar"]);
        let id = tmux::run(&[
            "split-window",
            "-d",
            "-h",
            "-b",
            "-f",
            "-l",
            SIDEBAR_WIDTH,
            "-t",
            pane,
            "-P",
            "-F",
            "#{pane_id}",
            &cmd,
        ])?;
        tmux::run(&["set-option", "-p", "-t", id.trim(), "@toomux_sidebar", "1"])?;
    }
    Ok(())
}

/// A tmux window name: the title if it's short, else its first few words.
pub fn window_name(title: &str) -> String {
    let mut out = String::new();
    for w in title.split_whitespace() {
        if !out.is_empty() && out.chars().count() + 1 + w.chars().count() > 24 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out.chars().take(32).collect()
}

/// Reopen a conversation that isn't running (pinned, or from before a
/// restart) in a new tmux window: where it was before a restart, else in the
/// current tmux session, else the adopt session.
/// Returns the new pane id.
pub fn revive(cfg: &Config, s: &Session) -> Result<String> {
    let account = s
        .account
        .context("the account it last ran on isn't configured")?;
    if !std::path::Path::new(&s.cwd).is_dir() {
        bail!("its folder {} no longer exists", s.cwd);
    }
    let launch = launch_for(cfg, s, account);
    // Under the name it had before a restart, if someone chose one.
    let name = s
        .restore
        .clone()
        .and_then(|(_, named)| named)
        .unwrap_or_else(|| window_name(&s.title));
    in_own_server(Some(&s.id), &name, &s.cwd, &launch)
}

/// Every session gets a tmux server of its own, so one server going down
/// (a stray `kill-server`, a crash) takes only that session with it.
/// Returns the qualified pane id.
fn in_own_server(id: Option<&str>, name: &str, cwd: &str, launch: &Launch) -> Result<String> {
    let server = tmux::new_server_name(id);
    tmux::new_server(&server, name, cwd, &launch.env, &launch.cmd)
}

/// Start a new Claude session in `folder` on `account`, in a tmux server of
/// its own. Returns the pane id.
pub fn start(cfg: &Config, folder: &str, account: usize, args: &[String]) -> Result<String> {
    if !std::path::Path::new(folder).is_dir() {
        bail!("{folder} isn't a folder");
    }
    let mut argv = vec!["env".to_string()];
    for v in registry::RUNTIME_VARS {
        argv.extend(["-u".to_string(), v.to_string()]);
    }
    let mut env = vec![format!(
        "TOOMUX_PROVIDER_SESSION_ID=launch-{}-{}",
        std::process::id(),
        crate::registry::now_ms()
    )];
    config_dir(cfg, account, &mut env, &mut argv);
    argv.push(expand(&cfg.claude_bin).display().to_string());
    let prepared =
        crate::context_policy::prepare_launch(cfg, account, Path::new(folder), args, &[], None);
    for (key, value) in prepared.env {
        env.push(format!("{key}={value}"));
    }
    argv.extend(prepared.args);
    let cmd = shell_words::join(&argv);
    let name = folder
        .rsplit('/')
        .find(|p| !p.is_empty())
        .unwrap_or("claude")
        .to_string();
    in_own_server(None, &name, folder, &Launch { env, cmd })
}

/// Start Claude Code's first-class account sign-in flow in its own pane.
///
/// New account folders may stop at interactive onboarding before a normal
/// prompt exists, so driving the slash-login command through the full client
/// is unreliable. The auth subcommand bypasses onboarding and opens OAuth
/// directly. Remote launches do not always carry a BROWSER variable, so
/// provide the platform's normal opener explicitly while still respecting any
/// caller-supplied BROWSER override.
pub fn start_auth_login(cfg: &Config, folder: &str, account: usize) -> Result<String> {
    if !std::path::Path::new(folder).is_dir() {
        bail!("{folder} isn't a folder");
    }
    let mut argv = vec!["env".to_string()];
    for v in registry::RUNTIME_VARS {
        argv.extend(["-u".to_string(), v.to_string()]);
    }
    let mut env = Vec::new();
    config_dir(cfg, account, &mut env, &mut argv);
    if std::env::var_os("BROWSER").is_none() {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        env.push(format!("BROWSER={opener}"));
    }
    argv.push(expand(&cfg.claude_bin).display().to_string());
    argv.extend(["auth".into(), "login".into(), "--claudeai".into()]);
    let cmd = shell_words::join(&argv);
    let name = format!(
        "{}-login",
        folder
            .rsplit('/')
            .find(|p| !p.is_empty())
            .unwrap_or("claude")
    );
    in_own_server(None, &name, folder, &Launch { env, cmd })
}

const CLAUDE_AUTH_URL_PREFIX: &str = "https://claude.com/cai/oauth/authorize?";

/// Open the OAuth link Claude printed in its private auth pane.
///
/// Claude Code currently says it is opening the browser even on Linux hosts
/// where no visible browser appears. Keep the auth process in its tmux pane,
/// read only enough scrollback to find its OSC-8/plain OAuth link, and hand
/// that URL straight to the platform opener. The URL is never logged or
/// returned to the caller.
pub fn open_auth_login_browser(pane: &str) -> Result<bool> {
    let screen = tmux::run(&["capture-pane", "-p", "-e", "-J", "-t", pane, "-S", "-80"])
        .context("reading Claude login pane")?;
    let Some(url) = auth_login_url_in(&screen) else {
        return Ok(false);
    };
    open_browser_target(OsStr::new(url))?;
    Ok(true)
}

/// Open a local file or URL in a visible GUI browser.
///
/// Linux desktop openers can report success while dispatching nowhere, so prefer an installed
/// browser executable and keep \`xdg-open\` only as the last compatibility fallback.
pub fn open_browser_target(target: &OsStr) -> Result<()> {
    let (opener, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if std::path::Path::new("/opt/google/chrome/google-chrome").is_file() {
        ("/opt/google/chrome/google-chrome", &["--new-window"])
    } else if std::path::Path::new("/usr/bin/google-chrome").is_file() {
        ("/usr/bin/google-chrome", &["--new-window"])
    } else if std::path::Path::new("/usr/bin/chromium").is_file() {
        ("/usr/bin/chromium", &["--new-window"])
    } else if std::path::Path::new("/usr/bin/chromium-browser").is_file() {
        ("/usr/bin/chromium-browser", &["--new-window"])
    } else if std::path::Path::new("/usr/bin/firefox").is_file() {
        ("/usr/bin/firefox", &["--new-window"])
    } else {
        ("xdg-open", &[])
    };
    let status = Command::new(opener)
        .args(args)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("opening browser with {opener}"))?;
    if !status.success() {
        bail!("the desktop browser opener exited with {status}");
    }
    Ok(())
}

fn auth_login_url_in(screen: &str) -> Option<&str> {
    let start = screen.find(CLAUDE_AUTH_URL_PREFIX)?;
    let rest = &screen[start..];
    let end = rest
        .char_indices()
        .find_map(|(i, c)| (c.is_whitespace() || c == '\u{1b}').then_some(i))
        .unwrap_or(rest.len());
    let url = &rest[..end];
    (url.len() <= 8192).then_some(url)
}

/// Text that was actually typed into Claude's current input box. Dim
/// suggestions/placeholders are excluded. None means the prompt can't be
/// inspected safely (copy mode, no visible prompt, or tmux failure).
pub fn prompt_draft(pane: &str) -> Option<String> {
    // Scrolled back (copy mode): what's typed can't be seen, and keys would go
    // to copy mode rather than the prompt.
    if tmux::run(&["display-message", "-p", "-t", pane, "#{pane_in_mode}"])
        .is_ok_and(|m| m.trim() != "0")
    {
        return None;
    }
    // -J joins terminal soft-wraps, so a long one-line draft stays one logical
    // line instead of looking like several unrelated screen rows.
    let screen = tmux::run(&["capture-pane", "-p", "-e", "-J", "-t", pane, "-S", "-40"]).ok()?;
    prompt_draft_in(&screen)
}

fn prompt_draft_in(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines
        .iter()
        .rposition(|l| strip_ansi(l).trim_start().starts_with('❯'))?;
    let mut draft = vec![typed_text(lines[start])];
    for line in lines.iter().skip(start + 1) {
        let plain = strip_ansi(line);
        if plain.trim_start().starts_with('─') || plain.trim_start().starts_with('❯') {
            break;
        }
        // Continuation rows do not repeat the prompt mark. Prepend one only
        // for the style-aware parser so dim suggestions remain excluded.
        let continued = typed_text(&format!("❯{line}"));
        if !continued.is_empty() || plain.trim().is_empty() {
            draft.push(continued);
        }
    }
    while draft.last().is_some_and(String::is_empty) && draft.len() > 1 {
        draft.pop();
    }
    Some(draft.join("\n"))
}

/// Whether Claude's input box in this pane is empty (only the placeholder),
/// so typing a command into it can't mix with an unsent draft.
pub fn prompt_empty(pane: &str) -> bool {
    prompt_draft(pane).is_some_and(|typed| typed.is_empty() || typed.starts_with("Try \""))
}

/// Clear exactly the draft we just preserved for a handover. If the prompt
/// changed between inspection and clearing, leave it alone: that is active
/// user input, not the stale draft we were asked to migrate.
pub fn clear_prompt_if(pane: &str, expected: &str) -> Result<()> {
    let current = prompt_draft(pane).context("can't read Claude's prompt before clearing it")?;
    if current != expected {
        bail!("something is typed in its prompt: it changed while handing over");
    }
    if current.is_empty() || current.starts_with("Try \"") {
        return Ok(());
    }
    tmux::run(&["send-keys", "-t", pane, "C-u"])?;
    sleep(Duration::from_millis(250));
    if !prompt_empty(pane) {
        bail!("something is typed in its prompt: couldn't clear the preserved draft");
    }
    Ok(())
}

/// Type `text` into an empty Claude prompt and submit it, making sure it went.
pub fn type_prompt(pane: &str, text: &str) -> Result<()> {
    tmux::run(&["send-keys", "-t", pane, "-l", text])?;
    sleep(Duration::from_millis(400));
    tmux::run(&["send-keys", "-t", pane, "Enter"])?;
    // A long text can arrive as a paste that swallows the first Enter.
    for _ in 0..3 {
        sleep(Duration::from_millis(1200));
        if prompt_empty(pane) {
            return Ok(());
        }
        tmux::run(&["send-keys", "-t", pane, "Enter"])?;
    }
    Ok(())
}

/// What was typed after the prompt mark. Claude Code draws its placeholder
/// and its suggested next prompt dim; only text in normal intensity was
/// typed by someone.
fn typed_text(line: &str) -> String {
    let mut out = String::new();
    let mut dim = false;
    let mut seen_mark = false;
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                let mut params = String::new();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        if d == 'm' {
                            let mut ps = params.split(';');
                            while let Some(p) = ps.next() {
                                match p {
                                    "2" => dim = true,
                                    "" | "0" | "22" => dim = false,
                                    // Colours carry arguments (5;n or 2;r;g;b) that aren't attributes.
                                    "38" | "48" | "58" => match ps.next() {
                                        Some("5") => {
                                            ps.next();
                                        }
                                        Some("2") => {
                                            ps.nth(2);
                                        }
                                        _ => {}
                                    },
                                    _ => {}
                                }
                            }
                        }
                        break;
                    }
                    params.push(d);
                }
            }
            continue;
        }
        if !seen_mark {
            seen_mark = c == '❯';
            continue;
        }
        if !dim {
            out.push(c);
        }
    }
    out.trim().to_string()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Name a session. toomux always shows the name; the tmux window takes it
/// when the session has the window to itself; and Claude gets `/rename` when
/// it's idle at an empty prompt (so the name also shows in its /resume list).
/// An empty name clears toomux's override. `for_you`: the name was chosen by
/// an agent to describe the work, not given by you, so it follows the work
/// across handovers; yours stay.
pub fn rename(cfg: &Config, s: &Session, name: &str, for_you: bool) -> Result<String> {
    let name = name.trim().to_string();
    crate::state::State::update(|st| {
        if name.is_empty() {
            st.names.remove(&s.id);
        } else {
            st.names.insert(s.id.clone(), name.clone());
        }
        if for_you && !name.is_empty() {
            st.chosen.insert(s.id.clone());
        } else {
            st.chosen.remove(&s.id);
        }
        if let Some(n) = st.slot_of(&s.id)
            && let Some(p) = st.pins[n].as_mut()
        {
            p.title = if name.is_empty() {
                s.title.clone()
            } else {
                name.clone()
            };
        }
    })?;
    if name.is_empty() {
        return Ok(format!("{} goes back to its own title", s.title));
    }
    let mut also = Vec::new();
    if let Some(p) = &s.pane {
        let panes = tmux::run(&["display-message", "-p", "-t", &p.id, "#{window_panes}"])
            .unwrap_or_default();
        if panes.trim() == "1"
            && tmux::run(&["rename-window", "-t", &p.id, &window_name(&name)]).is_ok()
        {
            also.push("tmux window");
        }
        let at_prompt = s.is_idle() || s.state == registry::State::Background;
        if at_prompt && !s.dormant && prompt_empty(&p.id) {
            tmux::run(&["send-keys", "-t", &p.id, "-l", &format!("/rename {name}")])?;
            sleep(Duration::from_millis(150));
            tmux::run(&["send-keys", "-t", &p.id, "Enter"])?;
            also.push("claude");
        }
    }
    let _ = cfg;
    Ok(match also.as_slice() {
        [] => format!("renamed to {name}"),
        _ => format!("renamed to {name} · {} too", also.join(" and ")),
    })
}

/// End a claude process the way a person would: ctrl-c, then escalate.
pub fn stop(s: &Session) -> Result<()> {
    let start = s.proc_start.as_deref();
    let gone = |d: Duration| {
        let t = Instant::now();
        while t.elapsed() < d {
            if !registry::alive(s.pid, start) {
                return true;
            }
            sleep(Duration::from_millis(100));
        }
        !registry::alive(s.pid, start)
    };
    for _ in 0..4 {
        match &s.pane {
            Some(p) => {
                tmux::run(&["send-keys", "-t", &p.id, "C-c"])?;
            }
            None => unsafe {
                libc::kill(s.pid, libc::SIGINT);
            },
        }
        if gone(Duration::from_millis(800)) {
            return Ok(());
        }
    }
    unsafe { libc::kill(s.pid, libc::SIGTERM) };
    if gone(Duration::from_secs(5)) {
        return Ok(());
    }
    unsafe { libc::kill(s.pid, libc::SIGKILL) };
    if gone(Duration::from_secs(2)) {
        return Ok(());
    }
    bail!("{} (pid {}) would not exit", s.title, s.pid)
}

fn refresh(cfg: &Config, s: &Session) -> Result<Session> {
    registry::load(cfg)
        .into_iter()
        .find(|x| x.pid == s.pid)
        .context("session has already ended")
}

pub fn wait_idle(cfg: &Config, s: &Session, limit: Duration) -> Result<Session> {
    let t = Instant::now();
    loop {
        let cur = refresh(cfg, s)?;
        if cur.is_idle() {
            return Ok(cur);
        }
        if t.elapsed() > limit {
            bail!(
                "{} was still working after {}m",
                s.title,
                limit.as_secs() / 60
            );
        }
        sleep(Duration::from_secs(2));
    }
}

/// Whether `cwd` is trusted. Claude Code doesn't treat `/` or the home dir as
/// covering everything beneath them, so those never count as trusted parents;
/// with `exact`, only the folder's own entry counts.
fn trusted(claude_json: &serde_json::Value, cwd: &str, exact: bool) -> bool {
    let home = crate::config::home();
    let mut p = Some(std::path::Path::new(cwd));
    while let Some(dir) = p {
        if dir != std::path::Path::new(cwd)
            && (exact || dir == std::path::Path::new("/") || dir == home)
        {
            break;
        }
        let key = dir.display().to_string();
        if claude_json.pointer(&format!(
            "/projects/{}/hasTrustDialogAccepted",
            key.replace('~', "~0").replace('/', "~1")
        )) == Some(&serde_json::Value::Bool(true))
        {
            return true;
        }
        p = dir.parent();
    }
    false
}

/// Folder trust is recorded per account. If the session's current account
/// already trusts this folder, record the same decision for the target account
/// so the resumed session doesn't stop at the trust prompt. Never grants trust
/// the person hasn't already given.
fn carry_trust(cfg: &Config, s: &Session, to: usize) -> Result<()> {
    // The session's own environment says where its .claude.json is.
    let from = match s.config_dir.as_deref() {
        Some(d) => expand(d).join(".claude.json"),
        None => crate::credentials::claude_json(&crate::config::home().join(".claude")),
    };
    let read = |path: &std::path::Path| -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    };
    let Some(src) = read(&from) else {
        return Ok(());
    };
    if !trusted(&src, &s.cwd, false) {
        return Ok(());
    }
    let path = crate::credentials::claude_json(&cfg.account_dir(to));
    // Live sessions on the target account rewrite this file too, and nothing
    // coordinates writers. So: write only if the file is unchanged since we
    // read it, and check afterwards that the entry survived.
    let stamp = |p: &std::path::Path| {
        std::fs::metadata(p)
            .ok()
            .map(|m| (m.len(), m.modified().ok()))
    };
    for _ in 0..6 {
        let before = stamp(&path);
        let Some(mut dst) = read(&path) else {
            return Ok(());
        };
        if trusted(&dst, &s.cwd, true) {
            return Ok(());
        }
        let projects = dst
            .as_object_mut()
            .context("unexpected .claude.json shape")?
            .entry("projects")
            .or_insert_with(|| serde_json::json!({}));
        let entry = projects
            .as_object_mut()
            .context("unexpected projects shape")?
            .entry(s.cwd.clone())
            .or_insert_with(|| serde_json::json!({}));
        if let Some(o) = entry.as_object_mut() {
            o.insert(
                "hasTrustDialogAccepted".into(),
                serde_json::Value::Bool(true),
            );
        }
        let tmp = path.with_file_name(format!(".claude.json.toomux-{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(&dst)?)?;
        if let Ok(meta) = std::fs::metadata(&path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        if stamp(&path) != before {
            let _ = std::fs::remove_file(&tmp);
            sleep(Duration::from_millis(150));
            continue;
        }
        std::fs::rename(&tmp, &path)?;
        sleep(Duration::from_millis(400));
        if read(&path).is_some_and(|v| trusted(&v, &s.cwd, true)) {
            return Ok(());
        }
    }
    bail!("another session kept rewriting {}", path.display())
}

/// After a relaunch, report whether the new session is stuck at a prompt that
/// needs the person (currently: the folder-trust question).
fn settle(pane: &str) -> Option<String> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(8) {
        sleep(Duration::from_millis(500));
        let screen = String::from_utf8_lossy(&tmux::capture(pane, 60)?).to_string();
        if screen.contains("Is this a project you created or one you trust") {
            return Some("it's asking you to trust the folder on this account".into());
        }
        if screen.contains("❯") && screen.contains("──") {
            return None;
        }
    }
    None
}

/// Relaunch the same conversation under another account, in place.
/// `force` accepts a session whose background tasks will be stopped.
pub fn switch(
    cfg: &Config,
    s: &Session,
    account: usize,
    wait: bool,
    force: bool,
) -> Result<String> {
    if s.account == Some(account) {
        bail!("{} is already on {}", s.title, cfg.accounts[account].name);
    }
    let s = if wait {
        wait_idle(cfg, s, Duration::from_secs(3 * 3600))?
    } else {
        let cur = refresh(cfg, s)?;
        if !cur.can_move() && !(force && cur.state == State::Background) {
            bail!(
                "{} is {} · switch once it's idle",
                cur.title,
                cur.state.section()
            );
        }
        cur
    };
    let launch = launch_for(cfg, &s, account);
    let target = &cfg.accounts[account].name;
    carry_trust(cfg, &s, account).context("copying folder trust")?;

    let Some(pane) = s.pane.clone() else {
        let id = place_in_tmux(cfg, &s, &launch)?;
        return Ok(format!("{} resumed on {target} in tmux ({id})", s.title));
    };

    // Keep the pane alive across the restart so it holds its place in the layout.
    tmux::run(&["set-option", "-p", "-t", &pane.id, "remain-on-exit", "on"])?;
    let result = (|| {
        stop(&s)?;
        let t = Instant::now();
        while tmux::pane_dead(&pane.id) == Some(false) && t.elapsed() < Duration::from_secs(5) {
            sleep(Duration::from_millis(100));
        }
        let mut args: Vec<&str> = vec!["respawn-pane", "-t", &pane.id, "-c", &s.cwd];
        for e in &launch.env {
            args.extend(["-e", e]);
        }
        args.push(&launch.cmd);
        tmux::run(&args)
    })();
    let _ = tmux::run(&["set-option", "-p", "-u", "-t", &pane.id, "remain-on-exit"]);
    result?;
    Ok(match settle(&pane.id) {
        Some(note) => format!("{} moved to {target} · {note}", s.title),
        None => format!("{} moved to {target}", s.title),
    })
}

/// Bring a session running in a plain terminal tab into tmux, same account.
pub fn adopt(cfg: &Config, s: &Session) -> Result<String> {
    if s.pane.is_some() {
        bail!("{} is already in tmux", s.title);
    }
    let cur = refresh(cfg, s)?;
    if !cur.can_move() && cur.state != State::Background {
        bail!(
            "{} is {} · bring it in once it's idle",
            cur.title,
            cur.state.section()
        );
    }
    let account = cur
        .account
        .context("can't tell which account this session uses")?;
    let launch = launch_for(cfg, &cur, account);
    let id = place_in_tmux(cfg, &cur, &launch)?;
    Ok(format!("{} is now in tmux ({id})", cur.title))
}

fn place_in_tmux(_cfg: &Config, s: &Session, launch: &Launch) -> Result<String> {
    stop(s)?;
    in_own_server(Some(&s.id), &window_name(&s.title), &s.cwd, launch)
}

pub fn close(cfg: &Config, s: &Session) -> Result<String> {
    let cur = refresh(cfg, s)?;
    stop(&cur)?;
    Ok(format!("{} closed", cur.title))
}

/// Run a switch in a detached process so it survives the popup closing.
pub fn switch_later(s: &Session, account_name: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args([
        "switch",
        &s.pid.to_string(),
        "--to",
        account_name,
        "--wait",
        "--notify",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // Reap it when it finishes (or is cancelled) so it doesn't linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

pub fn notify(msg: &str) {
    let _ = tmux::run(&["display-message", &format!("toomux · {msg}")]);
}

#[cfg(test)]
mod tests {
    use super::carried_args;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn interactive_launch_removes_inherited_no_color() {
        let argv = super::interactive_env_argv();
        let output = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .args(["sh", "-c", "printf '%s|%s' \"${NO_COLOR-unset}\" \"$TERM\""])
            .env("NO_COLOR", "1")
            .env("TERM", "tmux-256color")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "unset|tmux-256color"
        );
    }

    #[test]
    fn a_fresh_conversation_keeps_the_name() {
        let a = v(&[
            "claude",
            "--remote-control",
            "--name",
            "old",
            "--model",
            "opus",
        ]);
        assert_eq!(
            super::without_name(carried_args(&a)),
            v(&["--remote-control", "--model", "opus"])
        );
    }

    #[test]
    fn suggestions_and_placeholders_are_not_typed() {
        use super::{prompt_draft_in, typed_text};
        // As captured from real panes (capture-pane -e).
        assert_eq!(
            typed_text("\u{1b}[39m❯\u{a0}\u{1b}[2mkeep going, proceed autonomously\u{1b}[0m"),
            ""
        );
        assert_eq!(
            typed_text(
                "\u{1b}[38;5;246m❯\u{a0}\u{1b}[2m\u{1b}[39mPress up to edit queued messages\u{1b}[0m"
            ),
            ""
        );
        assert_eq!(
            typed_text("\u{1b}[39m❯\u{a0}fix the bug\u{1b}[0m"),
            "fix the bug"
        );
        assert_eq!(
            typed_text("\u{1b}[39m❯\u{a0}fix \u{1b}[2mthe bug\u{1b}[22m now"),
            "fix  now"
        );
        assert_eq!(
            typed_text("❯ \u{1b}[38;5;2mgreen text\u{1b}[0m"),
            "green text",
            "a colour index of 2 isn't dim"
        );
        assert_eq!(typed_text("❯ \u{1b}[38;2;2;2;2mrgb\u{1b}[0m"), "rgb");

        let screen = concat!(
            "\u{1b}[39m❯\u{a0}\u{1b}[38;2;255;255;255mPlease complete handover\u{1b}[39m\n",
            "\u{1b}[38;2;153;153;153mWorked for 9s · done\u{1b}[39m\n",
            "\u{1b}[39m❯\u{a0}'d v9\n",
        );
        assert_eq!(
            prompt_draft_in(screen).as_deref(),
            Some("'d v9"),
            "the live prompt wins over an older submitted prompt still on screen"
        );

        let multiline = concat!(
            "\u{1b}[39m❯\u{a0}first line\n",
            "\u{1b}[39m  second line\n",
            "\u{1b}[39m  third line\n",
            "\u{1b}[38;2;136;136;136m────────────────────────\u{1b}[39m\n",
            "  status line\n",
        );
        assert_eq!(
            prompt_draft_in(multiline).as_deref(),
            Some("first line\nsecond line\nthird line"),
            "hard newlines in the input box are preserved too"
        );
    }

    #[test]
    fn claude_auth_url_is_read_from_osc8_without_leaking_terminal_controls() {
        let url = "https://claude.com/cai/oauth/authorize?code=true&state=abc123";
        let screen = format!(
            "Opening browser to sign in…\nIf the browser didn't open, visit: \
             \u{1b}]8;;{url}\u{1b}\\{url}\u{1b}]8;;\u{1b}\\\nPaste code here if prompted >"
        );
        assert_eq!(super::auth_login_url_in(&screen), Some(url));
        assert_eq!(super::auth_login_url_in("no login link here"), None);
    }

    #[test]
    fn keeps_flags_drops_session_selectors_and_prompts() {
        let a = v(&[
            "claude",
            "--dangerously-skip-permissions",
            "--resume",
            "abc",
            "--model",
            "opus",
            "fix the bug",
            "--remote-control",
            "-c",
        ]);
        assert_eq!(
            carried_args(&a),
            v(&[
                "--dangerously-skip-permissions",
                "--model",
                "opus",
                "--remote-control"
            ])
        );
    }

    #[test]
    fn variadic_flags_keep_all_values_and_prompts_go_first() {
        let args: Vec<String> = [
            "claude",
            "--add-dir",
            "a",
            "b",
            "--model",
            "haiku",
            "--mcp-config",
            "x.json",
            "y.json",
            "-c",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            carried_args(&args),
            [
                "--add-dir",
                "a",
                "b",
                "--model",
                "haiku",
                "--mcp-config",
                "x.json",
                "y.json"
            ]
        );
    }

    #[test]
    fn handles_equals_forms() {
        let a = v(&[
            "claude",
            "--resume=abc",
            "--model=sonnet",
            "--session-id",
            "x",
        ]);
        assert_eq!(carried_args(&a), v(&["--model=sonnet"]));
    }
}
