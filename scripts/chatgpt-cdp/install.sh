#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
share="$HOME/.local/share/toomux"
browser_lib="$HOME/.local/lib/agent-browser-managed"
units="$HOME/.config/systemd/user"
bin="$HOME/.local/bin"
key="$HOME/.local/state/toomux/bonnie-bridge.key"
seed_profile="$HOME/.local/share/toomux-chatgpt-model-browser"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 78
}

wait_http() {
  local url=$1
  for _ in $(seq 1 120); do
    curl -fsS --max-time 1 "$url" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

mkdir -p "$share" "$browser_lib" "$units" "$bin"
[[ -r "$key" ]] || fail "missing provider key: $key"

for i in 1 2; do
  worker_profile="$HOME/.local/share/toomux-chatgpt-worker-$i"
  if [[ ! -d "$worker_profile" ]]; then
    [[ -d "$seed_profile" ]] || fail "missing authenticated seed profile: $seed_profile"
    tmp="${worker_profile}.tmp.$$"
    rm -rf "$tmp"
    mkdir -p "$tmp"
    cp -a --reflink=auto "$seed_profile/." "$tmp/"
    rm -f "$tmp/SingletonLock" "$tmp/SingletonSocket" "$tmp/SingletonCookie"
    chmod -R u+rwX,go-rwx "$tmp"
    mv "$tmp" "$worker_profile"
  fi
done

install -m 700 "$root/scripts/chatgpt-cdp/model-api.mjs" "$share/toomux-chatgpt-model-api.mjs"
install -m 700 "$root/scripts/chatgpt-cdp/anthropic-bridge.mjs" "$share/bonnie-anthropic-bridge.mjs"
install -m 700 "$root/scripts/chatgpt-cdp/worker-broker.mjs" "$share/worker-broker.mjs"
install -m 755 "$root/scripts/chatgpt-cdp/launch-browser.sh" "$browser_lib/launch-bonnie-browser.sh"
install -m 755 "$root/scripts/chatgpt-cdp/toomux-claude" "$bin/toomux-claude"
install -m 700 "$root/scripts/chatgpt-cdp/smoke.sh" "$share/toomux-chatgpt-model-smoke.sh"
install -m 700 "$root/scripts/chatgpt-cdp/stream-acceptance.mjs" "$share/toomux-chatgpt-stream-acceptance.mjs"
install -m 700 "$root/scripts/chatgpt-cdp/effort-acceptance.mjs" "$share/toomux-sol-effort-acceptance.mjs"
install -m 700 "$root/scripts/chatgpt-cdp/effort-affinity-acceptance.mjs" "$share/toomux-sol-effort-affinity-acceptance.mjs"
install -m 700 "$root/scripts/chatgpt-cdp/multiprocess-acceptance.mjs" "$share/toomux-chatgpt-multiprocess-acceptance.mjs"
install -m 700 "$root/scripts/chatgpt-cdp/failover-seed.mjs" "$share/toomux-chatgpt-failover-seed.mjs"
install -m 700 "$root/scripts/chatgpt-cdp/production-two-claude.sh" "$share/toomux-chatgpt-production-two-claude.sh"
ln -sfn "$share/toomux-chatgpt-model-smoke.sh" "$bin/toomux-bonnie-smoke"
ln -sfn "$share/toomux-chatgpt-stream-acceptance.mjs" "$bin/toomux-bonnie-stream-smoke"

cp "$root/deploy/systemd/toomux-chatgpt-worker-1.service" "$units/toomux-chatgpt-worker-1.service"
cp "$root/deploy/systemd/toomux-chatgpt-worker-2.service" "$units/toomux-chatgpt-worker-2.service"
for unit in   toomux-chatgpt-worker-api-1.service   toomux-chatgpt-worker-api-2.service   toomux-chatgpt-worker-broker.service   toomux-bonnie-bridge.service
do
  sed "s|@HOME@|$HOME|g" "$root/deploy/systemd/$unit" > "$units/$unit"
done

systemctl --user daemon-reload
systemctl --user disable --now   toomux-chatgpt-model-browser.service   toomux-chatgpt-model-api.service >/dev/null 2>&1 || true

if [[ "${1:-}" == "--restart" ]]; then
  systemctl --user enable     toomux-chatgpt-worker-1.service     toomux-chatgpt-worker-2.service     toomux-chatgpt-worker-api-1.service     toomux-chatgpt-worker-api-2.service     toomux-chatgpt-worker-broker.service     toomux-bonnie-bridge.service >/dev/null

  systemctl --user restart toomux-chatgpt-worker-1.service toomux-chatgpt-worker-2.service
  wait_http "http://127.0.0.1:9231/json/version" || fail "worker browser 1 did not become ready"
  wait_http "http://127.0.0.1:9232/json/version" || fail "worker browser 2 did not become ready"

  systemctl --user restart toomux-chatgpt-worker-api-1.service toomux-chatgpt-worker-api-2.service
  wait_http "http://127.0.0.1:34581/readyz" || fail "worker API 1 did not become ready"
  wait_http "http://127.0.0.1:34582/readyz" || fail "worker API 2 did not become ready"

  systemctl --user restart toomux-chatgpt-worker-broker.service
  wait_http "http://127.0.0.1:34561/readyz" || fail "worker broker did not become ready"

  systemctl --user restart toomux-bonnie-bridge.service
  wait_http "http://127.0.0.1:34560/readyz" || fail "Anthropic bridge did not become ready"
fi

printf 'Installed Toomux ChatGPT worker-pool provider.\n'
printf 'Status: %s\n' "toomux provider"
printf 'Smoke:  %s\n' "$bin/toomux-bonnie-smoke"
