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
