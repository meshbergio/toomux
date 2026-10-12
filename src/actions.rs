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
    let var = crate::credentials::config_dir_var(&cfg.account_dir(account));
    config_dir_as(var, env, argv);
}

fn config_dir_as(var: Option<String>, env: &mut Vec<String>, argv: &mut Vec<String>) {
    env.retain(|e| !e.starts_with("CLAUDE_CONFIG_DIR="));
    match var {
        Some(v) => env.push(format!("CLAUDE_CONFIG_DIR={v}")),
        None => argv.extend(["-u".to_string(), "CLAUDE_CONFIG_DIR".to_string()]),
    }
}

/// A new conversation id (a random UUID, as `--session-id` wants), so a
/// fresh launch's conversation is known before Claude records it.
fn new_session_id() -> Option<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .ok()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// Stands in for the conversation id of a launch that doesn't know it yet.
fn launch_marker() -> String {
    format!(
        "launch-{}-{}",
        std::process::id(),
        crate::registry::now_ms()
    )
}

/// Whether launch flags already choose the conversation (`--resume`, `-c` …).
fn selects_conversation(args: &[String]) -> bool {
    args.iter().any(|a| {
        let flag = a.split('=').next().unwrap_or(a);
        SESSION_FLAGS.contains(&flag) || SESSION_FLAGS_WITH_VALUE.contains(&flag)
    })
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
    let var = crate::credentials::config_dir_var(&cfg.account_dir(account));
    launch_in(cfg, s, account, var, fresh)
}

/// `launch_with`, its CLAUDE_CONFIG_DIR given (None: unset).
fn launch_in(
    cfg: &Config,
    s: &Session,
    account: usize,
    config_var: Option<String>,
    fresh: Option<&str>,
) -> Launch {
    let same_account = s.account == Some(account);
    let inherited: Vec<(String, String)> = s
        .env
        .iter()
        .filter(|(k, _)| same_account || !k.starts_with("TOOMUX_CONTEXT_"))
        .cloned()
        .collect();
    let mut env: Vec<String> = inherited.iter().map(|(k, v)| format!("{k}={v}")).collect();
    // The conversation this launch opens, never the one it was inherited
    // from: a handover successor would otherwise carry its predecessor's id.
    let new_id = fresh.and_then(|_| new_session_id());
    let provider_id = match (fresh, &new_id) {
        (None, _) => s.id.clone(),
        (Some(_), Some(id)) => id.clone(),
        (Some(_), None) => launch_marker(),
    };
    env.retain(|v| !v.starts_with(&format!("{}=", crate::running::ID_ENV)));
    env.push(format!("{}={provider_id}", crate::running::ID_ENV));
    // Panes inherit the tmux server's environment, which may carry another
    // session's runtime markers; strip them so the relaunch starts clean.
    let mut argv = interactive_env_argv();
    for v in registry::RUNTIME_VARS {
        argv.extend(["-u".to_string(), v.to_string()]);
    }
    config_dir_as(config_var, &mut env, &mut argv);
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
    if fresh.is_some() {
        carried = handover_flags(cfg, s, carried);
    }
    argv.extend(carried);
    if fresh.is_none() {
        argv.push("--resume".into());
        argv.push(s.id.clone());
    } else if let Some(id) = new_id {
        argv.extend(["--session-id".to_string(), id]);
    }
    Launch {
        env,
        cmd: shell_words::join(&argv),
    }
}

/// Whether `flag` (or its `--flag=value` form) is `name`.
fn is_flag(flag: &str, name: &str) -> bool {
    flag == name || flag.strip_prefix(name).is_some_and(|r| r.starts_with('='))
}

/// Whether Remote Control is on in a running session: started with
/// `--remote-control`, or turned on since (`/remote-control`), which Claude
/// records in its session registry.
fn remote_control_on(cfg: &Config, s: &Session) -> bool {
    if s.args.iter().any(|a| is_flag(a, "--remote-control")) {
        return true;
    }
    let record = session_dir(cfg, s)
        .join("sessions")
        .join(format!("{}.json", s.pid));
    std::fs::read_to_string(record)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .is_some_and(|v| v.get("bridgeSessionId").is_some_and(|b| !b.is_null()))
}

/// The fresh session's flags after a handover, with Remote Control and
/// bypass permissions as `handover_remote_control` and
/// `handover_skip_permissions` say ("keep", "on", "off").
fn handover_flags(cfg: &Config, s: &Session, args: Vec<String>) -> Vec<String> {
    with_launch_modes(
        args,
        &cfg.handover_remote_control,
        || remote_control_on(cfg, s),
        &cfg.handover_skip_permissions,
    )
}

fn with_launch_modes(
    args: Vec<String>,
    remote: &str,
    remote_was_on: impl Fn() -> bool,
    bypass: &str,
) -> Vec<String> {
    const BYPASS: &str = "--dangerously-skip-permissions";
    let bypassing = |a: &[String]| {
        a.iter().any(|x| x == BYPASS)
            || a.windows(2)
                .any(|w| w[0] == "--permission-mode" && w[1] == "bypassPermissions")
            || a.iter().any(|x| x == "--permission-mode=bypassPermissions")
    };
    let mut out: Vec<String> = Vec::new();
    let mut it = args.into_iter().peekable();
    while let Some(a) = it.next() {
        if is_flag(&a, "--remote-control") {
            // Its optional name is a separate word only when it isn't a flag.
            if a == "--remote-control" {
                it.next_if(|n| !n.starts_with('-'));
            }
            continue;
        }
        if bypass != "keep" {
            if a == BYPASS || a == "--permission-mode=bypassPermissions" {
                continue;
            }
            if a == "--permission-mode" && it.peek().is_some_and(|v| v == "bypassPermissions") {
                it.next();
                continue;
            }
            // With bypass on, another starting mode would contradict it.
            if bypass == "on" && is_flag(&a, "--permission-mode") {
                if a == "--permission-mode" {
                    it.next();
                }
                continue;
            }
        }
        out.push(a);
    }
    if bypass == "on" && !bypassing(&out) {
        out.push(BYPASS.into());
    }
    let remote = match remote {
        "on" => true,
        "off" => false,
        _ => remote_was_on(),
    };
    // Last, so the next word (a flag) is never read as its optional name.
    if remote {
        out.push("--remote-control".into());
    }
    out
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

/// A fresh launch that didn't come up. `screen` is what its pane showed, for
/// the handover log; the message says whether the old conversation is back.
#[derive(Debug)]
pub struct LaunchFailed {
    pub msg: String,
    pub screen: String,
}

impl std::fmt::Display for LaunchFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for LaunchFailed {}

/// How long a fresh Claude gets to show its prompt (or a screen that needs
/// the person) after it starts.
const START_WAIT: Duration = Duration::from_secs(20);

/// Restart a session in its own pane as a new conversation opening with
/// `prompt` (same account, same flags). Used by handover. On success, a note
/// when the fresh Claude stopped at a screen that needs the person (a theme
/// picker, the folder-trust question): it is running, but not yet working.
/// When it doesn't come up, the old conversation is resumed in its place and
/// the error is a [`LaunchFailed`].
pub fn relaunch_fresh(cfg: &Config, s: &Session, prompt: &str) -> Result<Option<String>> {
    ready_account(cfg, &session_dir(cfg, s), &s.cwd, false);
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
        // Respawning clears the screen: what shows now is the new Claude's.
        let gone = || tmux::pane_dead(&pane.id) != Some(false);
        match await_start(&pane.id, None, &gone, START_WAIT) {
            Started::Ready => Ok(None),
            Started::Blocked(note) => Ok(Some(note)),
            // A launch that dies at once (a bad flag, say) must not leave a
            // dead pane: bring the old conversation back instead.
            Started::Gone(screen) => {
                respawn(&pane.id, &s.cwd, &launch_for(cfg, s, account))?;
                let back = !matches!(
                    await_start(&pane.id, None, &gone, Duration::from_secs(10)),
                    Started::Gone(_)
                );
                Err(LaunchFailed {
                    msg: if back {
                        "the fresh session wouldn't start, so the old conversation is back in its pane"
                            .into()
                    } else {
                        format!(
                            "the fresh session wouldn't start, and neither would the old conversation: \
                             resume it with claude --resume {}",
                            s.id
                        )
                    },
                    screen,
                }
                .into())
            }
        }
    })();
    let _ = tmux::run(&["set-option", "-p", "-u", "-t", &pane.id, "remain-on-exit"]);
    result
}

fn is_shell(command: &str) -> bool {
    matches!(
        command.trim_start_matches('-'),
        "bash" | "zsh" | "fish" | "sh" | "dash" | "ksh"
    )
}

