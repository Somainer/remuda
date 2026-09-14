# Real-time intermediate state across claude / codex / grok

Author: coordinator sub-agent "parity".
Date: 2026-09-15 local.
Repo read (read-only, no writes, no commits, no demo scripts):
`<repo>` at `eefcbba`
(`merge: wt/ux-c1/status-vocabulary-and-notify into main`).
Installed binaries observed: `codex-cli 0.154.0`, `grok 1.0.30 (04b7ffed98c6)`,
`claude 2.1.270`.

Evidence tags used below:

- **[M]** measured by me in this session (commands and artifacts under
  `<coord-scratch>/realtime/scratch-parity/`).
- **[V]** verified by an existing repo evidence file (cited); I did not re-run it.
- **[S]** source/binary-string evidence only — the symbol exists in the installed
  build, but I did not see it fire.
- **[U]** unverified; stated as a gap, never as a capability.

Nothing here claims a capability from absence of evidence, and no number is
reported that I did not either measure or cite.

---

## 0. Executive summary

1. **The three harnesses are not asymmetric in the way D-028 §7 currently says.**
   §7 records codex as "item level, completed only, no per-token stream". That is
   true of the **rollout file**, and false of the **app-server channel**: codex
   0.154 ships `item/agentMessage/delta`, `item/reasoning/textDelta`,
   `item/commandExecution/outputDelta`, `turn/diff/updated`, `item/started`,
   `thread/queue/changed` as server notifications **[S]**, and
   `crates/remuda-codex-wire/src/notification.rs` already types every one of them.
   The crate is in the workspace but is a dependency of **no** driver.
2. **Every harness has a push channel for turn boundaries and tool boundaries.**
   claude: hooks (wired). codex: hooks — the installed binary's `hooks.json`
   event set is `PreToolUse, PermissionRequest, PostToolUse, PreCompact,
   PostCompact, SessionStart, SessionEnd, UserPromptSubmit, SubagentStart,
   SubagentStop, Stop, Interrupt` **[S]** — plus `notify` for turn completion
   **[V]**. grok: hooks, but with a **documented hole** (`PermissionRequest` is
   silently dropped) **[V]**. Only **assistant-text streaming** is genuinely
   asymmetric, and even there codex has a channel Remuda is not using.
3. **What Remuda ingests today is one harness deep.** The hook socket, the
   `MessageDisplay` fold and the screen signature are claude-only in practice;
   `codex_rollout.rs` / `grok_session.rs` are standalone parsers with no
   `SignalAdapter`, no driver, and no caller outside their own tests.
4. **Two live defects found while probing** (both new, neither in the evidence
   files): the screen tier's `WORKING_PHRASE = "esc to interrupt"` does not occur
   **anywhere** in a real claude 2.1.270 PTY turn **[M]**; and the emulator's
   retained `OscState` has **zero consumers** in the whole workspace, so the OSC
   tier that D-028 §4.2 ranks third is captured and thrown away.
5. **Recommendation**: do not add a new payload type. Add one
   `lifecycle.native` sub-shape — a **`turn.live` observation** — with a fixed
   9-value `phase` vocabulary, and reuse the three vocabularies the protocol
   already has (`SourceChannel`, `Completeness`, `SignalTier` /
   `CapabilityProvision`) as the fidelity tags. Details in §6.

---

## 1. What I ran (and what I did not)

Live probes, all in `<coord-scratch>/realtime/scratch-parity/`, all killed:

| # | Command shape | Purpose |
|---|---|---|
| A | `claude --settings ~/.claude/settings.relay.json --settings ./probe-settings.json --model sonnet -p "Reply with exactly: PROBE_OK"` | headless hook baseline |
| B | same flags, interactive, driven through a 45x120 `pty.openpty()` (`ptyprobe.py`) | streaming granularity |
| C | same, plus a real `Bash` tool call (`ptyprobe2.py`) | tool-boundary hooks + OSC capture |

`probe-settings.json` registers a throwaway `hook.sh` for `SessionStart,
UserPromptSubmit, MessageDisplay, PreToolUse, PostToolUse, Stop, Notification,
SessionEnd, PermissionRequest`; the script logs `{t, event, payload-subset}` and
prints `{}` (never a decision). The relay settings file was passed by path only;
its contents were never read, copied or logged.

Static (read-only) inspection of installed binaries: `strings` over
`@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex` and over
`~/.local/share/claude/versions/2.1.270`. No config was modified, no macOS
setting touched, no deploy/tunnel tooling run, and `codex` / `grok` were never
started.

