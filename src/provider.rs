use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::process::Command;

const READY_URL: &str = "http://127.0.0.1:34560/readyz";

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