fn current_command(pane: &str) -> Option<String> {
    tmux::run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{pane_current_command}",
    ])
    .ok()
    .map(|c| c.trim().to_string())
}

/// Wait until the pane's shell is back in the foreground and has finished
/// drawing its prompt: prompt hooks (`mise activate`, say) run commands of
/// their own first, and a line typed while they run can be lost.
fn wait_for_shell(pane: &str) -> Result<()> {
    let t = Instant::now();
    while !current_command(pane).is_some_and(|c| is_shell(&c)) {
        if t.elapsed() > Duration::from_secs(5) {
            bail!("the pane's shell didn't come back after the old session stopped");
        }
        sleep(Duration::from_millis(100));
    }
    let t = Instant::now();
    let mut last = screen_from(pane, None);
    while t.elapsed() < Duration::from_secs(3) {
        sleep(Duration::from_millis(300));
        let now = screen_from(pane, None);
        if now == last && current_command(pane).is_some_and(|c| is_shell(&c)) {
            break;
        }
        last = now;
    }
    // Anything left on the shell's line (a stray key) would run with ours.
    tmux::run(&["send-keys", "-t", pane, "C-e", "C-u"])?;
    sleep(Duration::from_millis(100));
    Ok(())
}

/// Where the pane's next output goes (its history plus the cursor's row), so
/// a later look can read only what came after.
fn screen_mark(pane: &str) -> Option<i64> {
    let out = tmux::run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{history_size} #{cursor_y}",
    ])
    .ok()?;
    let mut it = out.split_whitespace().map(|n| n.parse::<i64>().ok());
    Some(it.next()?? + it.next()??)
}

/// The pane's text: the visible screen, or everything from a mark on.
fn screen_from(pane: &str, mark: Option<i64>) -> String {
    let start = mark.and_then(|m| {
        let hist = tmux::run(&["display-message", "-p", "-t", pane, "#{history_size}"])
            .ok()?
            .trim()
            .parse::<i64>()
            .ok()?;
        Some((m - hist).to_string())
    });
    let mut args = vec!["capture-pane", "-p", "-J", "-t", pane];
    if let Some(s) = &start {
        args.extend(["-S", s]);
    }
    tmux::run(&args).unwrap_or_default()
}

/// The end of a screen, plain and bounded, for the handover log.
fn screen_tail(screen: &str) -> String {
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(30)..].join("\n");
    let skip = tail.chars().count().saturating_sub(4000);
    tail.chars().skip(skip).collect()
}

/// What a freshly started Claude did.
#[derive(Debug, PartialEq)]
enum Started {
    /// It shows its prompt.
    Ready,
    /// It is running but stopped at a screen that needs the person.
    Blocked(String),
    /// It never came up, or exited: what the pane showed.
    Gone(String),
}

/// Screens a fresh Claude can stop at before its prompt, each needing the
/// person, and what to say about it.
fn blocking_screen(screen: &str) -> Option<&'static str> {
    const SCREENS: &[(&str, &str)] = &[
        (
            "Is this a project you created or one you trust",
            "it's asking you to trust the folder on this account",
        ),
        (
            "Do you trust the files in this folder",
            "it's asking you to trust the folder on this account",
        ),
        (
            "Choose the text style that looks best with your terminal",
            "it's waiting at Claude's first-run theme picker",
        ),
        (
            "Select login method",
            "it's asking you to sign in to this account",
        ),
        (
            "Bypass Permissions mode",
            "it's asking you to accept bypass-permissions mode",
        ),
        (
            "Do you want to use this API key",
            "it's asking whether to use an API key",
        ),
        (
            "Press Enter to continue",
            "it's waiting at one of Claude's first-run screens",
        ),
    ];
    SCREENS
        .iter()
        .find(|(text, _)| screen.contains(text))
        .map(|(_, note)| *note)
}

/// Claude's normal input box: the prompt mark between its rules.
fn shows_prompt(screen: &str) -> bool {
    screen.contains('❯') && screen.contains("──")
}

/// Watch a just-started Claude until it shows its prompt, stops at a screen
/// that needs the person, or `gone` says it isn't running.
fn await_start(pane: &str, mark: Option<i64>, gone: &dyn Fn() -> bool, wait: Duration) -> Started {
    let t = Instant::now();
    loop {
        sleep(Duration::from_millis(250));
        let screen = screen_from(pane, mark);
        if gone() {
            // tmux pushes a dead pane's last output off its screen when it
            // says the pane is dead: look back a little further.
            let last = match mark {
                Some(_) => screen_from(pane, mark),
                None => tmux::run(&["capture-pane", "-p", "-J", "-t", pane, "-S", "-40"])
                    .unwrap_or_default(),
            };
            return Started::Gone(screen_tail(&last));
        }
        if let Some(note) = blocking_screen(&screen) {
            return Started::Blocked(format!("{note}: finish it in its pane"));
        }
        if shows_prompt(&screen) {
            return Started::Ready;
        }
        if t.elapsed() > wait {
            return Started::Blocked("it hasn't shown Claude's prompt yet: check its pane".into());
        }
    }
}

/// What the shell prints when a launch typed into it fails. Typed split by
/// quotes, so the echoed command line itself never reads as this.
const LAUNCH_FAILED: &str = "toomux:launch-failed";

