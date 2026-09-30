//! Claude Code accounts, each a folder chosen with `CLAUDE_CONFIG_DIR` and
//! holding its own login. Any account can stand alone, or join a group of
//! accounts that share one history: everything but the login (conversations,
//! memory, settings, skills, agents …) becomes a link into the group's folder
//! (`~/.claude-shared`, or `~/.claude-shared-<group>`), so a conversation
//! resumes under any of them and they all read the same memory. Accounts
//! join and leave groups at any time, and nothing is lost either way.
//!
//! Only what is listed here is shared. Anything Claude Code adds later stays
//! each account's own until it is listed, which is the safe way round: a new
//! kind of login file is never shared by accident.

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::{Path, PathBuf};

/// Folders every account shares.
pub const SHARED_DIRS: &[&str] = &[
    "projects",
    "sessions",
    "session-env",
    "shell-snapshots",
    "file-history",
    "todos",
    "plans",
    "jobs",
    "skills",
    "agents",
    "commands",
    "output-styles",
    "hooks",
    "plugins",
    "ide",
    "paste-cache",
    "image-cache",
    "uploads",
    "cache",
    "backups",
];

/// Files every account shares, when there is one.
pub const SHARED_FILES: &[&str] = &[
    "settings.json",
    "settings.local.json",
    "CLAUDE.md",
    "history.jsonl",
    "keybindings.json",
];

/// Never shared: what makes an account itself.
pub const OWN: &[&str] = &[".credentials.json", ".claude.json"];
const ACCOUNT_MARKER: &str = ".toomux-account";

/// The default group's folder.
pub fn shared_dir(home: &Path) -> PathBuf {
    group_dir(home, DEFAULT_GROUP)
}

pub const DEFAULT_GROUP: &str = "shared";

/// A group's folder: `~/.claude-shared`, or `~/.claude-shared-<group>`.
pub fn group_dir(home: &Path, group: &str) -> PathBuf {
    if group == DEFAULT_GROUP {
        home.join(".claude-shared")
    } else {
        home.join(format!(".claude-shared-{group}"))
    }
}

/// A group folder rather than an account.
pub fn is_group_dir(name: &str) -> bool {
    name == ".claude-shared" || name.starts_with(".claude-shared-")
}

/// Names are letters, digits, - and _.
pub fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("\"{name}\": a name is letters, digits, - and _");
    }
    if name == DEFAULT_GROUP || name.starts_with("shared-") {
        bail!("\"{name}\" is kept for group folders");
    }
    Ok(())
}

/// Resolve a user-supplied account root once, without following its final
/// symlink. Keeping the spelling of an existing symlink matters on macOS:
/// Claude Code's Keychain item is named after the configured folder path.
pub fn absolute_root(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("reading the current folder")?
            .join(path)
    };
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    bail!("account folder escapes the filesystem root");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

fn real_or_self(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    // Resolve the nearest existing ancestor, then append the missing tail.
    // Otherwise a new account below a symlinked parent could disguise HOME
    // or another protected location.
    let mut probe = path.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    while std::fs::symlink_metadata(&probe).is_err() {
        let Some(name) = probe.file_name().map(OsString::from) else {
            return path.to_path_buf();
        };
        tail.push(name);
        let Some(parent) = probe.parent() else {
            return path.to_path_buf();
        };
        probe = parent.to_path_buf();
    }
    let mut out = std::fs::canonicalize(&probe).unwrap_or(probe);
    for name in tail.into_iter().rev() {
        out.push(name);
    }
    out
}

/// Existing content that identifies a folder as Claude Code state. An empty
/// folder is also safe to adopt; toomux will populate its settings itself.
pub fn looks_like_account(path: &Path) -> bool {
    std::fs::symlink_metadata(path.join(ACCOUNT_MARKER)).is_ok()
        || OWN
            .iter()
            .chain(SHARED_DIRS)
            .chain(SHARED_FILES)
            .any(|name| std::fs::symlink_metadata(path.join(name)).is_ok())
}

