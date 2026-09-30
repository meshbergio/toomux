//! CLI orchestration for Claude Code account lifecycle operations.
//! Filesystem sharing/migration invariants live in toomux::accounts; this
//! module owns prompting, selection and user-facing account commands.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use toomux::config::Config;
use toomux::{config, paths, registry};

use crate::{human, install_hook, install_statusline, register_mcp};

#[derive(Subcommand)]
pub(crate) enum AccountCmd {
    /// Each account, whether it's logged in, and who it shares with (the default)
    List,
    /// Walk through naming accounts, their folders, and who shares
    Setup,
    /// A new account, or an existing Claude Code folder brought in
    Add {
        name: String,
        /// Its folder (default ~/.claude-<name>); an existing one is brought in as it is
        #[arg(long)]
        dir: Option<String>,
        /// Join a group straight away (the default group unless one is named)
        #[arg(long, num_args = 0..=1, default_missing_value = "shared", value_name = "GROUP")]
        share: Option<String>,
    },
    /// A new name, and with it a new folder when the folder is ~/.claude-<name>
    Rename {
        name: String,
        new_name: String,
        /// Move its folder here instead
        #[arg(long)]
        dir: Option<String>,
        /// Go ahead with Claude sessions open on it
        #[arg(long)]
        force: bool,
    },
    /// Accounts join a group and share one history (folders merged, nothing lost)
    Share {
        names: Vec<String>,
        /// Every account
        #[arg(long)]
        all: bool,
        /// Which group (default: ~/.claude-shared; another name makes ~/.claude-shared-<name>)
        #[arg(long, default_value = "shared")]
        group: String,
        #[arg(long)]
        dry_run: bool,
        /// Go ahead with Claude sessions open on these accounts
        #[arg(long)]
        force: bool,
    },
    /// Accounts leave their group and stand alone, keeping a copy of what they saw
    Unshare {
        names: Vec<String>,
        /// Start with no history instead of a copy (settings still come along)
        #[arg(long)]
        fresh: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
    },
    /// Stop using an account in toomux; its folder stays unless --delete
    Remove {
        name: String,
        /// Also delete its folder: its login and anything it doesn't share
        #[arg(long)]
        delete: bool,
        #[arg(long)]
        force: bool,
    },
}

pub(crate) fn run(what: AccountCmd) -> Result<()> {
    let mut cfg = Config::load().unwrap_or_default();
    match what {
        AccountCmd::List => {
            print!("{}", account_list(&cfg));
            Ok(())
        }
        AccountCmd::Setup => account_setup(cfg),
        AccountCmd::Add { name, dir, share } => {
            let i = account_add(&mut cfg, &name, dir.as_deref().map(config::expand))?;
            if let Some(group) = share {
                account_share(&cfg, &[i], &group, false, false)?;
            }
            Ok(())
        }
        AccountCmd::Rename {
            name,
            new_name,
            dir,
            force,
        } => {
            let i = pick(&cfg, std::slice::from_ref(&name), false)?[0];
            account_rename(
                &mut cfg,
                i,
                &new_name,
                dir.as_deref().map(config::expand),
                force,
            )
        }
        AccountCmd::Share {
            names,
            all,
            group,
            dry_run,
            force,
        } => {
            let idx = pick(&cfg, &names, all)?;
            account_share(&cfg, &idx, &group, dry_run, force)
        }
        AccountCmd::Unshare {
            names,
            fresh,
            dry_run,
            force,
        } => {
            let idx = pick(&cfg, &names, false)?;
            account_unshare(&cfg, &idx, fresh, dry_run, force)
        }
        AccountCmd::Remove {
            name,
            delete,
            force,
        } => {
            let i = pick(&cfg, std::slice::from_ref(&name), false)?[0];
            account_remove(&mut cfg, i, delete, force)
        }
    }
}

fn short(p: &std::path::Path) -> String {
    config::tilde(&p.display().to_string())
}

