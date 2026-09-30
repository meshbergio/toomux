//! Upkeep of what agents are told: Claude Code's memory folders, checked
//! against the disk.
//!
//! - Which workspace each memory folder belongs to. Claude Code names the
//!   folder after the workspace's path with every character but letters,
//!   digits and `-` made `-`, and deletes the transcripts that record the
//!   path after 30 days; so folders are matched against the folders on disk.
//! - A workspace that moved (its old folder gone, one clear new home): its
//!   memory is brought to the new home's memory folder, where sessions there
//!   load it. The old folder is left as it is.
//! - Paths a memory file cites that are gone. When a whole folder under home
//!   moved and exactly one folder of that name now holds the rest of the
//!   path, the path is corrected; otherwise it's listed, and search results
//!   say so. Temporary places (tmp worktrees, caches) are expected to go.
//!
//! Every file changed here is kept in the archive first, as it was.

use crate::config::Config;
use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// How deep under home workspaces are looked for.
const DEPTH: usize = 7;

/// Claude Code's folder name for a workspace path.
pub fn mangle(path: &str) -> String {
    path.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect()
}

/// The folders under home, by Claude Code's name for them and by their own.
pub struct Places {
    home: PathBuf,
    /// Folders Claude Code has run in (their project folders' names).
    worked: std::collections::HashSet<String>,
    by_folder: HashMap<String, PathBuf>,
    by_name: HashMap<String, Vec<PathBuf>>,
}

impl Places {
    pub fn scan(home: &Path, roots: &[PathBuf]) -> Self {
        let worked = roots
            .iter()
            .flat_map(|r| std::fs::read_dir(r).into_iter().flatten().flatten())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let mut p = Places { home: home.to_path_buf(), worked, by_folder: HashMap::new(), by_name: HashMap::new() };
        p.add(home);
        p.by_folder.insert("-".into(), PathBuf::from("/"));
        let mut stack = vec![(home.to_path_buf(), 0)];
        while let Some((d, depth)) = stack.pop() {
            if depth >= DEPTH {
                continue;
            }
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || matches!(name.as_str(), "node_modules" | "target" | "venv" | "__pycache__") {
                    continue;
                }
                let Ok(kind) = e.file_type() else { continue };
                if kind.is_dir() {
                    p.add(&e.path());
                    stack.push((e.path(), depth + 1));
                } else if kind.is_symlink() && e.path().is_dir() {
                    // A folder by another name: live, but not walked twice.
                    p.by_folder.insert(mangle(&e.path().to_string_lossy()), e.path());
                }
            }
        }
        p
    }

    /// Every repository's main folder (each once, whatever it's linked as).
    pub fn repos(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .by_folder
            .values()
            .filter(|d| d.join(".git").is_dir())
            .map(|d| std::fs::canonicalize(d).unwrap_or_else(|_| d.to_path_buf()))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn add(&mut self, d: &Path) {
        self.by_folder.insert(mangle(&d.to_string_lossy()), d.to_path_buf());
        if let Some(n) = d.file_name() {
            self.by_name.entry(n.to_string_lossy().into_owned()).or_default().push(d.to_path_buf());
        }
    }

    /// The workspace a memory folder's project folder belongs to, if it's there.
    pub fn live(&self, folder: &str) -> Option<&PathBuf> {
        self.by_folder.get(folder)
    }

    /// Where a workspace that's gone went: among folders that look like a
    /// workspace (a repository, or somewhere Claude Code has run), the one
    /// whose path shares the longest tail with the old one (its own name at
    /// least), if only one does.
    pub fn moved(&self, folder: &str) -> Option<&PathBuf> {
        if self.live(folder).is_some() {
            return None;
        }
        let mut best: Vec<(&PathBuf, usize)> = Vec::new();
        for (m, d) in &self.by_folder {
            let own = d.file_name().map(|n| mangle(&n.to_string_lossy())).unwrap_or_default();
            if own.len() < 4 || !folder.ends_with(&format!("-{own}")) || *d == self.home {
                continue;
            }
            if !d.join(".git").exists() && !self.worked.contains(m) {
                continue;
            }
            let shared = m.chars().rev().zip(folder.chars().rev()).take_while(|(a, b)| a == b).count();
            match best.first() {
                Some((_, s)) if shared < *s => {}
                Some((_, s)) if shared == *s => best.push((d, shared)),
                _ => best = vec![(d, shared)],
            }
        }
        (best.len() == 1).then(|| best[0].0)
    }

    /// A path whose first folder under home moved: where it is now, if
    /// exactly one folder of that name holds the rest of it. The rest must
    /// be there: a name alone (`~/go`) says nothing about where it went.
    pub fn relocate(&self, gone: &Path) -> Option<PathBuf> {
        let rest = gone.strip_prefix(&self.home).ok()?;
        let mut parts = rest.components();
        let top = parts.next()?.as_os_str().to_string_lossy().into_owned();
        if self.home.join(&top).exists() {
            return None;
        }
        let tail = parts.as_path();
        if tail.as_os_str().is_empty() {
            return None;
        }
        let found: Vec<PathBuf> = self
            .by_name
            .get(&top)?
            .iter()
            .map(|d| d.join(tail))
            .filter(|p| p.exists())
            .collect();
        (found.len() == 1).then(|| found.into_iter().next().unwrap())
    }
}

