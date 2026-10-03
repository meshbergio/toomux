use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const READY_URL: &str = "http://127.0.0.1:34560/readyz";
const RECONCILE_PATH: &str = "/v1/provider/sessions/reconcile";
const DEFAULT_BROKER_PORT: u16 = 34561;
const CLIENT_MODEL: &str = "chatgpt-browser";
const CLIENT_MODEL_FABLE: &str = "claude-fable-5-1";
const CLIENT_MODEL_FALLBACK: &str = "claude-opus-4-6";
const FABLE_MIN_CLAUDE_CODE: (u64, u64, u64) = (2, 1, 257);

fn parse_claude_code_version(raw: &str) -> Option<(u64, u64, u64)> {
    let token = raw.split_whitespace().next()?.trim_start_matches('v');
    let mut parts = token.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

fn supports_fable(raw: &str) -> bool {
    parse_claude_code_version(raw).is_some_and(|version| version >= FABLE_MIN_CLAUDE_CODE)
}

fn client_model_behaves_as() -> &'static str {
    let claude = std::env::var_os("TOOMUX_CLAUDE_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/bin/claude"))
        });
    let Some(claude) = claude else {
        return CLIENT_MODEL_FALLBACK;
    };
    let Ok(output) = Command::new(claude).arg("--version").output() else {
        return CLIENT_MODEL_FALLBACK;
    };
    if output.status.success() && supports_fable(&String::from_utf8_lossy(&output.stdout)) {
        CLIENT_MODEL_FABLE
    } else {
        CLIENT_MODEL_FALLBACK
    }
}

pub fn configure_client(json: bool) -> Result<()> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let settings_path = PathBuf::from(home).join(".claude-bonnie/settings.json");
    let mut settings = if settings_path.exists() {
        let bytes = fs::read(&settings_path)
            .with_context(|| format!("reading {}", settings_path.display()))?;
        serde_json::from_slice::<Value>(&bytes)
            .with_context(|| format!("parsing {}", settings_path.display()))?
    } else {
        serde_json::json!({})
    };

    let behaves_as = client_model_behaves_as();
    let changed = ensure_client_model_picker(&mut settings, behaves_as)?;
    if changed {
        write_json_atomic(&settings_path, &settings)?;
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "changed": changed,
                "model": CLIENT_MODEL,
                "behaves_as": behaves_as,
                "settings": settings_path,
            })
        );
    } else if changed {
        println!(
            "configured {CLIENT_MODEL} for Claude Code in {}",
            settings_path.display()
        );
    } else {
        println!(
            "Claude Code model mapping already current in {}",
            settings_path.display()
        );
    }
    Ok(())
}

fn ensure_client_model_picker(settings: &mut Value, behaves_as: &str) -> Result<bool> {
    let root = settings
        .as_object_mut()
        .context("Claude settings root must be a JSON object")?;
    if !root.contains_key("modelPicker") || root.get("modelPicker") == Some(&Value::Null) {
        root.insert("modelPicker".into(), serde_json::json!({ "options": [] }));
    }
    let picker = root
        .get_mut("modelPicker")
        .and_then(Value::as_object_mut)
        .context("Claude modelPicker must be a JSON object")?;
    if !picker.contains_key("options") || picker.get("options") == Some(&Value::Null) {
        picker.insert("options".into(), Value::Array(Vec::new()));
    }
    let options = picker
        .get_mut("options")
        .and_then(Value::as_array_mut)
        .context("Claude modelPicker.options must be a JSON array")?;

    let before = options.clone();
    let mut next = Vec::with_capacity(options.len().saturating_add(1));
    let mut kept = false;
    for row in options.drain(..) {
        let is_ours = row.get("model").and_then(Value::as_str) == Some(CLIENT_MODEL);
        if !is_ours {
            next.push(row);
            continue;
        }
        if kept {
            continue;
        }
        let mut row = row.as_object().cloned().unwrap_or_default();
        row.insert("model".into(), Value::String(CLIENT_MODEL.into()));
        row.insert("label".into(), Value::String("GPT-5.6 Sol High".into()));
        row.insert(
            "description".into(),
            Value::String("Standalone ChatGPT Browser API".into()),
        );
        row.insert("behavesAs".into(), Value::String(behaves_as.into()));
        next.push(Value::Object(row));
        kept = true;
    }
    if !kept {
        next.push(serde_json::json!({
            "model": CLIENT_MODEL,
            "label": "GPT-5.6 Sol High",
            "description": "Standalone ChatGPT Browser API",
            "behavesAs": behaves_as,
        }));
    }
    let changed = before != next;
    *options = next;
    Ok(changed)
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let parent = path
        .parent()
        .context("Claude settings path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let payload = serde_json::to_vec_pretty(value)?;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    file.write_all(&payload)?;
    file.write_all(b"\n")?;
    file.sync_all()?;

    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&temp, metadata.permissions())?;
    } else {
        #[cfg(unix)]
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&temp, path).with_context(|| format!("replacing {} atomically", path.display()))?;
    Ok(())
}

