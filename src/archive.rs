//! The archive: everything Claude Code keeps about a conversation, kept for
//! good.
//!
//! Claude Code deletes a conversation's files after `cleanupPeriodDays` (30
//! by default), and the index holds each turn only in part (its prompt and
//! the words that closed it). So every file under each account's
//! `projects/` (transcripts, subagents', tool results, attachments; not the
//! `memory/` folders, which upkeep looks after) is copied here, compressed,
//! once it has been quiet a day, and again whenever it changes. Nothing here
//! is ever deleted, and a whole turn can always be read back.

use crate::config::Config;
use anyhow::Result;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A file is copied once it has been left alone this long.
const QUIET_SECS: u64 = 24 * 3600;
/// How much one pass compresses, so a first backfill spreads over passes.
const PASS_BYTES: u64 = 2 << 30;
const LEVEL: i32 = 3;

pub fn dir() -> PathBuf {
    crate::paths::data().join("archive")
}

#[derive(Default, Debug)]
pub struct Pass {
    /// Files copied in this pass.
    pub copied: usize,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Files not yet copied: still changing, or past this pass's share.
    pub waiting: usize,
}

pub fn run(cfg: &Config) -> Result<Pass> {
    run_in(&crate::index::roots(cfg), &dir(), SystemTime::now())
}

fn run_in(roots: &[PathBuf], to: &Path, now: SystemTime) -> Result<Pass> {
    let mut pass = Pass::default();
    for root in roots {
        let to = to.join(root_name(root));
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let path = e.path();
                let Ok(kind) = e.file_type() else { continue };
                // What a link points at lives somewhere else.
                if kind.is_symlink() {
                    continue;
                }
                if kind.is_dir() {
                    if !(e.file_name() == "memory" && d.parent() == Some(root.as_path())) {
                        stack.push(path);
                    }
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                let Ok(modified) = md.modified() else { continue };
                let Ok(rel) = path.strip_prefix(root) else { continue };
                let target = copy_path(&to, rel);
                if std::fs::metadata(&target).and_then(|m| m.modified()).is_ok_and(|t| t == modified) {
                    continue;
                }
                let quiet = now.duration_since(modified).is_ok_and(|d| d.as_secs() >= QUIET_SECS);
                if !quiet || (pass.bytes_in > 0 && pass.bytes_in + md.len() > PASS_BYTES) {
                    pass.waiting += 1;
                    continue;
                }
                match copy(&path, &target, modified) {
                    Ok(out) => {
                        pass.copied += 1;
                        pass.bytes_in += md.len();
                        pass.bytes_out += out;
                    }
                    // Deleted or unreadable mid-pass: the next pass tries again.
                    Err(_) => pass.waiting += 1,
                }
            }
        }
    }
    Ok(pass)
}

/// Each account's folder is kept apart (two can hold the same path): named
/// after the folder `projects/` is in, without its dot (`claude-shared`).
fn root_name(root: &Path) -> String {
    root.parent().and_then(Path::file_name).map_or_else(|| "projects".into(), |n| n.to_string_lossy().trim_start_matches('.').to_string())
}

fn copy_path(to: &Path, rel: &Path) -> PathBuf {
    let mut name = rel.as_os_str().to_owned();
    name.push(".zst");
    to.join(name)
}

/// Compress `from` into `target`, stamped with the source's modified time
/// (read before copying, so a file that changes meanwhile is copied again).
fn copy(from: &Path, target: &Path, modified: SystemTime) -> Result<u64> {
    let parent = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let top = dir();
        if !top.exists() {
            std::fs::create_dir_all(&top)?;
        }
        let _ = std::fs::set_permissions(&top, std::fs::Permissions::from_mode(0o700));
    }
    let tmp = target.with_extension(format!("zst.{}", std::process::id()));
    let done = (|| -> Result<u64> {
        let src = std::fs::File::open(from)?;
        let out = std::fs::File::create(&tmp)?;
        zstd::stream::copy_encode(src, &out, LEVEL)?;
        out.set_modified(modified)?;
        Ok(out.metadata()?.len())
    })();
    match done {
        Ok(n) => {
            std::fs::rename(&tmp, target)?;
            Ok(n)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// A conversation's transcript (or one of its subagents'), from where Claude
/// Code keeps it, or from the archive once Claude Code has deleted it.
pub fn transcript(cfg: &Config, session: &str, agent: Option<&str>) -> Option<String> {
    transcript_in(&crate::index::roots(cfg), &dir(), session, agent)
}

fn transcript_in(roots: &[PathBuf], archive: &Path, session: &str, agent: Option<&str>) -> Option<String> {
    let rel = |project: &str| match agent {
        Some(a) => PathBuf::from(project).join(session).join("subagents").join(format!("agent-{a}.jsonl")),
        None => PathBuf::from(project).join(format!("{session}.jsonl")),
    };
    for root in roots {
        for p in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let live = root.join(rel(&p.file_name().to_string_lossy()));
            if let Ok(text) = std::fs::read_to_string(&live) {
                return Some(text);
            }
        }
    }
    for account in std::fs::read_dir(archive).into_iter().flatten().flatten() {
        for p in std::fs::read_dir(account.path()).into_iter().flatten().flatten() {
            if let Some(text) = read_copy(&copy_path(&account.path(), &rel(&p.file_name().to_string_lossy()))) {
                return Some(text);
            }
        }
    }
    None
}

fn read_copy(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    zstd::stream::read::Decoder::new(f).ok()?.read_to_string(&mut text).ok()?;
    Some(text)
}

/// Keep a file as it was before toomux changes it: beside the archive's
/// copies, stamped with when it was replaced.
pub fn keep_version(root: &Path, path: &Path, text: &str, archive: &Path) -> Result<()> {
    let target = version_path(archive, root, path, crate::registry::now_ms());
    std::fs::create_dir_all(target.parent().unwrap_or(archive))?;
    let packed = zstd::encode_all(text.as_bytes(), LEVEL)?;
    let tmp = target.with_extension(format!("zst.{}", std::process::id()));
    std::fs::write(&tmp, packed)?;
    std::fs::rename(tmp, target)?;
    Ok(())
}

fn version_path(archive: &Path, root: &Path, path: &Path, at_ms: i64) -> PathBuf {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut name = rel.as_os_str().to_owned();
    name.push(format!(".{at_ms}.zst"));
    archive.join(root_name(root)).join(name)
}

/// The kept versions of a file, oldest first.
pub fn versions(archive: &Path, root: &Path, path: &Path) -> Vec<PathBuf> {
    let probe = version_path(archive, root, path, 0);
    let Some(dir) = probe.parent() else { return Vec::new() };
    let stem = path.file_name().map(|n| format!("{}.", n.to_string_lossy())).unwrap_or_default();
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix(&stem)).and_then(|r| r.strip_suffix(".zst")).is_some_and(|ms| ms.parse::<i64>().is_ok())
        })
        .collect();
    found.sort();
    found
}