static PATH_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?:~|/home/[A-Za-z0-9._-]+)/[^\s`'"()<>\[\]{}*|,;]+"#).expect("path pattern"));

/// A cited path worth checking, trimmed of the sentence around it; None for
/// fragments, placeholders and other users' homes.
fn cited(raw: &str, home: &Path) -> Option<PathBuf> {
    let t = raw.trim_end_matches(['.', ':', '/', '!', '?']);
    if t.contains(['$', '…', '\\']) || t.contains("...") || t.ends_with(['-', '_']) {
        return None;
    }
    let p = match t.strip_prefix("~/") {
        Some(r) => home.join(r),
        None => PathBuf::from(t),
    };
    p.starts_with(home).then_some(p)
}

/// Whether a path that stopped at a space goes on past it: `~/AI Brain/x`
/// is matched as `~/AI`, which is not gone when `~/AI Brain` is there.
fn spaced(p: &Path, after: &str) -> bool {
    let Some(rest) = after.strip_prefix(' ') else { return false };
    let word: String = rest.chars().take_while(|c| !c.is_whitespace() && !"/`'\"()<>[]{}*|,;".contains(*c)).collect();
    !word.is_empty() && PathBuf::from(format!("{} {word}", p.display())).exists()
}

/// Places that are meant to go: scratch worktrees, caches, build output.
fn temporary(p: &Path, home: &Path) -> bool {
    p.strip_prefix(home).unwrap_or(p).components().any(|c| {
        let c = c.as_os_str().to_string_lossy();
        c.starts_with("tmp") || c.starts_with(".tmp") || matches!(c.as_ref(), ".cache" | "target" | "tmp" | "scratchpad")
    })
}

/// What a pass found and did.
#[derive(Default, Debug)]
pub struct Report {
    pub files: usize,
    pub cited: usize,
    /// Memory file -> the paths it cites that are gone (not temporary ones).
    pub gone: BTreeMap<String, Vec<String>>,
    pub temporary: usize,
    /// (file, old, new) paths corrected.
    pub fixed: Vec<(String, String, String)>,
    /// (old folder, new home, files brought).
    pub moved: Vec<(String, String, usize)>,
    /// Memory folders whose workspace is gone and can't be placed.
    pub homeless: Vec<String>,
    /// Memory folders whose workspace is gone but which were kept on purpose.
    pub archives: Vec<String>,
}

/// The folders under home, and where Claude Code has run.
pub fn places(cfg: &Config) -> Places {
    Places::scan(&crate::config::home(), &crate::index::roots(cfg))
}

