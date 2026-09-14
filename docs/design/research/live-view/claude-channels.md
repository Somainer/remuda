# Claude Code 2.1.270 — real-time signal channels for an external observer

**Target binary:** `~/.local/share/claude/versions/2.1.270` (Mach-O arm64, Bun-compiled, 198 MB)
**Measured on:** macOS 25.4.0 (arm64), 2026-09-15 — 5 instrumented sessions (4 interactive TUI under a real PTY + 1 `--print` stream-json), relay endpoint via `--settings ~/.claude/settings.relay.json`.
**Raw artifacts:** `<coord-scratch>/realtime/scratch-chanprobe/`

---

## 0. Source-of-truth table — "state on screen → earliest channel → latency → payload → fidelity"

Latencies are **milliseconds after the causal event** (the Enter keypress, or the API/tool event named in the row).
Command-hook numbers *include* ~22 ms of `fork`+`exec` (see §1.5) — subtract it for the in-process fire time.

| # | State the TUI shows | Earliest channel | Latency | Payload you get | Fidelity / caveats |
|---|---|---|---|---|---|
| 1 | **Prompt submitted / turn started** | OSC 0 title flips to `◐`, plus `OSC 9;4;3` | **16–31 ms** after Enter | busy/idle bit + session-title string | Free, no config. Title text is `Claude Code` until the AI title lands (~4 s into turn 1). Killed by `CLAUDE_CODE_DISABLE_TERMINAL_TITLE` / `terminalProgressBarEnabled:false` |
| 1b | same | `UserPromptSubmit` hook | **28–50 ms** | `prompt` (full text), `prompt_id`, `session_id`, `transcript_path`, `cwd`, `permission_mode`, `session_title` | Blocking-capable; 30 s timeout (`Qtt`). Requires workspace trust |
| 1c | same | screen repaint (prompt echoed into scrollback; footer → `esc to interrupt`) | 24–33 ms | rendered text | — |
| 1d | same | transcript JSONL `user` record | **472 ms** (record `timestamp` = 7 ms ⇒ **465 ms write lag**) | full user message + attachments | Slowest of the four |
| 2 | **Assistant text streaming in** | `MessageDisplay` hook | fires **3–13 ms before** the matching screen repaint | `{turn_id, message_id, index, final, delta}` | **`text` blocks only.** `delta` = whole newly-completed lines; ≥100 ms between flushes; final flush may end mid-line and is empty if the message ends on `\n`. Must be configured — with no hook the streaming machinery never engages |
| 2b | same | screen (`⏺ <text>` rows) | flush cadence, ≥100 ms granularity | rendered, wrapped, ANSI-styled | Re-renders/reflows; no stable block id |
| 2c | same | transcript `assistant` record (`apiBlockIndex`) | **+401 … +4 883 ms** after the first MessageDisplay flush (measured 401 / 576 / 1 477 / 2 115 / 4 883) | complete block | Record is *created* at `content_block_stop` but the *file append* is deferred (§2). Often lands **after** the `Stop` hook |
| 3 | **Tool row appears** (`⏺ Bash(cmd)` + `⎿ Waiting…`) | screen | — (reference) | rendered command line | — |
| 3b | same | `PreToolUse` hook | **+13.7 ms after the screen row** (it is *not* earlier) | `tool_name`, `tool_input`, `tool_use_id` + base | Blocks the tool; can deny / rewrite input |
| 3c | same | transcript `assistant`/`tool_use` | **+4 973 ms** (auto-approved: not flushed until the tool finished) / **+72 ms** (permission-prompt path) | full block | Wildly variable — see §2 |
| 4 | **"tool X running (34s)"** (`⎿ Running… (3s)`, pulsing `⏺`) | **screen only** | timer text 1 Hz; `⏺` pulses ~600 ms | text `Running… (Ns)`, `(ctrl+b to run in background)` | **No hook, no transcript record, no OSC.** Only machine-readable via the rendered line (or SDK `task_started` / `task_notification` if you own the process) |
| 5 | **Permission dialog up** | OSC 0 title flips to `✳` | **9 ms before** the dialog paints | glyph only ("needs attention / idle") | `✳` also means plain idle — disambiguate by `OSC 9;4` still being `3` |
| 5b | same | `PermissionRequest` hook | **+4.5 ms after** the dialog paints | `tool_name`, `tool_input`, `permission_suggestions[]` (addRules / setMode / addDirectories …) | Can auto-answer with `decision:{behavior:"allow"|"deny"}` |
| 5c | same | `Notification` hook, `notification_type:"permission_prompt"` | **+6 061 ms** after `PermissionRequest` (constant `RJe = 6000`; cancelled if answered first) | `message:"Claude needs your permission"`, `notification_type` | Deliberately delayed — useless as a "dialog opened" signal. Disable with `CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS` |
| 6 | **Tool finished, output tail shown** (`⎿  HELLO-PROBE`) | `PostToolUse` hook | **8.2 ms before** the screen shows the tail | `tool_name`, `tool_input`, **`tool_response` (full result)**, `tool_use_id`, `duration_ms` | `duration_ms` excludes permission + hook time |
| 6b | same | `PostToolBatch` hook | +32 ms after `PostToolUse` | `tool_calls:[{tool_name,tool_input,tool_use_id,tool_response}]` | Fires **exactly once** per batch, after every parallel call resolves |
| 6c | same | transcript `user`/`tool_result` | **+231 ms** after the screen | full result | Transcript's best case |
| 7 | **Turn ended** ("Cooked for 15s · done 12:25 AM") | `Stop` hook | **8.6–12.9 ms before** the OSC edge, **12.9–16.1 ms before** the screen line | `last_assistant_message` (text!), `stop_hook_active`, `background_tasks[]`, `session_crons[]` | Can re-open the turn (`decision:"block"`) |
| 7b | same | OSC 0 → `✳`, then `OSC 9;4;0` | +8.6 / +11.1 ms after `Stop` | idle bit | Cleanest free "turn over" edge |
| 7c | same | transcript `system` records | **+726 … +1 467 ms** after `Stop` | — | |
| 8 | **Spinner phrase** (`✽ Zigzagging… (14s · ↓ 103 tokens)`) | screen only | glyph ~110 ms, timer 1 Hz, token count ~110 ms | text | Verb is random from a ~200-word list and rotates mid-turn. **The token number is `round(chars/4)` of the streamed response, eased/animated** — *not* the API `output_tokens`. `(running stop hook · …)` appears inside the parens while hooks run |
| 9 | **Turn-duration line** | screen only (`showTurnDuration`) | — | `<Verb> for <Nm Ns> · done <h:mm AM>` | Verb ∈ {Baked, Brewed, Churned, Cogitated, Cooked, Crunched, Sautéed, Worked} |
| 10 | **Subagent lifecycle** | `SubagentStart` / `SubagentStop` hooks | at spawn / at subagent turn end | `agent_id`, `agent_type`; Stop adds `agent_transcript_path`, `last_assistant_message`, `background_tasks`, `session_crons` | Subagent hooks carry `agent_id` in the base object; the main thread never does |

