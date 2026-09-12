# Provider profiles

D-012: `delegation` is `none` (native CLI login, default), `gateway` (any Anthropic-Messages-compatible endpoint), or `direct` (single provider key; multi-key rotation is v2). This page is how you configure **your own gateway** inside Remuda instead of a hand-written `~/.claude/settings.relay.json` overlay.

## Hub API

Device-authenticated REST. OpenAPI: `crates/remuda-hub/openapi/openapi.json`.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/v1/providers` | List. Secret never returned. |
| POST | `/v1/providers` | Create. Body includes `authToken` once. |
| GET | `/v1/providers/{id}` | Fingerprint + last4 only. |
| PATCH | `/v1/providers/{id}` | Metadata and/or rotate `authToken`. |
| DELETE | `/v1/providers/{id}` | Drops the vault entry. |
| POST | `/v1/providers/{id}/test` | `GET {baseUrl}/v1/models` (or `{baseUrl}/models` if base already ends with `/v1`). Reports `reachable` / `ok` / `message`. |

Profile JSON: `id` (`pvp_…`), `name`, `kind` (`gateway`\|`direct`), `baseUrl`, `models`, `defaultModel`, `headers`, `defaultGateway`, `revision`, `secret: {present, last4, fingerprint}`.

The token is envelope-encrypted (ChaCha20-Poly1305) in the Hub data dir (`secrets/master.key` + `secrets/secrets.json`, mode `0600`). GET/list/test responses and logs must not include it.

## Node / driver overlay

x-place threads `delegation` + `providerProfileId` + `providerOverlay` on `instance.create`. Hub attaches the public overlay to the stored spec and injects `providerAuthToken` **only** on the Node RPC (not SQLite).

x-place should call:

```rust
use remuda_driver::{write_claude_provider_overlay, ClaudeProviderOverlay, Delegation, Secret};

let path = write_claude_provider_overlay(launch_dir, &ClaudeProviderOverlay {
    delegation: Delegation::Gateway, // or Direct
    base_url,
    model,
    secret: &secret,
    extra_env: &Default::default(),
})?;
// pass path as --settings; file is 0600; do not log contents
```

Gateway env: `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`, plus `model`. Direct env: `ANTHROPIC_API_KEY` (and `ANTHROPIC_BASE_URL` if set).

## Web

Provider page: list / create / edit / rotate token / test / delete / set default gateway.

New Session `delegation=gateway` selects the default gateway profile and prefills its `defaultModel`.

## How to enter a real gateway

1. Open **Provider** → **添加网关**.
2. Name (any label).
3. Base URL of the Anthropic-Messages-compatible endpoint, including `/v1` if the gateway expects it (example: `https://your-gateway.example/v1`).
4. Auth token (Bearer / `x-api-key`). Submitted once; afterwards the UI shows `••••last4` only.
5. Optional model list (one per line) and default model (New Session prefills this).
6. Check **设为默认网关**.
7. **测试连通** — dummy URLs such as `http://127.0.0.1:1` report `unreachable: …` with a clear message. A real gateway should list models or at least return HTTP 401/200.
8. New Session → **网关 (gateway)** uses that profile. No `~/.claude/settings.relay.json` required.

Do not put the token in `headers`. Do not commit tokens or screenshots that show the token.