/// One pass over memory; with `apply` false it only says what it would do.
pub fn run(cfg: &Config, places: &Places, apply: bool) -> Result<Report> {
    let home = crate::config::home();
    let roots = crate::index::roots(cfg);
    let r = run_in(&roots, places, &home, &crate::archive::dir(), apply && cfg.fix_memory)?;
    if apply {
        save_gone(&r.gone);
    }
    Ok(r)
}

fn run_in(roots: &[PathBuf], places: &Places, home: &Path, archive: &Path, apply: bool) -> Result<Report> {
    let mut r = Report::default();
    let cited = all_memory_text(roots);
    for root in roots {
        let mut folders: Vec<_> = std::fs::read_dir(root).into_iter().flatten().flatten().filter(|p| p.path().join("memory").is_dir()).collect();
        folders.sort_by_key(|f| f.file_name());
        for f in &folders {
            let name = f.file_name().to_string_lossy().into_owned();
            let recorded = crate::index::recorded_cwd(&f.path()).is_some_and(|c| Path::new(&c).is_dir());
            if recorded || places.live(&name).is_some() {
                continue;
            }
            if archived(&f.path().join("memory"), &name, &cited) {
                r.archives.push(name);
                continue;
            }
            match places.moved(&name) {
                Some(to) => {
                    let n = bring(root, &name, &mangle(&to.to_string_lossy()), archive, apply)?;
                    if n > 0 {
                        r.moved.push((name, to.display().to_string(), n));
                    }
                }
                None => r.homeless.push(name),
            }
        }
        for f in &folders {
            for e in std::fs::read_dir(f.path().join("memory")).into_iter().flatten().flatten() {
                let path = e.path();
                if path.extension().is_none_or(|x| x != "md") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                r.files += 1;
                check(root, &path, &text, places, home, archive, apply, &mut r)?;
            }
        }
    }
    Ok(r)
}

/// Every memory file's text, to see which folders live memory still points to.
fn all_memory_text(roots: &[PathBuf]) -> String {
    let mut all = String::new();
    for root in roots {
        for f in std::fs::read_dir(root).into_iter().flatten().flatten() {
            for e in std::fs::read_dir(f.path().join("memory")).into_iter().flatten().flatten() {
                if e.path().extension().is_some_and(|x| x == "md") {
                    all.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
                    all.push('\n');
                }
            }
        }
    }
    all
}

/// A folder kept on purpose after its workspace went: it says where its
/// memory migrated to (MIGRATED_TO_*.md), or other memory cites its files for
/// the full detail. It stays where those pointers expect it.
fn archived(dir: &Path, name: &str, cited: &str) -> bool {
    let marked = std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
        let n = e.file_name().to_string_lossy().to_uppercase();
        n.starts_with("MIGRATED_TO") || n.starts_with("MOVED_TO")
    });
    marked || cited.contains(&format!("{name}/memory/"))
}

/// Check one memory file's paths, correcting the ones that moved.
#[allow(clippy::too_many_arguments)]
fn check(root: &Path, path: &Path, text: &str, places: &Places, home: &Path, archive: &Path, apply: bool, r: &mut Report) -> Result<()> {
    let key = path.display().to_string();
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut gone = Vec::new();
    for m in PATH_REF.find_iter(text) {
        let Some(p) = cited(m.as_str(), home) else { continue };
        r.cited += 1;
        if p.exists() || spaced(&p, &text[m.end()..]) {
            continue;
        }
        if temporary(&p, home) {
            r.temporary += 1;
            continue;
        }
        // A path with a space in it can't be written back into prose and read
        // again as one path.
        match places.relocate(&p).filter(|n| !n.to_string_lossy().contains(char::is_whitespace)) {
            Some(now) => {
                // Written as it was: with ~ if it was, and the trimmed end kept.
                let old = m.as_str();
                let trimmed = old.trim_end_matches(['.', ':', '/', '!', '?']);
                let shown = match trimmed.strip_prefix("~/") {
                    Some(_) => format!("~/{}", now.strip_prefix(home).unwrap_or(&now).display()),
                    None => now.display().to_string(),
                };
                edits.push((m.start()..m.start() + trimmed.len(), shown.clone()));
                r.fixed.push((key.clone(), trimmed.to_string(), shown));
            }
            None => gone.push(p.display().to_string()),
        }
    }
    if !gone.is_empty() {
        gone.sort();
        gone.dedup();
        r.gone.insert(key, gone);
    }
    if apply && !edits.is_empty() {
        let mut out = text.to_string();
        for (range, new) in edits.into_iter().rev() {
            out.replace_range(range, &new);
        }
        crate::archive::keep_version(root, path, text, archive)?;
        write_as(path, &out)?;
    }
    Ok(())
}