pub fn read_version(path: &Path) -> Option<String> {
    read_copy(path)
}

/// How much the archive holds: files and bytes on disk.
pub fn size() -> (usize, u64) {
    let mut stack = vec![dir()];
    let (mut n, mut bytes) = (0, 0);
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => {
                    n += 1;
                    bytes += m.len();
                }
                Err(_) => {}
            }
        }
    }
    (n, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn quiet_files_are_kept_once_and_again_when_they_change() {
        let tmp = std::env::temp_dir().join(format!("toomux-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join(".acct/projects");
        let to = tmp.join("archive");
        let t = root.join("-work-app/s1.jsonl");
        let result = root.join("-work-app/s1/tool-results/r1.txt");
        let note = root.join("-work-app/memory/MEMORY.md");
        for p in [&t, &result, &note] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "{\"type\":\"user\"}\n".repeat(50)).unwrap();
        }
        let roots = vec![root.clone()];
        let now = SystemTime::now();
        let pass = run_in(&roots, &to, now).unwrap();
        assert_eq!((pass.copied, pass.waiting), (0, 2), "nothing is copied while it may still change");
        let later = now + Duration::from_secs(QUIET_SECS + 60);
        let pass = run_in(&roots, &to, later).unwrap();
        assert_eq!(pass.copied, 2, "the transcript and its tool result, not memory");
        assert!(!to.join("acct/-work-app/memory").exists());
        assert_eq!(read_copy(&to.join("acct/-work-app/s1.jsonl.zst")).unwrap(), std::fs::read_to_string(&t).unwrap());
        assert_eq!(run_in(&roots, &to, later).unwrap().copied, 0, "unchanged, not copied again");
        std::fs::write(&t, "{\"type\":\"user\"}\n{\"more\":1}\n").unwrap();
        let pass = run_in(&roots, &to, SystemTime::now() + Duration::from_secs(QUIET_SECS + 60)).unwrap();
        assert_eq!(pass.copied, 1, "changed, so copied again");
        std::fs::remove_file(&t).unwrap();
        assert!(read_copy(&to.join("acct/-work-app/s1.jsonl.zst")).unwrap().contains("more"), "kept after the original is gone");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn two_accounts_with_the_same_path_are_both_kept() {
        let tmp = std::env::temp_dir().join(format!("toomux-archive2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let roots = vec![tmp.join(".claude/projects"), tmp.join(".claude-work/projects")];
        for (i, r) in roots.iter().enumerate() {
            std::fs::create_dir_all(r.join("-p")).unwrap();
            std::fs::write(r.join("-p/pointer.json"), format!("{{\"account\":{i}}}")).unwrap();
        }
        let later = SystemTime::now() + Duration::from_secs(QUIET_SECS + 60);
        assert_eq!(run_in(&roots, &tmp.join("a"), later).unwrap().copied, 2);
        assert_eq!(run_in(&roots, &tmp.join("a"), later).unwrap().copied, 0, "neither overwrites the other");
        assert!(read_copy(&tmp.join("a/claude-work/-p/pointer.json.zst")).unwrap().contains('1'));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn a_deleted_transcript_is_read_from_the_archive() {
        let tmp = std::env::temp_dir().join(format!("toomux-archive3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let roots = vec![tmp.join(".claude/projects")];
        let live = roots[0].join("-p/s9.jsonl");
        let agent = roots[0].join("-p/s9/subagents/agent-a2.jsonl");
        for (p, text) in [(&live, "main"), (&agent, "sub")] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let to = tmp.join("a");
        assert_eq!(transcript_in(&roots, &to, "s9", None).as_deref(), Some("main"));
        run_in(&roots, &to, SystemTime::now() + Duration::from_secs(QUIET_SECS + 60)).unwrap();
        std::fs::remove_dir_all(roots[0].join("-p")).unwrap();
        assert_eq!(transcript_in(&roots, &to, "s9", None).as_deref(), Some("main"));
        assert_eq!(transcript_in(&roots, &to, "s9", Some("a2")).as_deref(), Some("sub"));
        assert!(transcript_in(&roots, &to, "nope", None).is_none());
        let _ = std::fs::remove_dir_all(tmp);
    }
}
