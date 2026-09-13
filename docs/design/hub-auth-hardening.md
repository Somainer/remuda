# Hub authentication hardening

Security review 2 tasks 1, 2, 10, 11, 15, and 17 are implemented on the Hub and Node boundaries.

- Static disk assets and the SPA index must resolve inside the canonical web root. Raw parent traversal, absolute paths, NUL, and backslashes are rejected before file access.
- The bootstrap access code pairs devices only. Nodes enroll with a one-shot enroll token minted by an authenticated device. Existing host identities re-announce with their own host token.
- All Hub responses carry CSP, `frame-ancestors 'none'`, `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, and `Referrer-Policy: no-referrer`. IBM Plex fonts are bundled locally. CSP permits inline styles for React and xterm layout; scripts remain restricted to same-origin files.
- Browser REST and follow connections use the HttpOnly, SameSite=Strict device cookie. REST also accepts native-client bearer headers. Follow query parameters never authenticate. Device tokens returned at login/pairing stay in memory; localStorage retains only device metadata and removes legacy token copies. Self-revocation expires the cookie on the server.
- The Node checks every logical `tty.write` key before enqueueing a command. The accepted vocabulary is the CLI key encoder's named keys, `ctrl+a` through `ctrl+z`, and printable non-whitespace single characters. Raw terminal byte streams have a separate transport contract.

## Authentication attempt budgets

`POST /v1/login` and `POST /v1/devices/pair` each allow a burst of 10 requests per peer IP, replenishing one request every 10 seconds. A shared global bucket allows a burst of 64 and replenishes 8 requests per second. Every attempt counts, including successful and malformed requests. Denial returns HTTP 429 with `Retry-After: 10`. The limits run before JSON parsing or secret verification.

The address is the TCP peer supplied by Axum `ConnectInfo`. Caller-supplied `Forwarded` and `X-Forwarded-For` are ignored. Users behind a reverse proxy share that proxy's per-IP budgets. Missing peer metadata fails closed on these endpoints. IPv4-mapped IPv6 addresses share the corresponding IPv4 budget. Bucket storage is capped at 4096 entries; entries idle for 10 minutes can be reclaimed.

## Indexed verification and upgrades

Host and device tokens retain their existing 64-hex format. Their first 16 characters select one row through a unique SQLite index; authentication still verifies the entire token against its salted Argon2 hash. A missing selector performs no Argon2 verification. No token scan remains.

Eight-character pairing codes use OS randomness and a 32-symbol alphabet. A unique four-character selector chooses one active candidate; issuance retries selector collisions. Argon2 checks the full code. Ten failed checks against a candidate lock it out. Codes expire after 10 minutes and can be consumed only once. Used, expired, and locked codes are removed when issuing another code.

Existing salted hashes cannot be backfilled without presenting the original secret. The schema migration keeps existing records and adds nullable selectors:

- Legacy hosts send their persisted `hostId`; the Hub verifies that single row and indexes it after successful authentication. Bootstrap does not regain access to an existing identity.
- The Web REST client sends `X-Remuda-Device-Id` from its non-secret stored metadata. A legacy cookie can migrate by verifying that one device row. Later REST and WebSocket requests use the index without this hint.
- Legacy device clients that lack a device identifier must log in or pair again. The identifier alone grants no access. Pairing codes issued before this migration must be reissued.

The production-browser smoke command is `pnpm build`, then `pnpm exec playwright test -c playwright.security.config.ts` from `web/`, with the agent's isolated Cargo target environment. It serves the built app through the real Hub on port 58880 by default and uses a fake Node; it does not invoke a model.

## D-018 client flow: device pairing vs Node enrollment

Task 12 split the single bootstrap secret in two. The live browser e2e has to follow it.

The bootstrap token is now **only** a device pairing access code. It still works
exactly as before for `POST /v1/login`; what changed is that it no longer enrolls
a **Node**.

### Live Hub e2e harness

`hub-auth.ts`, `LoginPage`, `pairing.spec.ts`, and the cookie assertions are
unaffected. `POST /v1/login` still takes `{bootstrapToken, deviceName}` and still
sets the same cookie.

The Playwright fake Node in `crates/remuda-hub/examples/hub_e2e.rs` must not
present the access code on `/v1/node`. After spawn it mints a one-shot enroll
token from the in-process Hub and passes that to `fake_node`:

```rust
let enroll = hub
    .mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
    .await?;