fn strongly_looks_like_account(path: &Path) -> bool {
    std::fs::symlink_metadata(path.join(ACCOUNT_MARKER)).is_ok()
        || [
            ".credentials.json",
            ".claude.json",
            "projects",
            "sessions",
            "session-env",
        ]
        .iter()
        .any(|name| std::fs::symlink_metadata(path.join(name)).is_ok())
}

/// Mark a folder that toomux has deliberately adopted as an account. Later
/// recursive deletion can then distinguish it from an unrelated directory
/// that merely happens to contain a settings.json.
pub fn mark_account(path: &Path) -> Result<()> {
    use std::io::Write;
    let marker = path.join(ACCOUNT_MARKER);
    if marker.exists() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&marker)
        .with_context(|| format!("marking {} as a toomux account", path.display()))?;
    file.write_all(b"managed by toomux\n")?;
    Ok(())
}

fn empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

/// Validate an account root against broad or sensitive locations. protected
/// contains toomux's own config/state/data/runtime folders. Both an ancestor
/// of one of those folders and a child inside one are rejected.
///
/// Existing paths are compared by their real targets as well, so a symlink
/// cannot disguise HOME, /, or another protected location.
pub fn validate_root(path: &Path, home: &Path, protected: &[PathBuf]) -> Result<PathBuf> {
    let path = absolute_root(path)?;
    let real = real_or_self(&path);
    let home = real_or_self(home);
    let protected: Vec<PathBuf> = protected.iter().map(|p| real_or_self(p)).collect();

    let root = Path::new("/");
    if path == root
        || path.parent() == Some(root)
        || real == root
        || real.parent() == Some(root)
        || home == real
        || home.starts_with(&real)
        || protected
            .iter()
            .any(|p| p == &real || p.starts_with(&real) || real.starts_with(p))
    {
        bail!(
            "{} is too broad or contains protected toomux/user state; choose a dedicated Claude Code account folder",
            path.display()
        );
    }
    if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_group_dir)
    {
        bail!(
            "{} is a shared account group, not an account root",
            path.display()
        );
    }
    if path.exists() && !path.is_dir() {
        bail!("{} is a file, not a folder", path.display());
    }
    Ok(path)
}

/// An existing populated directory may only be adopted when it already looks
/// like a Claude Code config directory. This prevents account add --dir from
/// installing hooks/settings into an unrelated folder by mistake.
pub fn validate_adoption(path: &Path) -> Result<()> {
    if path.is_dir() && !empty_dir(path) && !looks_like_account(path) {
        bail!(
            "{} is not empty and does not look like a Claude Code account; choose an empty folder or an existing Claude config folder",
            path.display()
        );
    }
    Ok(())
}

/// Re-check immediately before recursive deletion. A configured path may have
/// been edited since it was added, so deletion does not trust configuration
/// provenance alone.
pub fn validate_delete(path: &Path, home: &Path, protected: &[PathBuf]) -> Result<PathBuf> {
    let path = validate_root(path, home, protected)?;
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!(
            "{} is a symlink; refusing recursive deletion of an ambiguous account root",
            path.display()
        );
    }
    if path.is_dir() && !empty_dir(&path) && !strongly_looks_like_account(&path) {
        bail!(
            "{} no longer looks like a Claude Code account; refusing recursive deletion",
            path.display()
        );
    }
    Ok(path)
}

/// The group folder an account's links point into; None when it stands alone.
pub fn group_of(account: &Path) -> Option<PathBuf> {
    SHARED_DIRS.iter().chain(SHARED_FILES).find_map(|n| {
        let at = account.join(n);
        is_link(&at)
            .then(|| {
                std::fs::canonicalize(&at)
                    .ok()?
                    .parent()
                    .map(Path::to_path_buf)
            })
            .flatten()
    })
}

