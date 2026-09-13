# Codex 0.154.0 pre-spike fixtures

Source: real `codex-cli 0.154.0` runs on 2026-09-14 local time, using fresh
`CODEX_HOME` directories under `/tmp/remuda-codexspike/`. The client, TUI,
tools, hooks, approval handling and rollout writer are real; **model responses
are deterministic local Responses API fixtures**, with authentication disabled
for that provider. No account credentials were read or copied.

See [the evidence report](../../../../../docs/design/evidence/codex-signals-1.md)
for commands, binary/source identity, field inventory and VERIFIED/FAILED results.

- `interactive-0.154.0.jsonl`: all 97 records of one TUI session, including a
  shell tool, Enter steering, Tab queue, Esc interruption and a TUI approval.
- `hook-allow-0.154.0.jsonl`, `hook-deny-0.154.0.jsonl`: complete 18/17-record
  app-server runs with a trusted PermissionRequest hook resolving approval.
- `permission-request-0.154.0.jsonl`: two actual stdin payloads for that hook.
- `notify-0.154.0.jsonl`: actual notify argv captures, including title-generation
  child threads. This file is **not** rollout JSONL.
- `probe-server.py`, `probe-notify.py`, `probe-keys.py`: independently written
  local test apparatus for the interactive run. Copy them to the temporary
  paths in the report before use. `SPIKE_PANE` must name the caller-created
  test pane. They are manual reproduction helpers, not automated CI tests.

Scrubbing replaced personal home paths with `/home/dev`, the interactive cwd
with `/home/dev/work`, base/developer instruction text with explicit redaction
markers, and `world_state.state` with a redaction marker. Hook fixture instruction
text was likewise redacted. Probe-owned temporary paths, session IDs, timestamps,
ordinals, tool inputs/outputs and token counters remain for correlation. Counts
are synthetic model-reported usage, not measured billing. No source records
were dropped or reordered; the redacted context is not suitable for resuming.

`Compacted`, sparse/legacy schemas, malformed input and future event types are
covered with explicitly synthetic values in `tests/codex_rollout.rs`; they were
not observed in these runtime fixtures. The parser deliberately leaves
`turn_aborted` unknown under the requested pre-spike enum, never task-complete.
