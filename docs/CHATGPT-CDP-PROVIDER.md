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
4. `toomux provider [--json]`, a native health/status view over the local provider.

The wrapper prefers the standalone provider credential:

```text
~/.local/state/chatgpt-browser-api/api.key
```

and temporarily accepts the historical Toomux key path as a rollback fallback.

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

This copies only `toomux-claude` into `~/.local/bin` (or `$TOOMUX_BIN_DIR`). It does not install, start, stop or configure browser workers or provider services.

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
