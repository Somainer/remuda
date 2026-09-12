# remuda-codex-wire

JSON-RPC client for **`codex app-server --listen stdio://`**. Frozen at the
D-013 minimal surface: spawn, handshake, `thread/start`, `turn/start`,
typed notifications, and `turn/interrupt`.

Native framing is one JSON object per line **without** a `"jsonrpc":"2.0"`
field. Requests are `{id, method, params?}`; notifications `{method, params?}`;
responses `{id, result}` or `{id, error}`.

## Spawn

The binary path must be absolute. Optional `-c` overlays cover `model`,
`model_reasoning_effort`, and `approval_policy`. `CODEX_HOME` is injected only
into the child when `SpawnSpec.codex_home` is set.

```text
<abs-codex> app-server --listen stdio:// [-c model="gpt-5.6-sol"] …
```

Handshake: `initialize` (wait for result) then notification `initialized`.

Out of scope (D-013): `thread/resume`, `model/list`, `codex app-server daemon`,
`unix://`, and `ws://`. `unix://` is WebSocket-over-UDS, not JSONL; this crate
does not speak it.

## Notifications

Inbound `{method, params}` frames are typed (`item/*`, `turn/completed`,
`thread/status/changed`, …). Unknown methods and item `type` values become
`Unknown` instead of failing the reader. Server→client requests (approvals)
are `{id, method}` and must be answered with `{id, result}` via
`CodexAppServer::reply_result`.

## Differences from vibe-kanban

Lifted (Apache-2.0) from `crates/executors/src/executors/codex/jsonrpc.rs`.
Remuda uses a hand-written type subset, an absolute binary + `SpawnSpec`,
an `mpsc` inbound stream, and client name `remuda`. No executor auto-approve,
plan-mode, or transcript rewrite.

## Tests

- `tests/replay.rs` — one fixture replay of the 0.154.0 stdio probe.
- `tests/process.rs` — spawn the local stdio stub for handshake / turn / interrupt.
