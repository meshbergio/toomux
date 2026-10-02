#!/usr/bin/env bash
set -euo pipefail

# Own the Crew browser as an ordinary Chrome process and attach Agent Browser to
# it over loopback CDP. Agent Browser's own launch path intentionally marks the
# browser as automated (navigator.webdriver=true), which chatgpt.com's browser
# security check rejects. Attaching to externally owned Chrome preserves the
# real-browser contract without spoofing browser signals.

agent_browser_bin="${BONNIE_AGENT_BROWSER_BIN:-$HOME/.local/lib/agent-browser-managed/node_modules/agent-browser/bin/agent-browser-linux-x64}"
chrome_bin="${BONNIE_CHROME_BIN:-/opt/google/chrome/chrome}"
session="${BONNIE_AGENT_BROWSER_SESSION:-bonnie-crew}"
profile="${BONNIE_AGENT_BROWSER_PROFILE:-$HOME/.local/share/gpt-mcp-worker-browser}"
stable_port="${BONNIE_AGENT_BROWSER_CDP_PORT:-9222}"
start_url="${BONNIE_AGENT_BROWSER_START_URL:-https://chatgpt.com/}"
virtual_display="${BONNIE_AGENT_BROWSER_DISPLAY-:99}"
restore_last_session="${BONNIE_AGENT_BROWSER_RESTORE_LAST_SESSION:-1}"
disable_gpu="${BONNIE_AGENT_BROWSER_DISABLE_GPU:-0}"
chrome_log_level="${BONNIE_AGENT_BROWSER_CHROME_LOG_LEVEL:-}"
chrome_pid=""
daemon_pid=""
runtime_namespace="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/agent-browser/namespaces/${session}"

export AGENT_BROWSER_NAMESPACE="$session"
export AGENT_BROWSER_SESSION="$session"
export AGENT_BROWSER_IDLE_TIMEOUT_MS=0
unset WAYLAND_DISPLAY XAUTHORITY
if [[ -n "$virtual_display" ]]; then
  export DISPLAY="$virtual_display"
else
  unset DISPLAY
fi

