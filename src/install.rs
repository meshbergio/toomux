use anyhow::{Context, Result, bail};
use std::path::{Component, Path, PathBuf};

const FORMAT: &str = "1";
const FORMAT_FILE: &str = ".installer-format";
const BIN_DIR_FILE: &str = ".bin-dir";
const MANAGED_TMUX_FILE: &str = ".managed-tmux";
const PREVIOUS_LAUNCHER_FILE: &str = ".previous-toomux-launcher";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleInstall {
    pub root: PathBuf,
    pub version_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub managed_tmux: bool,
}

fn clean_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn safe_version_target(root: &Path, target: &Path) -> Option<PathBuf> {
    if target.is_absolute() {
        return None;
    }
    let mut parts = target.components();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Component::Normal(a)), Some(Component::Normal(b)), None)
            if a == "versions" && !b.is_empty() =>
        {
            let dir = root.join(target);
            (dir.is_dir() && dir.join("manifest.txt").is_file()).then_some(dir)
        }
        _ => None,
    }
}

fn safe_root(root: &Path) -> bool {
    if !root.is_absolute() || root == Path::new("/") {
        return false;
    }
    let Some(home) = dirs::home_dir() else {
        return true;
    };
    root != home
        && root != home.join(".local")
        && root != home.join(".local/lib")
        && root != home.join(".local/share")
}

fn from_exe(exe: &Path) -> Option<BundleInstall> {
    let bin = exe.parent()?;
    (bin.file_name()?.to_str()? == "bin").then_some(())?;
    let version_dir = bin.parent()?.to_path_buf();
    let versions = version_dir.parent()?;
    (versions.file_name()?.to_str()? == "versions").then_some(())?;
    let root = versions.parent()?.to_path_buf();
    if !safe_root(&root) || clean_text(&root.join(FORMAT_FILE)).as_deref() != Some(FORMAT) {
        return None;
    }
    let bin_dir = PathBuf::from(clean_text(&root.join(BIN_DIR_FILE))?);
    if !bin_dir.is_absolute() {
        return None;
    }
    let managed_tmux = clean_text(&root.join(MANAGED_TMUX_FILE)).is_some_and(|v| v.as_str() == "1");
    Some(BundleInstall {
        root,
        version_dir,
        bin_dir,
        managed_tmux,
    })
}