let node = tokio::spawn(fake_node(addr, enroll, host_id.clone(), pending, ready_tx));
```

`HUB_E2E_EXTERNAL=1` has no `RunningHub` to mint from; there the fake Node gets
an enroll token over HTTP — `POST /v1/login` with the access code, then
`POST /v1/hosts/enroll-token` with the resulting `remuda_device` cookie. The
printed `HUB_E2E_READY` line still emits the **access code** as `token`: that is
what the browser consumes.

### `POST /v1/login` — unchanged

```
POST /v1/login            Origin required
{ "bootstrapToken": "<access code>", "deviceName": "e2e-browser" }
```

`deviceName` defaults to `"device"`. Returns `{deviceId, token, name}` plus
`Set-Cookie: remuda_device=<token>; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000`
(`; Secure` when `cookie_secure`).

`401` on a wrong code, **or when the access code is past its TTL** — default 24 h
via `bootstrapTtlHours`, `0` disables. A long-lived e2e data dir can start failing
login purely from age; regenerate it, or rotate with `remuda hub rotate-bootstrap`,
which prints a new code and leaves paired devices working.

Repeat logins under the same `deviceName` are allowed and each mints a distinct
token. This was briefly one-shot-per-name and was reverted: `deviceName` is
unauthenticated, so refusing repeats stops no attacker while breaking honest
repeat clients (CLI, `hub --with-dispatcher`).

### `POST /v1/hosts/enroll-token` — new

```
POST /v1/hosts/enroll-token      requires an authenticated device + Origin
→ { "enrollTokenId": "obj_…", "token": "<64-hex>", "expiresAt": "…Z" }
```

Plaintext is returned once; only its Argon2 hash is stored. TTL is
`enrollTokenTtlMinutes`, default 60. The Node then dials `GET /v1/node` with
`Authorization: Bearer <enroll token>` and its hello result carries `nodeToken`.

Three rules that bite in fixtures:

- **Single use** — a second hello with the same token is `-32000 unauthenticated`.
  Two fake Nodes need two tokens.
- **New hosts only** — an enroll token naming an existing `hostId` is rejected even
  when fresh. That was A1's impersonation path.
- **Re-announce presents the stored `nodeToken`**, never a new enroll token. A
  fixture that restarts a Node must persist and replay it.

In-process compositions skip the HTTP round trip:
`RunningHub::mint_enroll_token(ttl_minutes)` for Nodes, and
`RunningHub::mint_device_token(name)` for a component needing an API credential
(used by the `hub --with-dispatcher` dispatcher).

### Unchanged

`/v1/devices/pair-code` and `/v1/devices/pair` behave as before, and presenting the
access code to `/v1/login` from any client — CLI, MCP, doctor, standalone
dispatcher — remains correct. That is what a pairing code is for.

### Fixtures already converted

`remuda-hub/tests/{hub,fleet,durability,lifecycle,push,worktree}.rs`,
`remuda/tests/mcp_hub.rs`, `remuda-feishu/tests/hub_dispatcher.rs`,
`remuda-node/tests/{wss,interactions}.rs` (eight `WssConfig::loopback` sites),
plus `remuda/src/cmd/dispatcher.rs` and `cmd/dev.rs` via the in-process minters.

Deliberately not converted, because they assert the refusal:
`remuda-hub/tests/ssh_hosts.rs::assert_bootstrap_cannot_claim_ssh_host` and the
negative case in `remuda-hub/tests/hub.rs`.