profile_chrome_pids() {
  local proc pid comm cmdline
  for proc in /proc/[0-9]*; do
    [[ -r "$proc/comm" && -r "$proc/cmdline" ]] || continue
    pid=${proc##*/}
    comm=$(cat "$proc/comm" 2>/dev/null || true)
    [[ "$comm" == chrome ]] || continue
    cmdline=$(tr '\0' ' ' <"$proc/cmdline" 2>/dev/null || true)
    [[ "$cmdline" == *"--user-data-dir=${profile}"* ]] && printf '%s\n' "$pid"
  done
}

reap_profile_chrome() {
  local -a pids=()
  mapfile -t pids < <(profile_chrome_pids)
  ((${#pids[@]} == 0)) || kill -TERM "${pids[@]}" 2>/dev/null || true
  for _ in $(seq 1 20); do
    mapfile -t pids < <(profile_chrome_pids)
    ((${#pids[@]} == 0)) && return 0
    sleep 0.1
  done
  mapfile -t pids < <(profile_chrome_pids)
  ((${#pids[@]} == 0)) || kill -KILL "${pids[@]}" 2>/dev/null || true
}

reset_agent_browser_runtime() {
  local pid_file="${runtime_namespace}/run/${session}.pid"
  local stale_pid="" stale_cmd=""
  if [[ -r "$pid_file" ]]; then
    stale_pid=$(cat "$pid_file" 2>/dev/null || true)
  fi
  if [[ "$stale_pid" =~ ^[0-9]+$ ]] && kill -0 "$stale_pid" 2>/dev/null; then
    stale_cmd=$(tr '\0' ' ' <"/proc/${stale_pid}/cmdline" 2>/dev/null || true)
    if [[ "$stale_cmd" != *"agent-browser"* ]]; then
      echo "Refusing to kill non-Agent-Browser pid ${stale_pid} from ${pid_file}" >&2
      exit 1
    fi
    kill -TERM "$stale_pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
      kill -0 "$stale_pid" 2>/dev/null || break
      sleep 0.1
    done
    kill -0 "$stale_pid" 2>/dev/null && kill -KILL "$stale_pid" 2>/dev/null || true
  fi
  # Runtime target/config files encode the previous launch mode. They are not
  # auth state: the durable Chrome profile above owns cookies and storage.
  rm -rf "$runtime_namespace"
}

if [[ "${1:-}" == "--cleanup" ]]; then
  reap_profile_chrome
  exit 0
fi

cleanup() {
  trap - EXIT INT TERM
  set +e
  if [[ -n "$daemon_pid" ]]; then
    kill -TERM "$daemon_pid" 2>/dev/null
    wait "$daemon_pid" 2>/dev/null
  fi
  if [[ -n "$chrome_pid" ]]; then
    kill -TERM "$chrome_pid" 2>/dev/null
  fi
  reap_profile_chrome
}

shutdown() {
  cleanup
  exit 0
}

trap cleanup EXIT
trap shutdown INT TERM

[[ -x "$agent_browser_bin" ]] || { echo "Agent Browser binary not found: $agent_browser_bin" >&2; exit 1; }
[[ -x "$chrome_bin" ]] || { echo "Chrome binary not found: $chrome_bin" >&2; exit 1; }

mkdir -p "$profile"
chmod 700 "$profile"
reset_agent_browser_runtime
reap_profile_chrome

chrome_args=(
  --remote-debugging-address=127.0.0.1
  --remote-debugging-port="$stable_port"
  --user-data-dir="$profile"
  --no-first-run
  --no-default-browser-check
  --disable-features=Translate
  --window-size=1280,900
)
[[ "$restore_last_session" == "0" ]] || chrome_args+=(--restore-last-session)
if [[ "$disable_gpu" == "1" ]]; then
  chrome_args+=(--disable-gpu --disable-webgl)
fi
if [[ -n "$chrome_log_level" ]]; then
  chrome_args+=("--log-level=$chrome_log_level")
fi

"$chrome_bin" "${chrome_args[@]}" "$start_url" &
chrome_pid=$!

ready=0
for _ in $(seq 1 80); do
  if ! kill -0 "$chrome_pid" 2>/dev/null; then
    echo "Crew Chrome exited before CDP became ready" >&2
    exit 1
  fi
  if curl --fail --silent --max-time 1 \
    "http://127.0.0.1:${stable_port}/json/version" >/dev/null; then
    ready=1
    break
  fi
  sleep 0.1
done
[[ "$ready" == 1 ]] || { echo "Stable Bonnie CDP endpoint did not become ready on port ${stable_port}" >&2; exit 1; }

# /json/version becomes reachable before Chrome necessarily has a page target.
# Connecting Agent Browser in that gap can leave it with no external target and
# cause the next command to start its own automation-marked browser. Wait for
# the real ChatGPT tab before attaching so the session is bound to this Chrome.
page_ready=0
for _ in $(seq 1 80); do
  if ! kill -0 "$chrome_pid" 2>/dev/null; then
    echo "Crew Chrome exited before its ChatGPT page became ready" >&2
    exit 1
  fi
  if curl --fail --silent --max-time 1 \
    "http://127.0.0.1:${stable_port}/json/list" \
    | jq -e 'any(.[]; .type == "page" and ((.url // "") | startswith("https://chatgpt.com")))' >/dev/null 2>&1; then
    page_ready=1
    break
  fi
  sleep 0.1
done
[[ "$page_ready" == 1 ]] || { echo "Crew Chrome did not expose its ChatGPT page target" >&2; exit 1; }

# Connect once so subsequent Agent Browser commands in the bonnie-crew namespace
# reuse this externally owned browser instead of launching their own Chrome.
"$agent_browser_bin" --session "$session" connect "$stable_port" --json >/dev/null
session_info=$("$agent_browser_bin" --session "$session" session info --json)
daemon_pid=$(jq -r '.data.runtime.backgroundPid // empty' <<<"$session_info")
[[ "$daemon_pid" =~ ^[0-9]+$ ]] || { echo "Could not resolve Agent Browser daemon pid" >&2; exit 1; }

# This is a product invariant, not bot-signal spoofing: if Agent Browser ever
# starts its own automation-marked browser again, stop rather than silently
# degrading into chatgpt.com's security interstitial.
webdriver=$("$agent_browser_bin" --session "$session" eval 'navigator.webdriver' --json | jq -r '.data.result | tostring')
[[ "$webdriver" == "false" ]] || {
  echo "Crew browser invariant failed: navigator.webdriver must be false" >&2
  exit 1
}

echo "Bonnie ordinary Chrome ready on 127.0.0.1:${stable_port}; Agent Browser attached"
while true; do
  sleep 5
  if ! kill -0 "$chrome_pid" 2>/dev/null; then
    echo "Bonnie Crew Chrome exited" >&2
    exit 1
  fi
  if ! kill -0 "$daemon_pid" 2>/dev/null; then
    echo "Bonnie Agent Browser daemon exited" >&2
    exit 1
  fi
  if ! curl --fail --silent --max-time 2 \
    "http://127.0.0.1:${stable_port}/json/version" >/dev/null; then
    echo "Bonnie stable CDP endpoint is unavailable" >&2
    exit 1
  fi
done
