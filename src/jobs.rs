//! Background commands that can outlive the Claude process that started them.
//!
//! When a session runs a Bash command in the background, the hook rewrites it
//! to `toomux job run <id>`: a detached host runs the command (in its own
//! process group, output to a log) and the Claude side becomes a follower that
//! streams the log and exits with the command's status. To Claude it looks and
//! behaves the same: output, completion notices, stopping it.
//!
//! The difference is what happens when the follower goes away. Normally (the
//! session stopped the task, or exited) the host stops the command, as Claude
//! would have. During a handover the job is marked to carry on: it keeps
//! running, and the fresh session re-attaches with `toomux job follow <id>`.
//!
//! Subagents' long foreground commands run as jobs too, to keep their prompt
//! cache warm: a subagent's cache lasts five minutes, and a command that
//! blocks longer makes the agent's whole context be written to the cache
//! again (1.25x its size) instead of read (0.1x). The follower waits at most
//! `keep_warm()` and then leaves the command running, saying so; the agent's
//! `toomux job follow <id> --more` carries on from there. Output and exit
//! status are the same as before, and the command still stops at the timeout
//! the agent gave it.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn dir() -> PathBuf {
    crate::paths::state().join("jobs")
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Job {
    pub id: String,
    pub command: String,
    pub cwd: String,
    /// The Claude session that owns it (updated when a handover passes it on).
    pub session: String,
    pub started_ms: i64,
    /// Process group of the running command.
    pub pgid: i32,
    /// running, exited, stopped
    pub status: String,
    pub exit: Option<i32>,
    /// Keep running without a follower: its session is handing over.
    #[serde(default)]
    pub carry_on: bool,
    /// When carrying on stops counting, if nobody has followed it by then
    /// (0: no limit). A handover that died halfway can't leave it running
    /// for ever.
    #[serde(default)]
    pub carry_until_ms: i64,
    /// Stop it here, as Claude would have at the timeout it was given (0:
    /// none).
    #[serde(default)]
    pub deadline_ms: i64,
    /// Bytes of its log the agent has seen, for `follow --more`.
    #[serde(default)]
    pub shown: u64,
}

/// How long a job waits unfollowed for the fresh session to pick it up.
const CARRY_MS: i64 = 30 * 60_000;
/// How long a job without a deadline waits for its agent to come back after
/// the follower stepped away.
const AWAY_MS: i64 = 10 * 60_000;

/// How long a subagent waits on a command in one go: under its five-minute
/// cache, with room for the call around it.
pub fn keep_warm() -> Duration {
    let secs = std::env::var("TOOMUX_KEEP_WARM_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(270);
    Duration::from_secs(secs)
}

fn path(id: &str) -> PathBuf {
    dir().join(format!("{id}.json"))
}
fn path_in(base: &Path, id: &str) -> PathBuf {
    base.join(format!("{id}.json"))
}
pub fn log_path(id: &str) -> PathBuf {
    dir().join(format!("{id}.log"))
}
fn followers_path(id: &str) -> PathBuf {
    dir().join(format!("{id}.followers"))
}
fn lock_path(id: &str) -> PathBuf {
    dir().join(format!("{id}.lock"))
}

pub fn load(id: &str) -> Option<Job> {
    load_from(&dir(), id)
}

fn load_from(base: &Path, id: &str) -> Option<Job> {
    std::fs::read_to_string(path_in(base, id))
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
}

fn save(j: &Job) -> Result<()> {
    save_in(&dir(), j)
}

fn save_in(base: &Path, j: &Job) -> Result<()> {
    std::fs::create_dir_all(base)?;
    let tmp = base.join(format!(".{}.{}", j.id, std::process::id()));
    std::fs::write(&tmp, serde_json::to_string(j)?)?;
    std::fs::rename(tmp, path_in(base, &j.id))?;
    Ok(())
}

/// Change a job on disk (re-reading first; the host and followers share it).
fn update(id: &str, f: impl FnOnce(&mut Job)) -> Result<Job> {
    update_in(&dir(), id, f)
}

fn update_in(base: &Path, id: &str, f: impl FnOnce(&mut Job)) -> Result<Job> {
    let _guard = crate::lock::exclusive(&base.join(format!("{id}.lock")))?;
    let mut j = load_from(base, id).with_context(|| format!("no job {id}"))?;
    f(&mut j);
    save_in(base, &j)?;
    Ok(j)
}

/// Record a job the hook is about to start. `timeout_ms` (0: none) is when
/// it will be stopped.
pub fn create(id: &str, command: &str, cwd: &str, session: &str, timeout_ms: i64) -> Result<()> {
    let now = crate::registry::now_ms();
    save(&Job {
        id: id.into(),
        command: command.into(),
        cwd: cwd.into(),
        session: session.into(),
        started_ms: now,
        status: "starting".into(),
        deadline_ms: if timeout_ms > 0 { now + timeout_ms } else { 0 },
        ..Default::default()
    })
}

/// Every job, newest first.
pub fn all() -> Vec<Job> {
    all_in(&dir())
}

fn all_in(base: &Path) -> Vec<Job> {
    let mut out: Vec<Job> = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            std::fs::read_to_string(e.path())
                .ok()
                .and_then(|r| serde_json::from_str(&r).ok())
        })
        .collect();
    out.sort_by_key(|j: &Job| std::cmp::Reverse(j.started_ms));
    out
}

