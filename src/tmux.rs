//! tmux, across servers. Every session toomux starts runs in a tmux server of
//! its own (`toomux-…`), so nothing done to one server (a `kill-server`, a
//! crash) takes the others with it. Pane ids are only unique within a server,
//! so toomux carries them qualified: `%3@toomux-ab12cd34`, `%7@default`.
//! A server outside tmux's socket directory goes by its socket's path:
//! `%2@/tmp/rig/sock`. `run` sends a command to the server its qualified
//! targets name, bare.

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Pane {
    /// Qualified: `%N@server`.
    pub id: String,
    pub session: String,
    pub window_index: String,
}

/// Servers toomux makes for its sessions are named with this.
pub const OWN: &str = "toomux-";

/// Where tmux keeps its sockets, as tmux itself works it out: the folder
/// resolved, as tmux does, so on macOS `/tmp` is `/private/tmp` and matches
/// the path tmux puts in `$TMUX`.
pub fn socket_dir() -> PathBuf {
    let base = std::env::var_os("TMUX_TMPDIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(|| "/tmp".into());
    let base = std::fs::canonicalize(&base).unwrap_or(base);
    base.join(format!("tmux-{}", unsafe { libc::getuid() }))
}

fn socket(server: &str) -> PathBuf {
    if server.starts_with('/') {
        return server.into();
    }
    socket_dir().join(server)
}

/// A server's name from its socket: the file name in tmux's socket
/// directory, or the whole path elsewhere.
pub fn server_at(path: &std::path::Path) -> Option<String> {
    if path.parent() == Some(socket_dir().as_path()) {
        return path.file_name()?.to_str().map(str::to_string);
    }
    path.is_absolute()
        .then(|| path.to_str().map(str::to_string))
        .flatten()
}

/// Split a qualified target into its server and the bare target.
pub fn split(target: &str) -> (Option<&str>, &str) {
    match target.rsplit_once('@') {
        Some((bare, server))
            if bare.starts_with('%')
                && !server.is_empty()
                && (server.starts_with('/') || !server.contains('/')) =>
        {
            (Some(server), bare)
        }
        _ => (None, target),
    }
}

pub fn bare(target: &str) -> &str {
    split(target).1
}

pub fn server_of(target: &str) -> Option<&str> {
    split(target).0
}

pub fn qualify(server: &str, pane: &str) -> String {
    format!("{}@{server}", bare(pane))
}

/// The server the calling process is inside, if any.
pub fn current_server() -> Option<String> {
    server_in(&std::env::var("TMUX").ok()?)
}

/// The server a `TMUX` value (`socket,pid,session`) points at.
pub fn server_in(tmux_var: &str) -> Option<String> {
    server_at(std::path::Path::new(tmux_var.split(',').next()?))
}

/// Run a tmux command. Qualified targets send it to their server (bare);
/// otherwise it goes where plain `tmux` would.
pub fn run(args: &[&str]) -> Result<String> {
    let server = args.iter().find_map(|a| server_of(a));
    match server {
        Some(s) => run_on(s, args),
        None => output(Command::new("tmux").args(args), args),
    }
}

/// Run a tmux command on one server by name.
pub fn run_on(server: &str, args: &[&str]) -> Result<String> {
    let bare: Vec<&str> = args.iter().map(|a| bare(a)).collect();
    let mut c = Command::new("tmux");
    c.arg("-S").arg(socket(server)).args(&bare);
    output(&mut c, &bare)
}

fn output(c: &mut Command, args: &[&str]) -> Result<String> {
    let out = c.output()?;
    if !out.status.success() {
        bail!(
            "tmux {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A command that will talk to `server` (for exec, attach, control mode).
pub fn command_on(server: &str) -> Command {
    let mut c = Command::new("tmux");
    c.arg("-S").arg(socket(server));
    c
}

pub fn inside() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Live servers: the default one and toomux's own. A socket of ours whose
/// server is gone (it crashed) is cleared away; others' are left alone.
pub fn servers() -> Vec<String> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(socket_dir()) else {
        return out;
    };
    for e in dir.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name != "default" && !name.starts_with(OWN) {
            continue;
        }
        match std::os::unix::net::UnixStream::connect(e.path()) {
            Ok(_) => out.push(name),
            Err(err)
                if err.kind() == std::io::ErrorKind::ConnectionRefused && name.starts_with(OWN) =>
            {
                let _ = std::fs::remove_file(e.path());
            }
            Err(_) => {}
        }
    }
    out.sort();
    out
}

/// A socket of ours whose server is gone: cleared away, so the name is free.
fn stale(server: &str) -> bool {
    let path = socket(server);
    let gone = matches!(std::os::unix::net::UnixStream::connect(&path), Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused);
    if gone {
        let _ = std::fs::remove_file(&path);
    }
    gone
}

/// A name for a new server of toomux's own, from the conversation's id when
/// there is one.
pub fn new_server_name(id: Option<&str>) -> String {
    let key: String = match id {
        Some(id) => id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect(),
        None => format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros())
                .unwrap_or(0)
                & 0xffff_ffff
        ),
    };
    let mut name = format!("{OWN}{key}");
    let mut n = 2;
    while socket(&name).exists() && !stale(&name) {
        name = format!("{OWN}{key}-{n}");
        n += 1;
    }
    name
}