/// Type a launch into the pane's shell. A leading space keeps it out of the
/// shell's history (ignorespace). If it fails (the folder is gone, Claude
/// exits with an error) the shell says so, so that is seen at once.
fn type_launch(pane: &str, cwd: &str, launch: &Launch) -> Result<()> {
    let line = format!(
        " cd {} && env {} {} || echo 'toomux:''launch-failed'",
        shell_words::quote(cwd),
        launch
            .env
            .iter()
            .map(|e| shell_words::quote(e).into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        launch.cmd
    );
    tmux::run(&["send-keys", "-t", pane, "-l", &line])?;
    sleep(Duration::from_millis(150));
    tmux::run(&["send-keys", "-t", pane, "Enter"])?;
    Ok(())
}

/// Start a launch from the pane's shell and watch it come up.
fn start_from_shell(pane: &str, cwd: &str, launch: &Launch, wait: Duration) -> Result<Started> {
    wait_for_shell(pane)?;
    let mark = screen_mark(pane);
    type_launch(pane, cwd, launch)?;
    // First the shell has to hand the terminal over: a launch that exits at
    // once looks like the shell never leaving the foreground.
    let t = Instant::now();
    while current_command(pane).is_some_and(|c| is_shell(&c)) {
        let screen = screen_from(pane, mark);
        if screen.contains(LAUNCH_FAILED) || t.elapsed() > Duration::from_secs(15) {
            return Ok(Started::Gone(screen_tail(&screen)));
        }
        sleep(Duration::from_millis(100));
    }
    let gone = || {
        current_command(pane).is_none_or(|c| is_shell(&c))
            || screen_from(pane, mark).contains(LAUNCH_FAILED)
    };
    Ok(await_start(pane, mark, &gone, wait))
}

fn relaunch_from_shell(
    cfg: &Config,
    s: &Session,
    pane: &str,
    account: usize,
    prompt: &str,
) -> Result<Option<String>> {
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
    match start_from_shell(pane, &s.cwd, &launch, START_WAIT)? {
        Started::Ready => Ok(None),
        Started::Blocked(note) => Ok(Some(note)),
        Started::Gone(screen) => {
            // Never leave a bare shell where the conversation was: resume the
            // old one, and say whether that worked.
            let back = start_from_shell(
                pane,
                &s.cwd,
                &launch_with(cfg, &plain, account, None),
                Duration::from_secs(10),
            )
            .is_ok_and(|b| !matches!(b, Started::Gone(_)));
            Err(LaunchFailed {
                msg: if back {
                    "the fresh session wouldn't start from the pane's shell, so the old conversation is back in its pane"
                        .into()
                } else {
                    format!(
                        "the fresh session wouldn't start from the pane's shell, and neither would the old \
                         conversation: resume it there with claude --resume {}",
                        s.id
                    )
                },
                screen,
            }
            .into())
        }
    }
}

/// A session outside tmux can't be restarted where it runs: stop it and start
/// the fresh conversation in a tmux window of its own (same account, same
/// flags). Returns the new pane.
pub fn relaunch_fresh_in_tmux(cfg: &Config, s: &Session, prompt: &str) -> Result<String> {
    ready_account(cfg, &session_dir(cfg, s), &s.cwd, false);
    let account = s.account.context("its account isn't known")?;
    let launch = launch_with(cfg, s, account, Some(prompt));
    let name = window_name(&s.title);
    stop(s)?;
    match in_own_server(Some(&s.id), &name, &s.cwd, &launch, Duration::from_secs(4)) {
        Ok(pane) => Ok(pane),
        Err(e) => {
            // It died at once: bring the old conversation back instead.
            let back = launch_for(cfg, s, account);
            let back = in_own_server(Some(&s.id), &name, &s.cwd, &back, RESUME_WAIT)?;
            bail!(
                "the fresh session wouldn't start ({e}), so the old conversation is back in tmux ({back})"
            )
        }
    }
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
    ready_account(cfg, &launch_dir(cfg, s.account), &s.cwd, false);
    let account = s
        .account
        .context("the account it last ran on isn't configured")?;
    if !std::path::Path::new(&s.cwd).is_dir() {
        bail!("its folder {} no longer exists", s.cwd);
    }
    // Two processes on one conversation both write its transcript.
    crate::running::ensure_free(cfg, &s.id, None)?;
    let launch = launch_for(cfg, s, account);
    // Under the name it had before a restart, if someone chose one.
    let name = s
        .restore
        .clone()
        .and_then(|(_, named)| named)
        .unwrap_or_else(|| window_name(&s.title));
    in_own_server(Some(&s.id), &name, &s.cwd, &launch, RESUME_WAIT)
}

/// How long a new session must stay up to count as started. Claude exits
/// within a second or two when it can't open what it was asked to (no such
/// conversation on this account, a bad flag).
const STAY_UP_WAIT: Duration = Duration::from_millis(1000);
const RESUME_WAIT: Duration = Duration::from_millis(2500);

/// Every session gets a tmux server of its own, so one server going down
/// (a stray `kill-server`, a crash) takes only that session with it.
/// Returns the qualified pane id, once the launch has stayed up `within`;
/// one that dies is cleared away and what it last printed makes the error.
fn in_own_server(
    id: Option<&str>,
    name: &str,
    cwd: &str,
    launch: &Launch,
    within: Duration,
) -> Result<String> {
    let server = tmux::new_server_name(id);
    let pane = tmux::new_server(&server, name, cwd, &launch.env, &launch.cmd)?;
    // The new pane is held when its process exits; let go once it's up.
    let held = survives(&pane, within, None).and_then(|()| {
        tmux::run(&["set-option", "-p", "-u", "-t", &pane, "remain-on-exit"])?;
        survives(&pane, Duration::ZERO, None)
    });
    if let Err(e) = held {
        let _ = tmux::run_on(&server, &["kill-server"]);
        return Err(e);
    }
    Ok(pane)
}

/// Ok if the process in `pane` (held on exit) is still running after
/// `within`; else an error with the last of what it printed (since `mark`,
/// for a pane that has shown something before).
fn survives(pane: &str, within: Duration, mark: Option<i64>) -> Result<()> {
    let t = Instant::now();
    loop {
        if tmux::pane_dead(pane) != Some(false) {
            return match last_words(pane, mark) {
                Some(said) => Err(anyhow::anyhow!("it exited at once: {said}")),
                None => Err(anyhow::anyhow!("it exited at once")),
            };
        }
        if t.elapsed() >= within {
            return Ok(());
        }
        sleep(Duration::from_millis(200));
    }
}

/// Where a pane's output now reaches, as a line of its history. Taken just
/// after a respawn (which clears the screen and homes the cursor), it tells
/// what the relaunch prints from what the pane showed before.
fn output_mark(pane: &str) -> Option<i64> {
    let at = tmux::run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{history_size} #{cursor_y}",
    ])
    .ok()?;
    let mut n = at.split_whitespace().map(|v| v.parse::<i64>().ok());
    Some(n.next()?? + n.next()??)
}

/// The last few lines a dead pane's process printed, on one line.
fn last_words(pane: &str, mark: Option<i64>) -> Option<String> {
    // capture-pane counts from the top of the screen, history above it.
    let history = || {
        tmux::run(&["display-message", "-p", "-t", pane, "#{history_size}"])
            .ok()?
            .trim()
            .parse::<i64>()
            .ok()
    };
    let start = match mark {
        Some(m) => (m - history()?).max(-60),
        None => -60,
    }
    .to_string();
    let screen = tmux::run(&["capture-pane", "-p", "-J", "-t", pane, "-S", &start]).ok()?;
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("Pane is dead"))
        .collect();
    let said = lines[lines.len().saturating_sub(3)..].join(" · ");
    (!said.is_empty()).then(|| said.chars().take(300).collect())
}

/// Start a new Claude session in `folder` on `account`, in a tmux server of
/// its own. Returns the pane id.
pub fn start(cfg: &Config, folder: &str, account: usize, args: &[String]) -> Result<String> {
    if !std::path::Path::new(folder).is_dir() {
        bail!("{folder} isn't a folder");
    }
    // The person picked this folder to work in: that's their trust.
    ready_account(cfg, &launch_dir(cfg, Some(account)), folder, true);
    let mut argv = interactive_env_argv();
    for v in registry::RUNTIME_VARS {
        argv.extend(["-u".to_string(), v.to_string()]);
    }
    // Name the new conversation up front, so it's known even before (or
    // without) Claude's own record of it.
    let mut args = args.to_vec();
    let named = crate::running::id_in_args(&[&["claude".to_string()], args.as_slice()].concat());
    let id = match named {
        Some(id) => Some(id),
        None if !selects_conversation(&args) => new_session_id().inspect(|id| {
            args.extend(["--session-id".to_string(), id.clone()]);
        }),
        None => None,
    };
    let mut env = vec![format!(
        "{}={}",
        crate::running::ID_ENV,
        id.unwrap_or_else(launch_marker)
    )];
    config_dir(cfg, account, &mut env, &mut argv);
    argv.push(expand(&cfg.claude_bin).display().to_string());
    let prepared =
        crate::context_policy::prepare_launch(cfg, account, Path::new(folder), &args, &[], None);
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
    in_own_server(None, &name, folder, &Launch { env, cmd }, STAY_UP_WAIT)
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
    in_own_server(None, &name, folder, &Launch { env, cmd }, STAY_UP_WAIT)
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
    // A dialog's selected option ("❯ 1. Yes") isn't the input box.
    if is_option(&strip_ansi(lines[start]))
        && lines
            .iter()
            .skip(start + 1)
            .take(6)
            .any(|l| is_option(&strip_ansi(l)))
    {
        return None;
    }
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

/// A numbered option row, as dialogs draw them ("❯ 1. Yes", "  2. No").
fn is_option(plain: &str) -> bool {
    let rest = plain.trim_start().trim_start_matches('❯').trim_start();
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(". ")
}

/// Only Claude's placeholder ("Try \"…\"") or nothing: no one's text.
fn placeholder(typed: &str) -> bool {
    typed.is_empty() || typed.starts_with("Try \"")
}

/// What Claude's input box in a pane holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Prompt {
    /// Nothing typed: only a placeholder or a dim suggestion.
    Empty,
    /// Text in normal intensity: an unsent draft (someone's, or toomux's own).
    Draft(String),
    /// It can't be read: scrolled back (copy mode), a dialog in the way, no
    /// prompt on screen, or tmux failing. Not the same as someone typing.
    Unknown,
}

pub fn prompt_state(pane: &str) -> Prompt {
    match prompt_draft(pane) {
        None => Prompt::Unknown,
        Some(typed) if placeholder(&typed) => Prompt::Empty,
        Some(typed) => Prompt::Draft(typed),
    }
}

/// Whether Claude's input box in this pane is empty (only the placeholder),
/// so typing a command into it can't mix with an unsent draft.
pub fn prompt_empty(pane: &str) -> bool {
    prompt_state(pane) == Prompt::Empty
}

/// How a prompt was cleared.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cleared {
    /// The input box reads empty now.
    Emptied,
    /// Editing keys didn't change the text at all: with unstyled output
    /// (NO_COLOR) that is a suggestion drawn like typed text, not a draft.
    NotEditable,
}

/// Clear exactly the draft we just preserved for a handover. If the prompt
/// changed between inspection and clearing, leave it alone: that is active
/// user input, not the stale draft we were asked to migrate.
/// `unstyled`: Claude draws without attributes here, so a suggestion can't be
/// told from typed text by its looks (see [`Cleared::NotEditable`]).
pub fn clear_prompt_if(pane: &str, expected: &str, unstyled: bool) -> Result<Cleared> {
    let current = prompt_draft(pane).context("can't read Claude's prompt before clearing it")?;
    if current != expected {
        bail!("something is typed in its prompt: it changed while handing over");
    }
    clear_prompt(pane, unstyled)
}