/// One change `share` makes to an account.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// The account's own copy becomes the shared one.
    Move { from: PathBuf, to: PathBuf },
    /// Both have a folder: the account's files go into the shared one; a
    /// file both have with other contents is kept as `<name>.from-<account>`.
    Merge {
        from: PathBuf,
        to: PathBuf,
        clashes: usize,
    },
    /// Both have history: the account's lines are added to the shared file.
    Append { from: PathBuf, to: PathBuf },
    /// Both have settings that don't disagree: the shared file gets the
    /// keys only the account had.
    Combine { from: PathBuf, to: PathBuf },
    /// A shared folder nobody had yet.
    Create { dir: PathBuf },
    /// The account's entry becomes a link to the shared one.
    Link { at: PathBuf, to: PathBuf },
    /// A file both have, the same: the account's goes, the link replaces it.
    Same { at: PathBuf },
    /// A file both have with other contents: left as the account's own.
    Differs { at: PathBuf, shared: PathBuf },
}

fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Files under `from` that `to` has too, with other contents.
fn clashes(from: &Path, to: &Path) -> usize {
    let mut n = 0;
    for e in std::fs::read_dir(from).into_iter().flatten().flatten() {
        let (a, b) = (e.path(), to.join(e.file_name()));
        if !exists(&b) {
            continue;
        }
        if a.is_dir() && !is_link(&a) && b.is_dir() {
            n += clashes(&a, &b);
        } else if std::fs::read(&a).ok() != std::fs::read(&b).ok() {
            n += 1;
        }
    }
    n
}

/// What sharing `account` (a `~/.claude-<name>` folder) would change.
pub fn plan(account: &Path, shared: &Path) -> Vec<Step> {
    let mut steps = Vec::new();
    for name in SHARED_DIRS {
        let (at, to) = (account.join(name), shared.join(name));
        if is_link(&at) {
            continue;
        }
        if at.is_dir() {
            if to.is_dir() {
                steps.push(Step::Merge {
                    from: at.clone(),
                    to: to.clone(),
                    clashes: clashes(&at, &to),
                });
            } else {
                steps.push(Step::Move {
                    from: at.clone(),
                    to: to.clone(),
                });
            }
        } else if !to.is_dir() {
            steps.push(Step::Create { dir: to.clone() });
        }
        steps.push(Step::Link { at, to });
    }
    for name in SHARED_FILES {
        let (at, to) = (account.join(name), shared.join(name));
        if is_link(&at) {
            continue;
        }
        let mine = std::fs::read(&at).ok();
        let theirs = std::fs::read(&to).ok();
        match (mine, theirs) {
            (None, None) => continue,
            (Some(_), None) => steps.push(Step::Move {
                from: at.clone(),
                to: to.clone(),
            }),
            (Some(a), Some(b)) if a == b => steps.push(Step::Same { at: at.clone() }),
            (Some(_), Some(_)) if *name == "history.jsonl" => steps.push(Step::Append {
                from: at.clone(),
                to: to.clone(),
            }),
            (Some(a), Some(b)) if combined(&a, &b).is_some() => steps.push(Step::Combine {
                from: at.clone(),
                to: to.clone(),
            }),
            (Some(_), Some(_)) => {
                steps.push(Step::Differs { at, shared: to });
                continue;
            }
            (None, Some(_)) => {}
        }
        steps.push(Step::Link { at, to });
    }
    steps
}