/// Start a new server holding one session (`name`) that runs `cmd` in `cwd`.
/// Returns the qualified pane id.
pub fn new_server(
    server: &str,
    name: &str,
    cwd: &str,
    env: &[String],
    cmd: &str,
) -> Result<String> {
    let mut args: Vec<&str> = vec![
        "new-session",
        "-d",
        "-s",
        name,
        "-n",
        name,
        "-c",
        cwd,
        "-P",
        "-F",
        "#{pane_id}",
    ];
    for e in env {
        args.extend(["-e", e.as_str()]);
    }
    args.push(cmd);
    // Not nested in whichever server we might be inside.
    let mut c = command_on(server);
    if let Some(conf) = server_conf() {
        c.arg("-f").arg(conf);
    }
    c.args(&args).env_remove("TMUX").env_remove("TMUX_PANE");
    // tmux makes its socket directory for -L, not for -S, and after a reboot
    // (/tmp cleared) it may not be there yet.
    if let Some(dir) = socket(server).parent() {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    let pane = output(&mut c, &args)?;
    // tmux can fail to start a server and still exit 0.
    if !pane.trim().starts_with('%') {
        bail!("tmux didn't start a server at {}", socket(server).display());
    }
    // The new server's global environment is a copy of ours, which may carry
    // this process's Claude session markers: later panes must not inherit them.
    for v in crate::registry::RUNTIME_VARS {
        let _ = run_on(server, &["set-environment", "-gu", v]);
    }
    Ok(qualify(server, pane.trim()))
}

/// The configuration toomux's servers start with: yours, less the plugins
/// that save and restore a whole server's layout (tmux-resurrect,
/// tmux-continuum). Those assume one server: in a new server of ours they
/// would restore your entire saved layout into it. Your other plugins run
/// as tpm would run them.
fn server_conf() -> Option<PathBuf> {
    let home = crate::config::home();
    let user = [home.join(".tmux.conf"), home.join(".config/tmux/tmux.conf")]
        .into_iter()
        .find(|p| p.is_file())?;
    let text = std::fs::read_to_string(&user).ok()?;
    let plugins = std::env::var_os("TMUX_PLUGIN_MANAGER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".tmux/plugins"));
    let out = crate::paths::state().join("server.tmux.conf");
    let _ = std::fs::create_dir_all(out.parent()?);
    std::fs::write(&out, filter_conf(&text, &user, &plugins)).ok()?;
    Some(out)
}

fn filter_conf(text: &str, from: &std::path::Path, plugins: &std::path::Path) -> String {
    let skip = |l: &str| {
        ["resurrect", "continuum", "tpm/tpm"]
            .iter()
            .any(|w| l.contains(w))
    };
    let mut out = format!(
        "# Written by toomux for its tmux servers: {} without tmux-resurrect,\n# tmux-continuum and tpm. Edit that file, not this one.\n",
        from.display()
    );
    let mut run = Vec::new();
    for l in text.lines() {
        let t = l.trim_start();
        if !t.starts_with('#') && skip(t) {
            continue;
        }
        out.push_str(l);
        out.push('\n');
        if t.starts_with("set -g @plugin ") || t.starts_with("set-option -g @plugin ") {
            let name = t
                .split(['\'', '"'])
                .nth(1)
                .unwrap_or("")
                .rsplit('/')
                .next()
                .unwrap_or("");
            if !name.is_empty() && name != "tpm" {
                run.push(plugins.join(name));
            }
        }
    }
    for dir in run {
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = files
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "tmux"))
            .collect();
        files.sort();
        for f in files {
            out.push_str(&format!(
                "run-shell {}\n",
                shell_words::quote(&f.display().to_string())
            ));
        }
    }
    out
}