/// A session's jobs that are still running.
pub fn running_for(session: &str) -> Vec<Job> {
    all()
        .into_iter()
        .filter(|j| j.session == session && j.status == "running" && alive_group(j.pgid))
        .collect()
}

/// Sessions that currently own at least one live background job.
///
/// The registry asks this once per refresh so an idle foreground prompt can
/// still be shown as `tasks running`. Keeping the process-group liveness check
/// here means stale job records never turn into false background state.
pub fn running_sessions() -> HashSet<String> {
    all()
        .into_iter()
        .filter(|j| j.status == "running" && alive_group(j.pgid))
        .map(|j| j.session)
        .collect()
}

/// Before a handover: let the session's jobs carry on without it.
pub fn carry_on(session: &str) {
    let until = crate::registry::now_ms() + CARRY_MS;
    for j in running_for(session) {
        let _ = update(&j.id, |j| {
            j.carry_on = true;
            j.carry_until_ms = until;
        });
    }
}

/// A handover that didn't happen: the jobs are the session's again, and stop
/// with it as usual.
pub fn release(session: &str) {
    for j in running_for(session) {
        let _ = update(&j.id, |j| j.carry_on = false);
    }
}

fn carrying(j: &Job, now: i64) -> bool {
    j.carry_on && (j.carry_until_ms == 0 || now < j.carry_until_ms)
}

/// After a handover: the jobs belong to the new session.
pub fn pass_on(old: &str, new: &str) {
    for j in all().into_iter().filter(|j| j.session == old) {
        let _ = update(&j.id, |j| j.session = new.to_string());
    }
}

fn alive_group(pgid: i32) -> bool {
    pgid > 0 && unsafe { libc::kill(-pgid, 0) } == 0
}

/// Alive and not just waiting to be reaped (a killed follower stays a zombie
/// until its parent gets round to it).
fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0 && !crate::platform::is_zombie(pid)
}

