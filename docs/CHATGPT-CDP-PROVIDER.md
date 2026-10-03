# ChatGPT Browser API integration

The ChatGPT browser-provider implementation is no longer owned by this repository.

Canonical source:

```text
meshbergio/chatgpt-browser-api
```

The standalone service owns:

- authenticated Chrome worker processes and profiles;
- Temporary Chat lifecycle and CDP automation;
- GPT-5.6 Sol model/effort attestation;
- OpenAI-compatible and Anthropic-compatible APIs;
- worker scheduling and affinity;
- service/systemd lifecycle;
- provider acceptance tests, configuration and releases.

Toomux is a client of that API.

## Toomux-owned integration

Toomux retains only:

1. `TOOMUX_PROVIDER_SESSION_ID` generation/carrying for stable worker affinity;
2. `scripts/toomux-claude`, which routes the dedicated `~/.claude-bonnie` account to the local Anthropic-compatible endpoint;
3. `scripts/install-chatgpt-browser-client.sh`, which installs only that thin wrapper and no provider runtime;
4. `toomux provider [--json]`, a native health/status view over the local provider;
5. `toomux provider --reconcile [--json]`, which sends the complete live Toomux session registry to the standalone broker as the `toomux` authority;
6. `toomux provider --configure-client`, which non-destructively registers the standalone provider model in the dedicated Claude Code profile.

The wrapper stamps Toomux-owned requests with `x-toomux-provider-session-id`. The standalone broker hashes the identifier for lease storage and keeps Toomux leases in their own authority scope. Toomux's periodic status tick triggers reconciliation about every 30 seconds as best-effort background work; provider outages do not block the TUI. A lease reported absent while its request is still in flight is expired only at the request boundary by the standalone broker.

The wrapper uses the standalone provider credential:

```text
~/.local/state/chatgpt-browser-api/api.key
```

The retired Toomux-owned provider key is no longer consulted by the live client integration. Historical migration and rollback behavior remains available in the standalone provider repository and release history.

The Claude wrapper requests the standalone provider's stable High alias, `chatgpt-browser`; the old `bonnie` model name is retained only as standalone compatibility for older external callers. On Claude Code 2.1.257 or later, the installer maps `chatgpt-browser` with `behavesAs: claude-fable-5-1`; older Claude Code releases fall back to `claude-opus-4-6`. The custom model ID is still the ID sent to the local provider. Existing Claude settings, picker rows and `replaceBuiltInOptions` policy are preserved.

Fable's client profile uses a leaner agent harness and enables its long-running-agent protocol, but Claude Code also assumes a native 1M context window for Fable. To keep Toomux's existing context-pressure boundary, `toomux-claude` defaults `CLAUDE_CODE_AUTO_COMPACT_WINDOW` to `200000`. An explicit caller value still wins. This preserves proactive compaction at 200K while retaining the Fable client behavior.

Claude Code assigns custom gateway models the list price of their `behavesAs` model. Toomux therefore ignores the status-line `total_cost_usd` field when the actual model ID is a custom/non-`claude-*` provider ID such as `chatgpt-browser`; transcript token accounting already treats such local-provider IDs as zero API spend.

## Runtime endpoint

The standalone provider's default Anthropic compatibility endpoint remains:

```text
http://127.0.0.1:34560
```

This keeps existing Claude/toomux sessions compatible across the ownership migration.

Install or refresh the Toomux-side wrapper from this repository with:

```bash
./scripts/install-chatgpt-browser-client.sh
```

This copies only `toomux-claude` into `~/.local/bin` (or `$TOOMUX_BIN_DIR`) and asks the installed Toomux binary to register the model mapping above. It does not install, start, stop or configure browser workers or provider services.

## Installing or developing the provider

Do not add browser automation or provider runtime code back to Toomux.

Use the standalone repository for installation, worker management, acceptance, browser/UI changes, scheduling, configuration and release work.

See that repository's:

- `README.md`
- `GOVERNANCE.md`
- `docs/ARCHITECTURE.md`
- `docs/OPERATIONS.md`
- `docs/ACCEPTANCE.md`

## Historical provenance

The provider was originally developed and accepted in Toomux in commits:

- `e1dca81 feat: add effort-aware ChatGPT Sol provider`
- `77b87f6 feat: add concurrent ChatGPT browser workers`

The implementation was extracted into `meshbergio/chatgpt-browser-api` on 3 October 2026. Historical commits remain available in Git; duplicated provider code is deliberately removed from the current Toomux tree.
