# Claude interactive queue / steer / interrupt evidence

Date: 2026-09-14 (Asia/Shanghai); timestamps below are UTC on 2026-09-13.
Scope: D-028 P4 prerequisite for the Claude harness; evidence only, no implementation changes.
Design references: [native PTY first §6–7](../native-pty-first.md#6-steer--排队--打断),
[D-028a](../decisions.md).

## Conclusion

**Enter during a running Bash tool admits a native queued message, then delivers it at
the tool boundary inside the existing turn.** Queue admission and boundary steering
are both present; they are not mutually exclusive classifications. It does not abort
the active tool or wait for the whole three-tool turn to end.

| Action | Key / PTY bytes (hex) | Observed behavior | Native journal / hook signal |
|---|---|---|---|
| Submit from idle | Enter, `0d`, after a separate body write | Starts a turn | `user`; `UserPromptSubmit` |
| Submit while Bash runs | Enter, `0d` | Queued in TUI; consumed after the active tool result, before the next Bash call | `enqueue` → `remove` + `attachment.queued_command`; hook fires at enqueue |
| Distinct installed queue binding | Ctrl+X then Enter, `18 0d` | Binding exists statically; both one-call and separated-key live trials still reached a tool boundary. No after-turn-only guarantee established | Same `enqueue` → `remove` + `queued_command` |
| Tab while working | `09` | Text stayed in composer; no submit hook or enqueue for `TAB_PROBE` | None for that marker |
| Esc while request is retrying | `1b` | Interrupts current request; queued text survives and starts another request | `[Request interrupted by user]`, `dequeue`, subsequent `user` |
| Esc while Bash runs | `1b` | Stops the observed sleep process; queued text survives; Claude stays alive | `dequeue`, errored `tool_result`, `[Request interrupted by user for tool use]`, subsequent queued `user`; no observed `PostToolUseFailure` |
| Bracketed paste containing CR | `1b5b3230307e` + body + `0d` + `1b5b3230317e` | Inserts text/newline; does not submit until a separate Enter outside the framing | No paste-only enqueue/hook; external Enter produces `enqueue` |
| Unframed body and CR in one write | UTF-8 body + `0d` | Also stayed in composer in the idle trial; separate Enter submitted it | No hook for one second, then `user` / submit hook after separate `0d` |

P4 should expose the observed **boundary steering through a queue** without promising
instant tool interruption or an after-turn-only queue key. A `remove` record alone
cannot classify a message as canceled. The observed successful removals carry
`reason:"absorbed_mid_turn"`. This report does not validate a Remuda driver or change its
current `unknown` capability flags.

## Installation and acquisition

- Installed `claude --version`: `2.1.270 (Claude Code)`.
- Executable SHA-256: `a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807`.
- Requested model: `claude-opus-5[1m]`; transcript assistant model: `claude-opus-5`.
  TUI displayed Opus 5, 1M context, xhigh effort. No model substitution was made.
- Scratch working directory: `/tmp/remuda-claudequeue/` (macOS canonicalizes it to
  `/private/tmp/remuda-claudequeue/`). A new sibling Herdr pane was created with
  `herdr pane split --current --direction right --cwd /tmp/remuda-claudequeue/ --no-focus`.
  Only that pane was controlled with `pane run`, `send-text`, `send-keys`, and `read`.
- Initial launch used the requested command, with `CLAUDE_CONFIG_DIR` unset:

  ```sh
  env -u CLAUDE_CONFIG_DIR claude --settings "$HOME/.claude/settings.relay.json" \
    --model 'claude-opus-5[1m]' --dangerously-skip-permissions
  ```

- Baseline session `db6c90db-633c-4960-8f30-2057bebbb13e` reached the TUI but its first
  request was interrupted during API retries before any tool executed. The complete
  measured tool/queue trials used instrumented session
  `4bd84da7-16cb-430f-ae97-d85f1ea22c1f` with the same model and normal user hooks.
- Instrumented launch used a throwaway **hook-only** `--settings` file. A local Python
  launcher read the relay file's `env` object directly into the child environment,
  removed `CLAUDE_CONFIG_DIR`, and called `os.execvpe` with the same model and bypass
  arguments. The relay file contains only `env` and `model`; its routing values were
  not printed or copied into the temporary settings file or fixtures. This is a
  configuration difference from the baseline, explicitly recorded here.
- No normal settings, keybindings, OS settings, or GUI were edited. There was no
  user keybindings file. Existing user hooks remained enabled; no `--bare`,
  `--safe-mode`, `--setting-sources` restriction, or isolated config directory was used.
- Transient upstream failures delayed several requests. One TUI error was
  `503 no eligible upstream account is currently available`. Retry periods are not
  counted as Bash execution. All tool claims below require actual tool records.

The logger registered `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
`PostToolUseFailure`, `Stop`, and `SessionEnd`. Each event used this settings shape
(replace `UserPromptSubmit` with the event name):

```json
{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"python3 /tmp/remuda-claudequeue/hook.py","timeout":5}]}]}}
```

`hook.py` read one event JSON from stdin and atomically appended one JSON line to
`/tmp/remuda-claudequeue/hooks.log`, using `O_APPEND` and mode `0600`:
`{"received_at": "<UTC wall clock>", "event": <unmodified event>}`. It produced no
hook decision/output. `received_at` measures logger invocation, not an internal
Claude timestamp. Input dispatches were separately timestamped before/after each
Herdr CLI call. A later 100 ms tail observer recorded append arrival metadata.

## Enter during a tool: admission, delivery, and final response

The initial prompt was: `Run sleep 20 three times with Bash, one at a time, then
summarize. Do not run them in the background.` The independent repeat appended
`This is a new independent run.` All six completed calls used separate foreground
`Bash` invocations of `sleep 20`, with `timeout: 60000`.

The clean repeat gives the strongest distinction from after-turn delivery:

| UTC time | Evidence |
|---|---|
| 18:29:30.462 | Assistant starts sleep 1, tool ID `toolu_vrtx_01PXn4kGri6V5wY2XbUv4H8j` |
| 18:29:30.515 | `PreToolUse` logger receives Bash event |
| 18:29:30.829 | `WAIT_E` is enqueued by Ctrl+X + Enter in one Herdr call |
| 18:29:32.217 | `STEER_F` is enqueued by plain Enter, while sleep 1 is still running |
| 18:29:32.280 | `UserPromptSubmit` receives `STEER_F` (63 ms after enqueue) |
| 18:29:51.961 | Sleep 1 `tool_result`, `is_error: false` |
| 18:29:51.967 | Both queued messages receive `remove`; their `queued_command` attachments follow the tool result in file order |
| 18:30:11.638 | Sleep 2 starts: the original three-tool turn has not ended |
| 18:30:33.401 | Sleep 2 result |
| 18:31:39.146 | Sleep 3 starts |
| 18:31:46.626 | Separated Ctrl+X / Enter trial enqueues `WAIT_I` |
| 18:32:00.795 | Sleep 3 result |
| 18:32:00.800 | `WAIT_I` removed and appended as `queued_command`, before final summary |

Representative **verbatim scrubbed native queue records**:

```jsonl
{"type":"queue-operation","operation":"enqueue","timestamp":"2026-09-13T18:29:32.217Z","sessionId":"4bd84da7-16cb-430f-ae97-d85f1ea22c1f","content":"STEER_F: Include ACK_F in the summary after completing all three sleeps."}
{"type":"queue-operation","operation":"remove","timestamp":"2026-09-13T18:29:51.967Z","sessionId":"4bd84da7-16cb-430f-ae97-d85f1ea22c1f","content":"STEER_F: Include ACK_F in the summary after completing all three sleeps.","reason":"absorbed_mid_turn"}
```

`STEER_F` spent 19.750 seconds in the queue and was removed 6 ms after the tool-result
timestamp, explicitly with `reason:"absorbed_mid_turn"`. The inserted attachment is an **additional native record type**, not a
new top-level `user` record. Its `attachment.prompt` carries the submitted text and
`source_uuid` identifies it. An adapter tailing only `user`/`assistant` would miss it.

In the earlier run, ordinary Enter enqueued `BOUNDARY_C` at 18:25:18.010 during
sleep 3 (tool call 18:25:17.477). Its `remove` at 18:25:38.884 followed the result
at 18:25:38.880. The same turn's final assistant text at 18:26:22.660 included
`ACK_C` and the three-sleep summary. This independently confirms model-visible
delivery before final output.

**Timestamp caveat:** a delivered `queued_command` attachment retains its enqueue
timestamp. For example, the `STEER_F` attachment is stamped 18:29:32.217 but is
appended after the 18:29:51.961 tool result. Some queue `remove` records are physically
written before the tool result despite having slightly later timestamps. Preserve
file order and both clocks; globally sorting by timestamp fabricates delivery order.

## Esc and queue survival

During the earlier API retry, `QUEUE_A` enqueued at 18:23:13.101 and its submit hook
arrived at 18:23:13.192. Esc interrupted that request. The transcript contains:

```jsonl
{"type":"queue-operation","operation":"dequeue","timestamp":"2026-09-13T18:23:28.141Z","sessionId":"4bd84da7-16cb-430f-ae97-d85f1ea22c1f"}
```

The interruption text is stamped 18:23:28.134; `QUEUE_A` becomes a new `user` record
at 18:23:28.147. Thus Esc did not clear the queued message. Claude automatically
started handling it, later ran the requested sleeps, and included `ACK_A` in its
answer. There was no active tool here, so this is not evidence of tool interruption.

Two actual tool-use interruption trials followed. The first queued `INTERRUPT_G`
at 18:34:18.697 and sent Esc at 18:34:19.771. It produced `dequeue` at
18:34:19.788, the queued `user` at 18:34:19.793, an errored tool result at
18:34:19.797, and `[Request interrupted by user for tool use]` at 18:34:19.798.
The later assistant reply was `ACK_G`; no additional tools ran.

To exclude a pre-execution cancellation, the last probe waited over five seconds
after `PreToolUse` and inspected the owned Claude process's descendants:

| UTC time | Observation |
|---|---|
| 18:37:31.095 | `Bash(sleep 20)` tool call, ID `toolu_vrtx_01S6UY5aFrA9PQdpiVtqae31` |
| 18:37:31.155 | `PreToolUse` received |
| 18:37:36.268 | Live process ancestry: Claude PID 56933 → zsh 11533 → sleep 11535 |
| 18:37:36.580 | `INTERRUPT_J` enqueued; submit hook received at 18:37:36.632 |
| 18:37:37.587 | Same sleep process still present immediately before Esc |
| 18:37:37.676 | Herdr Esc dispatch begins |
| 18:37:37.793 | Native `dequeue` |
| 18:37:37.801 | `tool_result.is_error: true` and interruption text |
| 18:37:37.803 | `INTERRUPT_J` becomes a top-level `user` record |
| 18:37:38.793 | sleep 11535 and zsh 11533 absent; Claude 56933 still alive |
| 18:38:24.955 | Assistant replies `ACK_J`; `Stop` logger receives it at 18:38:26.176 |

The errored result's `toolUseResult` is the literal `User rejected tool use`;
its content describes user rejection, despite the independent evidence that the
sleep really ran. It does not say `is_interrupt`. **Neither interrupted call
produced a `PostToolUseFailure` event in the registered logger**, nor a matching
`PostToolUse` completion. Do not require `PostToolUseFailure.is_interrupt` as the
only cancel acknowledgment on this version. For these observations, pair the
tool result and interrupt text with the actual process transition. The earlier
six successful sleeps each produced `PreToolUse` and `PostToolUse`.

Esc canceled the active tool/turn, not the pending input queue or Claude process.
The follow-up can start automatically without another Enter; treating `Esc` as
“all work has stopped” would be incorrect. The queue's `dequeue` can also precede
the interrupted tool-result record in physical file order.

## Keys, hooks, and paste

The TUI showed submitted text above the composer and the literal placeholder
`Press up to edit queued messages`. Its `?` shortcuts and `/help` General page
listed newline, editing, suspend, and other shortcuts but no separate queue/send-now
choice. `claude --help` likewise advertised no queue/steer keyboard distinction.
These observations concern the displayed help, not every hidden feature.

Read-only inspection of the pinned executable found this default map:

```javascript
{context:"Chat",bindings:{
  escape:"chat:cancel",enter:"chat:submit",
  "ctrl+x enter":"chat:queueSubmit","ctrl+j":"chat:newline"
}}
```

The embedded chord documentation gives a one-second timeout. Static code forwards
`wait:true` for `queueSubmit`, but the inspected tool-boundary queue filter does not
use `wait` as an after-turn scheduling barrier. It calls
`messageQueue.consume(...,{reason:"absorbed_mid_turn"})`; the shared remover emits
`operation:"remove"`. This static observation corroborates, rather than substitutes
for, the live records.

Both dispatch styles were tested: `herdr pane send-keys <pane> ctrl+x enter`, and
separate `ctrl+x` / `enter` calls whose dispatch starts were 18:31:46.294 and
18:31:46.616 (322 ms apart). Both were absorbed at a tool boundary. The earlier
`CHORD_D` happened **after the last tool boundary** and was dequeued after `Stop`;
that timing alone does not prove a special wait key. It is not used for that claim.

`UserPromptSubmit` fired at queue admission in every sampled queued submission,
including `QUEUE_A` (+91 ms), `PASTE_B` (+72 ms), `BOUNDARY_C` (+60 ms), `WAIT_E`
(+53 ms), `STEER_F` (+63 ms), and `WAIT_I` (+72 ms). The log has no second submit
hook for these messages when consumed at a boundary or dequeued after interruption.
Consequently this hook is not delivery acknowledgment. Several different queued
messages share one `prompt_id`; it is not a unique per-message deduplication key.

Raw byte calibration used a temporary `tty.setraw` reader in the same owned Herdr
pane between Claude sessions and after the final `/exit`. Observed values were
Enter `0d`, Esc `1b`, Tab `09`, Ctrl+X `18`, and literal `send-text BYTE_PROBE` →
`425954455f50524f4245`, without implicit framing. The final capture also verified
the explicit paste envelope as `1b5b3230307e425954455f46494e414c0d1b5b3230317e`.
One `send-keys ctrl+x enter` CLI call reached the reader as two reads (`18`, `0d`);
one CLI call does not imply one PTY read/write chunk.

While sleep 2 ran, the explicit paste write at 18:24:23.391 was
`ESC[200~PASTE_B: Reply only ACK_B.\rESC[201~`. It left the text and a blank line in
the composer. Neither a submit hook nor queue record appeared until a separately
sent Enter at 18:24:37.683 (`enqueue` 18:24:37.693). Paste therefore changes how the
embedded CR is handled; it does not create a separate queue mode. After external
Enter, `PASTE_B` followed the same boundary-delivery path as ordinary text.
This probe observed the paste behavior, not raw output DECSET `?2004` negotiation;
a production driver must still gate framing on its own terminal-mode observation.

The unframed comparison was one `send-text` call containing the next prompt plus
CR at 18:33:49.816. After one second the text and newline were still visible and
no submit hook had fired. A separate Enter at 18:33:51.001 produced a `user` record
at 18:33:51.013 and hook at 18:33:51.068. Thus the safe observed submission recipe
is **body write, allow input processing, then a separate CR write**. These probes
do not establish the minimum safe inter-write delay; the automated short gaps
were at least 150 ms. A single raw body-plus-CR write is not a verified substitute.

## Fixtures and limits

- [interactive-session.jsonl](../../../crates/remuda-testing/fixtures/claude-transcript/queue/interactive-session.jsonl): selected native transcript lines in original file order, including tool calls/results, queue operations, delivered `queued_command` attachments, interruption text, and final replies.
- [hooks.jsonl](../../../crates/remuda-testing/fixtures/claude-transcript/queue/hooks.jsonl): temporary logger lines, preserving receive timestamps and event payloads.
- [input-dispatch.jsonl](../../../crates/remuda-testing/fixtures/claude-transcript/queue/input-dispatch.jsonl): exact dispatch-start excerpts from the Herdr driver log; key names are CLI arguments, not inferred native records.
- [key-bytes-initial.jsonl](../../../crates/remuda-testing/fixtures/claude-transcript/queue/key-bytes-initial.jsonl) and [key-bytes-final.jsonl](../../../crates/remuda-testing/fixtures/claude-transcript/queue/key-bytes-final.jsonl): raw reader chunks; each reader's monotonic clock is local to that capture, not UTC.
- [process-probe.jsonl](../../../crates/remuda-testing/fixtures/claude-transcript/queue/process-probe.jsonl): timestamped descendant process observations for the final Esc probe; executable basenames only.
- [append-times.jsonl](../../../crates/remuda-testing/fixtures/claude-transcript/queue/append-times.jsonl): 100 ms observer metadata for later transcript appends. `line` refers to the original transcript, not the selected fixture's line number; absence before observer startup does not imply no event.

Source transcript: `$HOME/.claude/projects/-private-tmp-remuda-claudequeue/4bd84da7-16cb-430f-ae97-d85f1ea22c1f.jsonl`.
Fixture extraction removes `cwd` keys, replaces the home prefix with `$HOME`,
normalizes the scratch path, and omits unrelated context/hook-output attachments and
thinking-only records/signatures. No normal user context, credentials, private
hostnames, or personal paths are included. Existing message IDs, session IDs,
tool IDs, timestamps, `requestId`, `apiBlockIndex`, and source-link fields are
preserved where present. Fixtures are observations, not synthetic expected output.

`popAll` was not observed; no such event is invented. `MessageDisplay` streaming,
approvals, resume, queue persistence across process exit, and other Claude versions
are outside this spike. `MessageDisplay` is not used to infer queue delivery.
This installation's native transcript and hook observations are not themselves
Remuda journal acceptance evidence.

## Validation and cleanup

- All seven fixtures parse as JSONL. There are 56 selected native records and 34
  logger records. Eight tool calls pair with eight tool results (six successes,
  two interruptions). Native queue counts: nine `enqueue`, four `dequeue`, five
  `remove` with `reason:"absorbed_mid_turn"`, zero `popAll`.
- Hook counts: one `SessionStart`, 13 `UserPromptSubmit`, eight `PreToolUse`, six
  `PostToolUse`, five `Stop`, one `SessionEnd`. Zero `PostToolUseFailure` was
  observed in this configured session, including the confirmed running-process
  interruption; this is not a claim about all versions or configurations.
- The terminal displayed `$1.21` at the end of the instrumented run. This is the
  TUI estimate, not an independently audited provider bill; no additional model
  calls were made for fixture generation or review.
- `/exit` produced `SessionEnd` with `reason:"prompt_input_exit"` at
  18:39:54.481. After raw byte calibration, only the created pane was closed.
  The original pane/focus/layout remained, and the owned Claude/shell/sleep PIDs
  were absent. Inactive temporary hook settings/logger/launcher files were removed;
  native transcripts and scratch evidence logs were retained.
- Required gate: `./scripts/ci/secret-scan.sh` passed. `git diff --check`, JSONL
  parsing, private-path checks, and tool-call/result pairing passed. No code
  changes or Rust build/test claims are part of this evidence-only task.
