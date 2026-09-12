# remuda-claude-wire fixtures

Copied from `docs/research/cli-help/` unless noted.

| file | source |
|---|---|
| `claude-p-init.json` | `docs/research/cli-help/claude-p-init.json` (CLI 2.1.268 `system/init`) |
| `claude-permission-host-allow.jsonl` | `docs/research/cli-help/claude-permission-host-allow.jsonl` |
| `claude-permission-host-deny.jsonl` | `docs/research/cli-help/claude-permission-host-deny.jsonl` |
| `claude-askuser.jsonl` | `docs/research/cli-help/claude-askuser.jsonl` |
| `claude-hook-permission.jsonl` | `docs/research/cli-help/claude-hook-permission.jsonl` |
| `claude-hook-permission-allow.jsonl` | `docs/research/cli-help/claude-hook-permission-allow.jsonl` |
| `claude-hook-permission-deny.jsonl` | `docs/research/cli-help/claude-hook-permission-deny.jsonl` |
| `workflow-control-plane-s4.jsonl` | subset of `docs/research/cli-help/claude-workflow-canary-1.jsonl` (`task_*`, `background_tasks_changed`, first `result`) plus the second `result` reconstructed from `claude-control-plane.md` §4 (the canary process was killed after one result) |
| `synthetic-coverage.jsonl` | constructed frames for `keep_alive`, `stream_event`, H→C control, and **documented Unknown** types |
| `fake_claude.py` | local NDJSON peer for process tests (no model) |

Lines with `"_dir":"in"` are host stdin (H→C); every other object is Claude stdout (C→H). `_dir` is a capture annotation, not a protocol field.
