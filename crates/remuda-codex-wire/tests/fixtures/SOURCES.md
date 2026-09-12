# Fixture sources

- `codex-appserver-session.jsonl` — copy of `docs/research/cli-help/codex-appserver-session.jsonl` (Codex CLI 0.154.0 stdio probe, 2026-09-12). First line is a source comment skipped by the NDJSON codec.
- `server-request-approval.jsonl` — authored server→client approval request plus `{id,result}` reply; the live probe used `approvalPolicy: never` and never emitted a real approval.
- `fake-app-server.py` — authored stdio JSONL stub used by process tests. Not a Codex binary.
