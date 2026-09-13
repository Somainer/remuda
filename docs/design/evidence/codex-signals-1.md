# D-028 P6: Codex signals pre-spike 1

Date: 2026-09-14 local time (rollout UTC timestamps begin 2026-09-13).
Scope: standalone parser and evidence; no SignalAdapter or driver wiring.
Design references: [native-pty-first](../native-pty-first.md) §§3, 4.2, 6, 7,
13 P6, 14 risk 3; [decisions](../decisions.md) D-028 / D-028a.

**VERIFIED** means a stated runtime result was observed, or a source-only claim
is explicitly identified as such. **FAILED** identifies a tested hypothesis or
command that did not work. Limits do not imply unsupported capabilities.
The installed binary is real `codex-cli 0.154.0`; model responses were supplied
by a local deterministic Responses API server, with `requires_openai_auth=false`.
This tests client/core signals and controls, not hosted-model behavior or billing.
No credentials were read, copied, or printed; all configuration writes stayed
under `/tmp/remuda-codexspike/`. No macOS settings or OS GUI were used.

The installed executable's SHA-256 is
`4f85982624b3898c8991cb80c0981b2aa71070e3537046c9a95950318a95afcc`.
The separately inspected, unmodified Codex source checkout is
`498d40b29f6028dec9ef80af672ba1258980b54a`; this is not a claim of exact build
provenance. Source paths below are relative to that checkout. `CODEX_BIN` denotes
the installed `codex` command, and `CODEX_SOURCE` / `CODEX_SRC` the source checkout.
Personal installation paths are replaced by these variables in command evidence.

## Results that change the P6 assumptions

| Probe | Result | Consequence |
|---|---|---|
| Shadow hooks and trust | VERIFIED canonical per-handler hash and persisted trust | The earlier trust-gate blocker is resolved for this binary |
| PermissionRequest | VERIFIED blocking allow and deny; no stdin tool_use_id | A structured decision path exists; do not reuse PreToolUse correlation schema |
| notify | VERIFIED argv JSON, completed turns only in this run | Filter native thread identity; title-generation child turns also notify |
| rollout | VERIFIED completed records and source ordinals | Item-level evidence; no token delta guarantee |
| Queue provenance in rollout | FAILED to find an explicit enqueue/steer tag in the complete fixture | Delivered messages carry ordinary message/item records; queue-at-keypress needs another channel |
| Enter / Tab / Esc | VERIFIED steer / next-turn queue / turn interruption | Enter does not automatically interrupt a running tool |
| Session discovery | VERIFIED metadata, tool env, live writer FD; index is names | Do not treat the observer's inherited thread env or name index as a PID map |
| Plain TUI and app-server | VERIFIED conditional same-home daemon attachment | Universal “plain TUI cannot attach” is false; external steering itself was not run |

## A2, A3, A5: real TUI apparatus and commands

The manual helpers and captured JSONL are in
[`crates/remuda-driver/tests/fixtures/codex`](../../../crates/remuda-driver/tests/fixtures/codex/README.md).
They are independently written test apparatus, not copied Codex code.
The server emits real Responses API `response.created`,
`response.output_item.done`, and `response.completed` events. Tool calls are
`exec_command`; long probes use `sleep 20` with `yield_time_ms:10000`.
The approval probe only prints a fixed marker and the named `CODEX_THREAD_ID`.

Setup commands (the launch path is normalized to `codex`; helper filenames were
`server.py`, `notify.py`, `keys.py` in the temporary directory):

```sh
mkdir -p /tmp/remuda-codexspike/tui/home /tmp/remuda-codexspike/tui/work
cp crates/remuda-driver/tests/fixtures/codex/probe-server.py /tmp/remuda-codexspike/tui/server.py
cp crates/remuda-driver/tests/fixtures/codex/probe-notify.py /tmp/remuda-codexspike/tui/notify.py
cp crates/remuda-driver/tests/fixtures/codex/probe-keys.py /tmp/remuda-codexspike/tui/keys.py
python3 /tmp/remuda-codexspike/tui/server.py
```

The foreground server was kept in an owned exec session. Its dynamically chosen
loopback port was read from `/tmp/remuda-codexspike/tui/port` and substituted into
this shadow `config.toml`:

```toml
model = "gpt-5.4"
model_provider = "spike"
model_reasoning_effort = "low"
approval_policy = "on-request"
sandbox_mode = "workspace-write"
[model_providers.spike]
name = "Local deterministic spike"
base_url = "http://127.0.0.1:<port>/v1"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
[projects."/private/tmp/remuda-codexspike/tui/work"]
trust_level = "trusted"
[notice]
hide_full_access_warning = true
[features]
shell_snapshot = false
```

The temporary `launch.sh` exported only the shadow `CODEX_HOME`, explicitly
unset inherited `CODEX_THREAD_ID`, then exec'd:

```sh
codex -C /tmp/remuda-codexspike/tui/work --no-alt-screen \
  -c 'notify=["python3","/tmp/remuda-codexspike/tui/notify.py"]'
```

```console
$ test "${HERDR_ENV:-}" = 1
# exit 0
$ herdr pane split --current --direction right --cwd /tmp/remuda-codexspike/tui/work --no-focus
# result.pane.pane_id = "w5:p2C"; focused = false
$ herdr pane run w5:p2C 'sh /tmp/remuda-codexspike/tui/launch.sh'
# exit 0
$ herdr pane read w5:p2C --source recent-unwrapped --lines 60
GPT-5.4 is no longer available
1. Try new model
2. Use existing model
$ herdr pane send-keys w5:p2C down enter
# exit 0; selected existing model for the deterministic provider
$ herdr pane read w5:p2C --source recent-unwrapped --lines 50
>_ OpenAI Codex (v0.154.0)
model: gpt-5.4 low
```

Rendered excerpts omit box drawing and unrelated tips. The initial immediate
`send-text RUN_TOOL` + `send-keys enter` left text in the composer (paste-burst
handling); after reading the screen, another Enter submitted it. Subsequent
helpers separate text and Enter by 700 ms and inspect the resulting screen.
This is a transport timing observation, not a second semantic key binding.

### A2 — notify: VERIFIED

`probe-notify.py` decodes `sys.argv[-1]`; exact first captured payload (cwd
normalized in the committed fixture):

```json
{"argc":2,"payload":{"type":"agent-turn-complete","thread-id":"01a09c00-64d4-7ce1-8537-984e896b8e8a","turn-id":"01a09c00-fad9-7a13-a938-11b8732b4554","cwd":"/tmp/remuda-codexspike/tui/work","client":"codex-tui","input-messages":["RUN_TOOL"],"last-assistant-message":"SPIKE_COMPLETE RUN_TOOL"}}
```

`argc:2` is Python's script name plus one JSON argument. Source
`codex-rs/hooks/src/legacy_notify.rs:13-70` confirms JSON appended to argv,
stdin/stdout/stderr set to null, and spawn without awaiting command completion.
The return value is therefore not an approval decision channel. The final
capture contains 12 notifications: five for the main TUI's completed turns and
seven for task-title child threads. The interrupted main turn emitted no
turn-complete notification. Later `input-messages` contained accumulated user
messages, not just the latest submitted draft. Consumers must filter `thread-id`
and correlate `turn-id`; notification counts are not main-session turn counts.

### A5 — Enter, Tab and Esc: VERIFIED

Commands below ran in the same pane and rollout. The helpers use the exact
`herdr pane send-text`, `send-keys` and `read` operations shown in their source;
each mode starts a base prompt and waits 1.5 seconds before the control probe.

```sh
SPIKE_PANE=w5:p2C python3 /tmp/remuda-codexspike/tui/keys.py STEER
SPIKE_PANE=w5:p2C python3 /tmp/remuda-codexspike/tui/keys.py QUEUE
SPIKE_PANE=w5:p2C python3 /tmp/remuda-codexspike/tui/keys.py ESC
SPIKE_PANE=w5:p2C python3 /tmp/remuda-codexspike/tui/keys.py APPROVAL
```

The commands were separated by observed settlement and rollout inspection;
they are not a fire-and-forget sequence. The recorded excerpts were:

```text
STEER, before Enter:
Working (2s • esc to interrupt) · 1 background terminal running
› STEER_FOLLOWUP
tab to queue message

STEER, after Enter:
Messages to be submitted after next tool call (press esc to interrupt and send immediately)
  ↳ STEER_FOLLOWUP

QUEUE, after Tab:
Working (3s • esc to interrupt) · 1 background terminal running
Queued follow-up inputs
  ↳ QUEUED_FOLLOWUP
    ⌥ + ↑ edit last queued message

ESC, after Esc:
Conversation interrupted - tell the model what to do differently.
1 background terminal running · /ps to view · /stop to close
```

