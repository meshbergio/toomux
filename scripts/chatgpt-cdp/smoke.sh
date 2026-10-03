#!/usr/bin/env bash
set -euo pipefail

key_file="${HOME}/.local/state/toomux/bonnie-bridge.key"
broker="http://127.0.0.1:34561"
bridge="http://127.0.0.1:34560"
claude_wrapper="${HOME}/.local/bin/toomux-claude"
claude_config="${HOME}/.claude-bonnie"
worker_ports=(34581 34582)

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

[[ -r "$key_file" ]] || fail "missing model API key file"
[[ -x "$claude_wrapper" ]] || fail "missing toomux Claude wrapper"
key=$(<"$key_file")

ready=$(curl -fsS "$bridge/readyz") || fail "bridge readiness failed"
jq -e '
  .bridge == true
  and .upstream.ok == true
  and (.upstream.workers | length) >= 2
  and all(.upstream.workers[]; .healthy == true)
' <<<"$ready" >/dev/null || fail "worker pool is not deeply ready"

marker="TOOMUX_TEMP_CHAT_SMOKE_OK"
payload=$(jq -nc --arg marker "$marker" '{
  model:"bonnie",
  reasoning_effort:"high",
  provider_session_id:"smoke-direct",
  stream:false,
  messages:[{role:"user",content:("Reply with exactly: " + $marker)}]
}')
started=$(date +%s%3N)
response=$(curl -fsS   -H "Authorization: Bearer $key"   -H 'Content-Type: application/json'   --data "$payload"   "$broker/v1/chat/completions") || fail "direct broker completion failed"
elapsed_ms=$(( $(date +%s%3N) - started ))
actual=$(jq -r '.choices[0].message.content // ""' <<<"$response")
[[ "$actual" == "$marker" ]] || fail "unexpected direct model reply"

reset_ok=0
for _ in $(seq 1 120); do
  broker_state=$(curl -fsS "$broker/readyz" 2>/dev/null || true)
  if [[ -n "$broker_state" ]] && jq -e '.ok == true and all(.workers[]; .healthy == true)' <<<"$broker_state" >/dev/null 2>&1; then
    all_workers=1
    for port in "${worker_ports[@]}"; do
      worker_state=$(curl -fsS "http://127.0.0.1:$port/readyz" 2>/dev/null || true)
      if [[ -z "$worker_state" ]] || ! jq -e '.ok == true and .temporary == true and .urlKind == "temporary-root" and .active == 0 and .queued == 0' <<<"$worker_state" >/dev/null 2>&1; then
        all_workers=0
        break
      fi
    done
    if [[ "$all_workers" == 1 ]]; then
      reset_ok=1
      break
    fi
  fi
  sleep 0.1
done
[[ "$reset_ok" == 1 ]] || fail "worker Temporary Chats did not reset to clean roots"

tool_marker="TOOMUX_CLAUDE_TOOL_LOOP_OK"
probe=$(mktemp)
stream=$(mktemp)
trap 'rm -f "$probe" "$stream"' EXIT
printf '%s\n' "$tool_marker" >"$probe"

TOOMUX_PROVIDER_SESSION_ID=smoke-claude CLAUDE_CONFIG_DIR="$claude_config" "$claude_wrapper"   -p "Read $probe using the Read tool. Then reply with the exact file contents and nothing else."   --model bonnie   --dangerously-skip-permissions   --tools Read   --allowedTools Read   --output-format stream-json   --verbose >"$stream"

jq -e 'select(.type=="assistant") | .message.content[]? | select(.type=="tool_use" and .name=="Read")' "$stream" >/dev/null   || fail "Claude did not emit a Read tool call"
grep -F "$tool_marker" "$stream" >/dev/null   || fail "Claude tool result/final marker missing"

leases=$(curl -fsS "$broker/health" | jq -r '.leases')
printf 'PASS direct_ms=%s temporary_reset=PASS claude_tool_loop=PASS leases=%s workers=2\n' "$elapsed_ms" "$leases"