/// Two JSON settings files as one, when no key has a different value in
/// each; None when they disagree (or aren't JSON objects).
fn combined(mine: &[u8], theirs: &[u8]) -> Option<serde_json::Value> {
    fn join(a: &serde_json::Value, b: &serde_json::Value) -> Option<serde_json::Value> {
        match (a, b) {
            (serde_json::Value::Object(x), serde_json::Value::Object(y)) => {
                let mut out = y.clone();
                for (k, v) in x {
                    let v = match y.get(k) {
                        Some(w) => join(v, w)?,
                        None => v.clone(),
                    };
                    out.insert(k.clone(), v);
                }
                Some(serde_json::Value::Object(out))
            }
            _ => (a == b).then(|| a.clone()),
        }
    }
    let a: serde_json::Value = serde_json::from_slice(mine).ok()?;
    let b: serde_json::Value = serde_json::from_slice(theirs).ok()?;
    if !a.is_object() || !b.is_object() {
        return None;
    }
    join(&a, &b)
}

/// `name.md` -> `name.from-work.md`; `name` -> `name.from-work`.
fn kept_name(name: &str, account: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}.from-{account}.{ext}"),
        _ => format!("{name}.from-{account}"),
    }
}

fn merge(from: &Path, to: &Path, account: &str) -> Result<()> {
    for e in std::fs::read_dir(from)?.flatten() {
        let (a, b) = (e.path(), to.join(e.file_name()));
        if !exists(&b) {
            std::fs::rename(&a, &b).with_context(|| format!("moving {}", a.display()))?;
        } else if a.is_dir() && !is_link(&a) && b.is_dir() {
            merge(&a, &b, account)?;
        } else if std::fs::read(&a).ok() == std::fs::read(&b).ok() {
            std::fs::remove_file(&a)?;
        } else {
            let kept = to.join(kept_name(&e.file_name().to_string_lossy(), account));
            std::fs::rename(&a, &kept)?;
        }
    }
    std::fs::remove_dir(from)
        .with_context(|| format!("{} still holds something after merging", from.display()))
}

/// Carry out a plan. `account` names clashing files kept from it.
pub fn apply(steps: &[Step], account: &str) -> Result<()> {
    for s in steps {
        match s {
            Step::Move { from, to } => {
                if let Some(p) = to.parent() {
                    std::fs::create_dir_all(p)?;
                }
                // A rename, so nothing is copied and nothing half-written:
                // the shared folder must be on the same disk.
                std::fs::rename(from, to)
                    .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
            }
            Step::Merge { from, to, .. } => merge(from, to, account)?,
            Step::Append { from, to } => {
                let mut text = std::fs::read_to_string(to)?;
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&std::fs::read_to_string(from)?);
                std::fs::write(to, text)?;
                std::fs::remove_file(from)?;
            }
            Step::Combine { from, to } => {
                let v = combined(&std::fs::read(from)?, &std::fs::read(to)?)
                    .context("settings disagree now")?;
                std::fs::write(to, serde_json::to_string_pretty(&v)? + "\n")?;
                std::fs::remove_file(from)?;
            }
            Step::Create { dir } => std::fs::create_dir_all(dir)?,
            Step::Same { at } => std::fs::remove_file(at)?,
            Step::Link { at, to } => {
                if exists(at) {
                    bail!(
                        "{} is still there; not replacing it with a link",
                        at.display()
                    );
                }
                std::os::unix::fs::symlink(to, at)
                    .with_context(|| format!("linking {}", at.display()))?;
            }
            Step::Differs { .. } => {}
        }
    }
    Ok(())
}

/// Leaving a group: each link becomes the account's own again.
#[derive(Debug, PartialEq)]
pub enum Leave {
    /// Its own copy of what it shared, so nothing it could see disappears.
    Copy { from: PathBuf, at: PathBuf },
    /// Starting empty: the link goes, and Claude Code makes it anew.
    Unlink { at: PathBuf },
}

/// Settings are copied even when starting fresh: an account without them
/// would lose its hooks, status line and permissions.
const ALWAYS_COPIED: &[&str] = &[
    "settings.json",
    "settings.local.json",
    "keybindings.json",
    "CLAUDE.md",
];