The source composer `codex-rs/tui/src/bottom_pane/chat_composer.rs:91-98`
corroborates Enter submission and Tab queue while running (Tab submits when
idle). Runtime Enter did **not** abort the current turn: `STEER_BASE` and
`STEER_FOLLOWUP` share turn `01a09c01-a792-7961-a2b1-4d1ff7572421`.
The follow-up persisted after the tool returned at its yield boundary.
Tab's `QUEUED_FOLLOWUP` instead began turn
`01a09c02-4909-71e1-a06a-c9bbb4743a6e`, after `QUEUE_BASE`'s task_complete.
Esc persisted `event_msg/turn_aborted` with `reason:"interrupted"` and no
task_complete for that turn. The already-started `sleep 20` later completed
and persisted an item with the **old** turn ID, while another turn was active.
Turn interruption is not proof of tool-process termination.

The approval screen showed:

```text
Would you like to run the following command?
Reason: Approve the throwaway spike command that prints SPIKE_TOOL_OK?
$ printf "SPIKE_TOOL_OK thread=%s\n" "$CODEX_THREAD_ID"
› 1. Yes, proceed (y)
  3. No, and tell Codex what to do differently (esc)
Press enter to confirm or esc to cancel
```

`herdr pane send-keys w5:p2C enter` approved this one harmless test command.
The screen then reported `You approved codex to run ... this time`, the command
printed the matching thread ID, and the rollout recorded exit code 0 followed
by `task_complete`. No permission settings were changed by this answer.

### A3 — rollout record inventory: VERIFIED

The committed `interactive-0.154.0.jsonl` retains every record and its original
timestamp/ordinal, with explicit context redactions described in its README.
This reproducible extraction was run against the real file and the fixture:

```python
import collections, json
rows = [json.loads(line) for line in open(rollout_path)]
print(collections.Counter(row['type'] + (':' + row['payload']['type']
    if 'type' in row['payload'] else '') for row in rows))
```

| Outer / nested type | Count | Observed payload fields relevant to P6 |
|---|---:|---|
| session_meta | 1 | id, session_id, timestamp, cwd, originator, cli_version, source, thread_source, model_provider, base_instructions, history_mode, context_window |
| event_msg / task_started | 6 | turn_id, started_at, model_context_window, collaboration_mode_kind |
| event_msg / task_complete | 5 | turn_id, last_agent_message, started_at, completed_at, duration_ms, time_to_first_token_ms |
| event_msg / item_completed | 22 | thread_id, turn_id, item, started_at_ms, completed_at_ms |
| response_item / message | 15 | id, role, content[{type,text}], internal_chat_message_metadata_passthrough |
| response_item / reasoning | 5 | id, summary[{type:summary_text,text}], content:null, encrypted_content:null, internal_chat_message_metadata_passthrough |
| response_item / function_call | 5 | id, name, arguments (JSON string), call_id, internal_chat_message_metadata_passthrough |
| response_item / function_call_output | 5 | id, call_id, output (string), internal_chat_message_metadata_passthrough |
| token_usage_record | 10 | thread_id, turn_id, session_id, root_turn_id, response_id, usage, turn_token_usage, thread_token_usage |
| event_msg / token_count | 10 | info, rate_limits |
| turn_context | 6 | turn_id, root_turn_id, cwd, workspace_roots, current_date, timezone, approval_policy, approvals_reviewer, sandbox_policy, file_system_sandbox_policy, permission_profile, model, effort, collaboration_mode, summary, personality, comp_hash, multi_agent_version, realtime_active |
| event_msg / turn_aborted | 1 | turn_id, reason, started_at, completed_at, duration_ms |
| event_msg / thread_settings_applied | 5 | thread_id, thread_settings |
| world_state | 1 | full, state |

Total: **97 records**, ordinals 0–96. `item_completed.item.type` included
`UserMessage`, `AgentMessage`, `Reasoning`, and `CommandExecution` (case matters).
CommandExecution carries native id/process_id, command, cwd, status, stdout,
stderr, aggregated_output, exit_code and duration. Usage counters contain
input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens,
reasoning_output_tokens and total_tokens. Per-response usage and cumulative
token_count snapshots coexist; summing both double counts. The fixture's
tokens were intentionally supplied by the mock model, not billing evidence.

