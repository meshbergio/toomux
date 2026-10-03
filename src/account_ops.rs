//! Reusable Claude Code account lifecycle operations.
//!
//! The CLI prints the returned notes verbatim; the TUI renders the same notes.
//! Keeping mutation and wording here means both surfaces share one safety and
//! data-preservation implementation.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::{accounts, config, paths, registry, setup};

#[derive(Debug, Clone)]
pub struct Added {
    pub index: usize,
    pub dir: PathBuf,
    pub notes: Vec<String>,
}

fn short(p: &Path) -> String {
    config::tilde(&p.display().to_string())
}

fn protected() -> [PathBuf; 4] {
    [
        paths::config(),
        paths::state(),
        paths::data(),
        paths::runtime(),
    ]
}

/// Claude sessions running on an account right now.
pub fn open_on(cfg: &Config, i: usize) -> usize {
    registry::load(cfg)
        .iter()
        .filter(|s| s.account == Some(i))
        .count()
}

fn busy(cfg: &Config, i: usize, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    let n = open_on(cfg, i);
    if n > 0 {
        bail!(
            "{}: {n} Claude sessions are open on it; close them (or --force) and run it again",
            cfg.accounts[i].name
        );
    }
    Ok(())
}

/// Validate the name/folder pair without mutating anything.
pub fn validate_add(cfg: &Config, name: &str, dir: &Path) -> Result<PathBuf> {
    accounts::valid_name(name)?;
    if cfg.account_by_name(name).is_some() {
        bail!("there's already an account called {name}");
    }
    let dir = accounts::validate_root(dir, &config::home(), &protected())?;
    if (0..cfg.accounts.len()).any(|i| {
        std::fs::canonicalize(cfg.account_dir(i)).ok() == std::fs::canonicalize(&dir).ok()
            && dir.exists()
    }) {
        bail!("{} is already an account", short(&dir));
    }
    if dir.is_dir() {
        accounts::validate_adoption(&dir)?;
    }
    Ok(dir)
}

/// Add an account and install the same toomux integration the CLI always has.
pub fn add(cfg: &mut Config, name: &str, dir: Option<PathBuf>) -> Result<Added> {
    let dir = dir.unwrap_or_else(|| config::home().join(format!(".claude-{name}")));
    let dir = validate_add(cfg, name, &dir)?;
    let brought = dir.is_dir();
    std::fs::create_dir_all(&dir)?;
    accounts::mark_account(&dir)?;
    cfg.accounts.push(config::Account {
        name: name.to_string(),
        config_dir: short(&dir),
    });
    config::save_accounts(&cfg.accounts, None)?;
    let index = cfg.accounts.len() - 1;
    let mut notes = vec![format!(
        "{name}: {} {}, standing alone",
        short(&dir),
        if brought {
            "brought in as it is"
        } else {
            "made"
        }
    )];
    let bin = setup::integration_bin()?;
    notes.push(format!(
        "{name}: {}",
        setup::install_statusline(&dir, &bin)?
    ));
    notes.push(format!("{name}: {}", setup::install_hook(&dir, &bin)?));
    notes.push(format!("{name}: {}", setup::register_mcp(cfg, &dir, &bin)));
    if !crate::credentials::present(&dir) {
        notes.push(match crate::credentials::config_dir_var(&dir) {
            Some(_) => format!(
                "{name}: to log in, CLAUDE_CONFIG_DIR={} claude, then /login",
                short(&dir)
            ),
            None => format!("{name}: to log in, claude, then /login"),
        });
    }
    Ok(Added { index, dir, notes })
}