Not run: codex or grok turns (they need a model endpoint and the spike harness
the repo already has). All codex/grok runtime claims below are **[V]** from
`docs/design/evidence/codex-signals-1.md` / `grok-signals-1.md`, or **[S]**.

---

## 2. Channel inventory per harness

### 2.1 claude 2.1.270

| Tier | Channel | Status |
|---|---|---|
| Hook | 16 event names present in the binary, incl. `MessageDisplay`, `PostToolBatch`, `StopFailure`, `Elicitation`, `PermissionRequest`, `SubagentStart` **[S]**; `SessionStart / UserPromptSubmit / PreToolUse / PostToolUse / MessageDisplay / Stop / SessionEnd` all observed firing **[M]** | wired (`remuda-signal`), **off by default** (`REMUDA_PTY_HOOKS`, `crates/remuda-node/src/signal.rs:28`) |
| File | transcript JSONL, `~/.claude/projects/<enc cwd>/<uuid>.jsonl`; queue ledger `queue-operation`, `attachment.queued_command` **[V]** (`claude-queue-steer-1.md`) | tailed at 800 ms (`shell_pty.rs:83`) |
| OSC | `OSC 0;<spinner> <session title>`, `OSC 9;4;3;` at turn start, `OSC 9;4;0;` at end, `OSC 8` session hyperlink **[M]** | captured (`emulator.rs:61`), **never read** |
| Screen | vt100 grid via `remuda-screen` | `screen_status()` only; see defect D-2 |

Exact OSC payloads observed in probe C (22,521 raw bytes):
`0;◐ Claude Code`, `0;◑ Bash tool probe`, `0;✳ Bash tool probe`, `9;4;3;`
(x1), `9;4;0;` (x5), `8;id=…;https://claude.ai/code/session_…`.
Note `9;4;3;` has an **empty** percent field — a rule that expects `3;0` misses it.

### 2.2 codex 0.154

| Tier | Channel | Status |
|---|---|---|
| Hook | `hooks.json` event set `PreToolUse, PermissionRequest, PostToolUse, PreCompact, PostCompact, SessionStart, SessionEnd, UserPromptSubmit, SubagentStart, SubagentStop, Stop, Interrupt` **[S]**; `PermissionRequest` blocks and returns a real allow/deny **[V]** (`codex-signals-1.md` A1); trust = per-handler canonical hash written to `hooks.state` **[V]** | not wired at all |
| notify | argv-JSON spawn, `agent-turn-complete` only, fire-and-forget **[V]** (A2) | not wired |
| RPC (app-server) | `turn/started`, `turn/completed`, `turn/diff/updated`, `turn/plan/updated`, `item/started`, `item/completed`, `item/agentMessage/delta`, `item/reasoning/textDelta`, `item/reasoning/summaryTextDelta`, `item/commandExecution/outputDelta`, `command/exec/outputDelta`, `process/outputDelta`, `process/exited`, `item/fileChange/outputDelta`, `item/fileChange/patchUpdated`, `item/mcpToolCall/progress`, `hook/started`, `hook/completed`, `thread/queue/changed`, `thread/tokenUsage/updated` **[S]** | **types already exist** in `crates/remuda-codex-wire/src/notification.rs:28-118`; crate consumed by nobody (`grep remuda-codex-wire */Cargo.toml` → workspace manifest only) |
| File | rollout JSONL: `task_started`, `task_complete`, `item_completed`, `response_item`, `token_usage_record`, `turn_aborted`, `turn_context`; **no text deltas in a complete 97-record inventory** **[V]** (A3) | parser only (`codex_rollout.rs`), no adapter |
| OSC | **[U]** — never probed | — |