pub fn reconcile(session_ids: &[String], json: bool) -> Result<()> {
    let key_path = std::env::var("CHATGPT_BROWSER_API_KEY_FILE").unwrap_or_else(|_| {
        format!(
            "{}/.local/state/chatgpt-browser-api/api.key",
            std::env::var("HOME").unwrap_or_else(|_| "~".into())
        )
    });
    let key = fs::read_to_string(&key_path)
        .with_context(|| format!("reading standalone provider key {key_path}"))?;
    let key = key.trim();
    if key.is_empty() || key.contains(['\r', '\n']) {
        bail!("standalone provider key is empty or malformed");
    }

    let broker_port = std::env::var("CHATGPT_BROWSER_API_OPENAI_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(DEFAULT_BROKER_PORT);
    let body = serde_json::to_vec(&serde_json::json!({
        "source": "toomux-live-registry",
        "authority": "toomux",
        "complete": true,
        "provider_session_ids": session_ids,
    }))?;
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, broker_port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .context("connecting to the standalone provider broker")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "POST {RECONCILE_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{broker_port}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("reading provider reconciliation HTTP response")?;
    let header = std::str::from_utf8(&response[..split])
        .context("reading provider reconciliation HTTP headers")?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .context("reading provider reconciliation HTTP status")?;
    let value: Value = serde_json::from_slice(&response[split + 4..])
        .context("reading provider reconciliation response")?;
    if !(200..300).contains(&status) {
        let detail = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("provider rejected reconciliation");
        bail!("provider reconciliation failed with HTTP {status}: {detail}");
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", reconcile_summary(&value)?);
    }
    Ok(())
}

pub fn status(json: bool) -> Result<()> {
    let out = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "5",
            READY_URL,
        ])
        .output()
        .context("checking the local model provider")?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(
            "model provider isn't ready{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout)
        .context("reading the model provider readiness response")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", summary(&value)?);
    }
    Ok(())
}

fn reconcile_summary(value: &Value) -> Result<String> {
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("provider reconciliation did not report success");
    }
    let live = value.get("present").and_then(Value::as_u64).unwrap_or(0);
    let removed = value.get("removed").and_then(Value::as_u64).unwrap_or(0);
    let deferred = value.get("deferred").and_then(Value::as_u64).unwrap_or(0);
    let leases = value.get("leases").and_then(Value::as_u64).unwrap_or(0);
    Ok(format!(
        "provider leases reconciled · live {live} · removed {removed} · deferred {deferred} · total {leases}"
    ))
}

