//! `toomux mcp`: memory and kept outputs as MCP tools, over stdio. Claude
//! Code starts one per session; there is no server to keep running.

use crate::memory::{self, Memory};
use anyhow::Result;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

const INSTRUCTIONS: &str = "toomux keeps a memory shared by every session on this machine: what was asked and decided in each \
conversation (indexed as it happens), handover briefs from sessions that continued in a fresh one, every project's Claude Code memory \
files, and the full text of long command outputs. Before redoing work or asking the user something they may already have answered, mem_search it. When a Bash \
result says 'toomux · … The whole output is kept', use `toomux out <id>` or the output tool for the part you need rather than \
re-running the command.";

/// Protocol versions this server speaks, newest first.
const VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub fn serve() -> Result<()> {
    let mut stdin = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    let here = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        if stdin.read_until(b'\n', &mut raw)? == 0 {
            return Ok(());
        }
        // A stray byte mustn't take the tools away for the rest of the session.
        let line = String::from_utf8_lossy(&raw);
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => handle(&req, &here),
            Err(e) => Some(
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}}),
            ),
        };
        if let Some(reply) = reply {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
}

fn handle(req: &Value, here: &str) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => {
            // The client's version when we speak it, else our newest.
            let asked = req
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let version = VERSIONS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&VERSIONS[0]);
            Some(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "toomux", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "tools/list" => Some(json!({"tools": tools()})),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            Some(match call(name, &args, here) {
                Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                Err(e) => {
                    json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true})
                }
            })
        }
        "ping" => Some(json!({})),
        _ => None,
    };
    // Notifications (no id) get no reply.
    let id = id?;
    Some(match result {
        Some(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        None => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("no method {method}")}})
        }
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "mem_search",
            "description": "Search toomux memory: past conversations (what was asked and decided), handover briefs, saved notes and kept command outputs. Word and typo-tolerant matching. Defaults to this project plus global notes.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "What to look for, in plain words"},
                "where": {"type": "string", "enum": ["here", "all", "global"], "description": "here = this project and global notes (default); all = every project"},
                "session": {"type": "string", "description": "Only this session's conversation (a session id, or its first 8 characters)"},
                "limit": {"type": ["integer", "string"], "description": "How many results (default 8, at most 30)"}
            }, "required": ["query"]}
        },
        {
            "name": "mem_get",
            "description": "The full text of one memory, by the id mem_search showed (a prefix is enough). For a conversation turn, whole: true reads it back complete (every message, tool call and result), from its transcript or toomux's archive.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}, "whole": {"type": "boolean"}}, "required": ["id"]}
        },
        {
            "name": "mem_save",
            "description": "Save something worth knowing later (a decision, a fact about the system, a gotcha) so any session can find it.",
            "inputSchema": {"type": "object", "properties": {
                "content": {"type": "string"},
                "about": {"type": "string", "description": "A short label for where it came from or what it's about"},
                "where": {"type": "string", "enum": ["here", "global"], "description": "here = this project (default); global = every project"}
            }, "required": ["content"]}
        },
        {
            "name": "mem_forget",
            "description": "Remove a memory that is wrong, out of date or shouldn't be kept (a secret, say). With replaced_by (the id of a correction saved first) it only leaves search and stays readable, pointing to the correction; without, it is deleted for good.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string", "description": "The id mem_search showed (at least 8 characters)"},
                "replaced_by": {"type": "string", "description": "The id of the memory that corrects it"}
            }, "required": ["id"]}
        },
        {
            "name": "output",
            "description": "Part of a long command output that toomux kept whole (the id is in the Bash result's toomux note).",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"},
                "lines": {"type": "string", "description": "A line range such as 40-120"},
                "grep": {"type": "string", "description": "Only lines matching this regex (case-insensitive)"},
                "chars": {"type": "string", "description": "A character range such as 4000-8000 (for output with very long lines)"}
            }, "required": ["id"]}
        }
    ])
}

