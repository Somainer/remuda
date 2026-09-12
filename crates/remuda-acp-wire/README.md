# remuda-acp-wire

Minimal ACP v1 client for **grok agent stdio**. Frozen at this surface by
[D-013](../../docs/design/decisions.md): no `session/load`, no `grok agent serve`
WebSocket/TCP, no live-model tests.

Remuda does not implement an agent loop. This crate only speaks the grok-side
wire: spawn, `initialize` → `session/new` → `session/prompt` + `session/update`
→ `session/cancel`.

It uses official [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol)
2.1.0 for the live connection. Fixture replay uses a local NDJSON codec because
grok also emits `_x.ai/*` notifications and extra `sessionUpdate` tags.

## Spawn

```text
GROK_DISABLE_AUTOUPDATER=1 grok agent --always-approve --model grok-4.6 --no-leader stdio
```

`--no-leader` keeps this client off a shared grok leader. Do not pass
`--no-auto-update` on argv (use the env var). `initialize` sends empty
`clientCapabilities` so grok runs tools in the agent process (do not declare
`fs` / `terminal`). `--always-approve` means grok does not send
`session/request_permission`.

## Lifecycle

1. `initialize` `{protocolVersion:1, clientCapabilities:{}, clientInfo:{name:"runtime", version:<crate>}}`
2. `session/new` `{cwd, mcpServers:[], _meta:{yoloMode:true}}`
3. `session/prompt` `{sessionId, prompt:[{type:"text", text:…}]}`
4. `session/update` stream (`agent_message_chunk` / thought / `tool_call` / …)
5. prompt **result** is the turn terminal (`stopReason`)
6. `session/cancel` is a notification; `cancelled` arrives on the original prompt

## Tests

```
cargo test -p remuda-acp-wire
```

One fixture replay: `tests/fixtures/grok-acp-session.jsonl` (copy of
`docs/research/cli-help/grok-acp-session.jsonl`).
