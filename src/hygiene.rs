//! Repository hygiene: what agents leave behind in the repositories under
//! home, tidied where that's certain to lose nothing, listed otherwise.
//!
//! Tidied: worktrees whose folder is gone (`git worktree prune`), and
//! worktrees that are clean (nothing changed or untracked), whose commit is
//! already on the default branch, quiet a day, and not any process's current
//! folder (`git worktree remove`, never forced: git refuses anything dirty).
//! Branches are never deleted. Listed: uncommitted work and how long it has
//! sat, worktrees kept (unmerged or dirty), merged branches.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

const QUIET_SECS: u64 = 24 * 3600;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One repository's state, and what was tidied in it.
#[derive(Default, Debug)]
pub struct Repo {
    pub path: PathBuf,
    /// Changed or untracked files in its main folder, and how long the
    /// oldest change has sat (days).
    pub uncommitted: usize,
    pub oldest_days: u64,
    /// Worktrees removed (merged, clean, quiet), and pruned (folder gone).
    pub removed: Vec<PathBuf>,
    pub pruned: usize,
    /// Worktrees left: holding work not on the default branch, or changes.
    pub kept: usize,
    /// Local branches already merged into the default branch.
    pub merged_branches: usize,
}

#[derive(Default)]
struct Tree {
    path: PathBuf,
    head: String,
    prunable: bool,
    locked: bool,
}

fn worktrees(repo: &Path) -> Vec<Tree> {
    let Some(out) = git(repo, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    let mut all = Vec::new();
    let mut t = Tree::default();
    for line in out.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !t.path.as_os_str().is_empty() {
                all.push(std::mem::take(&mut t));
            }
        } else if let Some(p) = line.strip_prefix("worktree ") {
            t.path = PathBuf::from(p);
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            t.head = h.to_string();
        } else if line.starts_with("prunable") {
            t.prunable = true;
        } else if line.starts_with("locked") {
            t.locked = true;
        }
    }
    all
}

/// The branch everything lands on: origin's default, else main or master.
fn default_branch(repo: &Path) -> Option<String> {
    if let Some(r) = git(
        repo,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    ) {
        return Some(r.trim().to_string());
    }
    ["refs/heads/main", "refs/heads/master"]
        .into_iter()
        .find(|b| git(repo, &["rev-parse", "--verify", "--quiet", b]).is_some())
        .map(str::to_string)
}

/// Every process's current folder, so a worktree someone is in is left alone.
fn folders_in_use() -> Vec<PathBuf> {
    crate::platform::folders_in_use()
}

fn quiet(path: &Path, now: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .is_some_and(|d| d.as_secs() >= QUIET_SECS)
}

/// Look after one repository; with `apply` false, only say what would go.
pub fn tend(repo: &Path, apply: bool, in_use: &[PathBuf], now: SystemTime) -> Result<Repo> {
    let mut r = Repo {
        path: repo.to_path_buf(),
        ..Repo::default()
    };
    let base = default_branch(repo);
    let trees = worktrees(repo);
    // Git lists real paths; a folder reached through a symlink (macOS /var is
    // /private/var) must still count as in use.
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let in_use: Vec<PathBuf> = in_use.iter().map(|u| real(u)).collect();
    for t in trees.iter().skip(1) {
        if t.prunable {
            r.pruned += 1;
            continue;
        }
        let merged = base.as_deref().is_some_and(|b| {
            !t.head.is_empty() && git(repo, &["merge-base", "--is-ancestor", &t.head, b]).is_some()
        });
        let clean = git(&t.path, &["status", "--porcelain"]).is_some_and(|s| s.trim().is_empty());
        let here = real(&t.path);
        let used = in_use.iter().any(|u| u.starts_with(&here));
        if t.locked || !merged || !clean || used || !quiet(&t.path, now) {
            r.kept += 1;
            continue;
        }
        let path = t.path.to_string_lossy();
        if !apply || git(repo, &["worktree", "remove", &path]).is_some() {
            r.removed.push(t.path.clone());
        } else {
            r.kept += 1;
        }
    }
    if apply && r.pruned > 0 {
        let _ = git(repo, &["worktree", "prune"]);
    }
    if let Some(status) = git(repo, &["status", "--porcelain"]) {
        let files: Vec<&str> = status.lines().filter_map(|l| l.get(3..)).collect();
        r.uncommitted = files.len();
        r.oldest_days = files
            .iter()
            .filter_map(|f| {
                std::fs::metadata(repo.join(f.trim_matches('"')))
                    .and_then(|m| m.modified())
                    .ok()
            })
            .filter_map(|m| now.duration_since(m).ok())
            .map(|d| d.as_secs() / 86_400)
            .max()
            .unwrap_or(0);
    }
    if let Some(b) = &base {
        let short = b
            .trim_start_matches("refs/remotes/")
            .trim_start_matches("refs/heads/");
        r.merged_branches = git(
            repo,
            &["branch", "--merged", short, "--format=%(refname:short)"],
        )
        .map(|o| {
            o.lines()
                .filter(|l| !matches!(*l, "main" | "master") && *l != short)
                .count()
        })
        .unwrap_or(0);
    }
    Ok(r)
}