The direct answer to "which events stream during a turn, and their latency vs
the rollout file": the deltas (`agent_message`, `reasoning`, `commandExecution
output`, `turn_diff`) exist **only** on the app-server channel; the rollout
carries the same content **once, completed**, with `started_at_ms` /
`completed_at_ms` inside `item_completed`. So the rollout's lag is not a
transport delay you can shave — it is a **different granularity**: for a
`sleep 20` tool the rollout has nothing at all for 20 s, while
`item/commandExecution/outputDelta` would be emitting throughout **[S]**.

### 2.3 grok 1.0.30

| Tier | Channel | Status |
|---|---|---|
| Hook | 16 accepted event names incl. `PreToolUse` (deny/ask works, even under `--always-approve`); **`PermissionRequest` is silently ignored** **[V]** (`grok-signals-1.md` A3) | not wired |
| File | `updates.jsonl` = persisted ACP frames (`user_message_chunk`, `agent_thought_chunk`, `agent_message_chunk`, `tool_call`, `tool_call_update`, `hook_execution`, `turn_completed{stop_reason,elapsed_ms}`); `events.jsonl` = `turn_started`, `phase_changed`, `first_token`, `turn_ended{outcome,cancellation_*}`, `permission_requested`, `permission_resolved{wait_ms}`; `usage.json`; `active_sessions.json` **[V]** (A1/A2/A5) | parsers only (`grok_session.rs`), no adapter |
| RPC (ACP stdio) | `remuda-acp-wire` has a **live client** (`client.rs`, `spawn.rs` spawns `grok agent … stdio`) and classifies every `session/update` kind | `remuda-driver` imports only the **classifier** (`grok_session.rs:11`); the live client is unused |
| OSC | `OSC 0` title carrying `⚠ Action Required` / `Thinking` / `Running: <tool>`, `OSC 9;4;1;-1` and `9;4;0;0`, 72 deduped samples **[V]** (A4) | not wired |
| Screen | footer strings differ per state; `[stop]` chip is the anchor, not the braille glyph **[V]** | `remuda-screen` has no grok rules (`signature.rs` is claude-only) |

grok is the only harness whose **tool start** is a first-class persisted record
(`tool_call` lands at invocation, `tool_call_update` on progress/completion —
confirmed in the repo fixture `tests/fixtures/grok/tui-updates.jsonl`: `tool_call`
at 1789326032, `tool_call_update{completed}` at 1789326047).

---

## 3. Measured claude numbers (new, 2.1.270)

Probe B — interactive PTY, "Print the numbers 1 to 30, one per line":

```
   0.000  SessionStart
   8.370  UserPromptSubmit
  13.530  MessageDisplay  idx=0 final=false  "1\n2\n3\n4\n"
  13.632  MessageDisplay  idx=1 final=false  (+102 ms)
  13.726  MessageDisplay  idx=2 final=false  (+ 94 ms)
  13.917  MessageDisplay  idx=3 final=false  (+191 ms)
  14.124  MessageDisplay  idx=4 final=TRUE   (+207 ms)
  14.169  Stop                                (+ 45 ms after the final chunk)
```

Probe C — interactive PTY, forced `Bash` tool call then 20 lines:

```
   0.000  SessionStart
   6.313  UserPromptSubmit
  11.771  PreToolUse    tool=Bash
  16.099  PostToolUse   tool=Bash        (4.33 s after PreToolUse for `echo`)
  19.750  MessageDisplay idx=0 final=false
  19.974  MessageDisplay idx=1 final=false (+224 ms)
  20.006  MessageDisplay idx=2 final=TRUE  (+ 32 ms)
  20.032  Stop                              (+ 26 ms)
  47.725  SessionEnd    (on /quit)
```

Probe A — headless `-p`: a **single** `MessageDisplay` with `final:true`, `Stop`
365 ms later. This reproduces `docs/design/evidence/native-pty-3.md` §1.1 on a
newer build: **`-p` cannot be used to test streaming granularity**.

Conclusions that matter for the design:

- `MessageDisplay` chunk cadence in a real PTY is **~30–220 ms**, 3–5 chunks for
  a 20–30 line answer. "Line level, not token level" (§7) is still the honest
  claim; the chunks are whole lines and sometimes many lines at once.
- `index` is a chunk counter on one `message_id` — `signal_messages.rs` already
  documents and folds it this way (`crates/remuda-node/src/signal_messages.rs:45-60`).
- **`MessageDisplay` echoes non-model text too.** In the first (failed-model) run
  the delta was the *client's own* error string
  (`There's an issue with the selected model …`). A consumer that treats
  `MessageDisplay` as "assistant tokens" will put client diagnostics into the
  assistant bubble. It is a display echo, not a model stream.
- **Two `--settings` files merge.** Probe A ran with the relay settings *and* my
  hook settings and both took effect (relay model config applied, my hooks
  fired), and the user's own global `SessionEnd` hook fired as well **[M]**.
  `crates/remuda-driver/src/launch/shim.rs:270-273` currently **bails out of the
  overlay entirely** when the user's command line already contains `--settings`;
  that conservatism appears to be unnecessary on 2.1.270 and costs the
  Unification Principle a whole session's worth of signal.