Selected source ordinals establish the controls without relying on wall-clock
inference:

| Ordinal | Record |
|---:|---|
| 21 | task_started for STEER_BASE |
| 23 / 29 | base and steered user messages, same turn_id |
| 27 | tool output before steered user message is committed |
| 37 | same turn task_complete, last_agent_message = SPIKE_COMPLETE STEER_FOLLOWUP |
| 54 | QUEUE_BASE task_complete |
| 56 / 58 | new task_started and QUEUED_FOLLOWUP user message |
| 78 | turn_aborted, reason = interrupted |
| 84 | approval-required tool call with sandbox_permissions=require_escalated |
| 86 | late CommandExecution completion for the already interrupted turn |
| 87 / 88 / 96 | approved command completion, tool output, task_complete |

**FAILED** to establish a dedicated persisted enqueue/steer record: the full
inventory contains no queue-operation, queued_message, steer or keypress event.
The queue existed visibly before a user message was appended. Delivered user
messages use ordinary message/item records; same/different turn IDs distinguish
these *observed deliveries*, but cannot prove an arbitrary message was queued
with Tab or show its pending enqueue time. There is likewise no approval
request/decision event in this rollout inventory: screen or hook evidence is
needed to establish the decision. No text delta records were observed; do not
advertise token streaming from this file. Compacted was not runtime-observed
and has only an explicitly synthetic parser test.

Cleanup: `/quit` was submitted, `herdr pane process-info --pane w5:p2C` showed
only its original shell PID 47180, then `herdr pane close w5:p2C` returned
`{"type":"ok"}`. The owned local HTTP server was interrupted; its listening
port no longer appeared in `lsof`. Temporary evidence remains for audit.


### A1 — shadow CODEX_HOME hooks and PermissionRequest

**VERIFIED** against `codex-cli 0.154.0`. The read-only Codex source checkout used for mechanism inspection was commit `498d40b29f6028dec9ef80af672ba1258980b54a`; this is a source reference, not an assertion that the installed binary was built from exactly that commit. Commands below normalize the executable location to `codex`; all live data came from a fresh `/tmp/remuda-codexspike/hooks/` tree. Child environments retained only PATH/HOME/TMPDIR/LANG/TERM plus the shadow CODEX_HOME. No authentication files or keys were read or copied; a local deterministic Responses API provider with `requires_openai_auth = false` supplied the model events.

```console
$ codex --version
codex-cli 0.154.0
$ git -C "$CODEX_SRC" rev-parse HEAD
498d40b29f6028dec9ef80af672ba1258980b54a
$ CODEX_HOME=/tmp/remuda-codexspike/hooks/home codex hooks trust
error: unexpected argument 'trust' found
# exit 2; there is no `hooks trust` CLI subcommand.
$ CODEX_HOME=/tmp/remuda-codexspike/hooks/home codex -c hooks=true features list
Error: failed to load bootstrap configuration
Caused by:
    invalid type: boolean `true`, expected struct HooksToml
    in `hooks`
# exit 1
$ CODEX_HOME=/tmp/remuda-codexspike/hooks/home codex features list
hooks                                    stable             true
plugin_hooks                             removed            false
# exit 0; output filtered to hook feature names.
```

**FAILED**: `hooks = true` at the top level is not the feature switch. **VERIFIED**: `[features] hooks = true` (or `--enable hooks`) is the feature switch. The shadow user configuration was:

```toml
model = "gpt-5.4"
model_provider = "spike"
[features]
hooks = true
[model_providers.spike]
name = "Local deterministic spike"
base_url = "http://127.0.0.1:<ephemeral-port>/v1"
wire_api = "responses"
requires_openai_auth = false
```

`$CODEX_HOME/hooks.json` contained this hook definition (plus an identical SessionStart handler for the trust controls):

```json
{"hooks":{"PermissionRequest":[{"hooks":[{"type":"command","command":"python3 /tmp/remuda-codexspike/hooks/hook.py","timeout":60}]}]}}
```

**VERIFIED**: user-controlled hooks need a matching persisted trust hash unless the invocation explicitly bypasses hook trust. The probe launched `CODEX_HOME=/tmp/remuda-codexspike/hooks/home codex app-server` over stdio and sent these JSON-RPC messages:

```json
{"id":1,"method":"initialize","params":{"clientInfo":{"name":"remuda_hooks_spike","version":"0.1"},"capabilities":{"experimentalApi":true}}}
{"method":"initialized"}
{"id":2,"method":"hooks/list","params":{"cwds":["/tmp/remuda-codexspike/hooks/work"]}}
```

Selected exact `hooks/list` fields before trust:

```json
{"key":"/private/tmp/remuda-codexspike/hooks/home/hooks.json:permission_request:0:0","eventName":"permissionRequest","enabled":true,"isManaged":false,"currentHash":"sha256:e235c38a50e30568d488db8a9f574b773ad6934f4ca611377c5b2cc5a40c07a7","trustStatus":"untrusted"}
```

The trust operation sent was `config/batchWrite` with `reloadUserConfig:true`, `keyPath:"hooks.state"`, `mergeStrategy:"upsert"`, and this value:

```json
{"/private/tmp/remuda-codexspike/hooks/home/hooks.json:permission_request:0:0":{"trusted_hash":"sha256:e235c38a50e30568d488db8a9f574b773ad6934f4ca611377c5b2cc5a40c07a7"},"/private/tmp/remuda-codexspike/hooks/home/hooks.json:session_start:0:0":{"trusted_hash":"sha256:f951147270169767a1c94eb1b6ec49384dfdc0523091a92b1c4f0c9e2981c77d"}}
```

Its result contained `"status":"ok"` and `"filePath":"/private/tmp/remuda-codexspike/hooks/home/config.toml"`. A second `hooks/list` returned `trustStatus:"trusted"` for both definitions. macOS canonicalizes `/tmp` to `/private/tmp` here; callers should use the returned hook key, not construct it from an uncanonicalized path. This is the same trust-write shape used by the TUI `/hooks` action in source `codex-rs/tui/src/hooks_rpc.rs:55-91`; this probe exercised the RPC, not the slash-command UI.

**VERIFIED** hash calculation: source `codex-rs/hooks/src/engine/discovery.rs` normalizes each handler and replaces the matcher's handler vector with that one handler, adds snake_case `event_name`, serializes through TOML, then `codex-rs/config/src/fingerprint.rs:50-83` converts to JSON, recursively sorts object keys, emits compact JSON, and returns `sha256:<lowercase hex>`. Null optional fields disappear through TOML serialization. Timeouts are normalized (ordinary hooks default to 600 seconds; the probe explicitly used 60). The hash is per handler configuration, not the hooks.json bytes or script contents. RPC `eventName` is camelCase, which is not the hash spelling. Exact bytes and observed matching hash:

```console
$ python3 - <<'PY'
import hashlib, json
identity = {"event_name":"permission_request","hooks":[{"type":"command","command":"python3 /tmp/remuda-codexspike/hooks/hook.py","timeout":60,"async":False}]}
canonical = json.dumps(identity,sort_keys=True,separators=(',',':'))
print(canonical)
print('sha256:'+hashlib.sha256(canonical.encode()).hexdigest())
PY
{"event_name":"permission_request","hooks":[{"async":false,"command":"python3 /tmp/remuda-codexspike/hooks/hook.py","timeout":60,"type":"command"}]}
sha256:e235c38a50e30568d488db8a9f574b773ad6934f4ca611377c5b2cc5a40c07a7
```

**VERIFIED** negative controls with first completed turns, not merely thread creation:

```json
{"case":"untrusted","feature":true,"trust":["untrusted"],"session_start_hook_count":0}
{"case":"trusted","feature":true,"trust":["trusted"],"session_start_hook_count":1}
{"case":"feature_off","feature":false,"trust":[],"session_start_hook_count":0}
{"case":"exec_bypass","exit":0,"session_start_hook_count":1}
```

The last command was `CODEX_HOME=/tmp/remuda-codexspike/hooks/bypass codex exec --dangerously-bypass-hook-trust --skip-git-repo-check --json 'Say matrix complete.'` with an untrusted definition. **FAILED** as a way to bypass for app-server-created threads: placing `--dangerously-bypass-hook-trust` before `app-server` left the probe's untrusted hook unexecuted. Persisted trust worked without bypass in the allow/deny tests below.

**VERIFIED** PermissionRequest synchronously blocks and returns a real decision. The local API emitted `response.output_item.done` with a function call named `exec_command`, `call_id:"spike-call-1"` (deny) or `"spike-call-3"` (allow), and arguments:

```json
{"cmd":"printf hook-approved > /tmp/remuda-codexspike/hooks/approved-1","sandbox_permissions":"require_escalated","justification":"Test PermissionRequest hook with harmless temp marker"}
```

Each thread used `thread/start` with `approvalPolicy:"on-request"`, `sandbox:"read-only"`, `model:"gpt-5.4"`, `modelProvider:"spike"`, and the temporary working directory, followed by `turn/start`. The hook read stdin JSON, wrote a `waiting` marker, waited for a decision file, and returned this shape with either `deny` or `allow`:

```json
{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"spike decision deny"}}}
```

Selected exact probe output:

```text
HOOK_WAITING deny tool_executed []
HOOK_STILL_WAITING deny elapsed 1.29 tool_executed []
TURN_COMPLETED deny tool_executed []
HOOK_WAITING allow tool_executed []
HOOK_STILL_WAITING allow elapsed 1.28 tool_executed []
TURN_COMPLETED allow tool_executed ['approved-3']
```

`hook/completed` corroborated deny with `status:"blocked",durationMs:1160` and feedback `spike decision deny`; allow had `status:"completed",durationMs:1135`. The denied rollout recorded a `response_item/function_call_output` with `exec_command failed: CreateProcess { message: "Rejected(\"spike decision deny\")" }`. The allow rollout recorded exit code 0, and the command's actual marker file contained `hook-approved`. No `item/commandExecution/requestApproval` RPC was emitted during these hook-resolved requests. This is real core/app-server approval execution; interactive TUI trust-menu operation was not exercised by this sub-probe.

Exact captured PermissionRequest stdin (deny case):

```json
{"session_id":"01a09c00-e7bf-7852-8a8d-25d64d94c4ba","turn_id":"01a09c00-e8fd-71a3-a2e4-0f2ac16d2537","transcript_path":"/private/tmp/remuda-codexspike/hooks/home/sessions/2026/09/14/rollout-2026-09-14T02-21-40-01a09c00-e7bf-7852-8a8d-25d64d94c4ba.jsonl","cwd":"/tmp/remuda-codexspike/hooks/work","hook_event_name":"PermissionRequest","model":"gpt-5.4","permission_mode":"default","tool_name":"Bash","tool_input":{"command":"printf hook-approved > /tmp/remuda-codexspike/hooks/approved-1","description":"Test PermissionRequest hook with harmless temp marker"}}
```

**VERIFIED**: no `tool_use_id` is present in PermissionRequest stdin. The tool is normalized to `Bash`, even though the model called `exec_command`; the command and justification become `tool_input.command` and `tool_input.description`. The source schema `codex-rs/hooks/src/schema.rs:301-319` matches this observed shape and additionally permits optional `agent_id`/`agent_type` for subagents. `PreToolUse` and `PostToolUse` have `tool_use_id`, so their schema must not be reused for PermissionRequest. The app-server hook run ID contains the call-id suffix, but that field is not passed on stdin.

Limitation: these observations establish configuration trust and synchronous allow/deny. They do not establish timeout-deny policy; the probe supplied a decision before the configured 60-second timeout. Hashing a command string does not authenticate subsequent changes to the script at that path.


### A4 — Session discovery: VERIFIED (with limits)

Commands below use `CODEX_BIN=$(command -v codex)` and `CODEX_SOURCE` for the read-only source checkout; personal installation paths are normalized to these variables. The tested binary reports `codex-cli 0.154.0`; its native executable SHA-256 is `4f85982624b3898c8991cb80c0981b2aa71070e3537046c9a95950318a95afcc`. Read-only source checkout: `498d40b29f6028dec9ef80af672ba1258980b54a` (`2026-09-03 Box the TUI resume picker future (#42432)`). This identifies the inspected checkout, not proof that the binary was built from this exact commit.

`CODEX_THREAD_ID` is assigned by Codex when constructing **tool-child environments**, after shell-environment filters/overrides. It is not a reliable launch-time self-identification variable for the parent TUI: an inherited value may identify the calling parent Codex, and absent values need not be installed into the TUI process's own environment. Source command:

```sh
sed -n '130,160p' "$CODEX_SOURCE/codex-rs/protocol/src/shell_environment.rs"
```

Relevant exact output:

```rust
// Step 6 - Populate the thread ID environment variable when provided.
if let Some(thread_id) = thread_id {
    env_map.insert(CODEX_THREAD_ID_ENV_VAR.to_string(), thread_id.to_string());
}
```