/// Accounts by name, or all of them.
fn pick(cfg: &Config, names: &[String], all: bool) -> Result<Vec<usize>> {
    if all {
        return Ok((0..cfg.accounts.len()).collect());
    }
    if names.is_empty() {
        bail!("name the accounts (toomux account lists them), or --all");
    }
    names
        .iter()
        .map(|n| {
            cfg.account_by_name(n).ok_or_else(|| {
                let known: Vec<&str> = cfg.accounts.iter().map(|a| a.name.as_str()).collect();
                anyhow::anyhow!("no account \"{n}\" (there are: {})", known.join(", "))
            })
        })
        .collect()
}

/// Claude sessions running on an account right now.
fn open_on(cfg: &Config, i: usize) -> usize {
    registry::load(cfg)
        .iter()
        .filter(|s| s.account == Some(i))
        .count()
}

fn busy(cfg: &Config, i: usize, force: bool) -> Result<()> {
    let n = open_on(cfg, i);
    if n > 0 && !force {
        bail!(
            "{}: {n} Claude sessions are open on it; close them (or --force) and run it again",
            cfg.accounts[i].name
        );
    }
    Ok(())
}

fn account_list(cfg: &Config) -> String {
    use toomux::accounts;
    let mut out = String::new();
    if cfg.accounts.is_empty() {
        return "no accounts yet: toomux account setup, or toomux account add <name>\n".into();
    }
    let groups: Vec<Option<std::path::PathBuf>> = (0..cfg.accounts.len())
        .map(|i| accounts::group_of(&cfg.account_dir(i)))
        .collect();
    let width = cfg.accounts.iter().map(|a| a.name.len()).max().unwrap_or(0);
    let dirs: Vec<String> = (0..cfg.accounts.len())
        .map(|i| short(&cfg.account_dir(i)))
        .collect();
    let dw = dirs.iter().map(String::len).max().unwrap_or(0);
    for i in 0..cfg.accounts.len() {
        let dir = cfg.account_dir(i);
        let login = if toomux::credentials::present(&dir) {
            "logged in"
        } else {
            "not logged in"
        };
        let how = match &groups[i] {
            Some(g) => {
                let with: Vec<&str> = (0..cfg.accounts.len())
                    .filter(|&j| j != i && groups[j].as_ref() == Some(g))
                    .map(|j| cfg.accounts[j].name.as_str())
                    .collect();
                let own = accounts::describe(&dir, g).1;
                let own = if own.is_empty() {
                    String::new()
                } else {
                    format!("; its own: {}", own.join(", "))
                };
                if with.is_empty() {
                    format!("in {}, with no other account yet{own}", short(g))
                } else {
                    format!("shares with {} in {}{own}", with.join(", "), short(g))
                }
            }
            None => "stands alone".into(),
        };
        out.push_str(&format!(
            "{:<width$}  {:<dw$}  {login:<13}  {how}\n",
            cfg.accounts[i].name, dirs[i]
        ));
    }
    out
}

