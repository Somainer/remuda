# Fixture sources

Copied from `docs/research/cli-help/` (research traces, 2026-09-12). Originals stay in that tree; this crate copies the protocol samples other crates can depend on without reading gitignored research notes.

| Path | Source |
|---|---|
| `claude/claude-p-init.json` | `docs/research/cli-help/claude-p-init.json` — `claude -p --output-format stream-json` `system/init` (2.1.268) |
| `claude/claude-permission-host-allow.jsonl` | host allow Bash (`claude-interaction-probe.md` §1.2) |
| `claude/claude-permission-host-deny.jsonl` | host deny Bash |
| `claude/claude-askuser.jsonl` | AskUserQuestion round-trip |
| `claude/claude-*-no-prompt-tool.jsonl` | same probes without `--permission-prompt-tool stdio` |
| `claude/claude-hook-permission*.jsonl` | PermissionRequest hook allow/deny |
| `claude/claude-workflow-canary-*.jsonl` | Workflow cross-model canary |
| `codex/codex-appserver-session.jsonl` | `docs/research/cli-help/codex-appserver-session.jsonl` |
| `grok/grok-acp-serve.jsonl` | `grok agent serve` ACP |
| `grok/grok-acp-session.jsonl` | `grok agent stdio` ACP session |
| `grok/grok-headless-streaming-json.jsonl` | `grok -p --output-format streaming-json` |
| `agy/agy-stream-json-sample.jsonl` | `agy -p=… --output-format stream-json` |
| `scripts/*.jsonl` | authored for `fake-claude`; shapes follow `claude-stream-json-protocol.md` §§3–5 |
| `herdr/*.jsonl` | see `herdr/SOURCES.md` — live herdr 0.9.0 capture plus authored trust/observe frames |

The Codex OpenAPI schema tree (`codex-app-server-schema/`, 400+ files) is not copied; session NDJSON is enough for driver tests.