fn arg<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn call(name: &str, a: &Value, here: &str) -> Result<String> {
    let now = crate::registry::now_ms();
    match name {
        "output" => {
            let id = arg(a, "id").ok_or_else(|| anyhow::anyhow!("id is required"))?;
            let text = crate::capture::out(id, arg(a, "lines"), arg(a, "grep"), arg(a, "chars"))?;
            Ok(clip(text))
        }
        "mem_search" => {
            let m = Memory::open()?;
            let q = arg(a, "query").ok_or_else(|| anyhow::anyhow!("query is required"))?;
            // Models send numbers as strings now and then.
            let limit = a
                .get("limit")
                .and_then(|l| {
                    l.as_u64()
                        .or_else(|| l.as_str().and_then(|s| s.trim().parse().ok()))
                })
                .unwrap_or(8)
                .clamp(1, 30) as usize;
            let where_ = arg(a, "where").unwrap_or("here");
            let scopes: Vec<String> = match where_ {
                "all" => vec![],
                "global" => vec!["global".into()],
                _ => vec![memory::project_scope(here), "global".into()],
            };
            if let Some(s) = arg(a, "session")
                && (s.len() < 8 || !s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
            {
                anyhow::bail!("session is a session id or its first 8 characters");
            }
            let session = arg(a, "session").map(|s| format!("session:{s}"));
            let (scopes, prefix) = match &session {
                Some(_) => (vec![], session.as_deref()),
                None => (scopes, None),
            };
            let hits = m.search(&scopes, prefix, q, limit)?;
            // Too little here: what other projects know, marked as such (the
            // work may have been done from another folder).
            let mut elsewhere = Vec::new();
            if session.is_none() && where_ == "here" && hits.len() < limit {
                let seen: std::collections::HashSet<String> =
                    hits.iter().map(|h| h.id.clone()).collect();
                elsewhere = m
                    .search(&[], None, q, limit)?
                    .into_iter()
                    .filter(|h| !seen.contains(&h.id))
                    .take(limit - hits.len())
                    .collect();
            }
            if hits.is_empty() && elsewhere.is_empty() {
                return Ok(format!("nothing in memory matches {q:?}"));
            }
            let show = |h: &memory::Hit| {
                // A memory file citing paths that are gone may be out of date.
                let gone = h
                    .source
                    .strip_prefix("memory: ")
                    .map_or(0, |f| crate::upkeep::gone_in(f).len());
                let stale = match gone {
                    0 => String::new(),
                    1 => " · a path it cites is gone".to_string(),
                    n => format!(" · {n} paths it cites are gone"),
                };
                format!(
                    "{} · {} · {}{stale} · {}\n  {}",
                    &h.id[..12],
                    h.source,
                    when(now - h.created_ms),
                    h.scope,
                    h.snippet.replace('\n', " ")
                )
            };
            let mut text = hits.iter().map(show).collect::<Vec<_>>().join("\n");
            if !elsewhere.is_empty() {
                if hits.is_empty() {
                    text = "nothing from this project; from others:".into();
                } else {
                    text.push_str("\n\nfrom other projects:");
                }
                text.push('\n');
                text.push_str(&elsewhere.iter().map(show).collect::<Vec<_>>().join("\n"));
            }
            Ok(text)
        }
        "mem_get" => {
            let m = Memory::open()?;
            let id = arg(a, "id").ok_or_else(|| anyhow::anyhow!("id is required"))?;
            let e = m
                .get(id)?
                .ok_or_else(|| anyhow::anyhow!("no memory {id}"))?;
            if a.get("whole").and_then(Value::as_bool) == Some(true)
                && e.source.starts_with("session:")
            {
                let cfg = crate::config::Config::load().unwrap_or_default();
                let text = crate::index::whole_turn(&cfg, &e.source)?;
                return Ok(whole(&e.id, &e.source, text));
            }
            let replaced = e
                .superseded_by
                .map(|r| format!(" (replaced by {})", &r[..12]))
                .unwrap_or_default();
            let gone = e
                .source
                .strip_prefix("memory: ")
                .map(crate::upkeep::gone_in)
                .unwrap_or_default();
            let replaced = if gone.is_empty() {
                replaced
            } else {
                format!(
                    "{replaced}\nGone from disk since this was written (check before relying on them): {}",
                    gone.join(", ")
                )
            };
            Ok(clip(format!(
                "{} · {} · {}{replaced}\n\n{}",
                &e.id[..12],
                e.source,
                e.scope,
                e.content
            )))
        }
        "mem_save" => {
            let m = Memory::open()?;
            let content =
                arg(a, "content").ok_or_else(|| anyhow::anyhow!("content is required"))?;
            let scope = if arg(a, "where") == Some("global") {
                "global".to_string()
            } else {
                memory::project_scope(here)
            };
            let about = arg(a, "about").unwrap_or("note");
            let (id, dup) = m.index(&scope, &format!("note: {about}"), content, now)?;
            Ok(format!(
                "{} {}",
                if dup { "already saved as" } else { "saved as" },
                &id[..12]
            ))
        }
        "mem_forget" => {
            let m = Memory::open()?;
            let id = arg(a, "id").ok_or_else(|| anyhow::anyhow!("id is required"))?;
            match arg(a, "replaced_by") {
                Some(by) => {
                    m.supersede(id, by)?;
                    Ok("done: it no longer shows in search, and points to its correction".into())
                }
                None => {
                    let e = m.forget(id)?;
                    Ok(format!("deleted {} ({})", &e.id[..12], e.source))
                }
            }
        }
        _ => anyhow::bail!("no tool {name}"),
    }
}

fn when(ms: i64) -> String {
    match crate::registry::ago(ms) {
        a if a == "now" => "just now".into(),
        a => format!("{a} ago"),
    }
}

/// Keep a reply within what Claude Code shows of a tool result.
/// A whole turn: in full when it fits, else its start and a file with all of it.
fn whole(id: &str, source: &str, text: String) -> String {
    let head = format!("{} · {} · whole turn\n\n", &id[..12], source);
    if text.chars().count() <= 28_000 {
        return head + &text;
    }
    let dir = crate::capture::store_dir().join("turns");
    let path = dir.join(format!("{}.txt", &id[..12]));
    let kept = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(&path, &text))
        .is_ok();
    let start: String = text.chars().take(20_000).collect();
    if kept {
        format!(
            "{head}{start}\n[… {} chars in all. The whole turn is in {}: read the part you need (rg, sed -n).]",
            text.chars().count(),
            path.display()
        )
    } else {
        clip(head + &text)
    }
}

fn clip(s: String) -> String {
    const MAX: usize = 28_000;
    if s.chars().count() <= MAX {
        return s;
    }
    let head: String = s.chars().take(MAX).collect();
    format!(
        "{head}\n[… {} more chars: ask for a narrower range]",
        s.chars().count() - MAX
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaks_mcp() {
        let t = tools();
        let names: Vec<&str> = t
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["mem_search", "mem_get", "mem_save", "mem_forget", "output"]
        );
        assert!(call("nope", &json!({}), "/").is_err());
        let init = |v: &str| {
            handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": v}}), "/").unwrap()
        };
        assert_eq!(
            init("2025-03-26")["result"]["protocolVersion"],
            "2025-03-26",
            "a version we speak is kept"
        );
        assert_eq!(
            init("1999-01-01")["result"]["protocolVersion"],
            VERSIONS[0],
            "else ours"
        );
        assert!(
            handle(
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                "/"
            )
            .is_none(),
            "notifications get no reply"
        );
        assert_eq!(
            handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "nope"}), "/").unwrap()["error"]["code"],
            -32601
        );
    }
}