fn account_add(cfg: &mut Config, name: &str, dir: Option<std::path::PathBuf>) -> Result<usize> {
    toomux::accounts::valid_name(name)?;
    if cfg.account_by_name(name).is_some() {
        bail!("there's already an account called {name}");
    }
    let dir = dir.unwrap_or_else(|| config::home().join(format!(".claude-{name}")));
    let protected = [
        paths::config(),
        paths::state(),
        paths::data(),
        paths::runtime(),
    ];
    let dir = toomux::accounts::validate_root(&dir, &config::home(), &protected)?;
    if (0..cfg.accounts.len()).any(|i| {
        std::fs::canonicalize(cfg.account_dir(i)).ok() == std::fs::canonicalize(&dir).ok()
            && dir.exists()
    }) {
        bail!("{} is already an account", short(&dir));
    }
    let brought = dir.is_dir();
    if brought {
        toomux::accounts::validate_adoption(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    toomux::accounts::mark_account(&dir)?;
    cfg.accounts.push(config::Account {
        name: name.to_string(),
        config_dir: short(&dir),
    });
    config::save_accounts(&cfg.accounts, None)?;
    let i = cfg.accounts.len() - 1;
    println!(
        "{name}: {} {}, standing alone",
        short(&dir),
        if brought {
            "brought in as it is"
        } else {
            "made"
        }
    );
    let bin = std::env::current_exe()?.display().to_string();
    println!("{name}: {}", install_statusline(&dir, &bin)?);
    println!("{name}: {}", install_hook(&dir, &bin)?);
    println!("{name}: {}", register_mcp(cfg, &dir, &bin));
    if !toomux::credentials::present(&dir) {
        match toomux::credentials::config_dir_var(&dir) {
            Some(_) => println!(
                "{name}: to log in, CLAUDE_CONFIG_DIR={} claude, then /login",
                short(&dir)
            ),
            None => println!("{name}: to log in, claude, then /login"),
        }
    }
    Ok(i)
}

fn account_rename(
    cfg: &mut Config,
    i: usize,
    new: &str,
    dir: Option<std::path::PathBuf>,
    force: bool,
) -> Result<()> {
    toomux::accounts::valid_name(new)?;
    let old = cfg.accounts[i].name.clone();
    if cfg.account_by_name(new).is_some_and(|j| j != i) {
        bail!("there's already an account called {new}");
    }
    let here = cfg.account_dir(i);
    // The folder follows the name when it was named after it.
    let to = dir.or_else(|| {
        (here == config::home().join(format!(".claude-{old}")))
            .then(|| config::home().join(format!(".claude-{new}")))
    });
    if let Some(to) = to.as_ref().filter(|t| **t != here) {
        let protected = [
            paths::config(),
            paths::state(),
            paths::data(),
            paths::runtime(),
        ];
        let to = toomux::accounts::validate_root(to, &config::home(), &protected)?;
        if to.exists() {
            bail!("{} is already there", short(&to));
        }
        busy(cfg, i, force)?;
        if let Some(p) = to.parent() {
            std::fs::create_dir_all(p)?;
        }
        // Links inside point at the group by absolute path, so they move intact.
        std::fs::rename(&here, &to).with_context(|| {
            format!(
                "moving {} to {} (it must stay on the same disk)",
                short(&here),
                short(&to)
            )
        })?;
        cfg.accounts[i].config_dir = short(&to);
        println!("{old}: {} moved to {}", short(&here), short(&to));
        if let Some(said) = toomux::credentials::carry(&here, &to) {
            println!("{old}: {said}");
        }
        println!(
            "{old}: anything of yours that sets CLAUDE_CONFIG_DIR={} (aliases, scripts) wants the new folder",
            short(&here)
        );
    }
    cfg.accounts[i].name = new.to_string();
    config::save_accounts(&cfg.accounts, Some((&old, new)))?;
    // Pins and the restart record name accounts too.
    for f in ["state.json", "running.json", "restore.json"] {
        let path = paths::state().join(f);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        fn walk(v: &mut serde_json::Value, old: &str, new: &str) {
            match v {
                serde_json::Value::Object(m) => {
                    if m.get("account").and_then(|a| a.as_str()) == Some(old) {
                        m.insert("account".into(), new.into());
                    }
                    m.values_mut().for_each(|x| walk(x, old, new));
                }
                serde_json::Value::Array(a) => a.iter_mut().for_each(|x| walk(x, old, new)),
                _ => {}
            }
        }
        walk(&mut v, &old, new);
        let _ = std::fs::write(&path, v.to_string());
    }
    if old != new {
        println!("{old}: now called {new}");
    }
    Ok(())
}

fn account_share(
    cfg: &Config,
    idx: &[usize],
    group: &str,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    use toomux::accounts::{self, Step};
    if group != accounts::DEFAULT_GROUP {
        accounts::valid_name(group)?;
    }
    let home = config::home();
    let shared = accounts::group_dir(&home, group);
    let will = if dry_run { "would be " } else { "" };
    for &i in idx {
        let name = &cfg.accounts[i].name;
        let dir = cfg.account_dir(i);
        if let Some(g) = accounts::group_of(&dir)
            && std::fs::canonicalize(&shared).ok().as_ref() != Some(&g)
        {
            println!(
                "{name}: shares {} already; toomux account unshare {name} first",
                short(&g)
            );
            continue;
        }
        let steps = accounts::plan(&dir, &shared);
        if steps.is_empty() {
            println!(
                "{name}: already shares everything it can in {}",
                short(&shared)
            );
            continue;
        }
        if !dry_run {
            busy(cfg, i, force)?;
        }
        let (mut linked, mut merged, mut clashed) = (0, 0, 0);
        for s in &steps {
            match s {
                Step::Move { from, to } => {
                    println!("{name}: {} {will}moved to {}", short(from), short(to))
                }
                Step::Merge { clashes, .. } => {
                    merged += 1;
                    clashed += clashes;
                }
                Step::Append { from, to } => {
                    println!("{name}: {} {will}added to {}", short(from), short(to))
                }
                Step::Combine { from, to } => println!(
                    "{name}: {} {will}combined with {} (no setting differs)",
                    short(from),
                    short(to)
                ),
                Step::Create { .. } | Step::Same { .. } => {}
                Step::Link { .. } => linked += 1,
                Step::Differs { at, shared } => {
                    println!(
                        "{name}: {} differs from {}: left as {name}'s own; make them one and run it again",
                        short(at),
                        short(shared)
                    )
                }
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
            println!("{name}: {merged} folders {will}merged into the group's{kept}");
        }
        println!(
            "{name}: {linked} {} {will}linked to {}",
            if linked == 1 { "entry" } else { "entries" },
            short(&shared)
        );
    }
    Ok(())
}

fn account_unshare(
    cfg: &Config,
    idx: &[usize],
    fresh: bool,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    use toomux::accounts;
    let will = if dry_run { "would " } else { "" };
    for &i in idx {
        let name = &cfg.accounts[i].name;
        let dir = cfg.account_dir(i);
        let Some(group) = accounts::group_of(&dir) else {
            println!("{name}: stands alone already");
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
        println!(
            "{name}: {will}{} {}, {how}",
            if dry_run { "leave" } else { "left" },
            short(&group)
        );
        let left: Vec<&str> = (0..cfg.accounts.len())
            .filter(|&j| j != i && accounts::group_of(&cfg.account_dir(j)).as_ref() == Some(&group))
            .map(|j| cfg.accounts[j].name.as_str())
            .collect();
        if left.is_empty() && !dry_run {
            println!(
                "{}: no account shares it now; it stays until you delete it",
                short(&group)
            );
        }
    }
    Ok(())
}

fn account_remove(cfg: &mut Config, i: usize, delete: bool, force: bool) -> Result<()> {
    let name = cfg.accounts[i].name.clone();
    let dir = cfg.account_dir(i);
    busy(cfg, i, force)?;
    if delete && dir.exists() {
        let protected = [
            paths::config(),
            paths::state(),
            paths::data(),
            paths::runtime(),
        ];
        let dir = toomux::accounts::validate_delete(&dir, &config::home(), &protected)?;
        // Links into a group go as links: what the group holds stays.
        std::fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", short(&dir)))?;
        println!(
            "{name}: {} deleted (its login and anything it didn't share)",
            short(&dir)
        );
    } else {
        println!(
            "{name}: {} stays as it is; toomux account add {name} --dir {} brings it back",
            short(&dir),
            short(&dir)
        );
    }
    cfg.accounts.remove(i);
    config::save_accounts(&cfg.accounts, None)?;
    println!("{name}: no longer one of toomux's accounts");
    Ok(())
}

fn ask(q: &str, default: &str) -> Result<String> {
    use std::io::Write;
    if default.is_empty() {
        print!("{q}: ");
    } else {
        print!("{q} [{default}]: ");
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        bail!("stopped");
    }
    let a = line.trim();
    Ok(if a.is_empty() {
        default.to_string()
    } else {
        a.to_string()
    })
}

/// The walk-through: names and folders of the accounts there are, any new
/// ones, then who shares. Each answer takes effect as it is given.
fn account_setup(mut cfg: Config) -> Result<()> {
    use std::io::IsTerminal;
    use toomux::accounts;
    if !std::io::stdin().is_terminal() {
        bail!(
            "setup asks questions; without a terminal use toomux account add, rename, share and unshare"
        );
    }
    let home = config::home();
    if cfg.accounts.is_empty() {
        println!("no Claude Code accounts yet.");
    } else {
        println!("Claude Code accounts here:\n\n{}", account_list(&cfg));
        println!("name each one and choose its folder (enter keeps what's shown).");
        for i in 0..cfg.accounts.len() {
            let old = cfg.accounts[i].name.clone();
            let name = ask(&format!("\n{} is called", short(&cfg.account_dir(i))), &old)?;
            let here = cfg.account_dir(i);
            let suggested = if here == home.join(format!(".claude-{old}")) {
                home.join(format!(".claude-{name}"))
            } else {
                here.clone()
            };
            let dir = config::expand(&ask(&format!("{name}'s folder"), &short(&suggested))?);
            if (name != old || dir != here)
                && let Err(e) = account_rename(&mut cfg, i, &name, Some(dir), false)
            {
                println!("{e}");
            }
        }
    }
    loop {
        let name = ask("\nadd an account (a name, or enter to go on)", "")?;
        if name.is_empty() {
            break;
        }
        let dir = config::expand(&ask(
            &format!("{name}'s folder"),
            &format!("~/.claude-{name}"),
        )?);
        if let Err(e) = account_add(&mut cfg, &name, Some(dir)) {
            println!("{e}");
        }
    }
    if cfg.accounts.len() < 2 {
        println!("\none account: nothing to share. toomux account share joins accounts later.");
        return Ok(());
    }
    let shared = accounts::shared_dir(&home);
    let shared_c = std::fs::canonicalize(&shared).ok();
    let now: Vec<String> = (0..cfg.accounts.len())
        .filter(|&i| shared_c.is_some() && accounts::group_of(&cfg.account_dir(i)) == shared_c)
        .map(|i| cfg.accounts[i].name.clone())
        .collect();
    println!(
        "\naccounts that share see one history: every conversation and memory, resumable under any of them."
    );
    let answer = ask(
        "which share? names with spaces between, or none",
        &if now.is_empty() {
            "none".into()
        } else {
            now.join(" ")
        },
    )?;
    let want: Vec<String> = if answer == "none" {
        vec![]
    } else {
        answer.split_whitespace().map(str::to_string).collect()
    };
    let join = pick(
        &cfg,
        &want
            .iter()
            .filter(|n| !now.iter().any(|m| m.eq_ignore_ascii_case(n)))
            .cloned()
            .collect::<Vec<_>>(),
        false,
    )
    .unwrap_or_default();
    let leave: Vec<String> = now
        .iter()
        .filter(|m| !want.iter().any(|n| n.eq_ignore_ascii_case(m)))
        .cloned()
        .collect();
    if !join.is_empty() {
        account_share(&cfg, &join, accounts::DEFAULT_GROUP, false, false)?;
    }
    if !leave.is_empty() {
        let copy = ask(
            &format!(
                "{} leave: keep a copy of the history they saw? (y: a copy, n: start empty)",
                leave.join(", ")
            ),
            "y",
        )?;
        let idx = pick(&cfg, &leave, false)?;
        account_unshare(
            &cfg,
            &idx,
            !copy.to_lowercase().starts_with('y'),
            false,
            false,
        )?;
    }
    println!("\n{}", account_list(&cfg));
    Ok(())
}