/// Every repository under home (its main folder, not its worktrees).
pub fn run(places: &crate::upkeep::Places, apply: bool) -> Vec<Repo> {
    let in_use = folders_in_use();
    let now = SystemTime::now();
    places
        .repos()
        .into_iter()
        .filter_map(|p| tend(&p, apply, &in_use, now).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }

    #[test]
    fn only_merged_clean_quiet_worktrees_go() {
        let tmp = std::env::temp_dir().join(format!("toomux-hygiene-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let tmp = tmp.canonicalize().unwrap();
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a"), "1").unwrap();
        sh(&repo, &["add", "a"]);
        sh(&repo, &["commit", "-qm", "one"]);
        let wt = |name: &str, branch: &str| {
            let p = tmp.join(name);
            sh(
                &repo,
                &["worktree", "add", "-q", "-b", branch, &p.to_string_lossy()],
            );
            p
        };
        let done = wt("done", "done");
        let open = wt("open", "open");
        std::fs::write(open.join("b"), "2").unwrap();
        sh(&open, &["add", "b"]);
        sh(&open, &["commit", "-qm", "unmerged work"]);
        let dirty = wt("dirty", "dirty");
        std::fs::write(dirty.join("scratch"), "x").unwrap();
        let busy = wt("busy", "busy");
        let gone = wt("gone", "gone");
        std::fs::remove_dir_all(&gone).unwrap();
        std::fs::write(repo.join("a"), "changed").unwrap();
        let later = SystemTime::now() + Duration::from_secs(QUIET_SECS + 60);
        let seen = tend(&repo, false, &[busy.clone()], later).unwrap();
        assert_eq!(
            seen.removed,
            vec![done.clone()],
            "a dry run names only the merged, clean, unused one"
        );
        assert!(done.exists(), "and changes nothing");
        let r = tend(&repo, true, &[busy.clone()], later).unwrap();
        assert_eq!(r.removed, vec![done.clone()]);
        assert!(!done.exists() && open.exists() && dirty.exists() && busy.exists());
        assert_eq!((r.pruned, r.kept), (1, 3));
        assert_eq!(
            r.uncommitted, 1,
            "work in the main folder is listed, not touched"
        );
        assert_eq!(std::fs::read_to_string(repo.join("a")).unwrap(), "changed");
        let branches = git(&repo, &["branch", "--format=%(refname:short)"]).unwrap();
        assert!(branches.contains("done"), "branches are never deleted");
        assert!(
            tend(&repo, true, &[], SystemTime::now())
                .unwrap()
                .removed
                .is_empty(),
            "nothing goes the day it was touched"
        );
        let _ = std::fs::remove_dir_all(tmp);
    }
}
