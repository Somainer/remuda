# remuda-claude-wire

NDJSON adapter for Claude Code **print** mode (`claude -p --input-format stream-json --output-format stream-json`).

This crate is the wire layer only: types, a line codec, and a process that speaks the control protocol. It does not own Hub–Node RPC, journals, or Interaction cards.

## Command template

Spawn uses `Command` + `current_dir` (never `sh -c`, never `--cwd`):

```text
claude -p
  --input-format stream-json --output-format stream-json --verbose
  --include-partial-messages --include-hook-events --forward-subagent-text
  --replay-user-messages
  --permission-mode <mode>
  --permission-prompts host --permission-prompt-tool stdio
  --setting-sources user,project,local
  --settings <path-or-json> --model <id> --session-id <uuid>
```

Forbidden: `--bare`, `--no-session-persistence`, `--cwd`, `--bg`, argv prompt. Closing stdin ends the session after in-flight work; SIGTERM is not `interrupt`.

After spawn the crate always sends `control_request`/`initialize` and waits for the matching `control_response`, while still forwarding other stdout frames (`system/init` often arrives first). The stdout reader **does not** stop on the first `result` (Workflow emits two).

## Differences from vibe-kanban

vibe-kanban `crates/executors/src/executors/claude/{types,protocol,client}.rs` is a **subset** of the same protocol (Apache-2.0). This crate is a new implementation against `docs/research/claude-stream-json-protocol.md` / CLI 2.1.268, not a copy of those files.

| | vibe-kanban | remuda-claude-wire |
|---|---|---|
| `initialize` | sent, not waited; hooks optional | always sent, handshake waits for `request_id` echo; body is configurable (`hooks`, `hookCallbackIds`, `agents`, `mcpServers`/`sdkMcpServers`, `systemPrompt`, `perTaskStopAffordance`, …) |
| `--include-hook-events` | not passed | default on |
| `--forward-subagent-text` | not passed | default on |
| First `result` | `read_loop` **breaks** (drops Workflow’s second result) | keeps reading until EOF / kill |
| `--permission-prompt-tool` | `stdio` only when plan/approvals | always `host` + `stdio` (2.1.268 does not emit `can_use_tool` without it) |
| Unknown `type`/`subtype` | `Other(Value)` / omitted variants | `Unknown(Value)` at type **and** subtype; decode never fails |
| `blocked_path` vs `blocked_paths` | only `blocked_paths: Option<String>` | both fields; string or array |
| Permission result | camelCase `updatedInput` | same; allow always serializes `updatedInput` |
| Permission modes | `default` / `acceptEdits` / `plan` / `bypassPermissions` | plus `auto` / `dontAsk`; `manual` aliases `default` |

## Layout

- `types.rs` — stdout (`Outbound`) and stdin (`Inbound`) frames
- `codec.rs` — NDJSON lines, skip empty/CRLF, 8 MiB line cap
- `process.rs` — `ClaudeProcess::spawn`, `send_user`, `respond_control`, `interrupt`, `set_permission_mode`, `kill`

Live e2e (ignored): `cargo test -p remuda-claude-wire -- --ignored` uses `/tmp/remuda-claude-wire/`, `--model haiku`, `--max-budget-usd 0.3`.
