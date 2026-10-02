# ChatGPT CDP provider

Toomux can use the signed-in ChatGPT web application as a local model provider for Claude Code while keeping Claude Code as the sole tool executor.

## Provider contract

The canonical provider is **GPT-5.6 Sol**. Medium and High are reasoning-effort settings on that same provider, not different models.

- `bonnie` is the backwards-compatible **High** preset.
- `bonnie-medium` is the **Medium** preset.
- Direct OpenAI-compatible requests may use `model: "gpt-5.6-sol"` with `reasoning_effort: "medium" | "high"`.

Every request runs in ChatGPT **Temporary Chat**. The browser is kept authenticated and prewarmed, but each completed request reloads to a pristine `/?temporary-chat=true` root so disposable conversations do not accumulate in history.

The broker attests before submission that:

1. GPT-5.6 Sol is the checked web model.
2. The Thinking effort slider has the expected range 0..2.
3. Medium is slider value 1 (`Medium, 2 of 3.`).
4. High is slider value 2 (`High, 3 of 3.`).

If those invariants change, the request fails rather than silently using a different model or effort.

## Queueing

The browser lane is serialized. Requests may be marked `foreground` or `background`; queued foreground work is selected before queued background work. An active request is never preempted.

The bridge logs metadata-only request fingerprints (size, tool count, system-prompt hash, preset, and whether a large zero-tool request looks like background traffic). Prompt text is not logged.

## Local endpoints

- Anthropic compatibility bridge: `127.0.0.1:34560`
- OpenAI-compatible model API: `127.0.0.1:34561`
- Dedicated model browser CDP: `127.0.0.1:9223`

Use `toomux provider` for human-readable deep status or `toomux provider --json` for structured readiness.

## Prerequisites

The provider deliberately does not package authentication state. Before installation the machine must already have:

- a signed-in ChatGPT Chrome profile at `~/.local/share/toomux-chatgpt-model-browser`;
- the local provider key at `~/.local/state/toomux/bonnie-bridge.key` with user-only permissions;
- the browser/Xvfb support service used by the checked-in browser unit (`gpt-mcp-xvfb.service` on the current Linux deployment);
- Chrome, Node.js, `curl`, `jq`, and the managed Agent Browser binary used by `launch-browser.sh`.

## Installation

```bash
scripts/chatgpt-cdp/install.sh
# or install and restart the provider stack:
scripts/chatgpt-cdp/install.sh --restart
```

The installer refuses to proceed when the key or authenticated profile is absent. It installs the accepted runtime and acceptance probes, creates the two smoke-test command links, and reloads the user systemd configuration. The dedicated Chrome profile contains authentication state and is intentionally not stored in the repository.

## Acceptance

The checked-in probes mirror the production acceptance gates:

```bash
~/.local/bin/toomux-bonnie-smoke
node ~/.local/share/toomux/toomux-chatgpt-stream-acceptance.mjs
node ~/.local/share/toomux/toomux-sol-effort-acceptance.mjs
node ~/.local/share/toomux/toomux-sol-priority-acceptance.mjs
```

Acceptance covers direct inference, Temporary Chat reset, Claude tool use, streaming integrity, Medium/High selector attestation, foreground/background ordering, and post-restart durability.