fn summary(value: &Value) -> Result<String> {
    let state = value.get("upstream").unwrap_or(value);
    if !state.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        bail!("model provider reported not ready");
    }
    if let Some(workers) = state.get("workers").and_then(Value::as_array) {
        let healthy = workers
            .iter()
            .filter(|w| w.get("healthy").and_then(Value::as_bool) == Some(true))
            .count();
        let active: u64 = workers
            .iter()
            .filter_map(|w| w.get("active").and_then(Value::as_u64))
            .sum();
        let leases = state.get("leases").and_then(Value::as_u64).unwrap_or(0);
        let names = workers
            .iter()
            .map(|w| {
                let id = w.get("id").and_then(Value::as_str).unwrap_or("worker");
                let effort = w.get("effort").and_then(Value::as_str).unwrap_or("unknown");
                let worker_active = w.get("active").and_then(Value::as_u64).unwrap_or(0);
                let worker_leases = w
                    .get("lease_count")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        w.get("sessions")
                            .and_then(Value::as_array)
                            .map(|items| items.len() as u64)
                    })
                    .unwrap_or(0);
                let activity = if worker_active > 0 { "busy" } else { "idle" };
                format!("{id}:{effort}/{activity}/{worker_leases} leases")
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(format!(
            "gpt-5.6-sol · {healthy}/{} workers ready · active {active} · leases {leases} · {names}",
            workers.len()
        ));
    }
    let provider = state
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let effort = state
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let temporary = state
        .get("temporary")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let active = state.get("active").and_then(Value::as_u64).unwrap_or(0);
    let queued = state.get("queued").and_then(Value::as_u64).unwrap_or(0);
    let foreground = state
        .get("queued_foreground")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let background = state
        .get("queued_background")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let kind = state
        .get("urlKind")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    Ok(format!(
        "{provider} · {effort} · Temporary {} · {kind} · active {active} · queued {queued} ({foreground} fg, {background} bg)",
        if temporary { "ready" } else { "off" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_model_mapping_preserves_unrelated_settings_and_rows() {
        let mut settings = serde_json::json!({
            "model": "opus",
            "permissions": { "defaultMode": "default" },
            "modelPicker": {
                "replaceBuiltInOptions": false,
                "options": [
                    { "model": "company-model", "label": "Company" },
                    { "model": "chatgpt-browser", "label": "old", "behavesAs": "claude-haiku-4-5" },
                    { "model": "chatgpt-browser", "label": "duplicate" }
                ]
            }
        });
        assert!(ensure_client_model_picker(&mut settings, CLIENT_MODEL_FABLE).unwrap());
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["permissions"]["defaultMode"], "default");
        assert_eq!(settings["modelPicker"]["replaceBuiltInOptions"], false);
        let options = settings["modelPicker"]["options"].as_array().unwrap();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["model"], "company-model");
        assert_eq!(options[1]["model"], CLIENT_MODEL);
        assert_eq!(options[1]["behavesAs"], CLIENT_MODEL_FABLE);
        assert_eq!(options[1]["label"], "GPT-5.6 Sol High");
        assert!(!ensure_client_model_picker(&mut settings, CLIENT_MODEL_FABLE).unwrap());
    }

    #[test]
    fn client_model_mapping_creates_picker_without_replacing_builtins() {
        let mut settings = serde_json::json!({ "model": "opus" });
        assert!(ensure_client_model_picker(&mut settings, CLIENT_MODEL_FABLE).unwrap());
        let picker = settings["modelPicker"].as_object().unwrap();
        assert!(!picker.contains_key("replaceBuiltInOptions"));
        assert_eq!(picker["options"][0]["model"], CLIENT_MODEL);
        assert_eq!(picker["options"][0]["behavesAs"], CLIENT_MODEL_FABLE);
    }

    #[test]
    fn fable_requires_a_new_enough_claude_code() {
        assert!(!supports_fable("2.1.256 (Claude Code)"));
        assert!(supports_fable("2.1.257 (Claude Code)"));
        assert!(supports_fable("2.1.288 (Claude Code)"));
        assert!(supports_fable("v2.2.0"));
        assert!(!supports_fable("not-a-version"));
    }

    #[test]
    fn concise_summary_names_provider_effort_and_queue() {
        let v = serde_json::json!({
            "bridge": true,
            "upstream": {
                "ok": true,
                "provider": "gpt-5.6-sol",
                "reasoning_effort": "high",
                "temporary": true,
                "active": 0,
                "queued": 2,
                "queued_foreground": 1,
                "queued_background": 1,
                "urlKind": "temporary-root"
            }
        });
        assert_eq!(
            summary(&v).unwrap(),
            "gpt-5.6-sol · high · Temporary ready · temporary-root · active 0 · queued 2 (1 fg, 1 bg)"
        );
    }

    #[test]
    fn reconciliation_summary_never_exposes_session_ids() {
        let v = serde_json::json!({
            "ok": true,
            "authority": "toomux",
            "present": 7,
            "removed": 2,
            "deferred": 1,
            "leases": 9
        });
        assert_eq!(
            reconcile_summary(&v).unwrap(),
            "provider leases reconciled · live 7 · removed 2 · deferred 1 · total 9"
        );
    }

    #[test]
    fn worker_pool_summary_names_capacity_and_leases() {
        let v = serde_json::json!({
            "bridge": true,
            "upstream": {
                "ok": true,
                "leases": 2,
                "workers": [
                    {
                        "id": "worker-1",
                        "sessions": ["session-a"],
                        "lease_count": 1,
                        "effort": "high",
                        "active": 1,
                        "healthy": true
                    },
                    {
                        "id": "worker-2",
                        "sessions": ["session-b"],
                        "lease_count": 1,
                        "effort": "medium",
                        "active": 0,
                        "healthy": true
                    }
                ]
            }
        });
        assert_eq!(
            summary(&v).unwrap(),
            "gpt-5.6-sol · 2/2 workers ready · active 1 · leases 2 · worker-1:high/busy/1 leases, worker-2:medium/idle/1 leases"
        );
    }
}
