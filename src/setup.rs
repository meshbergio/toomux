//! Installing and taking back toomux's Claude Code integration.
//!
//! The binary and the TUI both add accounts, so the settings/hooks/MCP pieces
//! live here rather than in main.rs. Removal still keys off the same markers,
//! so anything the user changed remains theirs.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::{self, Config};

pub const BEGIN: &str = "# >>> toomux >>>";
pub const END: &str = "# <<< toomux <<<";

/// In each account's `commands/voyage.md`, so a toomux file is told from yours.
pub const VOYAGE_MARK: &str = "toomux voyage";
/// `/voyage` was `/quest` once: toomux's old `commands/quest.md` carries this.
pub const QUEST_MARK: &str = "toomux quest";

/// `/voyage`: toomux's hooks set, show and clear the voyage; this is what the
/// session reads when one is set.
pub const VOYAGE_COMMAND: &str = r#"---
description: Keep at an outcome until it's done, across handovers (toomux voyage)
argument-hint: <what done looks like> [--check "<command>"] [--budget <dollars>] [--persistence light|steady|hard|relentless] | clear | resume
---
Your voyage: $ARGUMENTS

Work toward it now and keep going. After every turn toomux checks whether it's done and, if not, sends you back with what's missing. It carries on across handovers, so a fresh session picks it up where you leave it. When it's done, end the turn showing the evidence: the command you ran and its result. If only the user can decide what comes next, say so plainly and stop.
"#;

/// `~/.tmux.conf` without toomux's block and without its segment in a
/// status-right of your own. None when there was nothing of toomux's in it.
pub fn strip_tmux(conf: &str) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let mut skipping = false;
    let mut changed = false;
    for l in conf.lines() {
        if l.trim() == BEGIN {
            skipping = true;
            changed = true;
            continue;
        }
        if l.trim() == END {
            skipping = false;
            continue;
        }
        if skipping {
            continue;
        }
        let t = l.trim_start();
        if (t.starts_with("set -g status-right ") || t.starts_with("set-option -g status-right "))
            && l.contains("toomux status")
        {
            out.push(strip_segment(l));
            changed = true;
            continue;
        }
        out.push(l.to_string());
    }
    // The blank line init left after its block.
    let mut text = out.join("\n");
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    changed.then(|| text.trim_end().to_string() + "\n")
}

/// A status-right value without `#(<anything>toomux status)` and the two
/// spaces init put after it.
pub fn strip_segment(s: &str) -> String {
    let Some(at) = s.find("toomux status)") else {
        return s.to_string();
    };
    let Some(open) = s[..at].rfind("#(") else {
        return s.to_string();
    };
    let tail = &s[at + "toomux status)".len()..];
    let spaces = tail.len() - tail.trim_start_matches(' ').len();
    format!("{}{}", &s[..open], &tail[spaces.min(2)..])
}

/// Claude Code keeps to 256 colours inside tmux unless this is set: the
/// status line's colours (a voyage's scene above all) need all of them. tmux
/// maps them down for a terminal that can't show them.
pub const TRUECOLOR_ENV: &str = "CLAUDE_CODE_TMUX_TRUECOLOR";

/// The path persisted into tmux/Claude integration must survive bundle upgrades.
/// `current_exe()` resolves the immutable bundle payload, so prefer the stable
/// ~/.local/bin launcher only when it resolves back to this exact executable.
/// Other install modes keep using their own executable path.
pub fn integration_bin() -> Result<String> {
    let exe = std::env::current_exe().context("resolving current toomux executable")?;
    Ok(integration_bin_for(
        &exe,
        &config::home().join(".local/bin/toomux"),
    ))
}

fn integration_bin_for(exe: &std::path::Path, stable: &std::path::Path) -> String {
    let resolved_exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    if std::fs::canonicalize(stable).ok().as_ref() == Some(&resolved_exe) {
        stable.display().to_string()
    } else {
        resolved_exe.display().to_string()
    }
}

