# remuda-codex-wire

JSON-RPC client for **`codex app-server --listen stdio://`**.

Native framing is one JSON object per line **without** a `"jsonrpc":"2.0"` field.
Requests are `{id, method, params?}`; notifications `{method, params?}`;
responses `{id, result}` or `{id, error}`.

## Spawn

The binary path is required to be absolute (digest/version pinning happens in
the driver layer). Optional `-c` overlays cover `model`,
`model_reasoning_effort`, and `approval_policy`. `CODEX_HOME` is injected only
into the child when `SpawnSpec.codex_home` is set.

```text
<abs-codex> app-server --listen stdio:// [-c model="gpt-5.6-sol"] …
```

Handshake: `initialize` (wait for result) then notification `initialized`.
Other methods before that yield native `-32600 Not initialized`.

## `unix://` is not JSONL

`--listen unix://` (and `unix:///abs/path.sock`) is **WebSocket over a Unix
domain socket** (HTTP Upgrade). Writing NDJSON at the socket — including via
`codex app-server proxy` — is parsed as a broken HTTP request
(`httparse error: invalid token`). Disable permessage-deflate if you ever
speak that transport. This crate only implements stdio JSONL and rejects
`Listen::Unix` / `Listen::Ws`.

Do not use `codex app-server daemon`; npm/node installs have no standalone
layout.

## Official protocol crate

`codex-app-server-protocol` was evaluated as a git dependency and **not**
taken. It depends on `rmcp`, `zstd`, `codex-protocol`, `codex-history`,
`codex-rollout`, `codex-secrets`, and other workspace crates, which would
pull most of `codex-rs` into Remuda and lock MSRV/compile time to upstream.
Types here are the subset used by initialize / thread / turn / model plus
item notifications, hand-written from the 0.154.0 schema and
`docs/research/cli-help/codex-appserver-session.jsonl`.

Unknown notification methods and item `type` values become `Unknown` instead
of failing the reader.

## Server→client requests

Approvals are JSON-RPC **requests** (`id` + `method`), not notifications.
Reply with `{id, result}` and **no** `method`. v2 decisions are
`accept` / `acceptForSession` / `decline` / `cancel`. Legacy
`execCommandApproval` uses `approved` / `denied` and must not be mixed with v2.

`CodexAppServer::reply_result` is the API for that reply.

## Differences from vibe-kanban

Lifted (Apache-2.0) from `crates/executors/src/executors/codex/jsonrpc.rs`
(pending-request map + stdout reader that also handles server-initiated
requests). Remuda changes:

| vibe-kanban | remuda-codex-wire |
| --- | --- |
| Depends on `codex-app-server-protocol` | Hand-written subset |
| `npx @openai/codex` command builder, worktree executor | Absolute binary + `SpawnSpec` |
| Auto-approve / plan-mode / review / MCP status inside the client | Thin wire only; Interaction lives in `remuda-driver` |
| Callbacks (`JsonRpcCallbacks`) | `mpsc` `Inbound` stream |
| Executor logging / JSONL transcript rewrite | No transcript rewrite |
| `experimentalApi: true` | Default `false` |
| Client name `vibe-codex-executor` | Stable `remuda` (`clientInfo.name` is logged upstream) |

## Tests

- `tests/replay.rs` — every line of the 0.154.0 probe deserializes; known
  notifications stay typed; unknown methods/items become `Unknown`.
- `tests/process.rs` — spawn `tests/fixtures/fake-app-server.py` as the
  pinned binary (it ignores the extra `app-server --listen stdio://` argv).
- `tests/live.rs` — `#[ignore]`; `gpt-5.6-sol`, prompt `Reply with exactly OK`.
  Uses `/tmp/remuda-codex-wire/` as cwd and the user-installed `codex` binary.
  Does **not** isolate `CODEX_HOME` (an empty home 401s). Allowed once.