/// `toomux job run <id>`: start the host, then follow it (for at most
/// `until`). Runs in Claude's shell, so the command gets that shell's
/// directory and environment.
pub fn run(id: &str, until: Option<Duration>) -> Result<i32> {
    let exe = std::env::current_exe()?;
    // In a scope of its own where systemd can make one, so closing the
    // terminal (whose scope systemd tears down) can't take a carried job
    // with it. Without systemd, or if it says no, the host runs plainly;
    // it only ever starts a command once, so a second try is harmless.
    let mut host = std::process::Command::new("sh");
    host.args([
        "-c",
        r#"command -v systemd-run >/dev/null 2>&1 && systemd-run --user --scope --quiet --collect -- "$0" job host "$1" 2>/dev/null || exec "$0" job host "$1""#,
    ])
    .arg(&exe)
    .arg(id)
    .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    use std::os::unix::process::CommandExt;
    unsafe {
        host.pre_exec(|| {
            libc::setsid();
            // Fork again and let the middle process exit, so the host is no
            // descendant of Claude: Claude kills its background commands'
            // whole process tree when it exits, and a handover must not take
            // the host with it.
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
    host.spawn().context("starting the job host")?.wait()?;
    follow(id, None, until, false)
}

/// `toomux job host <id>`: run the command and look after it. Never fails
/// once the command has started (the launcher would take a failure as the
/// host not starting, and try again).
pub fn host(id: &str) -> Result<()> {
    let job = load(id).context("no such job")?;
    if job.status != "starting" {
        return Ok(());
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(id))?;
    let mut cmd = std::process::Command::new("bash");
    cmd.args(["-c", &job.command])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            // Its own group, so stopping it stops everything it started.
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting the command")?;
    let pgid = child.id() as i32;
    let _ = update(id, |j| {
        j.pgid = pgid;
        j.status = "running".into();
    });
    let mut last_follower = Instant::now();
    let mut ever_followed = false;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let code = status.code().unwrap_or(
                128 + std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0),
            );
            let _ = update(id, |j| {
                j.status = if j.status == "stopping" {
                    "stopped"
                } else {
                    "exited"
                }
                .into();
                j.exit = Some(code);
            });
            return Ok(());
        }
        let followed = followers(id).into_iter().any(alive);
        if followed {
            ever_followed = true;
            last_follower = Instant::now();
        }
        let j = load(id).unwrap_or_default();
        if j.deadline_ms > 0 && crate::registry::now_ms() >= j.deadline_ms && j.status == "running"
        {
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(log_path(id)) {
                let _ = writeln!(
                    f,
                    "\n[toomux · stopped: it reached the {} timeout it was given]",
                    span(j.deadline_ms - j.started_ms)
                );
            }
            stop_group(pgid);
            let status = child.wait().ok();
            let _ = update(id, |j| {
                j.status = "stopped".into();
                j.exit = status.map(|s| s.code().unwrap_or(143));
            });
            return Ok(());
        }
        // Its follower is gone and no handover asked it to carry on (or the
        // fresh session never came for it): the session stopped it or ended,
        // so stop the command as Claude would.
        let abandoned =
            ever_followed && !followed && last_follower.elapsed() > Duration::from_secs(3);
        let never = !ever_followed && last_follower.elapsed() > Duration::from_secs(20);
        if (abandoned || never) && !carrying(&j, crate::registry::now_ms())
            || j.status == "stopping"
        {
            stop_group(pgid);
            let status = child.wait().ok();
            let _ = update(id, |j| {
                j.status = "stopped".into();
                j.exit = status.map(|s| s.code().unwrap_or(143));
            });
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn followers(id: &str) -> Vec<i32> {
    std::fs::read_to_string(followers_path(id))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

fn stop_group(pgid: i32) {
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    let t = Instant::now();
    while alive_group(pgid) && t.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(100));
    }
    if alive_group(pgid) {
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

/// `90s` -> "1m 30s", `600s` -> "10m".
fn span(ms: i64) -> String {
    let s = (ms.max(0) + 500) / 1000;
    match (s / 60, s % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m {s}s"),
    }
}

/// `toomux job follow <id> [--tail N] [--more]`: stream the log until the
/// command ends, then exit with its status. Following it again after a
/// handover puts it back under the session's care. `until` bounds the wait:
/// past it the command is left running and the follower says so. `more`
/// carries on from what the last follower showed, waiting `keep_warm()`.
pub fn follow(id: &str, tail: Option<usize>, until: Option<Duration>, more: bool) -> Result<i32> {
    let began = Instant::now();
    let until = if more {
        Some(until.unwrap_or_else(keep_warm))
    } else {
        until
    };
    std::fs::create_dir_all(dir())?;
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(followers_path(id))?;
        writeln!(f, "{}", std::process::id())?;
    }
    // Being followed again: normal rules apply from here.
    let _ = update(id, |j| {
        j.carry_on = false;
        j.carry_until_ms = 0;
    });
    let mut out = std::io::stdout().lock();
    let mut pos: u64 = if more {
        load(id).map_or(0, |j| j.shown)
    } else {
        0
    };
    if let Some(n) = tail.filter(|_| !more) {
        let bytes = std::fs::read(log_path(id)).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() > n {
            writeln!(
                out,
                "[… {} earlier lines: toomux job log {id}]",
                lines.len() - n
            )?;
        }
        for l in &lines[lines.len().saturating_sub(n)..] {
            writeln!(out, "{l}")?;
        }
        pos = bytes.len() as u64;
    }
    loop {
        if let Ok(mut f) = std::fs::File::open(log_path(id)) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len > pos {
                f.seek(SeekFrom::Start(pos))?;
                let mut buf = Vec::new();
                f.take(len - pos).read_to_end(&mut buf)?;
                out.write_all(&buf)?;
                out.flush()?;
                pos = len;
            }
        }
        let mut j = load(id).context("the job record is gone")?;
        if j.status == "running" && j.pgid > 0 && !alive_group(j.pgid) {
            // Killed with its host, which couldn't record it: say so once.
            std::thread::sleep(Duration::from_millis(1500));
            j = load(id).context("the job record is gone")?;
            if j.status == "running" && !alive_group(j.pgid) {
                update(id, |j| {
                    j.status = "lost".into();
                    j.exit = Some(137);
                })?;
                writeln!(out, "[toomux: job {id} was killed from outside]")?;
                return Ok(137);
            }
        }
        // Stopped from outside and its host gone before it could say so.
        if j.status == "stopping" && !alive_group(j.pgid) {
            std::thread::sleep(Duration::from_millis(1500));
            j = load(id).context("the job record is gone")?;
            if j.status == "stopping" {
                j.status = "stopped".into();
                let _ = update(id, |j| j.status = "stopped".into());
            }
        }
        match j.status.as_str() {
            "exited" | "stopped" => {
                // One more read for anything written just before the end.
                if std::fs::metadata(log_path(id))
                    .map(|m| m.len())
                    .unwrap_or(0)
                    > pos
                {
                    continue;
                }
                return Ok(j
                    .exit
                    .unwrap_or(if j.status == "stopped" { 143 } else { 0 }));
            }
            _ if until.is_some_and(|u| began.elapsed() >= u) => {
                // Step away, leaving it running until its deadline.
                let now = crate::registry::now_ms();
                let j = update(id, |j| {
                    j.shown = pos;
                    j.carry_on = true;
                    j.carry_until_ms = if j.deadline_ms > 0 {
                        j.deadline_ms
                    } else {
                        now + AWAY_MS
                    };
                })?;
                let stops = if j.deadline_ms > 0 {
                    format!(
                        "It stops in {} if still going (the timeout it was given).",
                        span(j.deadline_ms - now)
                    )
                } else {
                    format!("Unfollowed, it stops in {}.", span(AWAY_MS))
                };
                writeln!(
                    out,
                    "\n[toomux · still running after {}, as job {id}. `toomux job follow {id} --more` shows what it prints next and waits up to {} more; waiting in spells this short keeps your prompt cache warm. {stops}]",
                    span(now - j.started_ms),
                    span(keep_warm().as_millis() as i64),
                )?;
                out.flush()?;
                return Ok(0);
            }
            _ => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

pub fn stop(id: &str) -> Result<String> {
    let j = load(id).with_context(|| format!("no job {id}"))?;
    if j.status != "running" {
        bail!("job {id} isn't running ({})", j.status);
    }
    update(id, |j| j.status = "stopping".into())?;
    stop_group(j.pgid);
    Ok(format!("stopped job {id}"))
}

pub fn log(id: &str, tail: Option<usize>) -> Result<String> {
    let bytes = std::fs::read(log_path(id)).with_context(|| format!("no log for job {id}"))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(match tail {
        Some(n) => {
            text.lines()
                .rev()
                .take(n)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        }
        None => text,
    })
}

/// Prune finished jobs older than a week, and records whose command never
/// started (another hook's rewrite won, say) after ten minutes.
pub fn prune() {
    let now = crate::registry::now_ms();
    let old = |j: &Job| match j.status.as_str() {
        "running" | "stopping" if alive_group(j.pgid) => false,
        "starting" => j.started_ms < now - 10 * 60_000,
        _ => j.started_ms < now - 7 * 86_400_000,
    };
    for j in all().into_iter().filter(old) {
        for p in [
            path(&j.id),
            log_path(&j.id),
            followers_path(&j.id),
            lock_path(&j.id),
        ] {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_job_updates_are_serialised() {
        let tmp = std::env::temp_dir().join(format!("toomux-jobs-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let jobs_dir = tmp.join("jobs");
        let job = Job {
            id: "race".into(),
            command: "true".into(),
            cwd: "/tmp".into(),
            session: "session".into(),
            started_ms: crate::registry::now_ms(),
            status: "starting".into(),
            ..Default::default()
        };
        save_in(&jobs_dir, &job).unwrap();

        let mut threads = Vec::new();
        for _ in 0..12 {
            let jobs_dir = jobs_dir.clone();
            threads.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    update_in(&jobs_dir, "race", |j| j.shown += 1).unwrap();
                }
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(load_from(&jobs_dir, "race").unwrap().shown, 300);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn malformed_job_records_are_ignored_by_listing() {
        let tmp = std::env::temp_dir().join(format!("toomux-jobs-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let jobs_dir = tmp.join("jobs");
        std::fs::create_dir_all(&jobs_dir).unwrap();
        std::fs::write(path_in(&jobs_dir, "bad"), "{bad").unwrap();
        let good = Job {
            id: "good".into(),
            command: "true".into(),
            cwd: "/tmp".into(),
            session: "session".into(),
            started_ms: crate::registry::now_ms(),
            status: "starting".into(),
            ..Default::default()
        };
        save_in(&jobs_dir, &good).unwrap();
        let jobs = all_in(&jobs_dir);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "good");
        let _ = std::fs::remove_dir_all(tmp);
    }
}
