#!/usr/bin/env bash
set -euo pipefail

key_file="${HOME}/.local/state/toomux/bonnie-bridge.key"
model_api="http://127.0.0.1:34561"
bridge="http://127.0.0.1:34560"
claude_wrapper="${HOME}/.local/bin/toomux-claude"
claude_config="${HOME}/.claude-bonnie"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

[[ -r "$key_file" ]] || fail "missing model API key file"
[[ -x "$claude_wrapper" ]] || fail "missing toomux Claude wrapper"
key=$(<"$key_file")

ready=$(curl -fsS "$bridge/readyz") || fail "bridge readiness failed"
jq -e '.bridge == true and .upstream.ok == true and .upstream.authenticated == true and .upstream.temporary == true and .upstream.composer == true' <<<"$ready" >/dev/null   || fail "browser model is not deeply ready"

marker="TOOMUX_TEMP_CHAT_SMOKE_OK"
payload=$(jq -nc --arg marker "$marker" '{
  model:"bonnie",
  stream:false,
  messages:[{role:"user",content:("Reply with exactly: " + $marker)}]
}')
started=$(date +%s%3N)
response=$(curl -fsS   -H "Authorization: Bearer $key"   -H 'Content-Type: application/json'   --data "$payload"   "$model_api/v1/chat/completions") || fail "direct model completion failed"
elapsed_ms=$(( $(date +%s%3N) - started ))
actual=$(jq -r '.choices[0].message.content // ""' <<<"$response")
[[ "$actual" == "$marker" ]] || fail "unexpected direct model reply"

reset_ok=0
for _ in $(seq 1 50); do
  if state=$(curl -fsS "$model_api/readyz" 2>/dev/null)     && jq -e '.ok == true and .urlKind == "temporary-root" and .active == 0 and .queued == 0' <<<"$state" >/dev/null; then
    reset_ok=1
    break
  fi
  sleep 0.1
done
[[ "$reset_ok" == 1 ]] || fail "Temporary Chat did not reset to an empty root"

tool_marker="TOOMUX_CLAUDE_TOOL_LOOP_OK"
probe=$(mktemp)
stream=$(mktemp)
trap 'rm -f "$probe" "$stream"' EXIT
printf '%s\n' "$tool_marker" >"$probe"

CLAUDE_CONFIG_DIR="$claude_config" "$claude_wrapper"   -p "Read $probe using the Read tool. Then reply with the exact file contents and nothing else."   --model bonnie   --dangerously-skip-permissions   --tools Read   --allowedTools Read   --output-format stream-json   --verbose >"$stream"

jq -e 'select(.type=="assistant") | .message.content[]? | select(.type=="tool_use" and .name=="Read")' "$stream" >/dev/null   || fail "Claude did not emit a Read tool call"
grep -F "$tool_marker" "$stream" >/dev/null   || fail "Claude tool result/final marker missing"

printf 'PASS direct_ms=%s temporary_reset=PASS claude_tool_loop=PASS\n' "$elapsed_ms"