pub const PANES_FORMAT: &str =
    "#{pane_tty}\t#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_dead}";

thread_local! {
    /// A pane listing of one server that arrived another way (the shell's
    /// control connection), used instead of asking that server again.
    static GIVEN: std::cell::RefCell<Option<(String, String)>> = const { std::cell::RefCell::new(None) };
}

/// Use this listing of `server` for `panes_by_tty` until `forget_panes`.
pub fn give_panes(server: &str, listing: &str) {
    GIVEN.with(|g| *g.borrow_mut() = Some((server.to_string(), listing.to_string())));
}

pub fn forget_panes() {
    GIVEN.with(|g| *g.borrow_mut() = None);
}

/// A pane listing is used again for this long. A live pane keeps its tty, so
/// the only listing that goes out of date is one missing a new session, and a
/// session not found is looked up at once (`panes_on`). Kept in a file, so the
/// status line (a new process every tick) shares it with the shell.
const FRESH_MS: i64 = 10_000;

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Listings(HashMap<String, (i64, String)>);

fn listings_path() -> PathBuf {
    crate::watch::dir().join("panes.json")
}

fn load_listings() -> Listings {
    std::fs::read(listings_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_listings(l: &Listings) {
    let path = listings_path();
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::create_dir_all(crate::watch::dir()).is_ok()
        && std::fs::write(&tmp, serde_json::to_vec(l).unwrap_or_default()).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn list(server: &str) -> String {
    run_on(server, &["list-panes", "-a", "-F", PANES_FORMAT]).unwrap_or_default()
}

/// Live panes on every server, keyed by their tty (`/dev/pts/N`).
pub fn panes_by_tty() -> HashMap<String, Pane> {
    let given = GIVEN.with(|g| g.borrow().clone());
    let now = crate::registry::now_ms();
    let mut kept = load_listings();
    let mut fresh = Listings::default();
    let mut all = HashMap::new();
    for server in servers() {
        let listing = match (&given, kept.0.remove(&server)) {
            (Some((s, text)), _) if *s == server => (now, text.clone()),
            (_, Some((at, text))) if now - at < FRESH_MS && at <= now => (at, text),
            _ => (now, list(&server)),
        };
        all.extend(parse_panes(&server, &listing.1));
        fresh.0.insert(server, listing);
    }
    // Servers of someone else's making that a session was found in.
    for (server, (at, text)) in kept.0 {
        if now - at < FRESH_MS && at <= now {
            all.extend(parse_panes(&server, &text));
            fresh.0.insert(server, (at, text));
        }
    }
    save_listings(&fresh);
    all
}

/// Live panes of one server, keyed by tty, listed again unless that was
/// done in the last `within_ms`.
pub fn panes_on(server: &str, within_ms: i64) -> HashMap<String, Pane> {
    let now = crate::registry::now_ms();
    let mut kept = load_listings();
    if let Some((_, text)) = kept
        .0
        .get(server)
        .filter(|(at, _)| now - at < within_ms && *at <= now)
    {
        return parse_panes(server, text);
    }
    let text = list(server);
    let panes = parse_panes(server, &text);
    kept.0.insert(server.to_string(), (now, text));
    save_listings(&kept);
    panes
}

fn parse_panes(server: &str, out: &str) -> HashMap<String, Pane> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() == 5 && f[4] != "1").then(|| {
                (
                    f[0].to_string(),
                    Pane {
                        id: qualify(server, f[1]),
                        session: f[2].into(),
                        window_index: f[3].into(),
                    },
                )
            })
        })
        .collect()
}