pub fn plan_leave(account: &Path, fresh: bool) -> Vec<Leave> {
    let mut out = Vec::new();
    for name in SHARED_DIRS.iter().chain(SHARED_FILES) {
        let at = account.join(name);
        if !is_link(&at) {
            continue;
        }
        match std::fs::canonicalize(&at) {
            Ok(from) if !fresh || ALWAYS_COPIED.contains(name) => {
                out.push(Leave::Copy { from, at })
            }
            _ => out.push(Leave::Unlink { at }),
        }
    }
    out
}

fn copy_all(from: &Path, to: &Path) -> Result<()> {
    let m = std::fs::symlink_metadata(from)?;
    if m.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)?.flatten() {
            copy_all(&e.path(), &to.join(e.file_name()))?;
        }
    } else if m.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
    } else {
        std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    }
    Ok(())
}

pub fn apply_leave(steps: &[Leave]) -> Result<()> {
    for s in steps {
        match s {
            Leave::Copy { from, at } => {
                // Copied beside the link first, so a failure leaves the link.
                let tmp = at.with_file_name(format!(
                    ".{}.toomux-copy",
                    at.file_name().unwrap_or_default().to_string_lossy()
                ));
                let _ = std::fs::remove_dir_all(&tmp);
                copy_all(from, &tmp)?;
                std::fs::remove_file(at)?;
                std::fs::rename(&tmp, at)?;
            }
            Leave::Unlink { at } => std::fs::remove_file(at)?,
        }
    }
    Ok(())
}

/// Bytes a leave would copy.
pub fn leave_size(steps: &[Leave]) -> u64 {
    fn du(p: &Path) -> u64 {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            return 0;
        };
        if !m.is_dir() {
            return m.len();
        }
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| du(&e.path()))
            .sum()
    }
    steps
        .iter()
        .map(|s| {
            if let Leave::Copy { from, .. } = s {
                du(from)
            } else {
                0
            }
        })
        .sum()
}

