# remuda-node

Node process: local instance APIs and Hub control-plane transports.

## Outbound WSS (`transport::WssLink`)

JSON-RPC 2.0 over Hub `GET /v1/node` (see `crates/remuda-hub` README).
[`NodeTransport`] lives in `src/transport/mod.rs` because that Hub JSON-RPC
carrier is distinct from `HubCarrier` NDJSON (`StdioCarrier`).

`WssLink::connect` sends `node.hello` with host inventory, heartbeats, and
forwards `journal.append` through a bounded queue (callers wait when full).
On disconnect it reconnects with exponential backoff and does **not** replay
Hub `instance.*` commands. First enroll uses the bootstrap bearer; later
dials use `nodeToken` from hello.

## Host inventory

`inventory::collect` probes `claude` / `codex` / `grok` / `agy` / `gemini` on
PATH (canonical absolute path, `--version`, sha256), login markers without
reading secret values (`~/.claude.json` `oauthAccount` key presence;
`~/.codex/auth.json` / `~/.grok/auth.json` existence; `agy` `settings.json`
presence), Herdr version and socket, OS/kernel/libc, and cpu/mem. Results are
cached (default 30s) for hello and heartbeat. Hub stores `cli[]` as
`{kind,version,path,auth}` with `auth` in `logged_in` / `logged_out` /
`unknown`. `WssConfig::with_collected_inventory` fills hello `cli` from this
collector. The stdio `NodeHello::detect` path calls it when that module is linked.
