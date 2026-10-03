# ChatGPT CDP provider

Toomux can use the signed-in ChatGPT web application as a local model provider for Claude Code while Claude Code remains the sole tool executor.

## Architecture

The production provider is a **session-aware browser-worker pool**:

```text
Claude / Toomux session
        │
        ▼
Anthropic bridge :34560
        │
        ▼
worker broker :34561
   ┌────┴────┐
   ▼         ▼
worker 1    worker 2
API :34581  API :34582
CDP :9231   CDP :9232
Chrome      Chrome
Temporary   Temporary
Chat        Chat
```

Each worker is an independent authenticated Chrome process/profile with exactly one managed ChatGPT Temporary Chat page. The broker assigns a stable Toomux provider-session ID to a worker and preserves that affinity for subsequent model and tool-result turns.

The initial production pool contains two workers. The broker can keep multiple session leases on one worker when the pool is oversubscribed; requests on an individual worker remain serialized while different workers can generate concurrently.

### Why workers, not tabs?

We tested multiple Temporary Chat tabs in one Chrome process first. Separate tabs could retain independent state and accept prompt text, but simultaneous generation was not reliable: at both four-way and two-way concurrency one tab could fail to expose the ChatGPT Send control.

Two independent Chrome processes passed synchronized generation with overlapping in-flight windows and exact responses. Two concurrent real Claude Code `Read` tool loops also passed through separate workers. Browser process, not browser tab, is therefore the accepted concurrency boundary.

## Provider contract

The canonical provider is **GPT-5.6 Sol**. Medium and High are reasoning-effort settings on that same provider, not separate models.

- `bonnie` is the backwards-compatible **High** preset.
- `bonnie-medium` is the **Medium** preset.
- OpenAI-compatible requests may use `model: "gpt-5.6-sol"` plus `reasoning_effort: "medium" | "high"`.

Each worker attests the real ChatGPT web control before an effort change:

1. GPT-5.6 Sol is the selected web model.
2. Thinking effort is the 0..2 ARIA slider.
3. Medium is value 1 / `Medium, 2 of 3.`.
4. High is value 2 / `High, 3 of 3.`.

If those invariants change, the request fails rather than silently using another model or effort.

## Session affinity

Toomux supplies `TOOMUX_PROVIDER_SESSION_ID` when a Claude session is launched or resumed. The Toomux Claude wrapper forwards that identity as a private request header; the Anthropic bridge carries it as provider metadata; the worker broker uses it only for routing.

The session identifier is **not added to the model prompt**.

Affinity ownership lives only in the worker broker. The broker removes `provider_session_id` before forwarding to a worker API, so worker-local execution remains free to change Medium/High effort between requests.

If a leased worker becomes unhealthy, the broker removes the stale lease and reassigns the same Toomux session to a healthy worker. The bridge and broker remain available as long as at least one worker is healthy.

## Temporary Chat lifecycle

Each worker owns one authenticated Temporary Chat page.

For every model call:

1. the worker waits for any prior reset;
2. verifies authenticated Temporary Chat readiness;
3. applies/attests the requested Sol effort when necessary;
4. inserts and submits the prompt through the real ChatGPT UI;
5. streams/reads the assistant result;
6. reloads to a pristine `/?temporary-chat=true` root.

Disposable conversations therefore do not accumulate in ChatGPT history.

## Scheduling

The broker prefers:

1. an existing healthy session lease;
2. an idle healthy worker already at the requested effort;
3. the least-loaded healthy worker.

Each worker model API retains foreground/background ordering for work queued on that worker. Different browser workers can execute simultaneously.

Routing liveness uses the worker API's cheap `/health` endpoint, so a worker actively generating or resetting is not marked unhealthy merely because a deep CDP readiness probe takes several seconds. Deep `/readyz` remains available for status and acceptance checks.

## Local endpoints

- Anthropic compatibility bridge: `127.0.0.1:34560`
- Session-aware worker broker: `127.0.0.1:34561`
- Worker API 1: `127.0.0.1:34581`
- Worker API 2: `127.0.0.1:34582`
- Worker Chrome CDP 1: `127.0.0.1:9231`
- Worker Chrome CDP 2: `127.0.0.1:9232`

All provider listeners are localhost-only.

Use:

```bash
toomux provider
toomux provider --json
```

for native status.

## Authentication and profiles

Authentication state is deliberately not stored in Git.

The installer expects the local provider key at:

```text
~/.local/state/toomux/bonnie-bridge.key
```

Worker profiles live at:

```text
~/.local/share/toomux-chatgpt-worker-1
~/.local/share/toomux-chatgpt-worker-2
```

When a worker profile is absent, the installer seeds it from the existing signed-in profile:

```text
~/.local/share/toomux-chatgpt-model-browser
```

using a reflink-capable copy where supported and removing Chrome singleton locks.

## Installation

```bash
scripts/chatgpt-cdp/install.sh
```

Install and restart the complete production worker pool:

```bash
scripts/chatgpt-cdp/install.sh --restart
```

The installer retires the old single-browser/model services, installs the two browser workers, worker APIs, broker, bridge, wrapper and acceptance probes, then starts the stack in dependency order with readiness gates.

## Acceptance

Primary production smoke test:

```bash
~/.local/bin/toomux-bonnie-smoke
```

Additional checked-in acceptance probes:

```bash
node scripts/chatgpt-cdp/multiprocess-acceptance.mjs
bash scripts/chatgpt-cdp/production-two-claude.sh
node scripts/chatgpt-cdp/effort-affinity-acceptance.mjs
node scripts/chatgpt-cdp/failover-seed.mjs
```

The campaign acceptance includes:

- exact direct inference;
- streaming integrity;
- Temporary Chat reset to a clean root;
- real Claude Code tool-use/result loops;
- simultaneous Claude sessions on distinct workers;
- stable provider-session affinity;
- Medium ↔ High effort switching on a sticky worker;
- deliberate worker loss and lease reassignment;
- complete worker/browser/API/broker/bridge restart durability;
- single Temporary Chat page per browser worker;
- localhost-only listeners and systemd restart supervision.