/// Bring a moved workspace's memory files to its new home's memory folder.
/// A file already there with the same text is skipped; one with other text
/// comes in beside it, named after where it came from. The old index comes
/// in as a topic file of its own, with a line for it in the new index.
/// Returns how many files came.
fn bring(root: &Path, from: &str, to: &str, archive: &Path, apply: bool) -> Result<usize> {
    let src = root.join(from).join("memory");
    let dst = root.join(to).join("memory");
    if apply {
        std::fs::create_dir_all(&dst)?;
    }
    let tag = from.trim_start_matches('-').rsplit('-').next().unwrap_or("old").to_ascii_lowercase();
    let mut renamed: Vec<(String, String)> = Vec::new();
    let mut brought = 0;
    let mut names: Vec<_> = std::fs::read_dir(&src)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    for name in names.iter().filter(|n| n.ends_with(".md") && *n != "MEMORY.md") {
        let text = std::fs::read_to_string(src.join(name))?;
        let mut target = dst.join(name);
        if let Ok(there) = std::fs::read_to_string(&target) {
            if there == text {
                continue;
            }
            let other = format!("{}.from-{tag}.md", name.trim_end_matches(".md"));
            target = dst.join(&other);
            renamed.push((name.clone(), other));
            if std::fs::read_to_string(&target).is_ok_and(|t| t == text) {
                continue;
            }
        }
        if apply {
            write_as(&target, &text)?;
        }
        brought += 1;
    }
    // The old index, whole, as a topic file; its links follow any renames.
    let old_index = std::fs::read_to_string(src.join("MEMORY.md")).unwrap_or_default();
    if !old_index.trim().is_empty() {
        let topic = format!("moved-from-{}.md", from.trim_start_matches('-'));
        let mut body = old_index.clone();
        for (a, b) in &renamed {
            body = body.replace(&format!("({a})"), &format!("({b})"));
        }
        let text = format!(
            "---\nname: moved-from-{tag}\ndescription: the index of this workspace's memory from before it moved ({from})\nmetadata:\n  type: reference\n---\n\n{body}"
        );
        if std::fs::read_to_string(dst.join(&topic)).ok().as_deref() != Some(text.as_str()) {
            brought += 1;
            if !apply {
                return Ok(brought);
            }
            write_as(&dst.join(&topic), &text)?;
            let index = dst.join("MEMORY.md");
            let now = std::fs::read_to_string(&index).unwrap_or_default();
            if !now.contains(&topic) {
                if !now.is_empty() {
                    crate::archive::keep_version(root, &index, &now, archive)?;
                }
                let line = format!("- [Before the move]({topic}) — this workspace's memory index from {from}\n");
                let joined = if now.is_empty() || now.ends_with('\n') { format!("{now}{line}") } else { format!("{now}\n{line}") };
                write_as(&index, &joined)?;
            }
        }
    }
    Ok(brought)
}