**Bottom line:** the transcript JSONL is the *worst* channel on every row (0.23 s – 4.9 s late, and routinely after `Stop`). The cheapest real-time upgrade is **OSC 0 + OSC 9;4 off the PTY** (turn / idle / attention edges in ~20 ms, zero configuration); the cheapest *semantic* upgrade is a **hook set** delivered over an **`http` hook** rather than `command` (drops the ~22 ms spawn cost).

---

## 1. Channel 1 — hooks

### 1.1 Complete event list (bundle symbols `Pm` / `Ale` @ ~165209529)

```
PreToolUse, PostToolUse, PostToolUseFailure, PostToolBatch, Notification,
UserPromptSubmit, UserPromptExpansion, SessionStart, SessionEnd, Stop, StopFailure,
SubagentStart, SubagentStop, PreCompact, PostCompact, PreModelSwitch, PostModelSwitch,
PermissionRequest, PermissionDenied, Setup, TeammateIdle, TaskCreated, TaskCompleted,
Elicitation, ElicitationResult, ConfigChange, WorktreeCreate, WorktreeRemove,
InstructionsLoaded, CwdChanged, FileChanged, DirectoryAdded, MessageDisplay
```

**There is no `TaskUpdate` event in 2.1.270** (the brief asked for it): only `TaskCreated` and `TaskCompleted` exist.

### 1.2 Base payload present on every event (`Na()` @ 175106344; schema `ke()` @ ~167033000)

```json
{ "session_id", "transcript_path", "cwd", "scratchpad_dir?", "prompt_id?",
  "permission_mode?", "agent_id?", "agent_type?", "effort?": {"level": "high"} }
```

* `prompt_id` — UUID correlating a user prompt with every event until the next prompt; identical to the OTel `prompt.id` attribute. **This is the join key for an observer.**
* `agent_id` present ⇔ the hook fired inside a subagent.
* For a call served to a cloud session the base degrades to `{session_id:"served:<callerSessionId>", transcript_path:"", cwd, permission_mode, agent_id, effort}`.

### 1.3 Per-event payloads (verbatim from the embedded Zod schemas @ 167034000–167060000)