---

## 4. The matrix

Latency column: "push" = the harness spawns/sends at the moment of the event;
"poll ≤800 ms" = whatever the channel's freshness, Remuda's own promotion poller
(`crates/remuda-driver/src/shell_pty.rs:83`) bounds it.

### prompt accepted

| harness | best channel | latency | ingested today |
|---|---|---|---|
| claude | Hook `UserPromptSubmit` **[M]** | push; fires **at enqueue**, not at delivery **[V]** (`claude-queue-steer-1.md`, +53–91 ms) | **Yes** — `map.rs:159` → `MappedKind::TurnStarted`/status `working`; `node/signal.rs:111` → `Activity::Working`. Gated off by default |
| codex | Hook `UserPromptSubmit` **[S]** | push | **No** — no codex hook path; `overlay.rs:242 shadow_home()` returns a path and writes nothing; `shim.rs:281 passthrough_shim()` |
| grok | Hook `UserPromptSubmit` **[V]** (payload fixture exists) | push | **No**. File fallback `user_message_chunk` is parsed (`grok_session.rs:60`) but unwired, and it **cannot** prove enqueue time **[V]** |

### thinking / reasoning

| harness | best channel | latency | ingested today |
|---|---|---|---|
| claude | none dedicated. `MessageDisplay` for thinking blocks is **[U]**; transcript `thinking` records are authoritative but late | poll ≤800 ms | **Partial** — transcript mapper emits thought blocks; no live reasoning delta |
| codex | RPC `item/reasoning/textDelta` + `summaryTextDelta` **[S]**, typed at `notification.rs:76,79` | push | **No**. Rollout has `response_item/reasoning` **completed only** **[V]** |
| grok | File `agent_thought_chunk` in `updates.jsonl` **[V]**, parsed at `grok_session.rs:47` | file append (unmeasured **[U]**) | **No** (parser unwired) |

### tool started

| harness | best channel | latency | ingested today |
|---|---|---|---|
| claude | Hook `PreToolUse` (with `tool_use_id`, `tool_input`) **[M]** | push | **No, not as a tool event** — `map.rs:204` sends every tool hook to `MappedKind::Diagnostic` / status `observed`, deliberately ("tool events … are not turn boundaries"). It is journaled, it changes nothing |
| codex | Hook `PreToolUse` **[S]**; RPC `item/started` **[S]** | push | **No** |
| grok | File `tool_call` frame **[V]** (fixture-confirmed), or Hook `PreToolUse` **[V]** | file append **[U]** / push | **No** |

### tool output tail (live stdout while the tool runs)

| harness | best channel | latency | ingested today |
|---|---|---|---|
| claude | **none** — no hook, no transcript record until the result. Screen only | poll ≤800 ms, screen-derived | **No** |
| codex | RPC `item/commandExecution/outputDelta` / `command/exec/outputDelta` / `process/outputDelta` **[S]**, typed at `notification.rs:82` | push | **No**. Rollout has `aggregated_output` **only inside the completed item** **[V]** — nothing during a 20 s command |
| grok | ACP `tool_call_update` frames carry content before completion **[V]** (fixture: an update with content precedes the `completed` one) | file append **[U]** | **No** |

### tool finished

