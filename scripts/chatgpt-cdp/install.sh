#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
share="$HOME/.local/share/toomux"
browser_lib="$HOME/.local/lib/agent-browser-managed"
units="$HOME/.config/systemd/user"

mkdir -p "$share" "$browser_lib" "$units" "$HOME/.local/bin"

key="$HOME/.local/state/toomux/bonnie-bridge.key"
profile="$HOME/.local/share/toomux-chatgpt-model-browser"
[[ -r "$key" ]] || { echo "Missing provider key: $key" >&2; exit 78; }
[[ -d "$profile" ]] || { echo "Missing authenticated ChatGPT browser profile: $profile" >&2; exit 78; }

install -m 700 "$root/scripts/chatgpt-cdp/model-api.mjs" "$share/toomux-chatgpt-model-api.mjs"
install -m 700 "$root/scripts/chatgpt-cdp/anthropic-bridge.mjs" "$share/bonnie-anthropic-bridge.mjs"
install -m 755 "$root/scripts/chatgpt-cdp/launch-browser.sh" "$browser_lib/launch-bonnie-browser.sh"
install -m 700 "$root/scripts/chatgpt-cdp/smoke.sh" "$share/toomux-chatgpt-model-smoke.sh"
install -m 700 "$root/scripts/chatgpt-cdp/stream-acceptance.mjs" "$share/toomux-chatgpt-stream-acceptance.mjs"
install -m 700 "$root/scripts/chatgpt-cdp/effort-acceptance.mjs" "$share/toomux-sol-effort-acceptance.mjs"
install -m 700 "$root/scripts/chatgpt-cdp/priority-acceptance.mjs" "$share/toomux-sol-priority-acceptance.mjs"
ln -sfn "$share/toomux-chatgpt-model-smoke.sh" "$HOME/.local/bin/toomux-bonnie-smoke"
ln -sfn "$share/toomux-chatgpt-stream-acceptance.mjs" "$HOME/.local/bin/toomux-bonnie-stream-smoke"

cp "$root/deploy/systemd/toomux-chatgpt-model-browser.service" "$units/toomux-chatgpt-model-browser.service"
for unit in toomux-chatgpt-model-api.service toomux-bonnie-bridge.service; do
  sed "s|@HOME@|$HOME|g" "$root/deploy/systemd/$unit" > "$units/$unit"
done

systemctl --user daemon-reload

if [[ "${1:-}" == "--restart" ]]; then
  systemctl --user restart     toomux-chatgpt-model-browser.service     toomux-chatgpt-model-api.service     toomux-bonnie-bridge.service
fi

printf 'Installed Toomux ChatGPT CDP provider.\n'
printf 'Status: toomux provider\n'
printf 'Smoke:  ~/.local/bin/toomux-bonnie-smoke\n'