| Event | Event-specific fields |
|---|---|
| `PreToolUse` | `tool_name`, `tool_input`, `tool_use_id` |
| `PermissionRequest` | `tool_name`, `tool_input`, `permission_suggestions?` — array of `addRules` / `replaceRules` / `removeRules` / `setMode` / `addDirectories` / `removeDirectories` |
| `PostToolUse` | `tool_name`, `tool_input`, **`tool_response`**, `tool_use_id`, `duration_ms?` *("excludes permission-prompt and hook time")* |
| `PostToolUseFailure` | `tool_name`, `tool_input`, `tool_use_id`, `error`, `is_interrupt?`, `duration_ms?` |
| `PostToolBatch` | `tool_calls: [{tool_name, tool_input, tool_use_id, tool_response?}]` — *"Fired once after every tool call in a batch has resolved, before the next model request."* |
| `PermissionDenied` | `tool_name`, `tool_input`, `tool_use_id`, `reason` |
| `Notification` | `message`, `title?`, `notification_type` |
| `UserPromptSubmit` | `prompt`, `source?` ∈ `user|sdk|system|loop_wakeup|schedule_wakeup|poll_event`, `session_title` |
| `UserPromptExpansion` | `expansion_type` ∈ `slash_command|mcp_prompt`, `command_name`, `command_args`, `command_source?`, `prompt` |
| `SessionStart` | `source` ∈ `startup|resume|clear|compact|fork`, `agent_type?`, `model?`, `session_title?`, `seconds_since_last_response?`, `context_tokens?`, `prompt_cache_likely_expired?`, `estimated_cache_write_usd?` |
| `SessionEnd` | `reason` ∈ `clear|resume|logout|prompt_input_exit|other` |
| `Stop` | `stop_hook_active`, `last_assistant_message?` *("Text content of the last assistant message before stopping. Avoids the need to read and parse the transcript file.")*, `background_tasks?[]`, `session_crons?[]` |
| `StopFailure` | `error`, `error_details?`, `last_assistant_message?` |
| `SubagentStart` | `agent_id`, `agent_type` |
| `SubagentStop` | `stop_hook_active`, `agent_id`, `agent_transcript_path`, `agent_type`, `last_assistant_message?`, `background_tasks?[]`, `session_crons?[]` |
| `TaskCreated` / `TaskCompleted` | `task_id`, `task_subject`, `task_description?`, `teammate_name?`, `team_name?` *(deprecated)* |
| `TeammateIdle` | `teammate_name`, `team_name` *(deprecated)* |
| `Elicitation` | `mcp_server_name`, `message`, `mode?` ∈ `form|url`, `url?`, `elicitation_id?`, `requested_schema?` |
| `ElicitationResult` | `mcp_server_name`, `elicitation_id?`, `mode?`, `action` ∈ `accept|decline|cancel`, `content?` |
| `PreCompact` / `PostCompact` | `trigger` ∈ `manual|auto` + `custom_instructions` / `compact_summary` |
| `Pre/PostModelSwitch` | `from_model`, `to_model`, `requested_model`, `source`, `context_tokens`, `prompt_cache_warm`, `cache_ttl`, `estimated_cache_write_usd`, `pricing` |
| `ConfigChange` | `source` ∈ `user_settings|project_settings|local_settings|policy_settings|skills`, `file_path?` |
| `InstructionsLoaded` | `file_path`, `memory_type` ∈ `User|Project|Local|Managed`, `load_reason` ∈ `session_start|nested_traversal|path_glob_match|include|compact`, `globs?`, `trigger_file_path?`, `parent_file_path?` |
| `CwdChanged` / `FileChanged` / `DirectoryAdded` | `old_cwd,new_cwd` / `file_path,event ∈ change|add|unlink` / `directory,source` |
| `WorktreeCreate` / `WorktreeRemove` | `name` / `worktree_path` |
| **`MessageDisplay`** | `turn_id`, `message_id`, `index`, `final`, `delta` — see §1.4 |

`background_tasks[]` entry: `{id, type ('shell'|'subagent'|'monitor'|'workflow'), status, description, command?, agent_type?, server?, tool?, name?}` (strings capped at 1000 chars with a `… [+N chars]` marker).
`session_crons[]` entry: `{id, schedule, recurring, prompt}`.

`Notification.notification_type` values in the bundle: `permission_prompt`, `idle_prompt` ("Claude is waiting for your input", after `messageIdleNotifThresholdMs`, default 60 000), `agent_needs_input`, `agent_completed`, `worker_permission_prompt`, `elicitation_complete`, `elicitation_response`, `auth_success`.

### 1.4 `MessageDisplay` — what it really carries

**Assistant *text* only.** Not tool rows, not the spinner, not status lines, not thinking blocks, not tool output. Proof in both directions:

* Code (`f9n` @ 188578277, `m9n` @ 188582541): the streamed buffer is fed only from text deltas; the SDK path joins `content.filter(type === "text")`; if the joined text is `""` the hook is skipped entirely.
* Empirically: in the `ls -la /tmp` run the assistant message contained a `thinking` block and a `tool_use` block — **no** MessageDisplay fired for either; only the three text flushes produced events.

Schema text, verbatim:

> `turn_id` — UUID of the current turn.
> `message_id` — UUID of the assistant message being displayed. **Stable across every flush of the same message. Not the API `msg_…` id.**
> `index` — Zero-based index of this delta within the message. Increments by one per flush.
> `final` — True on the message's last flush. Exactly one flush per message has it.
> `delta` — **The newly completed lines since the prior flush. Always whole lines, except on the final flush which may end mid-line. The delta of the final flush is empty when the message ends on a newline; treat `final` as the end-of-message signal regardless.**