/// Keys that wipe Claude's input box from its end: kill each row back to
/// its start and join it to the one above, for every row it may have (soft
/// wraps included). Extra presses on an empty box do nothing.
fn clearing_keys(draft: &str) -> Vec<&'static str> {
    let rows = draft
        .lines()
        .map(|l| l.chars().count() / 40 + 1)
        .sum::<usize>()
        .clamp(1, 60);
    let mut keys = vec!["C-e"];
    for _ in 0..=rows {
        keys.extend(["C-u", "BSpace"]);
    }
    keys
}

/// Empty Claude's input box, verified by reading it back. Claude's ctrl-u
/// only clears back to the start of the cursor's row, so one press leaves the
/// rest of a multi-line (or wrapped) draft behind.
pub fn clear_prompt(pane: &str, unstyled: bool) -> Result<Cleared> {
    let read = || prompt_draft(pane).context("can't read Claude's prompt while clearing it");
    let first = read()?;
    let mut last = first.clone();
    for _ in 0..4 {
        if placeholder(&last) {
            return Ok(Cleared::Emptied);
        }
        let mut args = vec!["send-keys", "-t", pane];
        args.extend(clearing_keys(&last));
        tmux::run(&args)?;
        let t = Instant::now();
        loop {
            sleep(Duration::from_millis(150));
            let now = read()?;
            if now != last || t.elapsed() > Duration::from_millis(900) {
                last = now;
                break;
            }
        }
    }
    if placeholder(&last) {
        return Ok(Cleared::Emptied);
    }
    if unstyled && last == first {
        return Ok(Cleared::NotEditable);
    }
    bail!(
        "something is typed in its prompt: couldn't clear the preserved draft ({} characters stayed)",
        last.chars().count()
    )
}

/// Type `text` into an empty Claude prompt and submit it, making sure it
/// went: an error if it is still sitting in the input box.
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
    sleep(Duration::from_millis(1200));
    // Unreadable now (a dialog it opened, say) isn't proof it didn't go.
    if matches!(prompt_state(pane), Prompt::Draft(_)) {
        bail!("{NOT_SENT}");
    }
    Ok(())
}

/// What [`type_prompt`] says when Enter didn't send the text.
pub const NOT_SENT: &str = "toomux's text didn't send: it's still in Claude's prompt";

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

/// Claude Code records folder trust under the folder's real path; trust
/// given before (or under a symlinked path the person works in) is under
/// that path. If the account trusts `cwd` but not where it really is, record
/// the same decision for the real path, so a launch there doesn't stop at
/// the trust prompt. Never grants trust the person hasn't already given.
fn trust_real_path(config_dir: &std::path::Path, cwd: &str) {
    let Ok(real) = std::fs::canonicalize(cwd) else {
        return;
    };
    let real = real.display().to_string();
    if real == cwd {
        return;
    }
    let path = crate::credentials::claude_json(config_dir);
    let Some(json) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    else {
        return;
    };
    if trusted(&json, cwd, false) && !trusted(&json, &real, false) {
        let _ = mark_trusted(&path, &real);
    }
}

/// The config folder a session runs with.
fn session_dir(cfg: &Config, s: &Session) -> std::path::PathBuf {
    match s.config_dir.as_deref() {
        Some(d) => expand(d),
        None => launch_dir(cfg, s.account),
    }
}

/// The config folder a launch on `account` (None: Claude's default) uses.
fn launch_dir(cfg: &Config, account: Option<usize>) -> std::path::PathBuf {
    match account.filter(|a| *a < cfg.accounts.len()) {
        Some(a) => cfg.account_dir(a),
        None => crate::config::home().join(".claude"),
    }
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
    mark_trusted(
        &crate::credentials::claude_json(&cfg.account_dir(to)),
        &s.cwd,
    )
}

/// Record in one account's .claude.json that the person trusts `key`.
fn mark_trusted(path: &std::path::Path, key: &str) -> Result<()> {
    edit_claude_json(
        path,
        |v| trusted(v, key, true),
        |v| {
            let projects = v
                .as_object_mut()
                .context("unexpected .claude.json shape")?
                .entry("projects")
                .or_insert_with(|| serde_json::json!({}));
            let entry = projects
                .as_object_mut()
                .context("unexpected projects shape")?
                .entry(key.to_string())
                .or_insert_with(|| serde_json::json!({}));
            if let Some(o) = entry.as_object_mut() {
                o.insert(
                    "hasTrustDialogAccepted".into(),
                    serde_json::Value::Bool(true),
                );
            }
            Ok(())
        },
    )
}

/// Change one account's .claude.json with `apply`, unless `done` already
/// holds. A missing or unreadable file is left alone.
fn edit_claude_json(
    path: &std::path::Path,
    done: impl Fn(&serde_json::Value) -> bool,
    apply: impl Fn(&mut serde_json::Value) -> Result<()>,
) -> Result<()> {
    let read = |path: &std::path::Path| -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    };
    // Live sessions on the target account rewrite this file too, and nothing
    // coordinates writers. So: write only if the file is unchanged since we
    // read it, and check afterwards that the change survived.
    let stamp = |p: &std::path::Path| {
        std::fs::metadata(p)
            .ok()
            .map(|m| (m.len(), m.modified().ok()))
    };
    for _ in 0..6 {
        let before = stamp(path);
        let Some(mut dst) = read(path) else {
            return Ok(());
        };
        if done(&dst) {
            return Ok(());
        }
        apply(&mut dst)?;
        let tmp = path.with_file_name(format!(".claude.json.toomux-{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(&dst)?)?;
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        if stamp(path) != before {
            let _ = std::fs::remove_file(&tmp);
            sleep(Duration::from_millis(150));
            continue;
        }
        std::fs::rename(&tmp, path)?;
        sleep(Duration::from_millis(400));
        if read(path).is_some_and(|v| done(&v)) {
            return Ok(());
        }
    }
    bail!("another session kept rewriting {}", path.display())
}

/// Every .claude.json toomux knows of: each account's, and Claude's own
/// default ones (with and without `CLAUDE_CONFIG_DIR=~/.claude`).
fn known_claude_jsons(cfg: &Config) -> Vec<std::path::PathBuf> {
    let home = crate::config::home();
    let mut out: Vec<std::path::PathBuf> = (0..cfg.accounts.len())
        .map(|i| crate::credentials::claude_json(&cfg.account_dir(i)))
        .chain([
            home.join(".claude.json"),
            home.join(".claude").join(".claude.json"),
        ])
        .collect();
    out.dedup();
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(crate::config::canon(p)));
    out
}

/// What first-run onboarding records once it's done: with these, Claude
/// skips its welcome and theme picker.
const ONBOARDING_KEYS: &[&str] = &["hasCompletedOnboarding", "lastOnboardingVersion", "theme"];

fn onboarded(v: &serde_json::Value) -> bool {
    v.get("hasCompletedOnboarding") == Some(&serde_json::Value::Bool(true))
}

/// Get an account ready for a Claude launch in `cwd`, so it goes straight to
/// its prompt instead of stopping at screens the person has already been
/// through on this machine:
/// - first-run onboarding (the theme picker), when another account of
///   theirs has finished it;
/// - folder trust, when `chosen` (they just picked this folder to work in)
///   or when any of their accounts already trusts it.
///
/// An account with no .claude.json yet (never signed in) is left to Claude's
/// own sign-in screens.
fn ready_account(cfg: &Config, config_dir: &std::path::Path, cwd: &str, chosen: bool) {
    let path = crate::credentials::claude_json(config_dir);
    let canon = crate::config::canon(&path);
    let others: Vec<_> = known_claude_jsons(cfg)
        .into_iter()
        .filter(|p| crate::config::canon(p) != canon)
        .collect();
    ready_claude_json(&path, &others, cwd, chosen);
    trust_real_path(config_dir, cwd);
}

/// [`ready_account`] on one .claude.json, given the person's others.
fn ready_claude_json(
    path: &std::path::Path,
    others: &[std::path::PathBuf],
    cwd: &str,
    chosen: bool,
) {
    let read = |p: &std::path::Path| -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
    };
    let Some(target) = read(path) else {
        return;
    };
    let others: Vec<serde_json::Value> = others.iter().filter_map(|p| read(p)).collect();
    if !onboarded(&target)
        && let Some(src) = others.iter().find(|v| onboarded(v))
    {
        let _ = edit_claude_json(path, onboarded, |v| {
            let o = v.as_object_mut().context("unexpected .claude.json shape")?;
            for k in ONBOARDING_KEYS {
                match src.get(*k) {
                    Some(val) if !val.is_null() => {
                        o.insert((*k).to_string(), val.clone());
                    }
                    _ => {}
                }
            }
            Ok(())
        });
    }
    let real = std::fs::canonicalize(cwd)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| cwd.to_string());
    let elsewhere = || {
        others
            .iter()
            .any(|v| trusted(v, cwd, false) || trusted(v, &real, false))
    };
    if !trusted(&target, cwd, false) && (chosen || elsewhere()) {
        let _ = mark_trusted(path, cwd);
    }
    if !trusted(&target, &real, false) && (chosen || elsewhere()) {
        let _ = mark_trusted(path, &real);
    }
}