fn write_as(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension(format!("md.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---- what search shows -----------------------------------------------------

fn gone_path() -> PathBuf {
    crate::memory::path().with_file_name("gone.json")
}

#[derive(Serialize, Deserialize, Default)]
struct Gone {
    files: BTreeMap<String, Vec<String>>,
}

fn save_gone(g: &BTreeMap<String, Vec<String>>) {
    let tmp = gone_path().with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_string(&Gone { files: g.clone() }).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, gone_path());
    }
}

/// The paths a memory file cites that were gone at the last upkeep.
pub fn gone_in(file: &str) -> Vec<String> {
    let g: Gone = std::fs::read_to_string(gone_path()).ok().and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default();
    g.files.get(file).cloned().unwrap_or_default()
}

/// Memory files under `prefix` citing paths that are gone, with those paths.
pub fn gone_files(prefix: &str) -> Vec<(String, Vec<String>)> {
    let g: Gone = std::fs::read_to_string(gone_path()).ok().and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default();
    g.files.into_iter().filter(|(f, _)| f.starts_with(prefix)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rig(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let tmp = std::env::temp_dir().join(format!("toomux-upkeep-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let home = tmp.join("home");
        let root = tmp.join("acct/projects");
        let archive = tmp.join("archive");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        (tmp, home, root, archive)
    }

    #[test]
    fn folder_names_are_claude_codes() {
        assert_eq!(mangle("/home/sam/AI Brain/workspaces"), "-home-sam-AI-Brain-workspaces");
        assert_eq!(mangle("/home/sam/Northwind_Platform_Workspace"), "-home-sam-Northwind-Platform-Workspace");
    }

    #[test]
    fn a_kept_archive_is_neither_moved_nor_listed() {
        let (tmp, _, root, _) = rig("archive");
        let marked = root.join("-home-sam-old/memory");
        std::fs::create_dir_all(&marked).unwrap();
        std::fs::write(marked.join("MIGRATED_TO_NEW.md"), "moved").unwrap();
        let cited = root.join("-home-sam-older/memory");
        std::fs::create_dir_all(&cited).unwrap();
        let live = root.join("-home-sam-new/memory");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("t.md"), "full detail: ~/.claude/projects/-home-sam-older/memory/t.md").unwrap();
        let text = all_memory_text(&[root.clone()]);
        assert!(archived(&marked, "-home-sam-old", &text));
        assert!(archived(&cited, "-home-sam-older", &text));
        assert!(!archived(&live, "-home-sam-ne", &text), "a name that is only a prefix of another's");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn a_path_with_a_space_is_not_taken_for_gone() {
        let (tmp, home, _, _) = rig("spaced");
        std::fs::create_dir_all(home.join("AI Brain/work")).unwrap();
        assert!(spaced(&home.join("AI"), " Brain/work`"));
        assert!(!spaced(&home.join("AI"), " Mind/work"), "no such folder");
        assert!(!spaced(&home.join("AI"), "/Brain"), "no space after it");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn a_moved_folder_is_found_and_its_paths_corrected() {
        let (tmp, home, root, archive) = rig("move");
        std::fs::create_dir_all(home.join("storage/work/Cloud_App/src")).unwrap();
        std::fs::write(home.join("storage/work/Cloud_App/src/tokens.css"), "").unwrap();
        let ws = home.join("storage/work/Cloud_App");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        // A folder of the same name that isn't a workspace doesn't count.
        std::fs::create_dir_all(home.join("sdk/lib/Cloud_App")).unwrap();
        let places = Places::scan(&home, &[root.clone()]);
        let folder = mangle(&home.join("Cloud_App").to_string_lossy());
        assert_eq!(places.moved(&folder), Some(&ws), "its old name's tail finds it");
        assert!(places.moved(&mangle(&ws.to_string_lossy())).is_none(), "a live workspace hasn't moved");
        let mem = root.join(mangle(&ws.to_string_lossy())).join("memory");
        std::fs::create_dir_all(&mem).unwrap();
        let note = mem.join("styles.md");
        let text = "Tokens live in `~/Cloud_App/src/tokens.css`. Bench output was in ~/tmp-wt-bench/out.\nOld notes: ~/Gone_Folder/x.md.\n";
        std::fs::write(&note, text).unwrap();
        let r = run_in(&[root.clone()], &places, &home, &archive, true).unwrap();
        let now = std::fs::read_to_string(&note).unwrap();
        assert!(now.contains("`~/storage/work/Cloud_App/src/tokens.css`"), "{now}");
        assert_eq!(r.fixed.len(), 1);
        assert_eq!(r.temporary, 1, "a tmp worktree is expected to go");
        assert_eq!(r.gone.values().flatten().count(), 1, "what can't be placed is listed: {:?}", r.gone);
        let kept = crate::archive::versions(&archive, &root, &note);
        assert_eq!(kept.len(), 1, "the file as it was is in the archive");
        assert_eq!(crate::archive::read_version(&kept[0]).unwrap(), text);
        let again = run_in(&[root.clone()], &places, &home, &archive, true).unwrap();
        assert!(again.fixed.is_empty(), "nothing left to correct");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn a_moved_workspace_brings_its_memory_to_its_new_home() {
        let (tmp, home, root, archive) = rig("bring");
        let new_home = home.join("storage/projects/Platform_Workspace");
        std::fs::create_dir_all(new_home.join(".git")).unwrap();
        let old = root.join(mangle(&home.join("Platform_Workspace").to_string_lossy())).join("memory");
        let new = root.join(mangle(&new_home.to_string_lossy())).join("memory");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("MEMORY.md"), "- [Deploys](deploys.md) — how\n- [Same](same.md) — same\n").unwrap();
        std::fs::write(old.join("deploys.md"), "old deploy notes").unwrap();
        std::fs::write(old.join("same.md"), "identical").unwrap();
        std::fs::write(new.join("deploys.md"), "new deploy notes").unwrap();
        std::fs::write(new.join("same.md"), "identical").unwrap();
        std::fs::write(new.join("MEMORY.md"), "# Rules\n- be calm\n").unwrap();
        let places = Places::scan(&home, &[root.clone()]);
        let r = run_in(&[root.clone()], &places, &home, &archive, true).unwrap();
        assert_eq!(r.moved.len(), 1, "{:?}", r);
        assert_eq!(std::fs::read_to_string(new.join("deploys.md")).unwrap(), "new deploy notes", "what's there is kept");
        let from = std::fs::read_dir(&new).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert!(from.iter().any(|n| n.starts_with("deploys.from-") && n.ends_with(".md")), "{from:?}");
        let topic = from.iter().find(|n| n.starts_with("moved-from-")).expect("the old index comes as a topic");
        let body = std::fs::read_to_string(new.join(topic)).unwrap();
        assert!(body.contains("(deploys.from-") && body.contains("(same.md)"), "links follow renames: {body}");
        let index = std::fs::read_to_string(new.join("MEMORY.md")).unwrap();
        assert!(index.starts_with("# Rules\n- be calm\n") && index.contains(topic.as_str()), "{index}");
        assert!(old.join("deploys.md").exists(), "the old folder is left as it is");
        let again = run_in(&[root.clone()], &places, &home, &archive, true).unwrap();
        assert!(again.moved.is_empty(), "brought once: {:?}", again.moved);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn a_lookalike_that_isnt_a_workspace_is_not_a_new_home() {
        let (tmp, home, root, _) = rig("lookalike");
        std::fs::create_dir_all(home.join("android-sdk/lib/analytics-library/tracker")).unwrap();
        let places = Places::scan(&home, &[root.clone()]);
        assert!(places.moved(&mangle(&home.join(".local/state/ai-usage-tracker").to_string_lossy())).is_none());
        std::fs::create_dir_all(home.join("a")).unwrap();
        std::os::unix::fs::symlink(home.join("a"), home.join("link")).unwrap();
        std::fs::create_dir_all(home.join("x/deep/go")).unwrap();
        let places = Places::scan(&home, &[root.clone()]);
        assert!(places.relocate(&home.join("go")).is_none(), "a bare name is no evidence");
        assert!(places.live(&mangle(&home.join("link").to_string_lossy())).is_some(), "a linked folder is live");
        let _ = std::fs::remove_dir_all(tmp);
    }
}
