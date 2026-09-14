# Where Claude Code exposes Workflow progress (verified 2026-09-14 on claude 2.1.270, macOS)

Verified first-hand by the coordinator on real runs under
`~/.claude/projects/<project-slug>/<sessionId>/`. Paths below are relative to that session dir.

## 1. Anchor: the Workflow tool_result in the session transcript
The `tool_result` for a `Workflow` tool_use is plain text, e.g.
```
Workflow launched in background. Task ID: wrdsl2l9b
Summary: <meta.description>
Transcript dir: <session>/subagents/workflows/wf_fce70615-765
Script file: <session>/workflows/scripts/<meta.name>-wf_fce70615-765.js
```
Foreground runs return the script's return value in the same tool_result (verify the exact
text in the spike). Remuda's transcript mapper already sees this tool_result, so the run id
and both paths are available to the Node without any new hook.

## 2. Run directory `subagents/workflows/<runId>/`
- `journal.jsonl` (append-only, one JSON per line):
  - `{"type":"launched"}`
  - `{"type":"started","key":"v2:<sha256>","agentId":"a299d0…","label":"verify:doc-facts","phase":"Verify"}`
    (`label`/`phase` present on 2.1.26x+; older versions write `started` with only `key`+`agentId` —
    3744 such records vs 506 with label/phase across this machine's runs)
  - `{"type":"result","key":…,"agentId":…,"result":<agent return value>}`
  - `{"type":"failed","key":…,"agentId":…}`
  - NO record for cancelled/killed/paused was observed (25 runs). Killed/paused come from the
    main session's background-task state (TaskStop / TaskOutput status "paused"); the spike must
    run one TaskStop and record what lands (journal, tool_result, hooks).
  - `key` is the memo key (hash of prompt+opts); a retry after a terminal API error re-emits
    `started` with the SAME key → attempt count = number of `started` per key.
- `agent-<agentId>.meta.json`:
  `{"agentType":"workflow-subagent","description":"verify:doc-facts","workflowPhase":"Verify","spawnDepth":1,"requestShape":"foreground","requestNonInteractive":false,"model":"opus"}`
  → label (`description`), phase, model short name.
- `agent-<agentId>.jsonl`: the subagent's full transcript (types `user`/`assistant`/`attachment`),
  each `assistant` record has `message.model` (e.g. `claude-opus-5`), `message.usage`
  (input/output/cache tokens), `message.content[].type=="tool_use"` with `name` (latest tool),
  `timestamp`, `effort`. First record has `agentId`, `sessionId`, `version`.
  → duration = first→last timestamp; tokens = sum of usage; latest tool = last tool_use name.
- `workflows/scripts/<name>-<runId>.js`: the script; `export const meta = {…, phases:[{title,detail}]}`
  gives the phase list and order (Remuda's `parseWorkflowMeta` already parses name/description —
  extend it to `phases`).

## 3. Hooks (event names present in the 2.1.270 bundle)
- `SubagentStart` payload: `{hook_event_name, agent_id, agent_type}`; workflow agents have
  `agent_type == "workflow-subagent"` (matches meta.json `agentType`).
- `SubagentStop` payload: `{agent_id, agent_type, agent_transcript_path, last_assistant_message, stop_hook_active}`.
  → real-time start/stop per agent without polling, plus the transcript path. No phase/label in
  the payload: join on `agent_id` with `agent-<id>.meta.json` (written at spawn).
- `TaskCreated` / `TaskUpdate` / `TaskCompleted` are the agent-team task list
  (`task_id, task_subject, task_description, teammate_name, team_name`) — NOT workflow progress.
- `PostToolUse` fires inside subagents too (hook socket already carries them; the payload's
  `agent_id`/session identifies the subagent) → latest tool + tokens without reading files, if the
  spike confirms the socket receives subagent hook events.
- Errors: `WorkflowBudgetExceededError`, `WorkflowInputError`, `WorkflowAgentCapError`,
  `WorkflowRemotePreconditionError` are thrown into the tool_result text.

## 4. Version caveat
<remote-host> runs claude 2.1.221: `journal.jsonl` there has no `label`/`phase` on `started`;
check whether `agent-*.meta.json` exists at all on that version. This is exactly the
"退化成普通 tool 行 + 说明" case (decision 6) — the card must detect missing phase data, not crash.

## 5. Proposed Remuda data path (for the W brief)
Node-side, read-only, bounded: on a `Workflow` tool_result the Node parses the run dir + script
path (same host as the harness), tails `journal.jsonl` and each `agent-<id>.meta.json`, and reads
the tail of `agent-<id>.jsonl` only for the fields above (no content, no prompt/result bodies),
emitting an additive journal event `workflow.progress` keyed by runId:
`{runId, taskId, name, description, status, phases:[{title, agents:[{agentId,label,state:queued|running|done|failed|killed,model,latestTool,tokens,startedAt,endedAt,attempt}]}], totals:{agents,done,tokens,calls,elapsedMs}}`.
SubagentStart/Stop hooks (already relayed by the shim) are the low-latency trigger; the files are
the source of truth. `queued` agents are known from the script only after they start (pipeline()
items are not pre-announced) → show queued rows only when meta.phases + the script's item count
can be derived, otherwise show done/running counts without a total.