/// A session just started in `pane` that stopped at a screen that needs the
/// person (folder trust, sign-in, first-run), named; None once it shows its
/// prompt. Waits up to a few seconds.
pub fn needs_person(pane: &str) -> Option<String> {
    settle(pane)
}

/// After a relaunch, report whether the new session is stuck at a prompt that
/// needs the person (folder trust, first-run screens: see `blocking_screen`).
fn settle(pane: &str) -> Option<String> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(8) {
        sleep(Duration::from_millis(500));
        let screen = String::from_utf8_lossy(&tmux::capture(pane, 60)?).to_string();
        if let Some(note) = blocking_screen(&screen) {
            return Some(note.into());
        }
        if shows_prompt(&screen) {
            return None;
        }
    }
    None
}

/// The launch that puts a session back as it was: on its own account, or on
/// the config folder it ran with when that isn't one of the accounts
/// (`policy` then names the account whose context policy applies).
fn launch_back(cfg: &Config, s: &Session, policy: usize) -> Launch {
    match s.account {
        Some(a) => launch_for(cfg, s, a),
        None => launch_in(cfg, s, policy, s.config_dir.clone(), None),
    }
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
    // Open in another process too: moving this one would leave two.
    crate::running::ensure_free(cfg, &s.id, Some(s.pid))?;
    let launch = launch_for(cfg, &s, account);
    // If the target can't resume it, it goes back exactly as it ran.
    let back = launch_back(cfg, &s, account);
    let target = &cfg.accounts[account].name;
    let from = match (s.account, s.config_dir.as_deref()) {
        (Some(_), _) => s.account_name(cfg).to_string(),
        (None, Some(dir)) => crate::config::tilde(dir),
        (None, None) => "~/.claude".to_string(),
    };
    carry_trust(cfg, &s, account).context("copying folder trust")?;
    ready_account(cfg, &cfg.account_dir(account), &s.cwd, false);
    carry_conversation(cfg, &s, account)
        .with_context(|| format!("copying the conversation to {target}"))?;

    let Some(pane) = s.pane.clone() else {
        stop(&s)?;
        let name = window_name(&s.title);
        // What it wrote as it stopped goes across too.
        let moved = carry_conversation(cfg, &s, account)
            .and_then(|()| in_own_server(Some(&s.id), &name, &s.cwd, &launch, RESUME_WAIT));
        return match moved {
            Ok(id) => Ok(format!("{} resumed on {target} in tmux ({id})", s.title)),
            Err(e) => {
                let id = in_own_server(Some(&s.id), &name, &s.cwd, &back, RESUME_WAIT)
                    .with_context(|| {
                        format!("it wouldn't resume on {target} ({e}), nor again on {from}")
                    })?;
                bail!("it wouldn't resume on {target} ({e}) · it's back on {from} in tmux ({id})")
            }
        };
    };

    // Keep the pane alive across the restart so it holds its place in the
    // layout, and so a launch that dies can be read and replaced.
    tmux::run(&["set-option", "-p", "-t", &pane.id, "remain-on-exit", "on"])?;
    let result = (|| {
        stop(&s)?;
        let t = Instant::now();
        while tmux::pane_dead(&pane.id) == Some(false) && t.elapsed() < Duration::from_secs(5) {
            sleep(Duration::from_millis(100));
        }
        let moved = carry_conversation(cfg, &s, account).and_then(|()| {
            respawn(&pane.id, &s.cwd, &launch)?;
            let mark = output_mark(&pane.id);
            survives(&pane.id, RESUME_WAIT, mark)
        });
        if let Err(e) = moved {
            respawn(&pane.id, &s.cwd, &back)?;
            let mark = output_mark(&pane.id);
            survives(&pane.id, RESUME_WAIT, mark).with_context(|| {
                format!("it wouldn't resume on {target} ({e}), nor again on {from}")
            })?;
            bail!("it wouldn't resume on {target} ({e}) · it's back on {from}");
        }
        Ok(())
    })();
    let _ = tmux::run(&["set-option", "-p", "-u", "-t", &pane.id, "remain-on-exit"]);
    result?;
    Ok(match settle(&pane.id) {
        Some(note) => format!("{} moved to {target} · {note}", s.title),
        None => format!("{} moved to {target}", s.title),
    })
}

/// Conversations are kept under the config folder's `projects/`. Accounts
/// in one group share that; to any other account the conversation is copied
/// before it moves, or `--resume` there finds nothing. It's only ever
/// copied: the source is left as it is, and a different file already at the
/// target is never replaced (one the source merely extends is brought up to
/// date, as after an earlier move back and forth).
fn carry_conversation(cfg: &Config, s: &Session, to: usize) -> Result<()> {
    let Some(src) = s.transcript(cfg) else {
        return Ok(());
    };
    let dir = cfg
        .account_dir(to)
        .join("projects")
        .join(registry::project_dir_name(&s.cwd));
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    if src
        .parent()
        .and_then(canon)
        .is_some_and(|d| Some(d) == canon(&dir))
    {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    copy_extending(&src, &dir.join(format!("{}.jsonl", s.id)))?;
    // Its subagents' transcripts and kept tool output, as far as they go.
    let extra = src.with_extension("");
    if extra.is_dir() {
        copy_tree(&extra, &dir.join(&s.id))?;
    }
    Ok(())
}

/// Bring `dst` up to `src`: a new copy, or the rest of a file `src` extends.
/// Anything else at `dst` belongs to someone and stays as it is.
fn copy_extending(src: &Path, dst: &Path) -> Result<()> {
    let have = match std::fs::symlink_metadata(dst) {
        Ok(m) if !m.file_type().is_file() => {
            bail!("{} isn't a plain file, so it's left alone", dst.display())
        }
        Ok(m) => Some(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(len) = have {
        if !starts_with(src, dst, len)? {
            bail!(
                "{} already holds a different copy of this conversation, so it's left alone",
                dst.display()
            );
        }
        if std::fs::metadata(src)?.len() == len {
            return Ok(());
        }
    }
    let name = dst.file_name().context("no file name")?.to_string_lossy();
    let tmp = dst.with_file_name(format!(".{name}.toomux-{}", std::process::id()));
    let copied = std::fs::copy(src, &tmp).and_then(|_| std::fs::rename(&tmp, dst));
    if copied.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(copied?)
}

/// Whether the first `len` bytes of `src` are all of `prefix`.
fn starts_with(src: &Path, prefix: &Path, len: u64) -> Result<bool> {
    use std::io::Read;
    if std::fs::metadata(src)?.len() < len {
        return Ok(false);
    }
    let mut a = std::fs::File::open(src)?.take(len);
    let mut b = std::fs::File::open(prefix)?.take(len);
    let (mut x, mut y) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let n = a.read(&mut x)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
    }
}

/// Copy a folder's files across with `copy_extending`. Links aren't
/// followed, and a file that differs at the target keeps its own contents.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if std::fs::symlink_metadata(dst).is_ok_and(|m| !m.is_dir()) {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let Ok(kind) = e.file_type() else { continue };
        let to = dst.join(e.file_name());
        if kind.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else if kind.is_file() {
            let _ = copy_extending(&e.path(), &to);
        }
    }
    Ok(())
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
    let Some(account) = cur.account else {
        let dir = cur
            .config_dir
            .as_deref()
            .map(crate::config::tilde)
            .unwrap_or_else(|| "~/.claude".into());
        bail!(
            "{} runs on {dir}, which isn't one of toomux's accounts · `toomux account add <name> --dir {dir}` brings it in, then this can too",
            cur.title
        )
    };
    let launch = launch_for(cfg, &cur, account);
    let id = place_in_tmux(cfg, &cur, &launch)?;
    Ok(format!("{} is now in tmux ({id})", cur.title))
}