/// Join accounts to one history group. Returned lines are the CLI's exact
/// user-facing output, in the same order.
pub fn share(
    cfg: &Config,
    idx: &[usize],
    group: &str,
    dry_run: bool,
    force: bool,
) -> Result<Vec<String>> {
    use accounts::Step;
    if group != accounts::DEFAULT_GROUP {
        accounts::valid_name(group)?;
    }
    let home = config::home();
    let shared = accounts::group_dir(&home, group);
    let will = if dry_run { "would be " } else { "" };
    let mut out = Vec::new();
    for &i in idx {
        let name = &cfg.accounts[i].name;
        let dir = cfg.account_dir(i);
        if let Some(g) = accounts::group_of(&dir)
            && std::fs::canonicalize(&shared).ok().as_ref() != Some(&g)
        {
            out.push(format!(
                "{name}: shares {} already; toomux account unshare {name} first",
                short(&g)
            ));
            continue;
        }
        let steps = accounts::plan(&dir, &shared);
        if steps.is_empty() {
            out.push(format!(
                "{name}: already shares everything it can in {}",
                short(&shared)
            ));
            continue;
        }
        if !dry_run {
            busy(cfg, i, force)?;
        }
        let (mut linked, mut merged, mut clashed) = (0, 0, 0);
        for s in &steps {
            match s {
                Step::Move { from, to } => out.push(format!(
                    "{name}: {} {will}moved to {}",
                    short(from),
                    short(to)
                )),
                Step::Merge { clashes, .. } => {
                    merged += 1;
                    clashed += clashes;
                }
                Step::Append { from, to } => out.push(format!(
                    "{name}: {} {will}added to {}",
                    short(from),
                    short(to)
                )),
                Step::Combine { from, to } => out.push(format!(
                    "{name}: {} {will}combined with {} (no setting differs)",
                    short(from),
                    short(to)
                )),
                Step::Create { .. } | Step::Same { .. } => {}
                Step::Link { .. } => linked += 1,
                Step::Differs { at, shared } => out.push(format!(
                    "{name}: {} differs from {}: left as {name}'s own; make them one and run it again",
                    short(at),
                    short(shared)
                )),
            }
        }
        if !dry_run {
            std::fs::create_dir_all(&shared)?;
            accounts::apply(&steps, name)?;
        }
        if merged > 0 {
            let kept = if clashed > 0 {
                format!("; {clashed} files both had with other contents are kept as *.from-{name}")
            } else {
                String::new()
            };
            out.push(format!(
                "{name}: {merged} folders {will}merged into the group's{kept}"
            ));
        }
        out.push(format!(
            "{name}: {linked} {} {will}linked to {}",
            if linked == 1 { "entry" } else { "entries" },
            short(&shared)
        ));
    }
    Ok(out)
}

fn human(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{} MB", b >> 20),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b => format!("{b} B"),
    }
}

/// Leave history groups, keeping the same data-preservation semantics as the
/// CLI. fresh deliberately drops shared history while keeping settings.
pub fn unshare(
    cfg: &Config,
    idx: &[usize],
    fresh: bool,
    dry_run: bool,
    force: bool,
) -> Result<Vec<String>> {
    let will = if dry_run { "would " } else { "" };
    let mut out = Vec::new();
    for &i in idx {
        let name = &cfg.accounts[i].name;
        let dir = cfg.account_dir(i);
        let Some(group) = accounts::group_of(&dir) else {
            out.push(format!("{name}: stands alone already"));
            continue;
        };
        let steps = accounts::plan_leave(&dir, fresh);
        if !dry_run {
            busy(cfg, i, force)?;
            accounts::apply_leave(&steps)?;
        }
        let how = if fresh {
            "starting with no history (its settings come along)".to_string()
        } else {
            format!(
                "with its own copy of what it saw ({})",
                human(accounts::leave_size(&steps))
            )
        };
        out.push(format!(
            "{name}: {will}{} {}, {how}",
            if dry_run { "leave" } else { "left" },
            short(&group)
        ));
        let left: Vec<&str> = (0..cfg.accounts.len())
            .filter(|&j| j != i && accounts::group_of(&cfg.account_dir(j)).as_ref() == Some(&group))
            .map(|j| cfg.accounts[j].name.as_str())
            .collect();
        if left.is_empty() && !dry_run {
            out.push(format!(
                "{}: no account shares it now; it stays until you delete it",
                short(&group)
            ));
        }
    }
    Ok(out)
}