pub fn current() -> Option<BundleInstall> {
    let exe = std::env::current_exe().ok()?;
    from_exe(&std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn symlink_target_owned(path: &Path, root: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.file_type().is_symlink() {
        return false;
    }
    let Ok(target) = std::fs::read_link(path) else {
        return false;
    };
    let resolved = if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or_else(|| Path::new("/")).join(target)
    };
    let resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    resolved.starts_with(root)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

fn atomic_symlink(root: &Path, name: &str, target: &Path) -> Result<()> {
    let tmp = root.join(format!(".{name}.new.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    symlink(target, &tmp).with_context(|| format!("creating {}", tmp.display()))?;
    std::fs::rename(&tmp, root.join(name))
        .with_context(|| format!("switching {name} in {}", root.display()))
}

pub fn rollback() -> Result<String> {
    let install = current().context(
        "rollback is available for the self-contained installer; package-manager installs roll back with their package manager",
    )?;
    let current_link = std::fs::read_link(install.root.join("current"))
        .context("the self-contained install has no current bundle link")?;
    let previous_link = std::fs::read_link(install.root.join("previous"))
        .context("there is no previous verified bundle to roll back to")?;
    let current_dir = safe_version_target(&install.root, &current_link)
        .context("the current bundle link is not an installer-owned version")?;
    let previous_dir = safe_version_target(&install.root, &previous_link)
        .context("the previous bundle link is not an installer-owned version")?;
    if current_dir == previous_dir {
        bail!("current and previous point to the same bundle");
    }

    atomic_symlink(&install.root, "previous", &current_link)?;
    atomic_symlink(&install.root, "current", &previous_link)?;

    let version = clean_text(&previous_dir.join("manifest.txt"))
        .and_then(|m| {
            m.lines()
                .find_map(|line| line.strip_prefix("version="))
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            previous_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into()
        });
    Ok(format!(
        "rolled back to {version}; the bundle you left is now the next rollback"
    ))
}

pub fn uninstall_bundle(dry_run: bool) -> Result<Option<String>> {
    let Some(install) = current() else {
        return Ok(None);
    };
    let mut actions = Vec::new();

    for (name, enabled) in [("toomux", true), ("tmux", install.managed_tmux)] {
        if !enabled {
            continue;
        }
        let launcher = install.bin_dir.join(name);
        if symlink_target_owned(&launcher, &install.root) {
            actions.push(format!("launcher {}", launcher.display()));
            if !dry_run {
                std::fs::remove_file(&launcher)
                    .with_context(|| format!("removing {}", launcher.display()))?;
            }
        }
    }

    let previous_launcher = clean_text(&install.root.join(PREVIOUS_LAUNCHER_FILE))
        .map(PathBuf::from)
        .filter(|p| *p == install.bin_dir.join("toomux.pre-toomux-bundle"))
        .filter(|p| p.is_file());
    if let Some(previous) = previous_launcher {
        let launcher = install.bin_dir.join("toomux");
        actions.push(format!("restore previous launcher {}", previous.display()));
        if !dry_run {
            if launcher.exists() || launcher.is_symlink() {
                bail!(
                    "refusing to restore {} over an unexpected launcher",
                    previous.display()
                );
            }
            std::fs::rename(&previous, &launcher).with_context(|| {
                format!("restoring previous toomux launcher {}", previous.display())
            })?;
        }
    }

    actions.push(format!("runtime {}", install.root.display()));
    if !dry_run {
        std::fs::remove_dir_all(&install.root)
            .with_context(|| format!("removing {}", install.root.display()))?;
    }

    let verb = if dry_run { "would remove" } else { "removed" };
    Ok(Some(format!(
        "self-contained install: {verb} {}",
        actions.join(", ")
    )))
}

pub fn description() -> Option<String> {
    let install = current()?;
    let id = install
        .version_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    Some(format!("{} ({id})", install.root.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("toomux-install-{name}-{}", std::process::id()))
    }

    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let root = temp(name);
        let version = root.join("versions/0.3.0-deadbeef/bin");
        std::fs::create_dir_all(&version).unwrap();
        std::fs::write(root.join(FORMAT_FILE), "1\n").unwrap();
        std::fs::write(root.join(BIN_DIR_FILE), "/tmp/toomux-bin\n").unwrap();
        std::fs::write(root.join(MANAGED_TMUX_FILE), "1\n").unwrap();
        std::fs::write(
            root.join("versions/0.3.0-deadbeef/manifest.txt"),
            "bundle_format=1\nversion=0.3.0\n",
        )
        .unwrap();
        (root, version.join("toomux"))
    }

    #[test]
    fn recognizes_only_marker_owned_version_layout() {
        let (root, exe) = fixture("recognize");
        let install = from_exe(&exe).unwrap();
        assert_eq!(install.root, root);
        assert_eq!(install.bin_dir, PathBuf::from("/tmp/toomux-bin"));
        assert!(install.managed_tmux);
        let _ = std::fs::remove_dir_all(&install.root);
    }

    #[test]
    fn refuses_dangerous_or_unmarked_roots() {
        assert!(!safe_root(Path::new("/")));
        let (root, exe) = fixture("marker");
        std::fs::remove_file(root.join(FORMAT_FILE)).unwrap();
        assert!(from_exe(&exe).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn version_targets_are_exactly_one_directory_below_versions() {
        let (root, _) = fixture("target");
        assert!(safe_version_target(&root, Path::new("versions/0.3.0-deadbeef")).is_some());
        assert!(safe_version_target(&root, Path::new("../outside")).is_none());
        assert!(safe_version_target(&root, Path::new("versions/a/b")).is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