/// Stop a session and resume it in a tmux server of its own, unless its
/// conversation is open in another process too.
fn place_in_tmux(cfg: &Config, s: &Session, launch: &Launch) -> Result<String> {
    crate::running::ensure_free(cfg, &s.id, Some(s.pid))?;
    stop(s)?;
    in_own_server(
        Some(&s.id),
        &window_name(&s.title),
        &s.cwd,
        launch,
        RESUME_WAIT,
    )
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
    use crate::config::{Account, Config};
    use crate::registry::Session;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    const ID: &str = "0f0f0f0f-1234-4abc-8def-000000000003";

    fn cfg(dirs: &[&str]) -> Config {
        let mut c = Config::default();
        c.accounts = dirs
            .iter()
            .enumerate()
            .map(|(i, d)| Account {
                name: format!("acct{i}"),
                config_dir: d.to_string(),
            })
            .collect();
        c
    }

    fn session(account: Option<usize>, config_dir: Option<&str>) -> Session {
        Session {
            pid: 1,
            proc_start: None,
            id: ID.into(),
            cwd: "/w/x".into(),
            name: "s".into(),
            title: "s".into(),
            topic: None,
            pr: None,
            queued: None,
            pin: None,
            dormant: false,
            restore: None,
            state: crate::registry::State::Idle,
            waiting_for: None,
            limit: None,
            handover: None,
            since_ms: 0,
            started_ms: 0,
            account,
            config_dir: config_dir.map(str::to_string),
            args: v(&["claude", "--model", "opus"]),
            env: vec![],
            tty: None,
            pane: None,
            unregistered: false,
            twin: None,
            parked: None,
        }
    }

    fn provider_ids(env: &[String]) -> Vec<&str> {
        env.iter()
            .filter_map(|e| e.strip_prefix("TOOMUX_PROVIDER_SESSION_ID="))
            .collect()
    }

    #[test]
    fn a_resume_names_its_own_conversation_never_an_inherited_one() {
        let cfg = cfg(&["/nonexistent/a", "/nonexistent/b"]);
        let mut s = session(Some(0), None);
        // A handover successor inherits its predecessor's environment.
        s.env = vec![(
            "TOOMUX_PROVIDER_SESSION_ID".into(),
            "11111111-2222-4333-8444-555555555555".into(),
        )];
        let l = super::launch_for(&cfg, &s, 1);
        assert_eq!(provider_ids(&l.env), vec![ID]);
        assert!(l.cmd.ends_with(&format!("--resume {ID}")));
    }

    #[test]
    fn a_fresh_launch_names_the_new_conversation_it_opens() {
        let cfg = cfg(&["/nonexistent/a"]);
        let mut s = session(Some(0), None);
        s.env = vec![("TOOMUX_PROVIDER_SESSION_ID".into(), ID.into())];
        let l = super::launch_with(&cfg, &s, 0, Some("carry on"));
        let ids = provider_ids(&l.env);
        assert_eq!(ids.len(), 1);
        let new = ids[0];
        assert_ne!(new, ID, "not the predecessor's");
        assert!(l.cmd.contains(&format!("--session-id {new}")));
        assert!(!l.cmd.contains("--resume"));
        let parts: Vec<usize> = new.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
        assert_eq!(new.as_bytes()[14], b'4', "a version 4 UUID");
        assert_eq!(
            crate::running::id_in_args(&shell_words::split(&l.cmd).unwrap()).as_deref(),
            Some(new)
        );
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
    fn a_dialog_option_is_not_a_draft_in_the_prompt() {
        let dialog = concat!(
            "Do you want to make this edit?\n",
            "\u{1b}[36m❯ 1. Yes\u{1b}[39m\n",
            "  2. Yes, allow all edits this session\n",
            "  3. No, and tell Claude what to do differently\n",
        );
        assert_eq!(
            super::prompt_draft_in(dialog),
            None,
            "unreadable, not typing"
        );
        assert_eq!(
            super::prompt_draft_in("❯ 1. fix the thing\n──────\n").as_deref(),
            Some("1. fix the thing"),
            "a numbered draft without options beneath it is still a draft"
        );
    }

    #[test]
    fn first_run_screens_that_need_the_person_are_named() {
        use super::blocking_screen;
        let theme = "Let's get started.\n\nChoose the text style that looks best with your terminal\n❯ 1. Dark mode ✔\n";
        assert!(blocking_screen(theme).unwrap().contains("theme picker"));
        assert!(
            blocking_screen("Quick safety check: Is this a project you created or one you trust?")
                .unwrap()
                .contains("trust")
        );
        assert_eq!(blocking_screen("──────\n❯ \n──────\n"), None);
    }

    #[test]
    fn clearing_covers_every_row_of_a_draft() {
        let keys = super::clearing_keys("one\ntwo\nthree");
        assert_eq!(keys[0], "C-e");
        assert!(
            keys.iter().filter(|k| **k == "BSpace").count() >= 3,
            "each row is joined to the one above: {keys:?}"
        );
        let long = "x".repeat(200);
        assert!(super::clearing_keys(&long).len() > 2 * (200 / 40));
    }

    /// A private tmux server for one test (its own socket, no user
    /// configuration), with a folder for fake programs. None without tmux.
    struct Rig {
        dir: std::path::PathBuf,
        sock: String,
    }

    impl Rig {
        fn new(name: &str) -> Option<Rig> {
            let dir = std::env::temp_dir().join(format!("tmx-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            let sock = dir.join("sock").display().to_string();
            let ok = crate::tmux::command()
                .args(["-V"])
                .output()
                .is_ok_and(|o| o.status.success());
            if !ok {
                eprintln!("tmux isn't available: skipped");
                return None;
            }
            Some(Rig { dir, sock })
        }

        /// A pane running `cmd`, qualified with this rig's server.
        fn pane(&self, cmd: &str) -> String {
            let out = crate::tmux::command()
                .args(["-S", &self.sock, "-f", "/dev/null", "new-session", "-d"])
                .args(["-x", "120", "-y", "40", "-P", "-F", "#{pane_id}", cmd])
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .env_remove("NO_COLOR")
                .env("PS1", "$ ")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            crate::tmux::qualify(&self.sock, String::from_utf8_lossy(&out.stdout).trim())
        }

        /// An executable script in the rig's bin folder.
        fn script(&self, name: &str, body: &str) -> String {
            use std::os::unix::fs::PermissionsExt;
            let p = self.dir.join("bin").join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p.display().to_string()
        }

        fn screen(&self, pane: &str) -> String {
            super::screen_from(pane, None)
        }

        fn wait_for(&self, pane: &str, what: &str) -> bool {
            let t = std::time::Instant::now();
            while t.elapsed() < std::time::Duration::from_secs(10) {
                if self.screen(pane).contains(what) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            false
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = crate::tmux::command()
                .args(["-S", &self.sock, "kill-server"])
                .output();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A stand-in for Claude: with --resume it shows an input box and stays
    /// (as a process that isn't named like a shell, as Claude's isn't);
    /// `fresh` says what it does when started with a prompt instead.
    fn fake_claude(rig: &Rig, fresh: &str) -> String {
        let pid = rig.dir.join("pid").display().to_string();
        rig.script(
            "claude",
            &format!(
                "case \" $* \" in\n\
                 *' --resume '*) echo $$ > {pid}; printf '──────\\n❯ '; exec sleep 600 ;;\n\
                 *) {fresh} ;;\n\
                 esac\n"
            ),
        )
    }

    fn session_in(rig: &Rig, pane: &str) -> crate::registry::Session {
        let t = std::time::Instant::now();
        let pid_file = rig.dir.join("pid");
        let pid = loop {
            if let Some(p) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|p| p.trim().parse::<i32>().ok())
            {
                break p;
            }
            assert!(t.elapsed().as_secs() < 10, "the fake claude never started");
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let _ = std::fs::remove_file(&pid_file);
        crate::registry::Session {
            pid,
            proc_start: None,
            id: "0ld-c0nversation".into(),
            cwd: rig.dir.display().to_string(),
            name: "s".into(),
            title: "s".into(),
            topic: None,
            pr: None,
            queued: None,
            pin: None,
            dormant: false,
            restore: None,
            state: crate::registry::State::Idle,
            waiting_for: None,
            limit: None,
            handover: None,
            since_ms: 0,
            started_ms: 0,
            account: Some(0),
            config_dir: None,
            args: vec!["claude".into()],
            env: vec![],
            unregistered: false,
            twin: None,
            parked: None,
            tty: None,
            pane: Some(crate::tmux::Pane {
                id: pane.to_string(),
                session: "0".into(),
                window_index: "0".into(),
            }),
        }
    }

    fn cfg_for(rig: &Rig, claude: &str) -> crate::config::Config {
        let mut cfg = crate::config::Config::default();
        cfg.claude_bin = claude.to_string();
        let acct = rig.dir.join("acct");
        std::fs::create_dir_all(&acct).unwrap();
        cfg.accounts = vec![crate::config::Account {
            name: "t".into(),
            config_dir: acct.display().to_string(),
        }];
        cfg
    }

    #[test]
    fn toomux_text_that_stays_in_the_prompt_is_an_error_not_a_success() {
        let Some(rig) = Rig::new("unsent") else {
            return;
        };
        // An input box that never takes what's typed into it.
        let stuck = rig.script(
            "stuck",
            "printf '──────\\n❯ '; stty -icanon; exec sleep 600\n",
        );
        let pane = rig.pane(&stuck);
        assert!(rig.wait_for(&pane, "❯"));
        let e = super::type_prompt(&pane, "[toomux handover] write your brief").unwrap_err();
        assert!(e.to_string().starts_with(super::NOT_SENT), "{e}");
    }

    #[test]
    fn clearing_a_draft_is_checked_by_reading_the_prompt_back() {
        let Some(rig) = Rig::new("clear") else {
            return;
        };
        let prompt = rig.script("prompt", "printf '──────\\n❯ '; exec sleep 600\n");
        let pane = rig.pane(&prompt);
        assert!(rig.wait_for(&pane, "❯"));
        crate::tmux::run(&["send-keys", "-t", &pane, "-l", "half a thought"]).unwrap();
        assert!(rig.wait_for(&pane, "half a thought"));
        assert_eq!(
            super::prompt_state(&pane),
            super::Prompt::Draft("half a thought".into())
        );
        assert!(
            super::clear_prompt_if(&pane, "something else", false).is_err(),
            "a prompt that changed is left alone"
        );
        assert_eq!(
            super::clear_prompt_if(&pane, "half a thought", false).unwrap(),
            super::Cleared::Emptied
        );
        assert_eq!(super::prompt_state(&pane), super::Prompt::Empty);
        // Copy mode: the prompt can't be read, which is not someone typing.
        crate::tmux::run(&["copy-mode", "-t", &pane]).unwrap();
        assert_eq!(super::prompt_state(&pane), super::Prompt::Unknown);
    }

    #[test]
    fn a_fresh_session_that_wont_start_from_the_shell_puts_the_old_one_back() {
        let Some(rig) = Rig::new("shell") else {
            return;
        };
        let claude = fake_claude(&rig, "echo 'claude: fake start-up failure'; exit 1");
        let pane = rig.pane("sh");
        assert!(rig.wait_for(&pane, "$"));
        crate::tmux::run(&[
            "send-keys",
            "-t",
            &pane,
            "-l",
            &format!("{claude} --resume x"),
        ])
        .unwrap();
        crate::tmux::run(&["send-keys", "-t", &pane, "Enter"]).unwrap();
        let s = session_in(&rig, &pane);
        let cfg = cfg_for(&rig, &claude);
        let e = super::relaunch_fresh(&cfg, &s, "Continue from a handover").unwrap_err();
        let failed = e
            .downcast_ref::<super::LaunchFailed>()
            .expect("a launch failure carries its screen");
        assert!(
            failed.msg.contains("old conversation is back"),
            "{}",
            failed.msg
        );
        assert!(
            failed.screen.contains("fake start-up failure"),
            "the log gets what the pane showed: {}",
            failed.screen
        );
        // It's the old conversation in the pane again, not a bare shell.
        session_in(&rig, &pane);
        assert!(!super::is_shell(&super::current_command(&pane).unwrap()));
    }

    #[test]
    fn a_fresh_session_that_dies_at_once_in_its_own_pane_puts_the_old_one_back() {
        let Some(rig) = Rig::new("respawn") else {
            return;
        };
        let claude = fake_claude(&rig, "echo 'claude: bad flag'; exit 1");
        let pane = rig.pane(&format!("{claude} --resume x"));
        let s = session_in(&rig, &pane);
        let cfg = cfg_for(&rig, &claude);
        let e = super::relaunch_fresh(&cfg, &s, "Continue from a handover").unwrap_err();
        let failed = e.downcast_ref::<super::LaunchFailed>().unwrap();
        assert!(
            failed.msg.contains("old conversation is back"),
            "{}",
            failed.msg
        );
        assert!(failed.screen.contains("bad flag"), "{}", failed.screen);
        session_in(&rig, &pane);
        assert_eq!(crate::tmux::pane_dead(&pane), Some(false));
    }

    #[test]
    fn a_fresh_session_at_the_theme_picker_has_started_but_says_it_needs_you() {
        let Some(rig) = Rig::new("theme") else {
            return;
        };
        let claude = fake_claude(
            &rig,
            "printf 'Choose the text style that looks best with your terminal\\n❯ 1. Dark mode\\n  2. Light mode\\n'; exec sleep 600",
        );
        let pane = rig.pane(&format!("{claude} --resume x"));
        let s = session_in(&rig, &pane);
        let cfg = cfg_for(&rig, &claude);
        let note = super::relaunch_fresh(&cfg, &s, "Continue from a handover")
            .unwrap()
            .expect("a blocking screen is reported");
        assert!(note.contains("theme picker"), "{note}");
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
    fn a_handover_starts_with_remote_control_and_bypass_as_configured() {
        let modes = |args: &[&str], remote: &str, was_on: bool, bypass: &str| {
            super::with_launch_modes(v(args), remote, || was_on, bypass)
        };
        // keep: as the old session ran, including Remote Control turned on
        // with /remote-control after it started.
        assert_eq!(
            modes(&["--model", "opus"], "keep", true, "keep"),
            v(&["--model", "opus", "--remote-control"])
        );
        assert_eq!(
            modes(&["--dangerously-skip-permissions"], "keep", false, "keep"),
            v(&["--dangerously-skip-permissions"])
        );
        // on: added once, whatever the old session had; a named Remote
        // Control flag doesn't leave its name behind as a prompt.
        assert_eq!(
            modes(
                &["--remote-control", "phone", "--permission-mode", "auto"],
                "on",
                false,
                "on"
            ),
            v(&["--dangerously-skip-permissions", "--remote-control"])
        );
        assert_eq!(
            modes(&["--permission-mode=bypassPermissions"], "on", false, "on"),
            v(&["--dangerously-skip-permissions", "--remote-control"])
        );
        // off: both go.
        assert_eq!(
            modes(
                &[
                    "--remote-control=phone",
                    "--dangerously-skip-permissions",
                    "--permission-mode",
                    "bypassPermissions",
                    "--model",
                    "opus"
                ],
                "off",
                true,
                "off"
            ),
            v(&["--model", "opus"])
        );
        // off keeps another starting mode.
        assert_eq!(
            modes(&["--permission-mode", "auto"], "off", false, "off"),
            v(&["--permission-mode", "auto"])
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

    #[test]
    fn trust_given_under_a_symlinked_path_carries_to_the_real_path_and_nowhere_else() {
        let root = std::env::temp_dir().join(format!("toomux-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("real");
        for d in ["trusted", "untrusted"] {
            std::fs::create_dir_all(real.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(&real, root.join("link")).unwrap();
        let acct = root.join("acct");
        std::fs::create_dir_all(&acct).unwrap();
        let link = |d: &str| root.join("link").join(d).display().to_string();
        let real_of = |d: &str| {
            std::fs::canonicalize(real.join(d))
                .unwrap()
                .display()
                .to_string()
        };
        let json = acct.join(".claude.json");
        std::fs::write(
            &json,
            serde_json::json!({
                "projects": { link("trusted"): { "hasTrustDialogAccepted": true } },
                "other": 1
            })
            .to_string(),
        )
        .unwrap();
        super::trust_real_path(&acct, &link("trusted"));
        super::trust_real_path(&acct, &link("untrusted"));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert!(super::trusted(&v, &real_of("trusted"), true));
        assert!(
            !super::trusted(&v, &real_of("untrusted"), false),
            "no trust the person hasn't given"
        );
        assert_eq!(v["other"], 1, "the rest of the file is left as it was");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_launch_skips_first_run_screens_the_person_has_been_through_elsewhere() {
        let root = std::env::temp_dir().join(format!("toomux-ready-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["new", "known", "picked", "untouched"] {
            std::fs::create_dir_all(root.join("work").join(d)).unwrap();
        }
        let work = |d: &str| {
            std::fs::canonicalize(root.join("work").join(d))
                .unwrap()
                .display()
                .to_string()
        };
        let write = |name: &str, v: serde_json::Value| {
            let p = root.join(name);
            std::fs::write(&p, v.to_string()).unwrap();
            p
        };
        let read = |p: &std::path::Path| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
        };
        let other = write(
            "other.json",
            serde_json::json!({
                "hasCompletedOnboarding": true,
                "lastOnboardingVersion": "2.1.296",
                "theme": "dark",
                "projects": { work("known"): { "hasTrustDialogAccepted": true } }
            }),
        );
        let target = write(
            "target.json",
            serde_json::json!({ "userID": "keep", "projects": {} }),
        );
        let others = vec![other.clone(), root.join("missing.json")];

        // Handover, voyage, reopen: only what the person already gave.
        super::ready_claude_json(&target, &others, &work("known"), false);
        super::ready_claude_json(&target, &others, &work("untouched"), false);
        // A folder they just picked to start in.
        super::ready_claude_json(&target, &others, &work("picked"), true);

        let v = read(&target);
        assert!(super::onboarded(&v), "onboarding carried over");
        assert_eq!(v["lastOnboardingVersion"], "2.1.296");
        assert_eq!(v["theme"], "dark");
        assert_eq!(
            v["userID"], "keep",
            "the rest of the file is left as it was"
        );
        assert!(
            super::trusted(&v, &work("known"), true),
            "trust another account gave"
        );
        assert!(
            super::trusted(&v, &work("picked"), true),
            "a folder they picked"
        );
        assert!(
            !super::trusted(&v, &work("untouched"), false),
            "no trust the person hasn't given"
        );

        // Nothing to copy from: onboarding stays Claude's to run.
        let lone = write("lone.json", serde_json::json!({ "projects": {} }));
        super::ready_claude_json(&lone, &[], &work("new"), false);
        let v = read(&lone);
        assert!(!super::onboarded(&v) && !super::trusted(&v, &work("new"), false));

        // An account that has never been signed in gets no file made for it.
        let unsigned = root.join("unsigned.json");
        super::ready_claude_json(&unsigned, &others, &work("picked"), true);
        assert!(!unsigned.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_session_goes_back_on_the_config_folder_it_ran_with() {
        let cfg = cfg(&["/nonexistent/a", "/nonexistent/b"]);
        let dir_of = |l: &super::Launch| {
            l.env
                .iter()
                .filter_map(|e| e.strip_prefix("CLAUDE_CONFIG_DIR="))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let known = super::launch_back(&cfg, &session(Some(1), Some("/nonexistent/b")), 0);
        assert_eq!(dir_of(&known), vec!["/nonexistent/b"]);
        // A folder that isn't one of the accounts: that folder, not the target's.
        let own = super::launch_back(&cfg, &session(None, Some("/nonexistent/own")), 0);
        assert_eq!(dir_of(&own), vec!["/nonexistent/own"]);
        // None set: Claude's default, kept unset.
        let default = super::launch_back(&cfg, &session(None, None), 0);
        assert!(dir_of(&default).is_empty() && default.cmd.contains("-u CLAUDE_CONFIG_DIR"));
    }

    fn read(p: &std::path::Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn moving_a_conversation_copies_it_and_never_replaces_a_different_one() {
        let tmp = std::env::temp_dir().join(format!("toomux-carry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (a, b, c) = (tmp.join("a"), tmp.join("b"), tmp.join("c"));
        let src_dir = a.join("projects/-w-x");
        std::fs::create_dir_all(src_dir.join(ID).join("subagents")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&c).unwrap();
        // c shares a's conversations, as accounts in one group do.
        std::os::unix::fs::symlink(a.join("projects"), c.join("projects")).unwrap();
        let src = src_dir.join(format!("{ID}.jsonl"));
        std::fs::write(&src, "one\ntwo\n").unwrap();
        std::fs::write(src_dir.join(ID).join("subagents/agent-1.jsonl"), "sub\n").unwrap();
        std::fs::write(src_dir.join("unrelated.jsonl"), "theirs\n").unwrap();
        let s_ = |p: &std::path::Path| p.display().to_string();
        let cfg = cfg(&[&s_(&a), &s_(&b), &s_(&c)]);
        let s = session(Some(0), Some(&s_(&a)));

        super::carry_conversation(&cfg, &s, 1).unwrap();
        let dst = b.join(format!("projects/-w-x/{ID}.jsonl"));
        assert_eq!(read(&dst), "one\ntwo\n");
        assert_eq!(
            read(&b.join(format!("projects/-w-x/{ID}/subagents/agent-1.jsonl"))),
            "sub\n"
        );
        assert!(
            !b.join("projects/-w-x/unrelated.jsonl").exists(),
            "only this conversation moves"
        );

        // It went on where it was: the copy is brought up to date.
        std::fs::write(&src, "one\ntwo\nthree\n").unwrap();
        super::carry_conversation(&cfg, &s, 1).unwrap();
        assert_eq!(read(&dst), "one\ntwo\nthree\n");

        // A different conversation file at the target is never replaced.
        std::fs::write(&dst, "someone else's\n").unwrap();
        let err = super::carry_conversation(&cfg, &s, 1).unwrap_err();
        assert!(err.to_string().contains("different copy"), "{err}");
        assert_eq!(read(&dst), "someone else's\n");
        // Nor followed through a link.
        std::fs::remove_file(&dst).unwrap();
        std::os::unix::fs::symlink(tmp.join("elsewhere"), &dst).unwrap();
        assert!(super::carry_conversation(&cfg, &s, 1).is_err());
        assert!(!tmp.join("elsewhere").exists());

        // Shared conversations need no copy, and nothing is written.
        let before: Vec<_> = std::fs::read_dir(&src_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        super::carry_conversation(&cfg, &s, 2).unwrap();
        let after: Vec<_> = std::fs::read_dir(&src_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(before.len(), after.len());
        // The source is never touched.
        assert_eq!(read(&src), "one\ntwo\nthree\n");
        assert_eq!(read(&src_dir.join("unrelated.jsonl")), "theirs\n");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A private tmux socket folder and toomux home for one test.
    struct PrivateTmux(std::path::PathBuf);

    impl PrivateTmux {
        fn new(tag: &str) -> Option<Self> {
            crate::tmux::command()
                .arg("-V")
                .output()
                .ok()?
                .status
                .success()
                .then_some(())?;
            // Short: a socket's path has a length limit.
            let root =
                std::path::PathBuf::from(format!("/tmp/toomux-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("sock")).unwrap();
            unsafe {
                std::env::set_var("TMUX_TMPDIR", root.join("sock"));
                std::env::set_var("TOOMUX_HOME", root.join("home"));
                std::env::set_var("XDG_RUNTIME_DIR", root.join("run"));
            }
            Some(Self(root))
        }

        fn script(&self, name: &str, body: &str) -> String {
            use std::os::unix::fs::PermissionsExt;
            let p = self.0.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p.display().to_string()
        }
    }

    impl Drop for PrivateTmux {
        fn drop(&mut self) {
            for server in crate::tmux::servers() {
                let _ = crate::tmux::run_on(&server, &["kill-server"]);
            }
            unsafe {
                std::env::remove_var("TMUX_TMPDIR");
                std::env::remove_var("TOOMUX_HOME");
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_launch_that_dies_at_once_fails_with_what_it_printed_and_leaves_no_server() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = PrivateTmux::new("die") else {
            return; // no tmux here
        };
        let mut cfg = cfg(&["/nonexistent/a"]);
        cfg.claude_bin = t.script(
            "claude",
            "echo \"No conversation found with session ID: x\"; sleep 0.4; exit 1",
        );
        let err = super::start(&cfg, "/tmp", 0, &[]).unwrap_err();
        assert!(err.to_string().contains("No conversation found"), "{err}");
        let t = std::time::Instant::now();
        while !crate::tmux::servers().is_empty() && t.elapsed().as_secs() < 3 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(
            crate::tmux::servers(),
            Vec::<String>::new(),
            "its dead server is gone"
        );
    }

    #[test]
    fn a_launch_that_stays_up_is_returned_and_let_go_of() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = PrivateTmux::new("live") else {
            return;
        };
        let mut cfg = cfg(&["/nonexistent/a"]);
        cfg.claude_bin = t.script("claude", "sleep 30");
        let pane = super::start(&cfg, "/tmp", 0, &[]).unwrap();
        assert_eq!(crate::tmux::pane_dead(&pane), Some(false));
        let held =
            crate::tmux::run(&["show-options", "-p", "-v", "-t", &pane, "remain-on-exit"]).unwrap();
        assert_eq!(held.trim(), "", "no longer held open on exit");
        // It was told which conversation to open, and so was its environment.
        let pid: i32 = crate::tmux::run(&["display-message", "-p", "-t", &pane, "#{pane_pid}"])
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let id = crate::running::id_in_args(&crate::platform::cmdline(pid)).unwrap();
        assert!(
            crate::platform::environ(pid).contains(&format!("TOOMUX_PROVIDER_SESSION_ID={id}"))
        );
    }
}