/// Route Bash through toomux, so large outputs are kept whole but shown short.
pub fn install_hook(dir: &std::path::Path, bin: &str) -> Result<String> {
    let path = std::fs::canonicalize(dir.join("settings.json"))
        .unwrap_or_else(|_| dir.join("settings.json"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut v: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let wanted = [
        (
            "PreToolUse",
            "hook pre-tool",
            serde_json::json!({"matcher": "*", "hooks": [{"type": "command", "command": format!("{bin} hook pre-tool"), "timeout": 10}]}),
        ),
        (
            "Stop",
            "hook stop",
            serde_json::json!({"hooks": [{"type": "command", "command": format!("{bin} hook stop"), "timeout": 600}]}),
        ),
        (
            "UserPromptSubmit",
            "hook prompt",
            serde_json::json!({"hooks": [{"type": "command", "command": format!("{bin} hook prompt"), "timeout": 10}]}),
        ),
    ];
    let mut changed = false;
    for (event, mark, entry) in wanted {
        let mut list = v
            .pointer(&format!("/hooks/{event}"))
            .and_then(|l| l.as_array())
            .cloned()
            .unwrap_or_default();
        let ours = |e: &serde_json::Value| e.to_string().contains(mark);
        if list.iter().any(|e| ours(e) && *e == entry) {
            continue;
        }
        list.retain(|e| !ours(e));
        list.push(entry);
        let obj = v.as_object_mut().context("settings.json isn't an object")?;
        let hooks = obj.entry("hooks").or_insert(serde_json::json!({}));
        hooks
            .as_object_mut()
            .context("hooks isn't an object")?
            .insert(event.into(), serde_json::Value::Array(list));
        changed = true;
    }
    if !changed {
        return Ok("handover gate, handover trigger and output capping already on".into());
    }
    let backup = path.with_extension("json.pre-toomux-hooks");
    if !backup.exists() {
        std::fs::write(&backup, &raw)?;
    }
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&v)? + "\n")?;
    std::fs::rename(tmp, &path)?;
    Ok(format!(
        "handover gate, handover trigger and output capping on in {} (backup {})",
        config::tilde(&path.display().to_string()),
        config::tilde(&backup.display().to_string())
    ))
}

/// Memory and kept outputs as tools in every session of this account.
pub fn register_mcp(cfg: &Config, dir: &std::path::Path, bin: &str) -> String {
    let claude = config::expand(&cfg.claude_bin);
    let listed = crate::credentials::with_config_dir(
        std::process::Command::new(&claude).args(["mcp", "get", "toomux"]),
        dir,
    )
    .output();
    if listed.as_ref().is_ok_and(|o| o.status.success()) {
        return "memory tools already registered".into();
    }
    let added = crate::credentials::with_config_dir(
        std::process::Command::new(&claude)
            .args(["mcp", "add", "--scope", "user", "toomux", "--", bin, "mcp"]),
        dir,
    )
    .output();
    match added {
        Ok(o) if o.status.success() => "memory tools registered (user scope)".into(),
        Ok(o) => format!(
            "couldn't register memory tools: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => format!("couldn't register memory tools: {e}"),
    }
}

/// Make toomux the account's Claude Code status line, which is how it learns
/// the account's usage. An existing status line of the user's own is left alone.
pub fn install_statusline(dir: &std::path::Path, bin: &str) -> Result<String> {
    let path = dir.join("settings.json");
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut v: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let want = format!("{bin} statusline");
    let every = v
        .pointer("/statusLine/refreshInterval")
        .and_then(|r| r.as_u64());
    let colour = v.pointer(&format!("/env/{TRUECOLOR_ENV}")).is_some();
    match v.pointer("/statusLine/command").and_then(|c| c.as_str()) {
        Some(c) if c == want && every == Some(1) && colour => {
            return Ok("status line already reports usage".into());
        }
        Some(c) if !c.contains("toomux") => {
            return Ok(format!(
                "has its own status line ({c}); usage for it will come from the usage lookup"
            ));
        }
        _ => {}
    }
    let obj = v.as_object_mut().context("settings.json isn't an object")?;
    obj.insert(
        "statusLine".into(),
        serde_json::json!({"type": "command", "command": want, "padding": 0, "refreshInterval": 1}),
    );
    if !colour {
        let env = obj.entry("env").or_insert_with(|| serde_json::json!({}));
        if let Some(env) = env.as_object_mut() {
            env.insert(TRUECOLOR_ENV.into(), "1".into());
        }
    }
    let backup = path.with_extension("json.pre-toomux");
    if !backup.exists() {
        std::fs::write(&backup, &raw)?;
    }
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&v)? + "\n")?;
    std::fs::rename(tmp, &path)?;
    Ok(format!(
        "status line set to toomux in {} (backup {})",
        config::tilde(&path.display().to_string()),
        config::tilde(&backup.display().to_string())
    ))
}