/// Remove an account from configuration and optionally its marked account
/// root. Shared group contents are never recursively deleted.
pub fn remove(cfg: &mut Config, i: usize, delete: bool, force: bool) -> Result<Vec<String>> {
    let name = cfg.accounts[i].name.clone();
    let dir = cfg.account_dir(i);
    busy(cfg, i, force)?;
    let mut out = Vec::new();
    if delete && dir.exists() {
        let dir = accounts::validate_delete(&dir, &config::home(), &protected())?;
        std::fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", short(&dir)))?;
        out.push(format!(
            "{name}: {} deleted (its login and anything it didn't share)",
            short(&dir)
        ));
    } else {
        out.push(format!(
            "{name}: {} stays as it is; toomux account add {name} --dir {} brings it back",
            short(&dir),
            short(&dir)
        ));
    }
    cfg.accounts.remove(i);
    config::save_accounts(&cfg.accounts, None)?;
    out.push(format!("{name}: no longer one of toomux's accounts"));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    struct TestEnv {
        home: Option<OsString>,
        toomux_home: Option<OsString>,
        xdg_runtime: Option<OsString>,
    }

    impl TestEnv {
        fn new(root: &Path) -> Self {
            let home = std::env::var_os("HOME");
            let toomux_home = std::env::var_os("TOOMUX_HOME");
            let xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR");
            let fake_home = root.join("home");
            std::fs::create_dir_all(&fake_home).unwrap();
            unsafe {
                std::env::set_var("HOME", &fake_home);
                std::env::set_var("TOOMUX_HOME", root.join("toomux"));
                std::env::set_var("XDG_RUNTIME_DIR", root.join("runtime"));
            }
            Self {
                home,
                toomux_home,
                xdg_runtime,
            }
        }

        fn restore(name: &str, value: &Option<OsString>) {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            Self::restore("HOME", &self.home);
            Self::restore("TOOMUX_HOME", &self.toomux_home);
            Self::restore("XDG_RUNTIME_DIR", &self.xdg_runtime);
        }
    }

    fn rig(name: &str) -> PathBuf {
        let h =
            std::env::temp_dir().join(format!("toomux-account-ops-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(&h).unwrap();
        h
    }

    fn cfg() -> Config {
        let mut cfg = Config::default();
        cfg.accounts.clear();
        cfg.claude_bin = "/nonexistent/toomux-test-claude".into();
        cfg
    }

    #[test]
    fn add_creates_a_marked_configured_account_and_rejects_unsafe_reuse() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root = rig("add");
        let _vars = TestEnv::new(&root);
        let dir = root.join("accounts").join("alpha");
        let mut cfg = cfg();
        let added = add(&mut cfg, "alpha", Some(dir.clone())).unwrap();
        assert_eq!(added.index, 0);
        assert!(dir.join(".toomux-account").is_file());
        assert_eq!(cfg.accounts[0].name, "alpha");
        assert_eq!(config::expand(&cfg.accounts[0].config_dir), dir);
        let saved = std::fs::read_to_string(Config::path()).unwrap();
        assert!(saved.contains("name = \"alpha\""));
        assert!(saved.contains(&format!("config_dir = \"{}\"", dir.display())));
        assert!(add(&mut cfg, "alpha", Some(root.join("other"))).is_err());
        assert!(
            add(&mut cfg, "protected", Some(paths::config())).is_err(),
            "toomux's own config root must never become an account"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn share_and_unshare_round_trip_returns_notes_and_keeps_history() {
        let _env = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root = rig("share");
        let _vars = TestEnv::new(&root);
        let mut cfg = cfg();
        let dir = root.join("accounts").join("work");
        add(&mut cfg, "work", Some(dir.clone())).unwrap();
        std::fs::create_dir_all(dir.join("projects/p")).unwrap();
        std::fs::write(dir.join("projects/p/one.jsonl"), "one").unwrap();
        let joined = share(&cfg, &[0], "shared", false, true).unwrap();
        assert!(joined.iter().any(|n| n.contains("linked to")));
        assert!(accounts::group_of(&dir).is_some());
        let left = unshare(&cfg, &[0], false, false, true).unwrap();
        assert!(left.iter().any(|n| n.contains("left")));
        assert!(accounts::group_of(&dir).is_none());
        assert!(dir.join("projects/p/one.jsonl").is_file());
        let _ = std::fs::remove_dir_all(root);
    }
}
