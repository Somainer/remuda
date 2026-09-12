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

`WssLink::connect_runtime` is the stdio peer: it dispatches Hub
`instance.create` / `send` / `cancel` / `respond` into `DevNode`, streams
`journal.append` with sequence watermarks, and on reconnect resumes from the
Hub-acked seq (no Command replay, no duplicate appends). Frames use
`remuda_protocol::hubnode`.

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

## Local development API

`remuda dev` serves the local Node on `http://127.0.0.1:8787` by default. The
development router exposes instance create/read/command/journal REST endpoints,
a multiplexed JSON-RPC follow WebSocket at `/v1/client`, per-instance follow at
`/v1/instances/:id/follow`, and a fixed replay TTY frame at
`/v1/instances/:id/tty`. Follow sends a snapshot before buffered
sequence-numbered events; clients reconnect with their last durable sequence and
deduplicate by journal, sequence, and event ID.

Start the real local server instead of the web fixture backend:

```sh
cargo run -p remuda -- dev
VITE_MOCK=0 VITE_API_BASE=http://127.0.0.1:8787 pnpm --dir web dev
```

Create an instance and send a follow-up prompt:

```sh
curl -sS http://127.0.0.1:8787/v1/instances \
  -H 'content-type: application/json' \
  -d '{"prompt":"local fake-driver prompt"}'

curl -sS http://127.0.0.1:8787/v1/instances/ins_EXAMPLE/commands \
  -H 'content-type: application/json' \
  -d '{"operation":"send","prompt":"next turn"}'
```

Loopback development may run without a code. Trusted-LAN mode is explicit and
requires a private file:

```sh
chmod 600 /path/to/remuda-dev-code
cargo run -p remuda -- dev --dev-bind-lan \
  --access-code-file /path/to/remuda-dev-code
```

When configured, pass the code in `x-remuda-access-code` or use the
`remuda_dev_access_code` cookie. Browser CORS requests are accepted only from
the configured development origins.

## Node carriers and SSH stdio

The Node-to-Hub application carrier is the `HubCarrier` trait. The M1 main line
is a Node-initiated WSS connection. The alternate command

```sh
ssh host remuda node --stdio
```

uses UTF-8 NDJSON: each stdin line is one complete JSON application frame and
each stdout line is one complete JSON frame, capped at 1 MiB. Logs go only to
stderr. The first stdout frame is `node.hello` and includes CLI kinds with
absolute executable path, version, and auth state; Herdr version/socket; config
labels; and `maxInstances`. `remuda-ssh` owns SSH process setup and bridges this
stdio carrier; the Node crate does not invoke SSH.