pub fn capture(pane: &str, lines: u16) -> Option<Vec<u8>> {
    let start = format!("-{}", lines.max(1));
    run(&["capture-pane", "-p", "-e", "-t", pane, "-S", &start])
        .ok()
        .map(String::into_bytes)
}

/// None when the pane no longer exists.
pub fn pane_dead(pane: &str) -> Option<bool> {
    run(&["display-message", "-p", "-t", pane, "#{pane_dead}"])
        .ok()
        .map(|s| s.trim() == "1")
}

/// Show `pane` in this terminal: switch to it within the server we're in,
/// or move the terminal over to the pane's server. `client` names the
/// terminal when tmux can't be left to guess (it picks the busiest client,
/// which can be toomux's own control client).
pub fn attach_here(pane: &str, client: Option<&str>) -> Result<()> {
    let server = server_of(pane).unwrap_or("default");
    let to_client: Vec<&str> = client.map(|c| vec!["-c", c]).unwrap_or_default();
    if inside() && current_server().as_deref() == Some(server) {
        run(&[&["switch-client"], to_client.as_slice(), &["-t", pane]].concat())?;
        return Ok(());
    }
    let attach = format!(
        "{} -S {} attach-session -t {}",
        "tmux",
        shell_words::quote(&socket(server).display().to_string()),
        bare(pane)
    );
    if inside() {
        // The terminal leaves this server and attaches to the pane's.
        let target: Vec<&str> = client.map(|c| vec!["-t", c]).unwrap_or_default();
        Command::new("tmux")
            .arg("detach-client")
            .args(target)
            .args(["-E", &attach])
            .output()?;
        return Ok(());
    }
    use std::os::unix::process::CommandExt;
    let err = command_on(server)
        .args(["attach-session", "-t", bare(pane)])
        .exec();
    Err(err.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_servers_leave_out_layout_restoring_plugins() {
        let conf = "set -g mouse on\nset -g @plugin 'tmux-plugins/tpm'\nset -g @plugin 'tmux-plugins/tmux-resurrect'\nset -g @continuum-restore 'on'\n# resurrect notes stay\nrun '~/.tmux/plugins/tpm/tpm'\n";
        let out = filter_conf(
            conf,
            std::path::Path::new("/x/.tmux.conf"),
            std::path::Path::new("/nonexistent"),
        );
        assert!(out.contains("set -g mouse on") && out.contains("# resurrect notes stay"));
        assert!(
            !out.contains("@plugin 'tmux-plugins/tmux-resurrect'")
                && !out.contains("@continuum-restore")
                && !out.contains("tpm/tpm'")
        );
    }

    #[test]
    fn a_pane_listing_is_used_again_while_fresh() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("toomux-listings-{}", std::process::id()));
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &tmp) };
        // No such server: listing it for real finds nothing.
        let server = "/nonexistent/toomux-test.sock";
        let now = crate::registry::now_ms();
        let keep = |at: i64| {
            let mut l = Listings::default();
            l.0.insert(server.into(), (at, "/dev/pts/9\t%4\twork\t1\t0".into()));
            save_listings(&l);
        };
        keep(now - 1_000);
        assert_eq!(
            panes_on(server, 2_000)
                .get("/dev/pts/9")
                .map(|p| p.id.as_str()),
            Some("%4@/nonexistent/toomux-test.sock")
        );
        keep(now - 3_000);
        assert!(
            panes_on(server, 2_000).is_empty(),
            "older than asked for: listed again"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_qualified_pane_names_its_server() {
        assert_eq!(split("%3@toomux-ab12cd34"), (Some("toomux-ab12cd34"), "%3"));
        assert_eq!(split("%12@default"), (Some("default"), "%12"));
        assert_eq!(split("%3"), (None, "%3"));
        // Session names may hold an @; only pane ids are qualified.
        assert_eq!(split("me@work:"), (None, "me@work:"));
        assert_eq!(qualify("default", "%3@toomux-x"), "%3@default");
        assert_eq!(split("%2@/tmp/rig/sock"), (Some("/tmp/rig/sock"), "%2"));
        assert_eq!(socket("/tmp/rig/sock"), PathBuf::from("/tmp/rig/sock"));
        assert_eq!(split("%2@a/b"), (None, "%2@a/b"));
    }
}