The root TUI launch explicitly used `env -u CODEX_THREAD_ID`; its real shell-tool run then printed only this named variable:

```text
SPIKE_TOOL_OK thread=01a09c00-64d4-7ce1-8537-984e896b8e8a
```

The resulting first rollout `session_meta.payload.id` and `session_id` both equaled `01a09c00-64d4-7ce1-8537-984e896b8e8a`. Relevant extracted metadata:

```json
{"type":"session_meta","id":"01a09c00-64d4-7ce1-8537-984e896b8e8a","session_id":"01a09c00-64d4-7ce1-8537-984e896b8e8a","timestamp":"2026-09-13T18:21:06.646Z","cwd":"/tmp/remuda-codexspike/tui/work","originator":"codex-tui","cli_version":"0.154.0","source":"cli"}
```

There is no `pid` field in this session metadata. With the live **native** TUI process ID obtained from `herdr pane process-info --pane w5:p2C`, this exact read-only query mapped it to its file:

```sh
lsof -nP -p 51331 -Fn | rg 'rollout-|thread-writer-locks'
```

```text
n/private/tmp/remuda-codexspike/tui/home/thread-writer-locks/01a09c00-64d4-7ce1-8537-984e896b8e8a.lock
n/private/tmp/remuda-codexspike/tui/home/sessions/2026/09/14/rollout-2026-09-14T02-21-06-01a09c00-64d4-7ce1-8537-984e896b8e8a.jsonl
```

Before its first persisted turn, the same query found the writer lock but no rollout file. Thus PID→open-file discovery works for this embedded TUI after materialization; it is not a promise that the Node launcher PID holds the file, that an idle newly-created thread already has a file, or that a TUI using a shared daemon owns its rollout descriptor. A controlled `CODEX_HOME`, explicit session metadata, and writer-process ownership remain necessary disambiguation inputs.

`session_index.jsonl` is an append-only **name index**, not a live PID registry or a complete catalogue of all created threads. `codex-rs/rollout/src/session_index.rs:21-49` defines entries `{id,thread_name,updated_at}` and writes them for name updates. The root TUI index was absent both before and after its first completed tool run (`session_index_exists=False`). A second plain TUI attached to the isolated app-server was renamed with:

```sh
herdr pane send-text w5:p2D '/rename discovery-evidence'
herdr pane send-keys w5:p2D enter
```

After the rename settled, reading its shadow index returned:

```json
{"id":"01a09c01-4c35-7f00-9094-6c72466541cf","thread_name":"discovery-evidence","updated_at":"2026-09-13T18:24:31.434405Z"}
```

For session-id lookup, filenames live under `$CODEX_HOME/sessions/YYYY/MM/DD` and use a local-date timestamp; metadata timestamps are UTC. Source `codex-rs/rollout/src/recorder.rs:1628-1649` derives the directory from local time and renders `RolloutFileName`. The tested local `2026/09/14/02:21` filename therefore correctly contains a `2026-09-13T18:21Z` session timestamp. Do not infer a filesystem day from a UTC metadata timestamp alone.

### A6 — App-server / remote-control: VERIFIED observation; steering API source-verified only

A plain interactive 0.154.0 TUI **can attach to an already-running local app-server**: `CODEX_HOME=/tmp/remuda-codexspike/discovery/home "$CODEX_BIN" app-server --listen unix://` created the default private control socket; plain `env -u CODEX_THREAD_ID CODEX_HOME=/tmp/remuda-codexspike/discovery/home "$CODEX_BIN"` in a new Herdr pane, with provider settings only in that shadow home's `config.toml`, then became externally visible. An independent WebSocket client over `$CODEX_HOME/app-server-control/app-server-control.sock` sent `initialize`, `initialized`, and `thread/loaded/list`; exact response was `{"id":2,"result":{"data":["01a09c01-4c35-7f00-9094-6c72466541cf"],"nextCursor":null}}`. `thread/read` returned that same live thread with `status:{"type":"idle"}`, `cliVersion:"0.154.0"`, `source:"vscode"`, and path `/private/tmp/remuda-codexspike/discovery/home/sessions/2026/09/14/rollout-2026-09-14T02-22-05-01a09c01-4c35-7f00-9094-6c72466541cf.jsonl`. This is **conditional startup attachment**, not universal registration of any already-running TUI: source `codex-rs/tui/src/lib.rs:448-475,883-956` probes the default same-home Unix socket with a 50 ms timeout and uses it only when launch settings can be replayed; `-c` overrides, nondefault loader/strict settings, executor selection, and workload identity can require an embedded server instead. A separately launched arbitrary stdio or nondefault-socket app-server is not discovered by that default probe, and existing embedded TUI sessions are not thereby migrated. `codex-rs/app-server/README.md:234-235` documents `turn/steer` and `turn/interrupt` for loaded active turns; external active-turn steering was not exercised in this bounded spike. `codex remote-control --help` says `[experimental] Manage the app-server daemon with remote control enabled`, with `start`, `stop`, and `pair`; source `codex-rs/cli/src/remote_control_cmd.rs:65-151` starts/enables a server/daemon (the foreground form uses its own temporary socket), rather than attaching to an arbitrary PTY PID. No remote-control startup, pairing, credentials, or hosted connectivity were used.

