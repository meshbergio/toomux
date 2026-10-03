#!/usr/bin/env bash
set -euo pipefail
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT
printf 'PROD_POOL_A_OK\n' > "$tmpdir/a.txt"
printf 'PROD_POOL_B_OK\n' > "$tmpdir/b.txt"
run_one() {
  local sid=$1 file=$2 out=$3
  TOOMUX_PROVIDER_SESSION_ID="$sid" CLAUDE_CONFIG_DIR="$HOME/.claude-bonnie" "$HOME/.local/bin/toomux-claude"     -p "Read $file using the Read tool. Then reply with the exact file contents and nothing else."     --model bonnie --tools Read --allowedTools Read --output-format stream-json --verbose > "$out"
}
start=$(date +%s%3N)
run_one prod-claude-a "$tmpdir/a.txt" "$tmpdir/a.jsonl" & pa=$!
sleep 0.10
run_one prod-claude-b "$tmpdir/b.txt" "$tmpdir/b.jsonl" & pb=$!
wait "$pa"; ta=$(date +%s%3N)
wait "$pb"; tb=$(date +%s%3N)
for x in a b; do jq -e 'select(.type=="assistant") | .message.content[]? | select(.type=="tool_use" and .name=="Read")' "$tmpdir/$x.jsonl" >/dev/null; done
grep -F PROD_POOL_A_OK "$tmpdir/a.jsonl" >/dev/null
grep -F PROD_POOL_B_OK "$tmpdir/b.jsonl" >/dev/null
echo "elapsed_a_ms=$((ta-start)) elapsed_b_ms=$((tb-start))"
curl -fsS http://127.0.0.1:34561/health | jq -c '{leases,workers:[.workers[]|{id,sessions,active,healthy,effort}]}'
echo PRODUCTION_TWO_CLAUDE=PASS
