//! Taking back what `toomux init --apply` added: only what it added, found by
//! its markers, so anything you changed since stays as you left it.

use serde_json::Value;

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
