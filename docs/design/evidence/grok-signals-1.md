# D-028 P6: Grok signals pre-spike 1

Date: 2026-09-14 local time; captures begin 2026-09-13 UTC.
Scope: standalone session-file parser and evidence; no SignalAdapter or driver
wiring. This mirrors [the Codex spike](./codex-signals-1.md).
Design references: [native-pty-first](../native-pty-first.md) §§3, 4.2, 6, 7,
13 P6; [decisions](../decisions.md) D-028 / D-028a.

**VERIFIED** means an observed runtime result unless explicitly qualified as
source-only. **FAILED** means a tested hypothesis or command failed. An untested
alternative remains unverified; an absent record is not a universal
unsupported-capability claim.

The installed executable is **grok 1.0.30 (04b7ffed98c6)**, SHA-256
**d53b6e543e482716236748914331db50145c696ac7af91f1ebdedcf5654cfecb**.
The client, tool execution, permission UI, hooks, and native files are real.
An independently written local Chat Completions server supplied deterministic
reasoning/text/tool calls; there was no hosted-model inference. Its API-key
setting was an invalid synthetic placeholder, not an account credential.
No real credentials were read or copied into the apparatus or fixtures.
No user Grok/Claude configuration was modified, and no OS settings or GUI were
used. All probe writes stayed under /tmp/remuda-grokspike/.

GROK_BIN below denotes the installed executable, with its personal installation
path omitted. The [fixture README](../../../crates/remuda-driver/tests/fixtures/grok/README.md)
defines scrub substitutions and reproduction helpers. Native IDs, timestamps,
record order, and outcomes remain original.

## Results that change the P6 assumptions

| Probe | Result | Consequence |
|---|---|---|
| Completion | VERIFIED events.jsonl turn_ended and updates.jsonl turn_completed | Native completion and cancellation exist; no idle-screen inference is needed for these records |
| Queue / send now | VERIFIED Enter queues by default; empty Enter then cancels and sends | Do not reuse Codex Enter-as-steer semantics |
| Esc / Ctrl+C | VERIFIED Esc preserves the running turn and draft; Ctrl+C clears draft, then cancels | A single Ctrl+C with a draft is not an interrupt acknowledgement |
| Queue provenance | FAILED to find an ordinary enqueue record; VERIFIED redirect_kind on cancellation-followed input | Delivered user messages alone cannot prove keypress-time queuing |
| PermissionRequest hook | FAILED registration; VERIFIED PreToolUse deny and ask decisions | There is a hook gate, but not a verified PermissionRequest hook |
| Native permission RPC in updates | FAILED to find session/request_permission in the full TUI file | File observation cannot reconstruct or answer the original permission RPC |
| Claude hook compatibility | VERIFIED shadow GROK_HOME still discovers global Claude hooks | Disable activation with GROK_CLAUDE_HOOKS_ENABLED=0 |
| PID discovery | VERIFIED registry PID is the TUI process; entry removed during shutdown | Registry presence is not an independent liveness check |
| OSC | VERIFIED title and OSC 9;4 bytes in raw PTY capture | Herdr ANSI snapshots alone do not preserve original OSC writes |

## Real TUI apparatus and commands

The manual helpers are in the
[Grok fixture directory](../../../crates/remuda-driver/tests/fixtures/grok/README.md).
They are independently written apparatus, not copied Grok source. The server
returns Chat Completions SSE reasoning/text deltas, a terminal tool for RUN_TOOL,
an ask_user_question tool for QUESTION, and delays SLOW prompts for 12 seconds.
Its terminal command writes only a fixed marker inside the throwaway cwd.

Copy/start commands are given in the fixture README. The observed local port
was 50984; the launch uses that loopback endpoint. The shadow config was:

~~~toml
[models]
default = "spike"
[model.spike]
model = "spike"
base_url = "http://127.0.0.1:50984/v1"
name = "Local spike"
api_backend = "chat_completions"
context_window = 32000
[cli]
use_leader = false
[ui]
show_tips = false
[telemetry]
enabled = false
~~~

The temporary launch.sh, with only the executable path normalized:

~~~sh
#!/bin/sh
export XAI_API_KEY=remuda-local-placeholder
export GROK_MODELS_BASE_URL=http://127.0.0.1:50984/v1
export GROK_HOME=/tmp/remuda-grokspike/tui/home
export CLAUDE_CONFIG_DIR=/tmp/remuda-grokspike/claude-empty
export GROK_CLAUDE_HOOKS_ENABLED=0
export GROK_CLAUDE_MCPS_ENABLED=0
export GROK_AGENT_DASHBOARD=0
export GROK_SUGGESTIONS=0
exec "$GROK_BIN" --cwd /tmp/remuda-grokspike/tui/work \
  --no-alt-screen --no-subagents --no-plan -m spike
~~~

The six requested hook names, plus PermissionDenied, StopCancelled, SessionEnd,
and the deliberately tested PermissionRequest name, were registered under
the shadow home's hooks/probe.json. Each uses this shape:

~~~json
{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"python3 /tmp/remuda-grokspike/tui/capture-hook.py SessionStart"}]}]}}
~~~

The observer records stdin and a small allowlist of hook identity environment
variables. Only PreToolUse(run_terminal_command) containing SPIKE_TOOL_OK
returns a decision:

~~~json
{"decision":"ask","reason":"SPIKE_PERMISSION_PROBE"}
~~~

This deliberate ask made the harmless probe require a visible permission
answer. The TUI launch did **not** use --always-approve.

~~~console
$ test "$HERDR_ENV" = 1
# exit 0
$ herdr pane split --current --direction right --cwd /tmp/remuda-grokspike/tui/work --no-focus
# result.pane.pane_id = "w5:p2F"; focused = false
$ herdr pane run w5:p2F 'python3 /tmp/remuda-grokspike/tui/record.py'
# the owned pane starts the nested PTY recorder and real Grok
$ herdr pane read w5:p2F --source visible --lines 35 --format ansi
# styled snapshot, saved under the corresponding fixture name
~~~

record.py forwards and records the nested PTY output unchanged; child.py sets
that child PTY to **54 rows × 105 columns** before exec. This was necessary:
an initial zero-size child PTY **FAILED** to render and was replaced. An earlier
launch without the synthetic local API-key setting **FAILED** to enter the
usable TUI and showed authentication; it was quit without logging in.
Those failed startup attempts are separate from the complete recorded session.

The main native session is **01a09c24-46ef-7a03-9c89-88f1bc00bd0c**.
Snapshots were read through Herdr; raw OSC evidence came from the recorder.
Text and Enter are separated by 700 ms in keys.py to avoid paste timing being
mistaken for a semantic keybinding.

## A1 — Native session directory and lifecycle: VERIFIED

The actual canonical cwd was /private/tmp/remuda-grokspike/tui/work, even though
launch used /tmp. Its session folder was:

~~~text
$GROK_HOME/sessions/%2Fprivate%2Ftmp%2Fremuda-grokspike%2Ftui%2Fwork/
  01a09c24-46ef-7a03-9c89-88f1bc00bd0c/
~~~

Directory enumeration after the run:

~~~python
sorted(p.name for p in session_directory.iterdir())
~~~

~~~text
announcement_state.json
chat_history.jsonl
chat_history.jsonl.lock
events.jsonl
prompt_context.json
resources_state.json
rewind_points.jsonl
rewind_points.jsonl.lock
signals.json
summary.json
summary.json.lock
system_prompt.txt
terminal
title_refresh_idx
tool_definitions.json
updates.jsonl
updates.jsonl.lock
usage.json
~~~

The committed full-run fixtures have **54 updates, 74 events, and 27 history
records**, including startup/shutdown hooks and all seven turns: five completed,
two cancelled. summary.json reports num_messages 54, num_chat_messages 27,
current_model_id spike, and next_trace_turn 7. These are native counters, not a
claim that each record is a user-visible message or a billed model call.

Counting params.update.sessionUpdate and method in the complete update file:

~~~text
hook_execution:20 user_message_chunk:7 agent_thought_chunk:9
tool_call:2 tool_call_update:4 agent_message_chunk:5 turn_completed:7
session/update:27 _x.ai/session/update:27
~~~