/// For `account list`: which listed entries are shared, which are the
/// account's own.
pub fn describe(account: &Path, shared: &Path) -> (Vec<String>, Vec<String>) {
    let (mut linked, mut own) = (Vec::new(), Vec::new());
    for name in SHARED_DIRS.iter().chain(SHARED_FILES) {
        let at = account.join(name);
        if is_link(&at)
            && std::fs::canonicalize(&at).ok().is_some_and(|t| {
                t.parent()
                    .is_some_and(|p| std::fs::canonicalize(shared).ok().as_deref() == Some(p))
            })
        {
            linked.push(name.to_string());
        } else if exists(&at) {
            own.push(name.to_string());
        }
    }
    (linked, own)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rig(name: &str) -> PathBuf {
        let h = std::env::temp_dir().join(format!("toomux-accounts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(&h).unwrap();
        h
    }

    #[test]
    fn an_account_joins_and_leaves_losing_nothing() {
        let h = rig("leave");
        let shared = group_dir(&h, "family");
        assert_eq!(shared, h.join(".claude-shared-family"));
        let (a, b) = (h.join("claude-a"), h.join(".claude-b"));
        for acct in [&a, &b] {
            std::fs::create_dir_all(acct.join("projects/-w")).unwrap();
            std::fs::write(acct.join(".credentials.json"), "x").unwrap();
        }
        std::fs::write(a.join("projects/-w/a.jsonl"), "a").unwrap();
        std::fs::write(a.join("settings.json"), "{}").unwrap();
        std::fs::write(b.join("projects/-w/b.jsonl"), "b").unwrap();
        apply(&plan(&a, &shared), "a").unwrap();
        apply(&plan(&b, &shared), "b").unwrap();
        assert_eq!(
            group_of(&a).unwrap(),
            std::fs::canonicalize(&shared).unwrap()
        );
        assert!(
            plan(&a, &shared).is_empty(),
            "joining twice changes nothing"
        );

        // b leaves with its own copy: it still sees a's conversation too.
        apply_leave(&plan_leave(&b, false)).unwrap();
        assert!(group_of(&b).is_none() && !is_link(&b.join("projects")));
        assert!(b.join("projects/-w/a.jsonl").is_file() && b.join("projects/-w/b.jsonl").is_file());
        assert!(
            shared.join("projects/-w/b.jsonl").is_file(),
            "the group keeps it too"
        );

        // b comes back: its copy merges in again, nothing doubled.
        apply(&plan(&b, &shared), "b").unwrap();
        assert!(
            is_link(&b.join("projects")) && !shared.join("projects/-w/a.from-b.jsonl").exists()
        );
        apply_leave(&plan_leave(&b, false)).unwrap();

        // a leaves fresh: no history, but its settings come along.
        let steps = plan_leave(&a, true);
        assert!(steps.contains(&Leave::Unlink {
            at: a.join("projects")
        }));
        apply_leave(&steps).unwrap();
        assert!(
            !exists(&a.join("projects"))
                && a.join("settings.json").is_file()
                && !is_link(&a.join("settings.json"))
        );
        assert_eq!(
            std::fs::read_to_string(a.join(".credentials.json")).unwrap(),
            "x"
        );
        let _ = std::fs::remove_dir_all(h);
    }

    #[test]
    fn names_and_group_folders() {
        assert!(
            valid_name("home-2").is_ok()
                && valid_name("a b").is_err()
                && valid_name("shared").is_err()
        );
        assert!(
            is_group_dir(".claude-shared")
                && is_group_dir(".claude-shared-x")
                && !is_group_dir(".claude-sharedx")
        );
    }

    #[test]
    fn account_roots_cannot_be_broad_protected_or_disguised() {
        let h = rig("roots");
        let home = h.join("home");
        let config = home.join(".config/toomux");
        let state = home.join(".local/state/toomux");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let protected = vec![config.clone(), state.clone()];

        assert!(validate_root(Path::new("/"), &home, &protected).is_err());
        assert!(validate_root(Path::new("/tmp"), &home, &protected).is_err());
        assert!(validate_root(&home, &home, &protected).is_err());
        assert!(validate_root(&home.join(".config"), &home, &protected).is_err());
        assert!(validate_root(&config.join("account"), &home, &protected).is_err());
        assert!(validate_root(&home.join(".claude-shared"), &home, &protected).is_err());
        assert!(validate_root(&home.join(".claude-work"), &home, &protected).is_ok());

        let alias = h.join("home-alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        assert!(validate_root(&alias, &home, &protected).is_err());
        assert!(validate_root(&alias.join("new-account"), &home, &protected).is_ok());
        let protected_alias = h.join("protected-alias");
        std::os::unix::fs::symlink(&config, &protected_alias).unwrap();
        assert!(validate_root(&protected_alias.join("new-account"), &home, &protected).is_err());
        let _ = std::fs::remove_dir_all(h);
    }

    #[test]
    fn only_empty_or_claude_shaped_folders_are_adopted_or_deleted() {
        let h = rig("adopt");
        let empty = h.join("empty");
        let unrelated = h.join("documents");
        let claude = h.join("claude");
        for p in [&empty, &unrelated, &claude] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(unrelated.join("notes.txt"), "important").unwrap();
        std::fs::write(claude.join("settings.json"), "{}").unwrap();

        assert!(validate_adoption(&empty).is_ok());
        assert!(validate_adoption(&unrelated).is_err());
        assert!(validate_adoption(&claude).is_ok());
        assert!(!looks_like_account(&unrelated));
        assert!(looks_like_account(&claude));

        let home = h.join("home");
        std::fs::create_dir_all(&home).unwrap();
        assert!(validate_delete(&unrelated, &home, &[]).is_err());
        assert!(validate_delete(&empty, &home, &[]).is_ok());
        assert!(
            validate_delete(&claude, &home, &[]).is_err(),
            "settings alone are not enough to authorize recursive deletion"
        );
        mark_account(&claude).unwrap();
        assert!(validate_delete(&claude, &home, &[]).is_ok());
        let alias = h.join("claude-alias");
        std::os::unix::fs::symlink(&claude, &alias).unwrap();
        assert!(validate_delete(&alias, &home, &[]).is_err());
        let _ = std::fs::remove_dir_all(h);
    }

    #[test]
    fn two_existing_accounts_come_together_losing_nothing() {
        let h = rig("merge");
        let shared = shared_dir(&h);
        let (a, b) = (h.join(".claude-a"), h.join(".claude-b"));
        for (acct, text) in [(&a, "a's"), (&b, "b's")] {
            std::fs::create_dir_all(acct.join("projects/-w/memory")).unwrap();
            std::fs::write(acct.join("projects/-w/memory/MEMORY.md"), text).unwrap();
            std::fs::write(acct.join(format!("projects/-w/{text}.jsonl")), "{}").unwrap();
            std::fs::write(acct.join("projects/-w/memory/same.md"), "same").unwrap();
            std::fs::write(acct.join("history.jsonl"), format!("{text}\n")).unwrap();
            std::fs::write(acct.join("settings.json"), format!("{{\"x\":\"{text}\"}}")).unwrap();
            std::fs::write(acct.join(".credentials.json"), text).unwrap();
        }
        apply(&plan(&a, &shared), "a").unwrap();
        let steps = plan(&b, &shared);
        assert!(steps.contains(&Step::Merge {
            from: b.join("projects"),
            to: shared.join("projects"),
            clashes: 1
        }));
        assert!(
            steps
                .iter()
                .any(|s| matches!(s, Step::Differs { at, .. } if at.ends_with("settings.json")))
        );
        apply(&steps, "b").unwrap();
        let mem = shared.join("projects/-w/memory");
        assert_eq!(
            std::fs::read_to_string(mem.join("MEMORY.md")).unwrap(),
            "a's"
        );
        assert_eq!(
            std::fs::read_to_string(mem.join("MEMORY.from-b.md")).unwrap(),
            "b's"
        );
        assert!(
            !mem.join("same.from-b.md").exists(),
            "identical files aren't doubled"
        );
        assert!(shared.join("projects/-w/b's.jsonl").exists());
        assert_eq!(
            std::fs::read_to_string(shared.join("history.jsonl")).unwrap(),
            "a's\nb's\n"
        );
        assert!(
            !is_link(&b.join("settings.json")),
            "settings that differ stay the account's own"
        );
        assert_eq!(
            std::fs::read_to_string(b.join(".credentials.json")).unwrap(),
            "b's"
        );
        let (linked, own) = describe(&b, &shared);
        assert!(
            linked.contains(&"projects".to_string()) && own == vec!["settings.json".to_string()]
        );
        let _ = std::fs::remove_dir_all(h);
    }

    #[test]
    fn settings_combine_unless_they_disagree() {
        let v = combined(
            br#"{"model":"opus","hooks":{"Stop":[1]}}"#,
            br#"{"hooks":{"Stop":[1]},"statusLine":{"c":"t"}}"#,
        )
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"model":"opus","hooks":{"Stop":[1]},"statusLine":{"c":"t"}})
        );
        assert!(combined(br#"{"model":"opus"}"#, br#"{"model":"sonnet"}"#).is_none());
        assert!(
            combined(br#"{"hooks":{"Stop":[1]}}"#, br#"{"hooks":{"Stop":[2]}}"#).is_none(),
            "lists must match"
        );
        assert!(combined(b"not json", b"{}").is_none());
    }

    #[test]
    fn kept_names_keep_their_extension() {
        assert_eq!(kept_name("MEMORY.md", "b"), "MEMORY.from-b.md");
        assert_eq!(kept_name("notes", "b"), "notes.from-b");
        assert_eq!(kept_name(".hidden", "b"), ".hidden.from-b");
    }
}
