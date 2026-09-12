# remuda-hub

Axum Hub: device login, Node enrollment, cross-host instance index, journal
mirror, and the embedded Web UI. Hub does **not** run native CLIs; Nodes do.

## How to run

The composition root (`crates/remuda`) owns the production subcommand:

```text
remuda hub --data-dir /data --listen 127.0.0.1:8080
```

Until that CLI agent wires flags, call the library from `remuda` as
`remuda_hub::serve(HubConfig { data_dir, listen, .. })` or run the example:

```bash
REMUDA_DATA_DIR=/tmp/remuda-hub \
REMUDA_LISTEN=127.0.0.1:8080 \
REMUDA_COOKIE_SECURE=0 \
REMUDA_BOOTSTRAP_TOKEN=dev-bootstrap \
cargo run -p remuda-hub --example serve
```

On first start Hub writes `data-dir/bootstrap-token` (mode `0600`) when the
token is not supplied. Devices and Nodes exchange that bootstrap secret for a
long-lived token; Hub stores only an Argon2id hash.

Local HTTP should set `REMUDA_COOKIE_SECURE=0`. Production cookies are
`Secure` + `HttpOnly` + `SameSite=Lax`. Mutating requests with an `Origin`
header must match `Host` (or `HubConfig.allowed_origins`).

### Web UI

```bash
just web-build
cargo run -p remuda-hub --example serve --features embed-web
```

Feature `embed-web` rust-embeds `web/dist` at compile time when that directory
exists; otherwise the crate serves a 404 placeholder. `REMUDA_WEB_ROOT` can
point at a built `web/dist` without rebuilding.

## HTTP surface

| Method | Path | Auth | Notes |
| --- | --- | --- | --- |
| GET | `/healthz` | no | `{ok:true}` |
| POST | `/v1/login` | bootstrap token | Sets `remuda_device` cookie; returns device token |
| GET | `/v1/hosts` | device | Host registry: `label`, `labels[]`, `online`, `lastSeenAt`, `cli[]`, `herdr`, `resources`, `maxInstances`, `transport` |
| GET | `/v1/instances` | device | Cross-host index |
| POST | `/v1/instances` | device | Index + forward `instance.create` if the Node is online |
| POST | `/v1/instances/:id/commands` | device | Command ledger (`queued`/`accepted`/`settled`); same `commandId` is idempotent and **never resent** |
| GET | `/v1/instances/:id/journal` | device | Mirrored events + `durableSeq` |
| GET WS | `/v1/follow?instanceId=` | device | Snapshot (`asOfSeq`) then live `{type:event,seq}` |
| GET WS | `/v1/node` | host/bootstrap bearer | Node control plane |
| GET WS | `/node/v1/connect` | host/bootstrap bearer | Alias from `plan-phase0.md` §1.5 |
| GET | `/v1/hosts/:id` | device | Single host including last inventory |
| PATCH | `/v1/hosts/:id` | device | `labels`, `maxInstances`, `name` |
| POST | `/v1/placement/resolve` | device | Dry-run host selection |
| POST | `/v1/fleet/instances` | device | One Instance per selected host |
| GET | `/v1/fleet/:id` | device | Aggregated fleet status |
| POST | `/v1/fleet/:id/commands` | device | Broadcast send/cancel (per-instance commandId) |

## Hub ↔ Node JSON-RPC (over `/v1/node`)

Frames are JSON-RPC 2.0 (`jsonrpc`, `id`, `method`, `params`). The first
request **must** be hello. Wire names follow `protocol.md` §7; the task aliases
`node.*` are accepted.

| Method | Dir | Purpose |
| --- | --- | --- |
| `runtime.hello` / `node.hello` | Node→Hub | Identity + inventory (`host.labels`, `cli`, `herdr`, `resources`, `maxInstances`). First enroll returns `nodeToken`. |
| `runtime.heartbeat` / `node.heartbeat` | Node→Hub | Refresh `lastSeenAt` and the same inventory fields |
| `host.report` | Node→Hub | Inventory (`driverInventory` or `cli`) |
| `journal.append` | Node→Hub | Push an observation; Hub assigns `seq` and mirrors it |
| `tty.frame` | Node→Hub | Fan-out to follow sockets (not a journal seq) |
| `instance.create` | Hub→Node | Create; also accepted Node→Hub as a settlement notice |
| `instance.send` | Hub→Node | Prompt / steer |
| `instance.cancel` | Hub→Node | Cancel a Run |
| `instance.respond` / `interaction.respond` | Hub→Node | Interaction answer |

Reconnect never reissues a command that already has a forward intent. Timeouts
leave `state=queued`, `forwarded=true`, `resolution=unknown`.

### Carriers (D-013)

`NodeTransport` is the Hub-side session trait. `WssTransport` serves
`WS /v1/node` today. `StdioTransport` is the plug for
`ssh <host> remuda node --stdio` (no Hub listen port). `registry::routes()`,
`placement::routes()`, and `fleet::routes()` merge next to `http::routes()` /
`ws::routes()` in `router()`. `POST /v1/instances` accepts `placement`
(`host` / `labels` / `any`) and returns `PLACEMENT_UNSATISFIABLE` with
`reasons` when no host fits.

## SQLite (`data-dir/hub.sqlite`)

WAL writer thread (no `rusqlite::Connection` across `.await`): `devices`,
`hosts`, `instances` (index), `commands` (inbox), `journal` (cache). Node
journals remain authoritative.

## `deploy/compose.hub.yml`

Compose runs `remuda hub --config /etc/remuda/hub.toml` with `/data` mounted
and `REMUDA_MASTER_KEY_FILE` / `REMUDA_WEB_PASSWORD_FILE` from Docker secrets.
This crate maps:

| Compose | Hub |
| --- | --- |
| volume `/data00/remuda/hub:/data` | `HubConfig.data_dir` |
| secret `remuda_web_password` | bootstrap / web login secret (CLI agent) |
| no `ports:` | bind `0.0.0.0:8080` behind Caddy on `deploy_default` |
| `remuda-migrate` | additive SQLite migrations (journal crate); Hub `open` is still idempotent `CREATE TABLE IF NOT EXISTS` |

The Hub image is distroless and does not start Caddy, AsterGate, or cloudflared.