| harness | best channel | latency | ingested today |
|---|---|---|---|
| claude | Hook `PostToolUse` (`tool_response`, `duration_ms`) **[M]** | push (probe C: 4.33 s after `PreToolUse` for `echo`; that gap is the harness's, not ours) | **No as an event** (`Diagnostic`); the transcript's `toolUseResult` is mapped later at ≤800 ms |
| codex | Hook `PostToolUse` **[S]**; file `item_completed{CommandExecution, exit_code, duration}` **[V]** | push / append | **No** |
| grok | File `tool_call_update{status:"completed", rawOutput.exit_code}` **[V]**; Hook `PostToolUse` **[V]** | append / push | **No** |

### assistant text streaming

| harness | best channel | latency | ingested today |
|---|---|---|---|
| claude | Hook `MessageDisplay` | 30–220 ms per chunk **[M]**; fold to journal message <1 ms (`crates/remuda-node/tests/hook_latency.rs`, budget 300 ms) | **Yes** — `signal_messages.rs:65 message_delta()` + `:121 MessageAssembler::fold()` → §5.2 `open/append/close` |
| codex | RPC `item/agentMessage/delta` **[S]**, typed at `notification.rs:73` | push | **No** — and D-028 §7 currently tells the UI to say "this harness has no live text", which is only true of the file channel |
| grok | File `agent_message_chunk` **[V]** | append **[U]**; chunk granularity only — two SSE fragments landed as **one** record **[V]** | **No** |

### turn ended

| harness | best channel | latency | ingested today |
|---|---|---|---|
| claude | Hook `Stop` / `StopFailure` | push, 26–45 ms after the last text chunk **[M]** | **Yes** — `map.rs:166,174` → `TurnEnded`; `node/signal.rs:114` → `Activity::Idle` |
| codex | Hook `Stop` **[S]**; `notify agent-turn-complete` **[V]** (completed turns only; title-generation child threads notify too — filter `thread-id`); file `event_msg/task_complete` **[V]** | push / append | **No** |
| grok | File `events.jsonl turn_ended{outcome}` **[V]** — authoritative, because `turn_completed` is **also** emitted for cancelled turns **[V]** | append **[U]** | **No** |

### blocked on approval

| harness | best channel | latency | ingested today |
|---|---|---|---|
| claude | Hook `PermissionRequest` (blocking, `{"behavior":…}`) | push, blocks the agent | **Observed only** — registered (`event.rs:136`), marked blocking (`event.rs:155`), and the bus answers `{}` (`bus.rs:107`). Status `waiting` → `Activity::WaitingInteraction` (`node/signal.rs:115`). Screen path: `ScreenStatus::Blocked` → `agent_status` with `Completeness::ScreenDerived` (`promotion.rs:743-752`), ≤800 ms |
| codex | Hook `PermissionRequest` — **verified blocking allow/deny** **[V]**; stdin has **no `tool_use_id`** and the tool name is normalised to `Bash`, so the `PreToolUse` correlation schema must not be reused **[V]** | push, blocks | **No** |
| grok | **Screen only** **[V]** — `PermissionRequest` is not a registrable event, `PreToolUse` can `deny`/`ask` but cannot answer, and the ACP `session/request_permission` RPC is **not** persisted to `updates.jsonl` (54/54 frames are `session/update`) **[V]**. `events.jsonl permission_requested/resolved` is post-hoc reconciliation only (`wait_ms:0` does not mean "did not block") | ≤800 ms screen | **No** — and `shell_pty.rs:1151 respond_interaction()` answers only the transcript picker |

### interrupted

| harness | best channel | latency | ingested today |
|---|---|---|---|
| claude | **no hook** — transcript text `[Request interrupted by user(, for tool use)]` + errored `tool_result`; no `PostToolUseFailure` observed **[V]** | poll ≤800 ms | **No** as a distinct state; the turn simply stops looking working |
| codex | Hook `Interrupt` **[S]** (name present in the `hooks.json` event set); file `event_msg/turn_aborted{reason:"interrupted"}` **[V]** — and **an already-started tool can still complete afterwards under the old `turn_id`** **[V]** | push / append | **No** |
| grok | File `turn_ended{outcome:"cancelled", cancellation_context.trigger}` **[V]**; `Esc` is **not** the interrupt key, it takes two `Ctrl+C` **[V]** | append **[U]** | **No** |

### Cross-cutting: what is actually live in the product today

`REMUDA_PTY_HOOKS` (`node/signal.rs:28`) and `REMUDA_PTY_EMULATOR`
(`shell_pty.rs:70`) are **both off by default**. With both off, the only
intermediate state any harness produces is `screen_status()` over an
ANSI-stripped byte tail, every 800 ms, with claude-only phrases — see D-2.

---

## 5. Defects and gaps found while doing this

**D-1 (new, high): the codex "no live stream" claim is channel-scoped, not
harness-scoped.** `docs/design/native-pty-first.md` §7 instructs the UI to state
that codex has no character-level stream. The binary exposes
`item/agentMessage/delta` and friends **[S]** and `remuda-codex-wire` already
parses them. The honest statement is "the rollout file has no deltas; the
app-server channel does, and Remuda does not use it".

**D-2 (new, high): the screen tier's claude working-phrase is gone.**
`crates/remuda-screen/src/signature.rs:48` keys `Working` off the literal
`"esc to interrupt"`. In probe C's full 22,521-byte PTY capture the substring
`interrupt` occurs **zero times** **[M]**; the working row renders as
`✢ Grooving…` / `✻ Nucleating…`, ending with `✻ Cogitated for 13s · done 12:17 AM`.
With the prompt glyph `❯` on screen throughout, `screen_status()` therefore
returns `Idle` **during a running turn** on 2.1.270. The OSC channel had the
answer the whole time (`9;4;3;` on, `9;4;0;` off) — see D-3. This is exactly the
failure mode §10 anchor ⑤ warns about, one binary version later.

**D-3 (new, medium): OSC is captured and never read.** `emulator.rs:61` fills
`OscState{title, progress}` and `grid.rs:40` carries it, but a workspace-wide
grep for `OscState` / `.osc()` finds **only the re-export in `lib.rs:39`**.
Tier 3 of `Hook > File > OSC > Screen` is dead weight today. Also: claude emits
`9;4;3;` with an **empty** percent, so a matcher expecting `3;0` (as in the
emulator's own unit test) would miss it.

**D-4 (new, medium): `CLAUDE_CODE_CHILD_SESSION` silently kills the file tier.**
Both interactive probes inherited that marker from my own Claude Code session and
the child TUI printed
`⚠ Transcript saving is off — inherited CLAUDE_CODE_CHILD_SESSION marker · restart with CLAUDE_CODE_FORCE_SESSION_PERSISTENCE…` **[M]**,
and wrote **no** transcript JSONL — during the turn, 25 s after `Stop`, or after
a clean `/quit` (exit 0). Headless runs in the same cwd wrote theirs normally.
Remuda is protected today by the `env_clear` + allowlist in
`child_env.rs:25` / `shell_pty.rs:1298`, but (a) that protection is invisible —
nothing asserts it, and the var is not in the `DENY` list at `child_env.rs:50`,
so it survives only because it is not in `INHERIT`; (b) the failure is
**silent on the file channel and visible only on the screen channel**, which is
a strong argument for the per-channel health field proposed in §6.

**D-5 (structural): three finished parsers, zero adapters.**
`codex_rollout.rs` (tail + `locate_rollout`), `grok_session.rs` (two tails +
registry discovery), `remuda-codex-wire` (full notification enum),
`remuda-acp-wire` (full live ACP client incl. `spawn.rs` for
`grok agent … stdio`) all exist and are tested; none is reachable from a driver.
P6 is the missing wire, not missing knowledge.

**D-6 (minor): claude tool hooks are deliberately inert.** `map.rs:204` routes
`PreToolUse`/`PostToolUse`/`PostToolBatch` to `Diagnostic`. That was the right
call for "do not fake turn boundaries", but it means the one harness with a
wired push channel still shows tool activity only via the 800 ms transcript
poll. The `turn.live` phase vocabulary in §6 is what lets these become
`tool-started` / `tool-finished` without ever being mistaken for turn boundaries.

---

## 6. Recommendation: the `turn.live` observation

### 6.1 Shape

Do **not** add an `ObservationPayload` variant (that is a wire-schema change
across Hub, Node and web). Emit an ordinary
`ObservationPayload::Lifecycle(LifecyclePayload::Native(NativeLifecycle{…}))`
with `topic: LifecycleTopic::Turn` and a fixed `native_name: "turn.live"`, and
put the new vocabulary in fields that already exist:

```
NativeLifecycle {
  topic:        Turn,
  native_name:  "turn.live",
  native_id:    Knowledge<turnId>,        // Unknown when the harness has none
  status:       Knowledge<Phase>,         // the 9 values below, verbatim
  related_ids: {
     "phase":        <one of the 9>,      // duplicated here for projections
     "provision":    "native|emulated|unknown",
     "tier":         "hook|file|osc|screen",     // = SourceChannel of this obs
     "toolCallId":   <native id>,         // tool-* phases only
     "toolName":     <native name>,
     "messageId":    <native id>,         // text-* phases only
     "chunkIndex":   <u64>,
     "final":        "true|false",
     "outcome":      "completed|cancelled|failed",  // turn-ended only
     "latencyClass": "push|append|poll",
  },
  severity, affects_completion: false,
}
```

plus the envelope Remuda already stamps: `source.channel: SourceChannel`,
`completeness: Completeness`.

### 6.2 The phase vocabulary (exactly the nine states asked for)

`prompt-accepted`, `thinking`, `tool-started`, `tool-output`, `tool-finished`,
`text-streaming`, `turn-ended`, `blocked`, `interrupted`.

Rules that make it safe:

1. **Phase is not Activity.** `Activity` stays 4-valued
   (`enums.rs:81`) and is derived: `prompt-accepted|thinking|tool-*|text-streaming
   → Working`; `blocked → WaitingInteraction`; `turn-ended|interrupted → Idle`.
   No new state machine in the Hub.
2. **Only `prompt-accepted`, `turn-ended`, `interrupted` may move `Activity`.**
   Everything else is *intra-turn detail*, which is precisely why `map.rs:204`
   was right to refuse tool events as boundaries; `turn.live` gives them a home
   without giving them that power.
3. **`unknown` never collapses to `idle`** (D-028 §10) — a phase that no channel
   produced is simply absent, and the previous phase latches.
4. **Two channels may report the same phase; the higher tier wins, and the loser
   is still journaled.** Tie-break is the existing `Hook > File > OSC > Screen`
   order, which is already the `SourceChannel` enum (`enums.rs:448`).
5. **A low-information source must not overwrite a high-information value**
   (protocol §5.2's existing rule): a screen-derived `blocked` may not erase a
   hook-derived `toolName`.

### 6.3 Fidelity tags (D-028 §6 / §4.3 vocabulary, nothing new)

Per-phase, per-harness, three orthogonal tags — all three already exist in the
protocol, which is the point:

| tag | values | source of truth | meaning |
|---|---|---|---|
| `provision` | `native` / `emulated` / `unknown` | `CapabilityProvision`, `docs/design/protocol.md:478` | who produced the semantics: the harness, or Remuda inferring it |
| `tier` | `hook` / `file` / `osc` / `screen` (/`none`) | `SignalTier`, `protocol.md:81`; `SourceChannel`, `enums.rs:448` | which channel this instance actually got |
| `completeness` | `structured` / `partial` / `screen-derived` / `opaque` | `Completeness`, `enums.rs:441` | how much of the truth this record carries |

Worked assignments (what each harness would report on day one):

| phase | claude | codex (hooks+rollout) | codex (+app-server) | grok (files) |
|---|---|---|---|---|
| prompt-accepted | native/hook/structured | native/hook/structured | native/rpc/structured | native/hook/structured |
| thinking | unknown | unknown | native/rpc/partial | native/file/partial |
| tool-started | native/hook/structured | native/hook/structured | native/rpc/structured | native/file/structured |
| tool-output | **unknown** (no channel) | unknown | native/rpc/partial | native/file/partial |
| tool-finished | native/hook/structured | native/hook/structured | native/rpc/structured | native/file/structured |
| text-streaming | native/hook/**partial** (line level, display echo) | unknown | native/rpc/partial | native/file/partial (chunk, not token) |
| turn-ended | native/hook/structured | native/hook/structured (+`notify`) | native/rpc/structured | native/file/structured (`turn_ended.outcome`) |
| blocked | native/hook/structured (answerable at P5) | native/hook/structured (answerable) | native/rpc/structured | **emulated/screen/screen-derived** (D-028 §14 risk 15) |
| interrupted | emulated/file/partial (text marker) | native/file/structured (`turn_aborted`) | native/rpc/structured | native/file/structured |

Two honesty rules fall straight out of this table and must be enforced in the UI:

- `tool-output` for claude is `unknown`, **not** `unsupported`. Nobody has shown
  claude cannot stream tool output; there is simply no channel we have found.
- grok's `blocked` is `emulated/screen`: the screen is the only answer path, and
  `events.jsonl permission_resolved` is reconciliation, not a transport.

### 6.4 Adapter contract (one trait, three implementations)

`remuda-signal` already owns the top tier; give it the seam for the other three:

```
trait LiveTurnSource {
    fn poll(&mut self) -> Vec<TurnLive>;      // file tails, OSC, screen
    fn on_push(&mut self, ev: HookEvent) -> Option<TurnLive>;  // hook socket
    fn tier(&self) -> SignalTier;
    fn health(&self) -> ChannelHealth;        // see below
}
```

- claude: `MessageDisplay`→`text-streaming`, `PreToolUse`→`tool-started`,
  `PostToolUse`→`tool-finished`, `UserPromptSubmit`→`prompt-accepted`,
  `Stop|StopFailure`→`turn-ended`, `PermissionRequest|Elicitation|Notification`
  →`blocked`. This is a ~30-line change to `map.rs:148-210` plus a `phase` field;
  the existing `MappedKind` stays as-is so nothing downstream breaks.
- codex: `RolloutTail` (`codex_rollout.rs:265`) covers `prompt-accepted`,
  `tool-finished`, `turn-ended`, `interrupted` **today**; the hook overlay adds
  push versions and `blocked`; the app-server client (already typed) upgrades
  `thinking`, `tool-output`, `text-streaming` from `unknown` to `native/rpc`.
- grok: `SessionTail` over both files (`grok_session.rs:341`) covers everything
  except `blocked`, which stays screen-derived by construction.

**`ChannelHealth` is the one genuinely new field I would add**, because of D-4:
a channel that was expected and is producing nothing must be distinguishable
from a quiet agent. `{expected: bool, last_record_at: Option<Timestamp>,
reason: "ok"|"never-materialised"|"stalled"|"disabled"}`. Had this existed, the
transcript-saving-off session would have reported `file: never-materialised`
instead of looking like an agent that had nothing to say.

### 6.5 What this buys, concretely

- One UI component renders intermediate state for all three harnesses; the
  differences show up as tags, not as per-harness code paths.
- `signalTier` / `RuntimeCapability` (`protocol.md:81-93`) get a real producer:
  the tier a session reports is the max tier that produced a `turn.live` this run.
- The `emulated` cases stay visible, as D-028 §6 requires: grok's `blocked` is
  labelled screen-derived, so "approve" in the web UI can honestly say it will
  press a key rather than return a decision.
- D-2 stops being fatal: when the screen rules drift, the phase still arrives
  from the hook or file tier and the screen tier just reports lower confidence.

### 6.6 Sequencing (smallest first)

1. `phase` + the three tags on the existing claude hook path (`map.rs`), plus
   `Activity` derivation unchanged. No new wire types. Half a day.
2. Read `grid.osc` in `screen_status()` and add an OSC-derived phase
   (`9;4;3`→working, `9;4;0`→idle; grok's title carries `Running: <tool>` and
   `⚠ Action Required`). Fixes D-2 and D-3 together; still one crate.
3. `LiveTurnSource` + the codex/grok file tails wired to the existing parsers
   (P6 as scoped, now with a target shape to emit into).
4. Only then decide whether codex's app-server channel is worth a second
   carrier; `remuda-codex-wire` means the parsing cost is already paid.

---

## 7. Things this report deliberately does not claim

- No codex or grok process was started; every runtime statement about them is
  cited to the repo's own spike evidence or marked **[S]**/**[U]**.
- `[S]` string extraction proves a symbol is in the shipped binary. It does not
  prove the event is registrable in `hooks.json`, that it fires, or what its
  payload looks like. `Stop` and `Interrupt` appear in codex's PascalCase event
  list; I did not find their snake_case twins in the same blob, so even their
  canonical hash spelling is unconfirmed.
- The 4.33 s `PreToolUse`→`PostToolUse` gap for `echo` in probe C is reported as
  observed. I did not isolate whether it is shell-snapshot setup, permission
  handling, or model round-trip.
- I could not re-measure "transcript record precedes `MessageDisplay` by 48 ms"
  (`native-pty-3.md` §1.4) because D-4 suppressed the transcript entirely in my
  PTY probes. That ordering remains **[V]** from the repo, not **[M]**.
- File-append latency for codex rollout and grok `updates.jsonl` is **[U]** in
  both the repo evidence and here. Any adapter that promises a freshness budget
  for the file tier must measure it first; note grok's `updates.jsonl`
  timestamps are whole seconds and cannot support a sub-second claim.

## 8. Artifacts

- `<coord-scratch>/realtime/scratch-parity/hooks.log`, `hooks2.log` — hook
  timelines for probes B and C.
- `<coord-scratch>/realtime/scratch-parity/pty-out.txt`, `pty-out2.txt` — raw
  PTY captures (the OSC and working-row evidence).
- `<coord-scratch>/realtime/scratch-parity/ptyprobe.py`, `ptyprobe2.py`,
  `hook.sh`, `hook2.sh`, `probe-settings*.json` — the apparatus.
- Probe claude sessions live in
  `~/.claude/projects/-private-tmp-remuda-coord-realtime-scratch-parity-work/`
  (three headless transcripts; the two PTY sessions wrote none — D-4).
- No repo file was modified; `git status` in the checkout is unchanged apart from
  the pre-existing untracked `target-*` directories.