/// Claude Code settings without toomux's status line and hooks; events left
/// with no hooks, and a `hooks` left empty, go too. Whether anything changed.
pub fn strip_settings(v: &mut Value) -> bool {
    let mut changed = false;
    let Some(obj) = v.as_object_mut() else {
        return false;
    };
    if obj
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains("toomux statusline"))
    {
        obj.remove("statusLine");
        changed = true;
    }
    if let Some(env) = obj.get_mut("env").and_then(Value::as_object_mut) {
        if env.get(TRUECOLOR_ENV).and_then(Value::as_str) == Some("1") {
            env.remove(TRUECOLOR_ENV);
            changed = true;
        }
        if env.is_empty() {
            obj.remove("env");
        }
    }
    if let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) {
        for list in hooks.values_mut() {
            if let Some(a) = list.as_array_mut() {
                let before = a.len();
                a.retain(|e| !e.to_string().contains("toomux hook "));
                changed |= a.len() != before;
            }
        }
        hooks.retain(|_, l| l.as_array().is_none_or(|a| !a.is_empty()));
        if hooks.is_empty() {
            obj.remove("hooks");
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tmux_block_and_segment_go_and_nothing_else() {
        let conf = "set -g mouse on\nset -g status-right \"#(/b/toomux status)  %H:%M \"\n\n# >>> toomux >>>\nbind -n M-s x\n# <<< toomux <<<\n\nset -g @plugin 'tpm'\n";
        assert_eq!(
            strip_tmux(conf).unwrap(),
            "set -g mouse on\nset -g status-right \"%H:%M \"\n\nset -g @plugin 'tpm'\n"
        );
        assert_eq!(strip_tmux("set -g mouse on\n"), None, "nothing of toomux's");
    }

    #[test]
    fn a_status_right_keeps_its_own_spacing() {
        assert_eq!(strip_segment("#(/x/toomux status)  #h"), "#h");
        assert_eq!(strip_segment("A #(/x/toomux status)  B"), "A B");
        assert_eq!(strip_segment("#(/x/toomux status)"), "");
        assert_eq!(strip_segment("#h"), "#h");
    }

    #[cfg(unix)]
    #[test]
    fn integration_bin_prefers_matching_stable_launcher() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("toomux-integration-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("versions/v1/bin")).unwrap();
        std::fs::create_dir_all(root.join(".local/bin")).unwrap();
        let exe = root.join("versions/v1/bin/toomux");
        std::fs::write(&exe, b"test").unwrap();
        let stable = root.join(".local/bin/toomux");
        symlink(&exe, &stable).unwrap();

        assert_eq!(
            integration_bin_for(&exe, &stable),
            stable.display().to_string()
        );

        std::fs::remove_file(&stable).unwrap();
        let other = root.join("versions/v2/bin/toomux");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, b"other").unwrap();
        symlink(&other, &stable).unwrap();
        assert_eq!(
            integration_bin_for(&exe, &stable),
            exe.display().to_string()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn settings_lose_only_toomux() {
        let mut v: Value = serde_json::from_str(
            r#"{"model":"x","env":{"CLAUDE_CODE_TMUX_TRUECOLOR":"1"},"statusLine":{"type":"command","command":"/b/toomux statusline"},
                "hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"/b/toomux hook pre-tool"}]},
                                       {"matcher":"Bash","hooks":[{"type":"command","command":"mine"}]}],
                         "Stop":[{"hooks":[{"type":"command","command":"/b/toomux hook stop"}]}]}}"#,
        )
        .unwrap();
        assert!(strip_settings(&mut v));
        assert_eq!(v.pointer("/hooks/PreToolUse/0/matcher").unwrap(), "Bash");
        assert!(v.get("statusLine").is_none() && v.pointer("/hooks/Stop").is_none());
        assert!(
            v.get("env").is_none(),
            "the truecolour switch goes, and its empty env"
        );
        assert_eq!(v["model"], "x");
        assert!(!strip_settings(&mut v), "twice is a no-op");
        let mut own: Value =
            serde_json::from_str(r#"{"statusLine":{"command":"my-line"}}"#).unwrap();
        assert!(!strip_settings(&mut own), "a status line of your own stays");
    }
}