Flush machinery constants (`@188578150`): `qo = 10` ⇒ **`Rn = 100 ms` minimum inter-flush interval**; `Tn = 3` max concurrent hook invocations; `In = 10 000 ms` hook timeout. The flush boundary is `raw.lastIndexOf("\n") + 1`, so **a message containing no newline produces exactly one flush, at `finalize()` (end of the whole assistant message, i.e. `message_stop`) — not at `content_block_stop`.**

Measured cadence (run `text2`, prompt = "numbers one through twenty, one per line"):

```
5375.9 ms  idx=0 final=false  'one\ntwo\nthree\nfour\n'
5479.0 ms  idx=1 final=false  'five\n...\nseventeen\n'       (+103.1 ms  -> the 100 ms throttle)
5509.5 ms  idx=2 final=true   'eighteen\nnineteen\ntwenty'   (+30.5 ms   -> final flush is not throttled)
```

Screen repaints trailed each flush by 3.9–13.4 ms.

Return `{"hookSpecificOutput":{"hookEventName":"MessageDisplay","displayContent":"…"}}` to *replace* the delta on screen (display-only; the stored message and what the model sees are untouched). A read-only hook (empty stdout) leaves the TUI on its `isLive` path and does **not** gate rendering.

**Mode difference:** in `--print` / SDK mode the streaming machinery is bypassed — `MessageDisplay` fires **once per completed assistant message** with `index:0, final:true, delta:<whole text>`. Verified in the print probe.

### 1.5 Firing point relative to the API stream and the transcript write

From the `--print --include-partial-messages --include-hook-events` probe (single merged clock, ms from process spawn):

```
 5302.2  stream_event content_block_start (tool_use/Bash)
 5815.6 .. 6157.3  content_block_delta x13 (input_json_delta)
 6157.6  SDK "assistant" frame (block complete)
 6158.5  hook_started PreToolUse            <-- at content_block_stop, before message_delta
 6158.6  stream_event content_block_stop
 6181.3  PreToolUse hook process main()     (+22.8 ms = fork/exec)
 6211.9  message_delta  / 6212.0 message_stop
 7432.3  transcript: assistant/tool_use hits disk        (+1273.7 ms)
10851.5  hook_started PostToolUse  (process entry 10883.0)  duration_ms=4663
10885.6  SDK "user" frame (tool_result)
10886.3  hook_started PostToolBatch
11277.2  transcript: user/tool_result hits disk          (+391.6 ms)
45244.0  last text delta
45248.8  hook_started MessageDisplay  (process entry 45265.9)
45278.9  SDK "assistant" frame / 45279.0 content_block_stop
46201.8  transcript: assistant/text hits disk            (+922.8 ms)
46427.9  hook_started Stop
46447.2  result frame
```

**Command-hook spawn overhead**, measured as `hook_started` SDK frame → hook process `main()` entry (33 KB static C binary): **15.8, 17.1, 18.8, 22.3, 22.8, 31.5, 32.0 ms → median ≈ 22 ms.**

### 1.6 Hook transport types (settings schema @ ~165233800) — how to beat the 22 ms

| `type` | Shape | Notes for an observer |
|---|---|---|
| `command` | `{command, timeout?, statusMessage?, once?, async?, asyncRewake?, rewakeMessage?, cloud?}` | ~22 ms spawn floor |
| **`http`** | `{url, timeout?, headers?, allowedEnvVars?, statusMessage?, once?}` — **POSTs the hook input JSON to a URL** | **Best for a live observer**: no process spawn, keep-alive to `127.0.0.1`. Must return JSON (empty body = `{}`) |
| `mcp_tool` | `{server, tool, input?}` with `${path}` interpolation from the hook input | |
| `prompt` | LLM-evaluated (`$ARGUMENTS` = hook input JSON), `model?`, `continueOnBlock?` | |
| `agent` | agentic verifier | |

Modifiers available on every type: `if:` condition, **`statusMessage`** (your text appears inside the TUI spinner parens while the hook runs — we observed the built-in `(running stop hook · 15s · ↓ 110 tokens)`), `once`; plus on `command`: `async:true` (background, non-blocking) and `asyncRewake:true` (background, wakes the model on exit code 2).

### 1.7 Hook output contract relevant to an observer (`ede()` @ ~167053000)

```jsonc
{ "continue"?: bool, "suppressOutput"?: bool, "stopReason"?: string,
  "decision"?: "approve" | "block", "systemMessage"?: string, "reason"?: string,
  "terminalSequence"?: string,          // see §3.5
  "hookSpecificOutput"?: { "hookEventName": "...", ... } }
```

Alternative shape `{"async": true, "asyncTimeout"?: number}` to detach.

### 1.8 Hook gotchas (measured / read)

