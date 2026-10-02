# toomux mobile web remote

## Production architecture

The phone remote is the same toomux product surface, not a second dashboard.

The workstation remains authoritative. Each paired browser gets an isolated host-side `toomux shell`;
the browser renders its ANSI frame and sends terminal input. Session discovery, accounts, handovers,
voyages, usage, memory and every other product rule therefore stay in the Rust host implementation.

```text
phone browser
  -> https://toomux.com/remote/  (Leptos/WASM renderer + input client)
  -> purpose-bound ByteTraverse WebRTC DataChannel
  -> btv-homebox capability: toomux
  -> fixed operator target http://10.30.0.1:7462
  -> authenticated toomux remote API v2
  -> per-device isolated toomux shell
```

The public site is only static bootstrap/UI. It is never a relay for private session data.

## Authority model

Transport reachability is not toomux authority. A fresh phone QR contains two independent one-time
authorities in the URL fragment:

1. a 15-minute single-use ByteTraverse enrollment ticket purpose-bound to `toomux`;
2. a separate 256-bit toomux application invite, also expiring after 15 minutes and stored by the
   workstation only as SHA-256.

After ByteTraverse is established, the browser spends the application invite through
`POST /api/v1/web/pair`. Redemption issues the ordinary random per-device toomux bearer. The
one-time app invite is spent before durable device authority is written, so replay cannot mint a
second device.

The ByteTraverse `console` and `bonnie-operator` capabilities do not imply `toomux`.
The `toomux` capability exposes only the explicit remote allowlist: health, web pairing, self
revocation, snapshot, TUI frame/input/close, and memory graph/page.

The HomeBox target is fixed by the workstation operator. The browser cannot supply a host, port,
URL or proxy target.

## Secret handling

The QR fragment is scrubbed from the browser URL before the WASM application loads. It must never
enter a query string, HTTP access log, analytics event, normal application log or browser history.

The browser device bearer is wrapped with AES-GCM under a non-extractable WebCrypto key before it
is stored in IndexedDB. Revocation removes the durable host grant; self-revocation also removes the
local wrapped bearer.

## QR rendering

A terminal cannot reliably report its font-cell pixel aspect ratio. In particular, common VTE/tmux
sessions report terminal rows/columns while pixel dimensions are zero. A character-cell QR can
therefore be visibly stretched as the terminal/window changes and is not an acceptable credential
renderer.

`toomux remote phone` and the in-app Remote action now:

1. mint the same one-use transport + app invite;
2. write an owner-only `0600` local page at
   `~/.local/state/toomux/remote/phone-pair.html`;
3. render the QR as integer 1x1 SVG modules inside a square viewBox with a four-module quiet zone;
4. open that local page in a real GUI browser;
5. remove the local page when the app invite is spent or found expired.

The terminal overlay is only the control/status surface; it no longer pretends terminal cells are
square pixels.

Acceptance requires the production jsQR scanner to decode the rendered code at phone, tablet and
desktop viewport sizes.

## Browser implementation

The production client lives in the `toomux-site` repository as a CSR-only Leptos/WASM app.

Rust owns:
- ANSI/SGR parsing and explicit terminal-cell rendering;
- high-DPI canvas drawing;
- exact 1-based terminal tap coordinates;
- touch scrolling and long-press/right-click semantics;
- printed toomux shortcut hit targets;
- hardware/soft keyboard mapping;
- 140 ms host frame polling and reconnect state;
- the memory viewer handoff.

A narrow JavaScript bridge owns browser-only facilities:
- the reviewed ByteTraverse WebRTC client;
- IndexedDB/WebCrypto bearer wrapping;
- camera access and jsQR scanning;
- authenticated request carriage.

The ByteTraverse browser transport is vendored by exact committed SHA in toomux-site and must not
be edited in place.

## Durable services

Host remote listener source:
`deploy/systemd/toomux-remote.service`

Installed listener:
`~/.config/systemd/user/toomux-remote.service`

ByteTraverse HomeBox adds the source-controlled drop-in from the ByteTraverse repository:
`deploy/systemd/bytetraverse.service.d/50-toomux.conf`

The live HomeBox must include:
`--toomux-target http://10.30.0.1:7462`

## Acceptance evidence — 2 October 2026

Host:
- full Rust suite: 166 passed, 1 ignored;
- MCP integration: passed;
- clippy all targets/features with warnings denied: passed;
- installed `toomux-remote.service` runs the same qualified installed binary.

ByteTraverse:
- purpose-bound browser security contract: 62/62 passed in a clean detached qualification tree;
- btv-session and btv-homebox suites: passed;
- clippy for btv-session, btv-console and btv-homebox, all targets with warnings denied: passed;
- the temporary qualification worktree was removed after producing the qualified release;
- live HomeBox runs the purpose-bound `toomux` target.

Browser/site:
- invite parser/build/CSP/provenance contract: passed;
- Rust ANSI/touch unit tests: passed;
- fresh release WASM rebuild: passed;
- production scanner decoded the same real QR rendered at 390x844, 768x1024 and 1280x720;
- a new browser identity negotiated `toomux`, spent the app grant, rendered a non-empty host TUI,
  accepted help/Escape input, and returned memory graph/page;
- QR-free reload reconnected from stored device credentials with no fragment and no new app grant;
- replay of the spent QR was rejected with `bad-or-used-ticket` and created no new device grant;
- browser self-revoke removed its host grant/local bearer and could not reconnect without pairing;
- host-side administrative revoke was detected on the next poll and returned the browser to pairing.

Test-only ByteTraverse `toomux` identities from acceptance were revoked after validation.
