# Fixtures

Captured from `docs/research/herdr-herdrx.md` (2026-09-12, herdr 0.9.0, protocol 22):

- `ping-response.json` — §1.2 `nc -U` ping
- `error-response.json` — §1.5 missing `pane_id` on `pane.agent_status_changed`
- `events.jsonl` — §1.2 subscribe ack + mixed dotted/underscore event names
- `terminal-frame.jsonl` — `pty-driver-spike.md` §4.3 observe JSONL (`bytes` is base64 `"hello"`)