* **Workspace trust gates hooks entirely** (`T5s`: `if (GZ()) { "Skipping … hook execution - workspace trust not accepted"; return }`). `-p` implies trust; interactive needs the dialog accepted once.
* Default timeout constant `Tp = 600000 ms`; `UserPromptSubmit` and model-switch use `Qtt = Ztt = 30000`; `MessageDisplay` uses `10000`.
* `SessionStart` and `Setup` always emit `hook_started` / `hook_response` on the SDK stream; every other event only with `--include-hook-events` (`dIe` @ 170852161).
* `hook_progress` frames poll a running hook's stdout **every 1000 ms** — a streaming sub-channel for long hooks.
* Six events take an extra render-flush await around execution: `PreToolUse, PermissionRequest, UserPromptSubmit, UserPromptExpansion, TaskCompleted, TeammateIdle` (`k5s` @ 175146408).
* `CLAUDE_CODE_HOOKS_SAME_THREAD=1` runs hooks on the main thread instead of the `hooks-worker.js` worker.

---

## 2. Channel 2 — the transcript JSONL

### 2.1 How records are produced

* Path: `~/.claude/projects/<slugified-cwd>/<session_id>.jsonl` (`yf()` @ 175375709). Subagents get their own file (surfaced as `agent_transcript_path`).
* **One record per content block, created at `content_block_stop`** (streaming loop @ 174201500). Each record carries `apiBlockIndex` (the API block index) and its own ISO `timestamp`. There is **no partial write, no placeholder record, no `partial:true` flag** — `apiBlockIndex` exists only so blocks can be re-stitched into one API message on read (`hYs` / `DBr`, option `echoApiBlockOrder`).
* Appends go through `mC()` (@175467315, "appendEntryToFileAsync"): `JSON.stringify(entry) + "\n"` on a **per-path serialized async queue**, plus a sync variant (`vUr`) and a torn-tail-repairing variant (`yBt`). There is no debounce and no size batching *inside the writer* — the latency comes from **when the caller commits the entry**, which is not at block completion.

### 2.2 Measured write lag (record's own `timestamp` → bytes visible on disk, 1 ms `stat` poller)

| Record | run | record `timestamp` (rel. Enter) | visible on disk | **lag** |
|---|---|---|---|---|
| `user` (the prompt) | text2 | 7 ms | 472 ms | **465 ms** |
| `assistant` / `text` ("I'll run the command.") | tui4 | 3 898 ms | 10 585 ms | **6 687 ms** |
| `assistant` / `tool_use` (Bash) | tui4 | 5 603 ms | 10 585 ms | **4 983 ms** |
| `user` / `tool_result` | tui4 | 10 347 ms | 10 585 ms | **239 ms** |
| `assistant` / `text` (final answer) | tui4 | 14 607 ms | 15 029 ms | **423 ms** |
| `assistant` / `text` (20 numbers) | text2 | 5 475 ms | 7 491 ms | **2 016 ms** |
| `system` (turn end) | tui4 | 15 765 ms | 16 491 ms | **726 ms** |
| `assistant` / `tool_use` | print | — | — | **1 274 ms** |

The pattern: entries are appended in **batches at commit points** (a permission prompt going up, a tool result landing, turn end), together with the housekeeping records (`mode`, `permission-mode`, `atis-latch`, `last-prompt`, `ai-title`, `attachment`). In the auto-approved Bash run, the assistant message that *started* the tool did not reach disk until the tool had finished — **~5 s after the row was already on screen**.

### 2.3 Consequences for the observer

* **The transcript is never the earliest channel for anything.** Even `tool_result`, its best case, is 231 ms behind the screen.
* Assistant text frequently lands **after** the `Stop` hook (text1: transcript 4 847 ms vs `Stop` 4 525 ms; text2: 7 491 vs 6 024). A transcript-driven view shows the answer *after* announcing that the turn is over.
* Record `timestamp` ≠ write time; differencing them understates staleness by 0.2–6.7 s.
* File-watch latency is *not* the bottleneck — a 1 ms poll (and equally kqueue/FSEvents) sees the append immediately; the writer is late.
* Record types you must tolerate when tailing: `mode`, `permission-mode`, `atis-latch`, `last-prompt`, `ai-title`, `custom-title`, `file-history-snapshot`, `attachment` (can exceed 100 KB), `pr-link`, `ended-by-model`, `system`, plus `assistant` / `user`.
* `CLAUDE_CODE_CHILD_SESSION` in the inherited environment **silently disables transcript persistence** ("Transcript saving is off — inherited CLAUDE_CODE_CHILD_SESSION marker"). Any observer that spawns Claude Code from inside another Claude Code session must strip it or set `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1`. This bit the first probe run.
* Session file rotation threshold `ULr = 52428800` (50 MB).

---

## 3. Channel 3 — what the TUI emits into the PTY for free

Captured byte-exactly from `pty-tui4.raw` / `pty-perm1.raw` (33.8 KB / 28.3 KB for a full turn).

### 3.1 OSC 0 — window title (the single best free signal)

