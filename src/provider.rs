use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::process::Command;
use std::time::Duration;

const READY_URL: &str = "http://127.0.0.1:34560/readyz";
const RECONCILE_PATH: &str = "/v1/provider/sessions/reconcile";
const DEFAULT_BROKER_PORT: u16 = 34561;

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