Standard chunks use session/update; extensions use _x.ai/session/update.
The five persisted agent_message_chunk records each contain SPIKE_COMPLETE,
although the local server emitted SPIKE_ and COMPLETE separately. Thus this
short probe verifies native chunk-shaped records, not persistence of every
model token/SSE delta; record names alone do not establish token granularity.
A complete first-turn user echo, with unchanged identity:

~~~json
{"timestamp":1789326032,"method":"session/update","params":{"sessionId":"01a09c24-46ef-7a03-9c89-88f1bc00bd0c","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"RUN_TOOL"},"_meta":{"modelId":"spike","promptIndex":0}},"_meta":{"eventId":"01a09c24-46ef-7a03-9c89-88f1bc00bd0c-4","agentTimestampMs":1789326032271}}}
~~~

**VERIFIED explicit completion** in both files; these excerpts show the full
event and the update payload respectively:

~~~json
{"ts":"2026-09-13T19:00:47.759Z","type":"turn_ended","outcome":"completed"}
{"sessionUpdate":"turn_completed","prompt_id":"a759f86b-bfcd-4fbf-b8e1-e1180acfc33f","stop_reason":"end_turn","elapsed_ms":15608}
~~~

**VERIFIED explicit interruption**, after the second Ctrl+C:

~~~json
{"ts":"2026-09-13T19:01:42.187Z","type":"turn_ended","outcome":"cancelled","cancellation_category":"mid_turn_abort","cancellation_context":{"trigger":"ctrl_c"}}
{"sessionUpdate":"turn_completed","prompt_id":"735123be-3539-4593-86e1-c99746db34d6","stop_reason":"cancelled","elapsed_ms":2857}
~~~

The cancelled update also has frame metadata cancelTrigger ctrl_c and
cancellationCategory MidTurnAbort. The tag turn_completed therefore does not
by itself mean success; inspect stop_reason.

**FAILED ordinary enqueue provenance hypothesis**: the complete files have no
enqueue/queue-operation record for QUEUED_FOLLOWUP. Its user_message_chunk and
UserPromptSubmit occur when delivered after SLOW_QUEUE completes. **VERIFIED
limited redirect provenance**: turn 4, QUESTION after the earlier cancellation,
has redirect_kind cancel_then_send; turn 6, NOW_FOLLOWUP after send-now
cancellation, has redirect_kind queued_after_cancel. These records make
“there is no queue/steer provenance anywhere” false, but do not reconstruct
ordinary queue insertion/edit/removal at keypress time.

## A2 — active_sessions.json and PID lifetime: VERIFIED

Read the shadow registry while the final TUI was active:

~~~sh
cat /tmp/remuda-grokspike/tui/home/active_sessions.json
ps -o pid,ppid,pgid,tty,stat,comm -p 24069,24068
~~~

The registry entry, with only cwd normalized to the committed fixture:

~~~json
[{"session_id":"01a09c24-46ef-7a03-9c89-88f1bc00bd0c","pid":24069,"cwd":"/workspace/grok-spike","opened_at":"2026-09-13T19:00:18.851955Z"}]
~~~

The process observations were PID 24069, PPID 24068, PGID 24069, TTY ttys067
for Grok; 24068 was the Python recorder. Thus the registry identified the real
TUI process, not the recorder or the parent Herdr shell.

~~~sh
herdr pane send-keys w5:p2F ctrl+q ctrl+q
cat /tmp/remuda-grokspike/tui/home/active_sessions.json
~~~

~~~json
[]
~~~

The entry disappeared during native shutdown before every process had finished
exiting. Grok subsequently appeared as a zombie while the simple recorder
wrapper still held its outer PTY. The owned wrapper was interrupted explicitly.
The final process query for owned PIDs 24069, 24068, 18916, and 18915 returned
no entries; Herdr process-info reported the original shell PID 10648.
The owned pane was then closed and the local server stopped; a port check
found no listener. Removal from the registry alone was not treated as cleanup.

**Source-only, not a crash experiment**: the separately inspected public source
registers std::process::id(), removes clean exits, and later collects dead-PID
entries using kill(pid, 0). Lock contention can leave an orphan; PID reuse is
not independently identity-checked by that collector.
See the pinned [registry implementation](https://github.com/xai-org/grok-build/blob/37949780c144e37df692e3d669051a21fec24f20/crates/codegen/xai%2Dgrok-active-sessions/src/lib.rs)
and [TUI registration effect](https://github.com/xai-org/grok-build/blob/37949780c144e37df692e3d669051a21fec24f20/crates/codegen/xai-grok-pager/src/app/effects/mod.rs#L106).
Crash cleanup was not exercised in this spike.

## A3 — Hooks, payloads, and compatibility

### Event-name acceptance: VERIFIED; PermissionRequest: FAILED

An isolated hooks/event-inventory.json registered one echo command per candidate.
The read-only inspection did not execute those commands:

~~~sh
python3 /tmp/remuda-grokspike/hooks/launch.py inspect --json \
  > /tmp/remuda-grokspike/hooks/inspect-inventory.json
~~~

Filtering the output to the INVENTORY command targets gave:

~~~text
SessionStart -> session_start
UserPromptSubmit -> user_prompt_submit
PreToolUse -> pre_tool_use
PostToolUse -> post_tool_use
PostToolUseFailure -> post_tool_use_failure
PermissionDenied -> permission_denied
Stop -> stop
StopFailure -> stop_failure
StopCancelled -> stop_cancelled
Notification -> notification
SubagentStart -> subagent_start
SubagentStop -> subagent_stop
SubagentEnd -> subagent_stop
PreCompact -> pre_compact
PostCompact -> post_compact
SessionEnd -> session_end
~~~

PermissionRequest and the deliberate unknown name NotAnEvent were silently
omitted while neighboring entries loaded. No PermissionRequest stdin was
captured. The accepted inventory is not a claim that every event was triggered.

### Six stdin payloads: VERIFIED

[hook-payloads.jsonl](../../../crates/remuda-driver/tests/fixtures/grok/hook-payloads.jsonl)
preserves one real stdin payload and the five allowlisted identity environment
variables for each requested event. SessionStart/UserPromptSubmit/Stop samples
come from the first headless probe; PreToolUse/PostToolUse from the corrected
successful headless tool probe; Notification from the earlier real TUI
permission session. Their distinct session IDs are retained.

Common fields observed: hookEventName, hook_event_name, sessionId/session_id,
cwd, workspaceRoot, timestamp, and permissionMode/permission_mode.
hookEventName contains snake_case; hook_event_name contains PascalCase.

| Event | Additional observed stdin |
|---|---|
| SessionStart | source new; no transcriptPath or promptId in the sample |
| UserPromptSubmit | prompt TOOL and promptId; no transcriptPath in the sample |
| PreToolUse | toolName/tool_name, toolUseId/tool_use_id, toolInput/tool_input, toolInputTruncated false, transcript path aliases; no promptId |
| PostToolUse | tool identity/input, toolResult/tool_response, toolResultTruncated false, durationMs/duration_ms, isBackgrounded false, transcript aliases; no promptId |
| Stop | reason end_turn, stopHookActive false, lastAssistantMessage HOOK_COMPLETE, backgroundTasks [], sessionCrons [], promptId and transcript aliases |
| Notification | notificationType permission_prompt, message “Tool permission requested”, level info, permissionMode auto, transcript aliases; no promptId |

Hook child variables were GROK_HOOK_EVENT, GROK_HOOK_NAME, GROK_SESSION_ID,
GROK_WORKSPACE_ROOT, and CLAUDE_PROJECT_DIR. The last is a compatibility alias,
not proof the originating harness is Claude. The headless shutdown also fired
SessionEnd(reason shutdown) and Stop(reason shutdown). Count only appropriately
correlated turn completion, not every Stop invocation.

The first headless tool attempt **FAILED** schema validation because its
run_terminal_command omitted description. The corrected arguments were:

~~~json
{"command":"printf HOOK_TOOL_EXECUTED","description":"Emit fixed probe marker"}
~~~

This successful baseline produced the exact terminal output HOOK_TOOL_EXECUTED,
then the deterministic model returned HOOK_COMPLETE.

### PreToolUse deny and ask: VERIFIED

The separate shadow hook returned this JSON when its owned deny marker existed:

~~~json
{"decision":"deny","reason":"SPIKE_DENIED"}
~~~

~~~sh
touch /tmp/remuda-grokspike/hooks/deny
python3 /tmp/remuda-grokspike/hooks/launch.py \
  --always-approve --output-format streaming-json -p TOOL \
  > /tmp/remuda-grokspike/hooks/tool-deny.out
~~~

Exact relevant output:

~~~json
{"type":"tool_call_update","toolCallId":"hook-tool-1","status":"failed","content":[{"type":"content","content":{"type":"text","text":"Hook denied: SPIKE_DENIED"}}],"rawOutput":null,"locations":[]}
~~~

[The complete denial fixture](../../../crates/remuda-driver/tests/fixtures/grok/hook-deny-updates.jsonl)
records runs[0].status as:

~~~json
{"status":"failed","error":"denied: SPIKE_DENIED","elapsed_ms":121,"blocked":true}
~~~

There is no successful tool result or PostToolUse for that denied call.
Thus PreToolUse denial works even with --always-approve. In the final TUI,
the separate ask decision produced the visible prompt and Notification described
in A5. No claim is made that a PreToolUse allow decision itself answers an ACP
permission request.

The installed binary's embedded Hooks guide also documents UserPromptSubmit
and Stop blocking, and PreToolUse allow/deny/ask/defer. Those additional
decision variants were not all exercised. Static discovery used
strings "$GROK_BIN"; the hook guide occupies lines 49500–49920 of that extraction.

### Claude cross-read and neutralization: VERIFIED

A synthetic SessionStart hook was placed under a throwaway CLAUDE_CONFIG_DIR.
The same inspect command ran once with GROK_CLAUDE_HOOKS_ENABLED=1 and again
with 0; outputs were reduced to counts and own event names, without committing
personal hook bodies:

~~~sh
GROK_HOME=/tmp/remuda-grokspike/hooks/home \
CLAUDE_CONFIG_DIR=/tmp/remuda-grokspike/hooks/claude \
GROK_CLAUDE_HOOKS_ENABLED=1 GROK_CLAUDE_MCPS_ENABLED=0 \
GROK_CLAUDE_SKILLS_ENABLED=0 GROK_CLAUDE_RULES_ENABLED=0 \
GROK_CURSOR_HOOKS_ENABLED=0 GROK_CURSOR_MCPS_ENABLED=0 \
GROK_CLAUDE_AGENTS_ENABLED=0 \
"$GROK_BIN" --cwd /tmp/remuda-grokspike/hooks/work inspect --json
~~~

~~~text
on:  claude_compatibility_counts={"enabled":26}, shadow_claude_found=false
off: claude_compatibility_counts={"disabled":26}, shadow_claude_found=false
own_events=[notification,permission_denied,post_tool_use,pre_tool_use,
            session_end,session_start,stop,stop_cancelled,user_prompt_submit]
~~~

The enabled sources were actual global ~/.claude settings hooks despite the
shadow GROK_HOME. **FAILED**: CLAUDE_CONFIG_DIR did not redirect this hook
discovery path. **VERIFIED**: GROK_CLAUDE_HOOKS_ENABLED=0 disabled activation.
It does not prohibit every Claude configuration read: inspect still inventories
disabled hooks and independently reports Claude permission sources. Personal
hooks were not executed by this read-only comparison.

## A4 — TUI keys, footer states, and terminal emissions

### Enter / queue / send now: VERIFIED

The sequential probes used the committed keys helper; each next probe began
after the preceding state was inspected:

~~~sh
python3 /tmp/remuda-grokspike/tui/keys.py w5:p2F QUEUE
python3 /tmp/remuda-grokspike/tui/keys.py w5:p2F INTERRUPT
python3 /tmp/remuda-grokspike/tui/keys.py w5:p2F SENDNOW
~~~

For QUEUE, the helper submits SLOW_QUEUE, waits 1.2 seconds, then types
QUEUED_FOLLOWUP while reasoning is still running. Before Enter:

~~~text
Thinking… 1.7s
❯ QUEUED_FOLLOWUP
Enter:queue | Shift+Tab:mode | Ctrl+c:cancel | Ctrl+Enter:send now
~~~

After Enter:

~~~text
#1 QUEUED_FOLLOWUP
Thinking… 2.4s
Enter:send now | Shift+Tab:mode | Ctrl+c:cancel | Ctrl+;:queue
~~~

The current turn finishes at 19:01:18.458Z; QUEUED_FOLLOWUP starts turn 2 at
19:01:18.490Z. This is next-turn queueing in the observed configuration.
**FAILED Enter-as-immediate-steer hypothesis**. A separately configured steer
mode was not tested.

For SENDNOW, Enter first queues NOW_FOLLOWUP; a second Enter on the empty
composer cancels SLOW_SENDNOW and immediately starts the queued follow-up:

~~~json
{"ts":"2026-09-13T19:02:27.073Z","type":"turn_ended","outcome":"cancelled","cancellation_category":"mid_turn_abort","cancellation_context":{"trigger":"send_now"}}
~~~

The next turn has redirect_kind queued_after_cancel. The footer advertises
Ctrl+Enter for send now and Ctrl+; for the queue pane; those physical chords
were not separately exercised. The verified send-now transport is empty Enter.
The separately pinned [keyboard guide](https://github.com/xai-org/grok-build/blob/37949780c144e37df692e3d669051a21fec24f20/crates/codegen/xai-grok-pager/docs/user-guide/03-keyboard-shortcuts.md#L280)
describes these defaults and a configurable steer behavior; its source revision
is not treated as installed-build provenance.

### Esc / Ctrl+C: VERIFIED behavior; Esc-cancels hypothesis: FAILED

INTERRUPT types UNSENT_DRAFT during SLOW_INTERRUPT, then sends Esc,
Ctrl+C, Ctrl+C with snapshots between each step:

~~~text
After Esc:
Press Ctrl+c to cancel the turn
Thinking… 2.3s
❯ UNSENT_DRAFT

After first Ctrl+C:
Thinking… 2.7s
# composer empty, turn still running

After second Ctrl+C:
# working row and cancel footer disappear
# native turn_ended outcome=cancelled, trigger=ctrl_c
~~~

Esc retained the draft and continued the turn. The first Ctrl+C cleared the
draft; the second cancelled. UNSENT_DRAFT never became a user_message_chunk.
These are terminal key bytes, not an OS-delivered SIGINT.

### Footer/state excerpts: VERIFIED

ANSI/style and exact spacing are retained in the named fixtures; excerpts
below omit box drawing and unrelated privacy/suggestion text:

| State | Observed excerpt |
|---|---|
| Idle | Tab/→:accept suggestion; Shift+Tab:mode; Ctrl+.:shortcuts |
| Working, empty composer | Thinking… with [stop] chip; Shift+Tab:mode; Ctrl+c:cancel |
| Working, typed text | Enter:queue; Ctrl+c:cancel; Ctrl+Enter:send now |
| Working, queued row | #1 QUEUED_FOLLOWUP; Enter:send now; Ctrl+;:queue |
| Permission | Four numbered approval choices; Ctrl+o:always-approve; Ctrl+c:cancel |
| Question | Waiting on answers for Choose the probe result.; Enter:submit; Tab:next answer; Esc:scrollback; Shift+x:dismiss |

The permission snapshot is clipped immediately after Esc: at the observed
width. Its action cannot be inferred from that incomplete text. Permission
and question views are distinct blocked interfaces; a thinking spinner or
[stop] chip can coexist with a question waiting on the user.

### OSC title / OSC 9;4: VERIFIED raw bytes

The requested Herdr captures used:

~~~sh
herdr pane read w5:p2F --source visible --lines 35 --format ansi
~~~

**FAILED original-byte-stream hypothesis**: this gives styled rendered rows,
not the original PTY output. Title/progress OSC controls may already have been
consumed as terminal state changes. A read-only Herdr source cross-check at
1a7c691559bb6ea8ad366bce68f87f8c3f6db098 traced pane read through
src/app/api/panes.rs, src/app/api_helpers.rs, and src/pane/terminal.rs to
read_ansi_viewport/recent row snapshots. No installed-source equivalence is
claimed. Absence of OSC in a pane snapshot cannot establish non-emission.

The process inherited TERM=xterm-256color, TERM_PROGRAM=ghostty,
TERM_PROGRAM_VERSION=1.3.1, COLORTERM=truecolor, and HERDR_ENV=1. No terminal
brand override or host terminal-setting change was applied.
The independent nested PTY recorder preserved the actual bytes. Extraction
kept the first occurrence of each distinct OSC 0 / OSC 9;4 sequence, with
original byte offsets; 72 distinct samples are in
[osc-samples.jsonl](../../../crates/remuda-driver/tests/fixtures/grok/osc-samples.jsonl).
Selected exact JSON-escaped samples:

~~~json
{"byte_offset":0,"bytes":"\u001b]0;grok\u0007"}
{"byte_offset":145198,"bytes":"\u001b]9;4;1;-1\u0007"}
{"byte_offset":153259,"bytes":"\u001b]0;⚠ Action Required - ⠦ - Write the fixed probe marker in the thro… - RUN_TOOL - grok\u0007"}
{"byte_offset":212467,"bytes":"\u001b]9;4;0;0\u0007"}
{"byte_offset":216289,"bytes":"\u001b]0;⠦ - Thinking - RUN_TOOL - grok\u0007"}
{"byte_offset":327142,"bytes":"\u001b]0;⠸ - Running: ask_user_question - SPIKE_COMPLETE - grok\u0007"}
~~~

The activity/progress channel is coarse; it is not an approval schema.
Source-only corroboration at the pinned public revision shows terminal-brand
gating for progress and configurable title/progress support. It does not prove
universal emission across terminals. See
[progress emission](https://github.com/xai-org/grok-build/blob/37949780c144e37df692e3d669051a21fec24f20/crates/codegen/xai-grok-pager/src/notifications/progress.rs)
and [title emission](https://github.com/xai-org/grok-build/blob/37949780c144e37df692e3d669051a21fec24f20/crates/codegen/xai-grok-pager/src/notifications/title.rs).

## A5 — Permission request/answer persistence

**VERIFIED visible permission probe**: RUN_TOOL was submitted in the final TUI
without --always-approve. The targeted PreToolUse ask decision displayed:

~~~text
printf SPIKE_TOOL_OK > spike-result.txt
hook 'global/probe:pre_tool_use[0].hooks[0]' asks: SPIKE_PERMISSION_PROBE
1 (●) Yes, and don't ask again for anything (always-approve mode)
2 (○) Yes, proceed
3 (○) No, reject (type to add feedback)
4 (○) Never allow: printf
~~~

The selected answer was the single approval, not the initial always-approve
choice:

~~~sh
herdr pane send-keys w5:p2F 2
~~~

The request and answer were persisted in events.jsonl:

~~~json
{"ts":"2026-09-13T19:00:32.440Z","type":"permission_requested","tool_name":"run_terminal_command"}
{"ts":"2026-09-13T19:00:46.792Z","type":"permission_resolved","tool_name":"run_terminal_command","decision":"allow","wait_ms":14352}
~~~

updates.jsonl held the tool_call, hook_execution records, and subsequent
tool_call_update with status completed and rawOutput.exit_code 0. The marker
file contained SPIKE_TOOL_OK. **FAILED raw permission-RPC persistence
hypothesis**: all 54 frames have method session/update or _x.ai/session/update;
there is no session/request_permission frame, request options list, or original
RPC answer envelope. The event answer is a decision summary keyed by tool name,
not a complete permission correlation/response transport.

The question probe used:

~~~sh
herdr pane send-text w5:p2F QUESTION
# after text/paste settling:
herdr pane send-keys w5:p2F enter
herdr pane read w5:p2F --source visible --lines 35 --format ansi
herdr pane send-keys w5:p2F 1 enter
~~~

**VERIFIED** ask_user_question input persisted both options; its completed
tool_call_update recorded the selected Alpha answer in rawOutput.UserAnswered.
The question waited about 14 seconds, while its permission_resolved event had
wait_ms 0. That permission allow was not the user's question answer.

The independent --always-approve baseline likewise recorded permission_prompt,
permission_requested, and permission_resolved(decision allow, wait_ms 0).
A phase named permission_prompt alone does not establish visible blocking.
Use positive UI/hook/request evidence and maintain the distinction between
permission and question interactions. The hook value permissionMode auto also
coexisted with turn_started.yolo_mode false and the visible permission dialog;
it must not be equated with --always-approve.

## Source boundary

Public Grok source was inspected at commit
37949780c144e37df692e3d669051a21fec24f20, whose SOURCE_REV is
c4ea71cfdbcdb21e32e41bc25a0043d7d4836714. These differ from the installed
revision 04b7ffed98c6. Selected files were fetched read-only under the temporary
probe directory; public-source claims above are qualified accordingly.

~~~sh
curl -fsSL https://api.github.com/repos/xai-org/grok-build/commits/main
curl -fsSL 'https://api.github.com/repos/xai-org/grok-build/git/trees/37949780c144e37df692e3d669051a21fec24f20?recursive=1'
curl -fsSL https://raw.githubusercontent.com/xai-org/grok-build/37949780c144e37df692e3d669051a21fec24f20/SOURCE_REV
~~~

The tree response reported truncated false. These source checks support
mechanism explanations; the committed real captures establish installed
behavior.

## B — Parser scope and limits

[grok_session.rs](../../../crates/remuda-driver/src/grok_session.rs) uses
remuda-acp-wire classification for params.update, accepting both observed RPC
method spellings. It exposes agent message/thought/user chunks, tool calls and
updates, hook_execution, and Unknown kinds while retaining the complete frame.
Non-text content is preserved. Missing optional native fields stay absent;
invalid JSON or missing discriminators return per-record errors.

The events reader types TurnStarted, PhaseChanged, and FirstToken, retaining
other kinds and their full data. In this requested pre-spike API, turn_ended,
turn_completed, redirect metadata, and hook_annotation remain accessible
through Unknown/full-record data; no driver lifecycle inference is added.

SessionTail polls either JSONL file by byte offset, emits only complete lines,
preserves split UTF-8 bytes until a newline, handles CRLF/blank lines, and resets
on file shrink. Equal-or-larger file replacement requires a new tail; no inode
replacement detection is claimed. A malformed line does not consume later
records when callers parse each emitted line independently.

The active registry reader tolerates extra fields but rejects malformed
identities. locate_session selects a unique PID or cwd match, percent-encodes
the registry's native cwd spelling, and checks the expected existing directory.
Ambiguous matches yield no result; symlink components are not followed.
Registry data does not independently establish PID ownership, freshness,
or liveness.

The module comment points at native-pty-first §4.2. There is no driver,
materializer, protocol, launch, generic-PTY, or SignalAdapter wiring.
Tests exercise the real fixtures plus sparse/future/malformed records, split
UTF-8, partial lines, truncation, exact/ambiguous discovery, and unsafe paths.

## Checks

The worker used its dedicated CARGO_TARGET_DIR, CARGO_INCREMENTAL=0, and
CARGO_BUILD_JOBS=2; the personal absolute target path is intentionally omitted.
Completed checks:

~~~sh
cargo fmt --all
nice cargo build -p remuda-driver --locked
nice cargo clippy -p remuda-driver --all-targets --locked -- -D warnings
nice cargo test -p remuda-driver --locked
./scripts/ci/secret-scan.sh
~~~

**VERIFIED** fmt, build, and clippy passed, with zero clippy warnings.
The complete driver test run passed **218 tests, 0 failures, 10 ignored**
across 16 test binaries, plus zero doctests. All 14 Grok integration tests
passed. The final secret scan passed on the completed implementation, fixtures,
and evidence. `git diff --check` and fixture JSON/apparatus syntax checks passed.