Reproducible external observation command (notifications are intentionally excluded from output because they may contain machine identity):

```sh
CODEX_HOME=/tmp/remuda-codexspike/discovery/home python3 - <<'PY'
import json, os, socket, websocket
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(os.environ['CODEX_HOME'] + '/app-server-control/app-server-control.sock')
w = websocket.create_connection('ws://localhost/', socket=s, timeout=5)
def request(message):
    w.send(json.dumps(message))
    while True:
        result = json.loads(w.recv())
        if result.get('id') == message['id']:
            return result
request({'id':1,'method':'initialize','params':{'clientInfo':{'name':'remuda_discovery','version':'0.1'},'capabilities':{'experimentalApi':True}}})
w.send('{"method":"initialized"}')
print(json.dumps(request({'id':2,'method':'thread/loaded/list','params':{}})))
w.close()
PY
```

Cleanup: closed only the discovery-created pane `w5:p2D` and terminated its foreground app-server native PID `51606` and Node launcher PID `51596`; root TUI pane `w5:p2C` was not steered or closed.

## Parser boundary and validation

`codex_rollout.rs` preserves source timestamp/ordinal, optional caller ordinal
fallback, typed known records, original tool/usage JSON, and Unknown type names.
Malformed JSON returns an error for the caller to report/skip; missing or new
fields do not stop parsing other lines. It does not deduplicate item/message
representations, infer turn association, interpret Unknown as task-complete,
sum token snapshots, or send approval decisions. The eventual integration is
documented against native-pty-first §4.2 only.

`RolloutTail` reads from a byte offset, holds unfinished lines as bytes (including
split UTF-8), emits newline-complete lines, and restarts after file shrink. A
same-size/larger file replacement requires a fresh tail. `locate_rollout` uses
the selected CODEX_HOME (or HOME/.codex), checks session_meta.id, and scans active
before archived files without following directory/file symlinks. It returns
None when multiple matching files exist in the selected scope: source
`codex-rs/thread-store/src/local/thread_rollout_resolver.rs:1-5,73-110` explains
that thread/revert keeps the ID and can leave an obsolete immutable rollout.
The current file then requires live writer/PID or SQLite evidence. Root
`session_id` alone is not accepted as thread identity. No database dependency or
driver integration was added in this spike.

**VERIFIED** checks on the complete owned change set:

```sh
export CARGO_TARGET_DIR="$REMUDA_MAIN/target-c-codexspike"
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=2
cargo fmt --all
nice -n 10 cargo build -p remuda-driver --locked
nice -n 10 cargo clippy -p remuda-driver --all-targets --locked -- -D warnings
nice -n 10 cargo test -p remuda-driver --locked
./scripts/ci/secret-scan.sh
git diff --check
```

`REMUDA_MAIN` denotes the coordinator's main checkout solely for the mandated
target directory. Build and clippy exited 0; fmt and diff-check produced no
errors; secret-scan returned `secret-scan: pass`. Test totals were **204 passed,
0 failed, 10 ignored**, including **12 codex_rollout tests**. Ignored tests are
existing live-harness/host-dependent cases, plus the child-environment helper
that is re-executed by its parent test. New tests cover every requested record
variant, the real interactive file, malformed/future data, raw argument/output
preservation, partial-line append and split UTF-8, truncation, metadata identity,
symlinks, duplicate reverted files and root-only identity rejection. Runtime
Task A probes are the separate manual evidence above.
