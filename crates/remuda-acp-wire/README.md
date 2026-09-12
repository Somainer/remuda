# remuda-acp-wire

ACP v1 client for **grok agent** (stdio and `grok agent serve` WebSocket).

Remuda does not implement an agent loop. This crate is the grok-side wire:
spawn or connect, `initialize` → `session/new` / `session/load` →
`session/prompt` + `session/update` stream → `stopReason`, plus raw `_x.ai/*`.

It uses the official [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol)
2.1.0 SDK for live connections. Fixture replay uses a local NDJSON codec because
grok emits `_x.ai/*` notifications and inner `sessionUpdate` tags
(`hook_execution`, `pending_interaction`, …) that are not in stable
`SessionUpdate`.

This is **not** a copy of vibe-kanban `crates/executors/src/executors/acp/`
(older `impl acp::Client` trait, Gemini/Qwen harness, log normalizer). Differences:

| | vibe-kanban ACP | remuda-acp-wire |
|---|---|---|
| SDK | pre-2.x `Client` trait | 2.1.0 `Client.builder()` / `ByteStreams` / `Lines` |
| fs/terminal | not implemented, still may declare | **never declared** so grok runs tools in-agent |
| permissions | approval service / auto-allow | `--always-approve`; incoming `request_permission` is cancelled |
| transport | child stdio | stdio **and** serve `/ws` with `Authorization: Bearer` |
| extensions | ignored | `_x.ai/*` passthrough (leading `_` required) |

## Spawn (stdio)

```text
GROK_DISABLE_AUTOUPDATER=1 grok agent --always-approve --model grok-4.6 --no-leader stdio
```

`--no-leader` is required so this client is not attached to a shared grok
leader. Do not pass `--no-auto-update` on the argv (use the env var).

## Serve (WebSocket)

`grok agent serve --bind 127.0.0.1:PORT --secret <token>` prints
`ws://127.0.0.1:PORT/ws?server-key=…`. This crate connects to `/ws` and sends
`Authorization: Bearer <token>`. It **strips** `server-key` from the URL so the
secret is not logged as a query parameter. `X-Server-Key` is not used.

## Lifecycle

1. `initialize` `{protocolVersion:1, clientCapabilities:{}, clientInfo:{name:"runtime", version:<crate>}}`
2. `session/new` `{cwd, mcpServers:[], _meta:{yoloMode:true}}` when always-approve
3. `session/prompt` `{sessionId, prompt:[{type:"text", text:…}]}`
4. `session/update` stream: `agent_message_chunk` / `agent_thought_chunk` /
   `tool_call` / `tool_call_update` / `plan` / …
5. prompt **result** is the turn terminal (`stopReason`)
6. `session/cancel` is a notification; cancelled arrives on the original prompt
7. `session/load` restores (replays history when the agent supports it)

Unknown `sessionUpdate` tags and non-ACP methods become `Unknown`. Headless
`streaming-json` (`{type:"text"|"end"}`) is a different protocol — the codec
returns `NotAcp`.

## Tests

```
cargo test -p remuda-acp-wire
cargo test -p remuda-acp-wire -- --ignored   # two live grok turns; /tmp/remuda-acp-wire
```

Fixtures under `tests/fixtures/` are copies of `docs/research/cli-help/`.
