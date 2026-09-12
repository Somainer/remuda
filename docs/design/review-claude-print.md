# Review: `claude-print` driver (`184357e`)

Adversarial check of `crates/remuda-driver/src/claude_print.rs` against
`docs/research/claude-stream-json-protocol.md` and
`docs/research/claude-interaction-probe.md`. Line numbers refer to the
pre-fix file (commit `184357e` / workspace copy at review time).

## Verified (no change)

| Check | Evidence |
| --- | --- |
| Initialize-before-user | `ClaudeProcess::spawn_command` writes `initialize` and waits for the matching `control_response` before `launch` returns (`process.rs` 367–415). `Driver::send` is not called from `launch`. |
| `request_id` echo | `respond_interaction` / `send_control` use `Inbound::control_success(pending.request_id, …)` (`claude_print.rs` 463–465, 587–598). Wire envelope puts `request_id` under `response` (`types.rs` 1514–1527). |
| Permission camelCase | `PermissionResult` serializes `updatedInput` / `updatedPermissions` (`types.rs` 138–163). Driver builds that type (`claude_print.rs` 1451–1498). |
| Stdin stays open across turns | Only `close()` calls `close_stdin` (470–475). `send` writes another `user` frame. |
| Unknown stdout `type` | `Outbound::Unknown` → opaque observation (`650`). Map errors are logged; the reader keeps going (`503–508`). |
| `--bare` / `--no-session-persistence` at materialize | `flags.rs` BANNED + `claude_argv` refuse those tokens. Print launch currently **does not** re-check the materialized argv (gap below). |
| Handshake timeout | Passed through to the wire crate (`269–277`). |

## P0

### P0-1 Resume builds a fixture spec, not the live launch

`resume` (`482–499`) calls `stub_spec_for_resume` (`1603–1608`), which deserializes `tests/fixtures/instance-spec.json` and only overwrites `host` / `cwd=/tmp`. That drops:

- `model_id` (fixture is `null` → first profile model, not the session’s model)
- `permission_mode` (fixture `manual`, not the live bypass/dontAsk/host mode)
- `settingsOverlay` / cwd / workspace of the original instance
- `--settings` path the first launch wrote

Stream-json §3 / probe §0: `--resume` must keep the same settings + model as the original process. `claude_pty` rebuilds argv from the previous recipe; print did not.

**Fix:** persist `last_spec` on `Inner` at successful `launch`; `resume` clones it and only swaps `SessionAction::Resume`.

## P1

### P1-1 Every `result` sets `affects_completion: true`

`map_result` (`921–941`) marks **every** result lifecycle as process-completing. Workflow emits two `result` frames (`result_index` 0 then 1; stream-json §5 item 1, `workflow.jsonl`). The first is “turn done / waiting on workflow”, not run completion. The reader already stays open (good); the observation flag was wrong.

**Fix:** `affects_completion` is false unless this is a terminal result (`result_index` absent or `> 0`, and `queued_turn_count` is 0/absent). Index `0` is never terminal.

### P1-2 Bot/agent `dontAsk` not rejected in the driver gate

`reject_bot_bypass` (`1522–1530`) only checks `BypassPermissions`. Materializer also rejects `DontAsk` for `LaunchOrigin::Bot` (`materializer.rs` 467–473). Probe: do not default `dontAsk` on bot paths. If materialize order changes, print would spawn `dontAsk` for a bot.

**Fix:** treat `DontAsk` like bypass in `reject_bot_bypass`.

### P1-3 Print launch does not refuse `--bare` / `--no-session-persistence` on the final argv

PTY/bg call `refuse_bare` on the recipe argv. Print relies solely on the materializer. Stream-json §1 / §6: never `--bare`, never `--no-session-persistence`.

**Fix:** `refuse_prohibited_argv` after `materialize` / `apply_bypass_flag`.

## P2

| ID | Where | Note |
| --- | --- | --- |
| P2-1 | `close` 470–479 | Drops `events` and `live` without joining the map task; late frames can be lost. |
| P2-2 | `close` 475 | `wait()` after stdin close has no timeout; a stuck Workflow wait can hang close. Native does wait on bg tasks — document, don’t kill. |
| P2-3 | `handle_can_use_tool` 554–567 | AutoAllow/AutoDeny still emit `interaction.requested` then answer immediately. Harmless; broker may show a flash of pending. |
| P2-4 | `send` 419–426 | Prompt `origin` is ignored; gating uses `ClaudePrintOptions.origin` only. |
| P2-5 | `handle_frame` 641–642 | `control_cancel_request` is ignored (protocol: stop waiting). Print has no in-flight host control wait except pending tools keyed by CLI `request_id`. |
| P2-6 | `map_stream` 997–1029 | Only `delta.text`; thinking/tool JSON deltas are dropped (partial completeness). |

## Tests

`crates/remuda-driver/tests/claude_print_review.rs` (does not touch `claude_print_process.rs`).
