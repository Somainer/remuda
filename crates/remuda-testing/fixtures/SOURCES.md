# Fixture sources

Copied from `docs/research/cli-help/` (research traces, 2026-09-12). Originals stay in that tree; this crate copies the protocol samples other crates can depend on without reading gitignored research notes.

| Path | Source |
|---|---|
| `claude/claude-p-init.json` | `docs/research/cli-help/claude-p-init.json` — `claude -p --output-format stream-json` `system/init` (2.1.268) |
| `claude/claude-permission-host-allow.jsonl` | host allow Bash (`claude-interaction-probe.md` §1.2) |
| `claude/claude-permission-host-deny.jsonl` | host deny Bash |
| `claude/claude-askuser.jsonl` | AskUserQuestion round-trip |
| `claude/claude-askuser-auto.jsonl` | auto-mode AskUserQuestion answered by a PreToolUse hook (`docs/design/evidence/askq-pretooluse-1.md`, claude 2.1.277) |
| `claude/claude-exit-plan-mode-{allow,deny}.jsonl` | plan-mode ExitPlanMode via host `can_use_tool` (`docs/design/evidence/delegated-decisions-3.md`, claude 2.1.277, `--permission-mode plan --permission-prompts host --permission-prompt-tool stdio`). Fully sanitized VCR captures; sanitization runs on RECONSTRUCTED content: each streamed tool_use input is rebuilt from its `input_json_delta` fragments (which split sensitive strings across lines), path-scrubbed whole, then collapsed into one fragment the driver still parses. All absolute paths collapse to `/workspace/project/...`, the initialize model catalog is reduced to `test-model` plus the `default` sentinel, org-tagged message/tool ids (including JSON-object keys) become `msg_replay_*`/`toolu_replay_*`, UUIDs become deterministic replay UUIDs, non-JSON diagnostic lines are dropped; `_dir:"in"` marks the three host checkpoints. The replay peer verifies the whole permission-response envelope. |
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