Wire form: `OSC 0 ; <glyph> <space> <session-title> BEL`. Observed literals (Python repr):

```
b'\x1b]0;\xe2\x9c\xb3 Claude Code\x07'                          # U+2733 idle
b'\x1b]0;\xe2\x97\x90 HELLO-PROBE bash sleep probe\x07'         # U+25D0 busy
b'\x1b]0;\xe2\x97\x91 HELLO-PROBE bash sleep probe\x07'         # U+25D1 busy (alternate)
```

| glyph | meaning | when |
|---|---|---|
| `✳` U+2733 | idle **or waiting for the user** (permission dialog / elicitation) | at ready; at turn end (+8.6 ms after `Stop`); when a permission dialog opens (**9 ms before it paints**) |
| `◐` U+25D0 / `◑` U+25D1 | busy — **alternating every ≈960 ms** | for the whole turn |

* Title text is `Claude Code` until the AI-generated session title lands (~4.2 s into turn 1), then the title; it also updates on `/rename` and from a `sessionTitle` returned by `SessionStart` / `UserPromptSubmit` hooks.
* Turn-start edge: **16–31 ms after Enter** (measured 15.9 / 17.0 / 18.7 / 24.5 / 25.0 / 30.7). Turn-end edge: **+8.6 / +10.4 ms after the `Stop` hook**.
* Kill switch: `CLAUDE_CODE_DISABLE_TERMINAL_TITLE`.
* **`showStatusInTerminalTab`** (gated by the `tengu_terminal_sidebar` feature flag; default off) swaps the title for a *structured* form the bundle itself parses (`fu()` @ 189568974):
  `session running · <name> · <8-hex id> · <chips…>` / `session waiting for a prompt · …` / `session waiting`.
  If that flag can be enabled, the title becomes a first-class structured status channel.

### 3.2 OSC 9;4 — progress

```
b'\x1b]9;4;3;\x07'   -> indeterminate (busy)   : 18.7-30.7 ms after Enter
b'\x1b]9;4;0;\x07'   -> off / idle             : +11.1 / +12.9 ms after Stop
```

Producer (`Kke` @ 190296203): `enabled && (isLoading || hasToolsInProgress || hasPendingBackgroundWork) ? "indeterminate" : "completed"` — only states **3** and **0** were ever emitted; **no percentage is used**. Setting: `terminalProgressBarEnabled`, **default `true`**, described in-bundle as *"Emit OSC 9;4 progress sequences during long operations"*.

Together, OSC 0 + OSC 9;4 give you `idle | busy | needs-input` at ~20 ms with zero configuration changes.

### 3.3 What is *not* emitted

* **No BEL** outside OSC terminators (0 standalone bells in both runs).
* **No alt-screen switch** in the default inline mode (`ESC[?1049h/l` count = 0). Only the fullscreen modes would use it.
* No OSC 777 / OSC 99 / OSC 133 shell-integration marks of its own.
* No cursor-position reports from the app — but it *queries* DA1 (`ESC[c`), DA2, XTVERSION (`ESC[>0q`), kitty-keyboard (`ESC[?u`) and OSC 10/11 at startup. **An observer PTY must answer these or startup stalls** (this was a real failure mode in probe #2).

### 3.4 Frame delimiters — DECSET 2026 (synchronized output)

Every TUI repaint is wrapped in `ESC[?2026h … ESC[?2026l`. **149 pairs for a 16 s turn ≈ 9 frames/s.** This is a free, unambiguous "one atomic screen state" boundary: feed the bytes to a VT parser and snapshot at each `2026l`. That is exactly how the screen timings in this report were taken (`pyte`, 150×45).

### 3.5 Hook-injected terminal sequences — the best available trick

Hook output may include `terminalSequence`, which Claude Code writes to its own tty on your behalf (`Ent` / `Cnt` @ 170851669–170852161):

* Allowed `Ps`: **`{0, 1, 2, 9, 99, 777}`**, plus a bare `BEL`.
* Max **4096 bytes** (`Yho`); payload sanitised to printable (C0/C1/DEL stripped).
* OSC 9 bodies **may not begin with a digit** unless they match `^4;[0-4](;(100|\d{1,2})?)?$` — so `OSC 9;{"e":"tool_start"}` is legal, `OSC 9;123…` is not.
* Terminator BEL or ST; automatically wrapped in tmux/screen DCS passthrough (`$E`).

So an observer that only owns the emulator (no side channel) can still get **structured, framed, in-band events** by registering hooks whose sole output is a `terminalSequence` carrying an OSC 777 JSON payload, then parsing OSC 777 out of the PTY stream. Latency = hook latency (≈22 ms for `command`, less for `http`).

### 3.6 Screen grammar you can regex (if you must scrape)

| Element | Marker | Notes |
|---|---|---|
| assistant / tool row | `⏺ ` (U+23FA) + text | the `⏺` **pulses on/off every ~600 ms while a tool runs** |
| result / continuation | `⎿  ` (U+23BF) | `⎿  Waiting…` → `⎿  Running…` → `⎿  Running… (3s)` → `⎿  <output tail>` |
| background hint | `(ctrl+b to run in background)` | appears under a long-running Bash |
| spinner line | `<glyph> <Verb>… (<Ns> · ↓ <N> tokens)` | glyph cycles `· ✢ ✳ ✶ ✻ ✽` ~110 ms; `Verb` drawn from a ~200-entry `spinnerVerbs` list and **rotates mid-turn**; variants `(running stop hook · …)`, `(compacting …)` |
| turn end | `<Verb> for <Nm Ns> · done <h:mm AM>` | Verb ∈ Baked / Brewed / Churned / Cogitated / Cooked / Crunched / Sautéed / Worked |
| footer | `⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt · ← for agents` | `esc to interrupt` present ⇔ a turn is in flight; `? for shortcuts` ⇔ idle |
| mode | `⏸ manual mode on` / `⏵⏵ auto mode on` / `⏵⏵ bypass permissions` | |
| permission dialog | `Bash command` / `Do you want to proceed?` / `1. Yes` / `2. Yes, and don't ask again…` / `3. Yes, and switch to auto mode` / `4. No` / `Esc to cancel · Tab to amend` | |
| API trouble | `Waiting for API response · will retry in … · check your network`, ` · esc to interrupt` | `CLAUDE_STREAM_FIRST_BYTE_TIMEOUT_MS` tunes the first-byte wait |

**Fidelity warning on `↓ N tokens`:** from `@186740870`, the displayed value is `round(animated(responseLengthChars) / 4)`, where the animation eases toward the true value in ≤50-char steps every 50 ms. It is a *character-count estimate*, not `usage.output_tokens`, and it lags the true value by up to a second.

Layout caveat: rows are re-ordered on final render. In run `tui4` the tool row `⏺ Bash(…)` appeared at 5 612 ms and the *preceding* text `⏺ I'll run the command.` only at 5 716 ms — scraping order ≠ logical order until the message settles.

---

## 4. Channel 4 — SDK `stream-json` (only if the observer owns the process)

If the observer can spawn Claude Code itself rather than attach to a human's TUI, this is strictly the best channel: it is the API stream, unbuffered, on stdout.

```
claude -p --output-format stream-json --include-partial-messages --include-hook-events --verbose
```

Frame types observed: `system[hook_started | hook_response | hook_progress | init | status | task_started | task_notification | compact_boundary | session_state_changed]`, `stream_event{message_start | content_block_start | content_block_delta | content_block_stop | message_delta | message_stop | ping}`, `assistant`, `user`, `result`.
`message_start` carries `ttft_ms`; `result` carries `ttft_ms`, `ttft_stream_ms`, `time_to_request_ms`, `first_content_frame_ms`, `duration_api_ms`, full `usage` / `modelUsage` / cost, `permission_denials`, `subagent_stats`.

Caveat: **interactive TUI sessions do not emit this.** `--include-partial-messages` is explicitly rejected outside `--print` + `stream-json` (`Error: --include-partial-messages requires --print and --output-format=stream-json`).

---

## 5. Undocumented flags & env that change what streams

**Flags** (from `--help` and the arg validator strings):
`--include-partial-messages`, `--include-hook-events`, `--forward-subagent-text` (subagent text/thinking re-emitted as assistant/user messages with `parent_tool_use_id`), `--replay-user-messages`, `--session-mirror` *(SDK-internal: "Emit transcript_mirror frames on stdout")*, `--prompt-suggestions`, `--verbose`, `-d/--debug [filter]` (e.g. `api,hooks` or `!1p,!file`), `--debug-file <path>`, `--setting-sources user,project,local`, `--permission-prompts host|none`, `--no-session-persistence`, `--bare`, `--safe-mode`, `--restricted`.

**Environment** (grepped from the binary's env-name table):

| Var | Effect |
|---|---|
| `CLAUDE_CODE_INCLUDE_PARTIAL_MESSAGES` | env form of `--include-partial-messages` |
| `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS` | adds `{"type":"system","subtype":"session_state_changed","state":"running|idle|requires_action"}` frames |
| `CLAUDE_CODE_EMIT_TOOL_USE_SUMMARIES` | extra tool-use summary frames |
| `CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING` | finer-grained tool-input streaming from the API |
| `CLAUDE_CODE_DISABLE_TERMINAL_TITLE` | kills OSC 0 |
| `CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS` | kills the 6 s `Notification(permission_prompt)` |
| `CLAUDE_CODE_HOOKS_SAME_THREAD` | run hooks on the main thread instead of the hooks worker |
| `CLAUDE_CODE_DISABLE_HOOK_FORWARDING` | stop forwarding hooks to cloud/child sessions |
| `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1` | force transcript writing even under `CLAUDE_CODE_CHILD_SESSION` |
| `CLAUDE_CODE_CHILD_SESSION` | **(inherited)** disables transcript persistence |
| `CLAUDE_CODE_TERMINAL_RECORDING`, `CLAUDE_PTY_RECORD` | built-in PTY recording |
| `CLAUDE_CODE_DEBUG_REPAINTS`, `CLAUDE_CODE_FRAME_TIMING_LOG`, `CLAUDE_CODE_FRAME_TIMING_SAMPLE_EVERY` | ink render instrumentation |
| `CLAUDE_CODE_DEBUG_LOGS_DIR`, `CLAUDE_CODE_DEBUG_LOG_LEVEL`, `CLAUDE_CODE_SESSION_LOG`, `CLAUDE_CODE_DIAGNOSTICS_FILE` | debug log sinks |
| `CLAUDE_CODE_THINKING_DISPLAY_UPDATES` (default on) | whether thinking / connector text is surfaced for display |
| `CLAUDE_CODE_TEE_SDK_STDOUT`, `CLAUDE_CODE_PERFETTO_TRACE` | SDK stdout tee / perfetto trace |
| `CLAUDE_STREAM_FIRST_BYTE_TIMEOUT_MS` | first-byte wait before the "no response from the API" banner |

There is **no** flag or env that makes the transcript file write earlier, and **no** flag that makes `MessageDisplay` flush more often than the hard-coded 100 ms / newline boundary.

---

## 6. Recommended observer architecture

1. **Tier 0 — free, from the emulator, ~20 ms.** Parse `OSC 0` (glyph → `busy | idle | needs-input`; text → session title) and `OSC 9;4;{3,0}` from the PTY. Snapshot the rendered screen at each `ESC[?2026l`. This alone gives correct turn boundaries and an attention flag without touching the user's configuration.
2. **Tier 1 — semantics, ~5–30 ms.** Install an **`http`-type** hook set pointed at a local port for
   `UserPromptSubmit, MessageDisplay, PreToolUse, PermissionRequest, PostToolUse, PostToolBatch, PostToolUseFailure, PermissionDenied, Stop, StopFailure, SubagentStart, SubagentStop, TaskCreated, TaskCompleted, Notification, Elicitation, SessionStart, SessionEnd`.
   Join on `prompt_id` + `tool_use_id` + `message_id`. This yields prompt text, every tool's input *and full response*, assistant text in ≤100 ms line batches, permission requests with suggestions, and turn end with `last_assistant_message`. Hooks need workspace trust; `command` hooks cost ~22 ms of spawn.
3. **Tier 2 — the gaps Tier 1 cannot cover.** `⎿ Running… (Ns)` elapsed time, the spinner phrase, tool output tails *before* the tool finishes, and dialog contents exist **only on screen**. Take them from the Tier-0 screen snapshots (or, if you own the process, from SDK `task_started` / `task_notification`).
4. **Tier 3 — transcript.** Use it only as the durable, ordered record for reconciliation and backfill, never for liveness. Expect 0.23–4.9 s of lag and arrival out of order relative to `Stop`.
5. **If you cannot add a side channel:** have the Tier-1 hooks also return `terminalSequence` with an OSC 777 JSON payload, so the PTY stream itself becomes self-describing (§3.5).

---

## 7. Raw artifacts (all under `<coord-scratch>/realtime/scratch-chanprobe/`)

| File | What |
|---|---|
| `hooktap.c` / `hooktap` | 33 KB C hook logger; timestamps `CLOCK_REALTIME` at `main()` entry and after reading stdin |
| `work/.claude/settings.json` | project settings registering all 33 hook events |
| `ptyprobe.py` | 1 ms transcript watcher + hook-log reader |
| `run_tui2.py`, `run_perm.py`, `run_perm2.py`, `run_text.py` | PTY drivers (pyte screen model, terminal-query responder, dialog state machine) |
| `probe_print.py` | `--print --output-format stream-json` probe with per-frame timestamps |
| `result-tui4.json` | Bash(sleep 4) + 5-line answer, auto mode |
| `result-perm1.json` / `result-perm2.json` | permission dialog answered at 1.2 s / held 9 s (Notification) |
| `result-text1.json` / `result-text2.json` | pure streaming text |
| `result-print.json` | SDK stream reference timeline |
| `pty-*.raw` | raw PTY bytes, `!dI`-framed with wall-clock timestamps |
| `ctx.py`, `hookschema.txt` | binary grep helper; extracted Zod hook-schema region |

**Probe hygiene.** All sessions ran in `<coord-scratch>/realtime/scratch-chanprobe/work` with `--setting-sources project` (user settings not loaded), were killed at the end, and no processes remain (`ps` clean). The repo `<repo>` was never read from, written to, or executed; no deploy or tunnel scripts were run; macOS System Settings were not touched. The relay settings file was only ever passed by path to `--settings`; its contents were not copied anywhere. One user-visible side effect: the workspace-trust and hook-approval dialogs were accepted once for the `/tmp/.../work` scratch directory (which records `hasTrustDialogAccepted` for that path in `~/.claude.json`).
