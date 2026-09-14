# Remuda live structured view — design

**Inputs:** `claude-channels.md` (claude 2.1.270 channel probe, 5 instrumented sessions),
`remuda-pipeline.md` (hop-by-hop trace of the shipped pipeline, 1 live turn),
`harness-parity.md` (9-state × 3-harness matrix + 2 new live defects).
**Design context:** `docs/design/native-pty-first.md` §5/§7/§10 (D-028),
`docs/design/evidence/native-pty-3.md`.
**Repo state read:** `main` @ `6f7dfe8` (c-hookgap merged), worktrees `c-hookgap`, `c-tuiopt`.

**Product constraints this design is bound by.** The structured view must reflect what the
terminal shows with as little lag as possible; the D-028 ladder `Hook > File > OSC > Screen`
decides *authority*; nothing may be shown as done on evidence weaker than the ladder allows;
`claude-print` is retired, so there is no print-mode streaming to fall back on; the
effort / parity / capability vocabulary already exists and is not to be re-invented.

---

## 0. Where the evidence disagrees, and which wins

Four real contradictions across the three reports plus the repo's own evidence. Resolving them
first, because three of the four change what the code should do.

### 0.1 Does `MessageDisplay` lead or trail the transcript? — **claude-channels wins**

| source | claim |
|---|---|
| `native-pty-3.md` §1.4 (claude **2.1.221**) | transcript assistant record at 07:37:07.993, `MessageDisplay` chunk 0 at **+48 ms** → "the hook is an echo, not a preview" |
| `claude-channels.md` §0 row 2c, §2.2 (claude **2.1.270**, 1 ms `stat` poller) | the transcript *append* lands **+401 … +4 883 ms** after the first `MessageDisplay` flush (measured 401 / 576 / 1 477 / 2 115 / 4 883) |

**claude-channels wins**, for a stated methodological reason rather than by recency:
`native-pty-3` compared the transcript record's **own `timestamp` field** against hook receive
time. `claude-channels` §2.3 shows that field is *not* the write time and that differencing the
two "understates staleness by 0.2–6.7 s" — it polled `stat` at 1 ms and measured **bytes visible
on disk**, which is the only thing a tailer can actually read. It also shows assistant text
routinely reaching disk **after** the `Stop` hook (`text1`: transcript 4 847 ms vs `Stop`
4 525 ms; `text2`: 7 491 vs 6 024) — impossible if the hook were an echo of a completed write.
`harness-parity.md` §7 explicitly could **not** re-measure the ordering (defect D-4 suppressed
the transcript in both PTY probes), so `native-pty-3`'s ordering claim is unreplicated on 2.1.270.

**Consequences for the code.**
1. The hook tier is the live tier for text. The transcript is reconciliation, never liveness.
2. `signal_messages.rs` is still right to assume **no ordering** between the two channels
   (`signal_messages.rs:29-37`); do not "simplify" it now that hooks appear to lead.
3. `native-pty-3` §1.3 is **untouched** by this: hook `message_id` (a UUID) and transcript
   `msg_vrtx_*` remain **disjoint id spaces**. Messages cannot be joined by id. Tools can
   (§2.3 below) — that difference is the whole of the flicker design.

### 0.2 Are hooks the earliest channel? — **both are right; the ladder is about authority, not arrival**

`claude-channels.md` §0 measures three rows where a *free* PTY signal beats the hook:

| row | free signal | hook | delta |
|---|---|---|---|
| turn started | `OSC 0` `◐` + `OSC 9;4;3` at **16–31 ms** after Enter | `UserPromptSubmit` 28–50 ms | OSC leads by ~12 ms |
| tool row appears | screen row | `PreToolUse` **+13.7 ms after** the row | hook trails |
| permission dialog | `OSC 0` `✳` **9 ms before** the dialog paints | `PermissionRequest` **+4.5 ms after** it paints | OSC leads by ~14 ms |

But `remuda-pipeline.md` §2 shows the hook beats the channel Remuda *actually uses* by
**seconds**, not milliseconds: the transcript's `assistant/tool_use` record for an
auto-approved Bash did not reach disk until the tool finished — **+4 973 ms after the row was
already on screen** (`claude-channels` §0 row 3c). And `harness-parity.md` D-2 shows the screen
tier is actively *wrong* on 2.1.270.

**Resolution — rule, not a ranking tweak.** The D-028 ladder stays exactly as written for
*authority*. The 10–15 ms in which OSC leads is not worth a second state machine. OSC gets one
job and one job only:

> **OSC may raise `busy`; it may never lower it, and it may never produce content.**

That is safe in both directions: `OSC 9;4;3` at +16–31 ms is the cheapest correct "something
started" and costs nothing if a hook confirms it 15 ms later; `OSC 9;4;0` arrives **+8.6 /
+11.1 ms after `Stop`** (claude-channels §3.2), so the hook always wins the turn-end race anyway
and OSC never needs the authority to end anything.

### 0.3 Does `MessageDisplay` carry thinking? — **claude-channels wins: text only**

`native-pty-first.md` §7 and `harness-parity.md` §4 both record claude's `thinking` row as
**[U]**. `claude-channels.md` §1.4 closes it in both directions: the code path
(`f9n` @188578277, `m9n` @188582541) feeds the streamed buffer **only** from text deltas and the
SDK path filters `type === "text"`; empirically, an assistant message containing a `thinking`
block and a `tool_use` block produced **no** `MessageDisplay` for either.

**Consequence:** claude's `thinking` phase is `unknown` on the hook tier, forever, until someone
finds a channel. Thought content keeps coming from the transcript at file-tier latency
(measured 1.62 s native→browser in `remuda-pipeline` §2). **Do not** promise live thinking for
claude, and **do not** let the spinner phrase stand in for it — the spinner verb is drawn at
random from a ~200-word list and *rotates mid-turn* (claude-channels §3.6), so it carries zero
information about what the model is doing.

### 0.4 "codex has no live stream" — **harness-parity wins, but only as scoped**

`native-pty-first.md` §7 tells the UI to state that codex has no character-level stream.
`harness-parity.md` §0.1 / D-1 shows that is true of the **rollout file** and false of the
**app-server channel**: the installed 0.154 binary carries `item/agentMessage/delta`,
`item/reasoning/textDelta`, `item/commandExecution/outputDelta`, `turn/diff/updated`,
`thread/queue/changed`, and `crates/remuda-codex-wire/src/notification.rs:28-118` already types
every one.

**Resolution:** the honest statement is channel-scoped — *"the rollout file has no deltas; the
app-server channel does, and Remuda does not use it."* But the evidence tag is **[S]**
(strings in the shipped binary); nothing was observed firing. So codex's `text-streaming` /
`tool-output` capability stays **`unknown`**, not `supported`, until measured. **Out of scope
for these batches** — `r-p6-adapters` owns `codex_adapter.rs` / `grok_adapter.rs`. What this
design owes P6 is a *target shape to emit into*, which §2 provides.

### 0.5 One thing every report agrees on, and it is the headline

The structured view is not slow because of a slow channel. It is slow because **the channel is
not connected**:

- `remuda-pipeline.md` §3: `claude-pty` registers exactly **one** hook (`SessionStart`,
  `claude_pty.rs:1297-1390`). The full relay (`launch/overlay.rs:81` — 12 events when the report
  was written, **13 on `main` today**, `SubagentStop` having been added) is started **only** from
  `shell_pty.rs:500`. Measured: **1 hook event in a 108 s turn**.
- `remuda-pipeline.md` §4: `ToolCallState::Running` (`enums.rs:530`), `ResultStage::Partial`
  (`enums.rs:536`) and `SourceChannel::Screen` (`enums.rs:448`) have **zero emitters**
  workspace-wide. `ContentStatus::Streaming` (`enums.rs:505`) comes only from
  `signal_messages.rs:174` (dead for `claude-pty`) and `claude_print_stream.rs:54` (retired).
- `harness-parity.md` D-3: the emulator's retained `OscState` (`grid.rs:38-45`,
  `emulator.rs:173`) has **zero consumers** outside the `lib.rs` re-export.
- `harness-parity.md` D-6: `map.rs:155` deliberately routes every tool hook to
  `MappedKind::Diagnostic` — journaled, changes nothing.

So the cheapest large win is **emitting into vocabulary that already exists**, not adding
vocabulary. That is the spine of §2 and §3.

---

## 1. Latency: measured today vs target, per state

### 1.1 End-to-end, native event → browser WebSocket frame

"Today" is `remuda-pipeline.md` §2 — one `claude-pty` turn on the demo Hub, host load average
61–67, so absolutes are inflated; the *structure* (which hop, and how it scales) is not.
Rows marked ⚠ are load-sensitive.

| # | State | Today: channel | Today: native→browser | Target channel | Target p50 | Target p99 |
|---|---|---|---|---|---|---|
| 1 | prompt accepted | transcript `user` record | **3.09 s** ⚠ | `UserPromptSubmit` hook | **≤ 150 ms** | ≤ 250 ms |
| 2 | thinking | transcript `thinking` | 1.62 s ⚠ | *unchanged* (no claude channel — §0.3) | 1.6 s | — |
| 3 | tool started | transcript `tool_use` | **2.55 s** (open) / 3.05 s (close) ⚠ | `PreToolUse` hook | **≤ 150 ms** | ≤ 250 ms |
| 4 | tool running / elapsed | **none — 20.6 s with 0 journal events** while the TTY pushed 218 frames | **∞** | `PreToolUse` anchor + **browser-local timer** | ≤ 150 ms to first frame, then 1 Hz local, **0 extra wire events** | — |
| 5 | tool output tail (live) | none | **∞** | **still none for claude** — labelled `unknown`, never faked | — | — |
| 6 | tool finished | transcript `tool_result` (its best case) | 0.60 s | `PostToolUse` hook (fires **8.2 ms before** the screen tail) | **≤ 150 ms** | ≤ 250 ms |
| 7 | assistant text | transcript, whole block at once | 0.78 s, **one shot** | `MessageDisplay` fold | **≤ 250 ms to first line**, then the harness's own 30–220 ms cadence | ≤ 350 ms |
| 8 | turn ended | screen-derived `agent_status`, `Completeness::ScreenDerived`, **no `Stop` ever journaled** | (inferred) | `Stop` / `StopFailure` hook | **≤ 150 ms**, `Completeness::Structured` | ≤ 250 ms |
| 9 | blocked on approval | screen poll ≤ 800 ms | ≤ 0.8 s | `PermissionRequest` hook (observe-only until P5) | **≤ 150 ms** | ≤ 250 ms |
| 10 | interrupted | not represented | — | `bus.rs:208-222` already synthesises it — surface it | **≤ 150 ms** | ≤ 250 ms |

### 1.2 How the 150 ms target is composed

| hop | mechanism | cost |
|---|---|---|
| harness → relay `main()` | `command` hook `fork`+`exec` | **22 ms median** (measured 15.8 / 17.1 / 18.8 / 22.3 / 22.8 / 31.5 / 32.0, n=7) |
| relay → `SignalBus` | per-instance UDS, `hook.sock` 0600 | < 1 ms |
| `map_event` + live fold | pure, in-process | **0 ms measured** (`hook_latency.rs`) |
| journal append (Node) | `store.append_driver_observation` | sub-ms (`native-pty-3` §2) |
| Node → Hub | `forward_event` (`runtime_wss.rs:756`) | **1 RTT — after r-p2-createlag** (today: median **2.06 s** first event ⚠, **+0.39 s per extra event**, 6-event batch = **2.8 s**, of which 2.4 s pure serialization) |
| Hub → browser | `broadcast` fan-out (`ws.rs:907-1030`) | **~0** (measured inside the hop above) |

**Two of these are not ours to fix, and saying so is part of the design.**

- The **22 ms hook spawn** is removable: `claude-channels.md` §1.6 documents an undocumented
  **`http` hook type** (`{url, timeout?, headers?, allowedEnvVars?, statusMessage?, once?}`)
  that POSTs the payload with no process spawn. That is a `launch/overlay.rs` change — **owned
  by `c-hookgap`**. Recorded here as the next cheap win, **not** claimed by any batch below.
- The **uplink serialization** is exactly what `r-p2-createlag` was briefed to find
  ("find and fix whatever serialises RPC handling on the runtime link"). `runtime_wss.rs` is
  **theirs**. Every p50 target above assumes their fix lands; §4.2 states the test that makes
  the assumption falsifiable instead of load-bearing.

### 1.3 What we deliberately do **not** try to speed up

- **Transcript poll 300 ms** (`claude_pty.rs:885`, `TranscriptTail::poll`
  `claude_transcript.rs:521`), measured median 230 ms / max 250 ms. Dropping it to 50 ms saves
  ~200 ms on a channel whose *writer* is 239 ms – 6 687 ms late (`claude-channels` §2.2). It is
  not the bottleneck, it touches a contested file, and after §2 the transcript is no longer on
  the liveness path at all. **Skip it.**
- **`↓ N tokens`** on the spinner line. It is `round(animated(responseLengthChars) / 4)`
  (claude-channels §3.6) — a character estimate, eased in ≤50-char steps every 50 ms, *not*
  `usage.output_tokens`. Never surface it as a token count.
- **`Notification(permission_prompt)`**. Hard-coded `RJe = 6000` ms late (claude-channels §0
  row 5c). Useless as a liveness signal; keep it registered as an interaction observation only.

---

## 2. The live layer

### 2.1 Principle: additive, and almost entirely into vocabulary that already exists

`harness-parity.md` §6.1 recommends **no new `ObservationPayload` variant**, and that is right —
a variant is a wire-schema change across Hub, Node, web and `gen:api`, and the protocol crate is
W3-exclusive (D-028 §13 rule ③) with two in-flight batches already queued on it
(`r-effortsync`'s `effortEffective`, `ux-c2`'s `commandId`).

This design goes one step further than §6.1: **three of the four observation kinds need no new
shape at all**, because the shapes exist and have zero emitters (§0.5).

| live kind | carried as | protocol change | source channel | fidelity |
|---|---|---|---|---|
| **`turn.state`** | `ObservationPayload::Lifecycle(Native{topic: Turn, native_name: "turn.live"})`, phase in `related_ids` | **none** (`related_ids` is a free-form `BTreeMap<String,String>`) | `Hook` (claude), later `File`/`Rpc` | `Structured` |
| **`tool.progress` (start)** | existing `ObservationPayload::ToolCall` with **`state: ToolCallState::Running`** (`enums.rs:530`, 0 emitters today) | **none** | `Hook` | `Structured` |
| **`tool.progress` (finish)** | existing `ObservationPayload::ToolResult`, `stage: Final`, `exitCode`, `outcome` | **none** | `Hook` | `Structured` |
| **`tool.progress` (output tail)** | existing `ResultStage::Partial` (`enums.rs:536`, 0 emitters today) | **none** | **no claude channel** — reserved for codex `item/commandExecution/outputDelta` and grok `tool_call_update` at P6 | `Partial` when it exists |
| **`message.delta`** | **already exists** — `signal_messages.rs:121` fold → `open`/`append`/`close` with `ContentStatus::Streaming` | none | `Hook` | `Partial` (line level, and a display echo — see 2.5) |
| **`channel.health`** | **derived in the browser** from the journal + `nativeRef.signalTier` (§2.6) | **none** | — | — |

The only genuinely new Rust surface is `Id::derive` (§2.3), twelve lines in `scalar.rs`, with no
schema change and therefore no `gen:api` diff.

### 2.2 `turn.live` — the phase vocabulary

Exactly the nine values `harness-parity.md` §6.2 specifies, verbatim, so the matrix in that
report is the acceptance table:

```
prompt-accepted · thinking · tool-started · tool-output · tool-finished
text-streaming  · turn-ended · blocked · interrupted
```

Payload (all in `NativeLifecycle`, nothing new on the wire):

```
NativeLifecycle {
  topic:       LifecycleTopic::Turn,
  native_name: "turn.live",
  native_id:   Knowledge<turnId>,       // Unknown when the harness has none
  status:      Knowledge<"working"|"idle"|"waiting">,   // load-bearing spellings, unchanged
  related_ids: {
    "phase":        <one of the nine>,
    "since":        <RFC3339 ms — when this phase started>,   // the elapsed anchor
    "provision":    "native" | "emulated" | "unknown",        // CapabilityProvision, enums.rs:291
    "tier":         "hook" | "file" | "osc" | "screen",       // SignalTier, enums.rs:309
    "toolCallId":   <native tool_use_id>,       // tool-* phases only
    "toolName":     <native name>,
    "messageId":    <hook message_id>,          // text-* phases only
    "chunkIndex":   <u64>, "final": "true"|"false",
    "outcome":      "completed" | "cancelled" | "failed",     // turn-ended only
    "promptId":     <hook prompt_id>,           // the join key, see below
    "ppid":         <agent pid>,                // already stamped by map.rs:67
  },
  severity: Info, affects_completion: false,
}
```

**`prompt_id` is the join key.** `claude-channels.md` §1.2: it is a UUID present on the base
payload of *every* hook event until the next prompt, identical to the OTel `prompt.id`. It
already reaches `related_ids["promptId"]` (`map.rs:77`). It is what lets a projection say
"these four tool calls and this message belong to that prompt" without inventing a turn model.
Caveat recorded in D-028 §14 risk 6 and enforced by test: **several queued prompts share one
`prompt_id`** — it is a grouping key, never a dedupe key.

**Five safety rules** (`harness-parity.md` §6.2, adopted unchanged, each one a test in §4):

1. **Phase is not `Activity`.** `Activity` stays 4-valued (`enums.rs:81`) and is *derived*:
   `prompt-accepted|thinking|tool-*|text-streaming → Working`; `blocked → WaitingInteraction`;
   `turn-ended|interrupted → Idle`. No new state machine in the Hub.
2. **Only `prompt-accepted`, `turn-ended`, `interrupted` may move `Activity`.** This is why
   `map.rs:155` was right to refuse tool events as turn boundaries (D-6); `turn.live` gives them
   a home without giving them that power. `hook_activity()` in `signal.rs` is **not touched**.
3. **`unknown` never collapses to `idle`** (D-028 §10). A phase nobody produced is *absent*;
   the previous phase latches.
4. **Two channels may report the same phase; the higher tier wins and the loser is still
   journaled.** Tie-break is the existing `SourceChannel` order.
5. **A low-information source may not overwrite a high-information value** (protocol §5.2's
   existing rule): a screen-derived `blocked` may not erase a hook-derived `toolName`.

**Plus the two this design adds, both of which fall directly out of §0:**

6. **The screen and OSC tiers may raise `busy`, never lower it** (§0.2). Concretely:
   `OSC 9;4;3` may move `idle → working`; **only** a hook/file event, or the OSC tier *when
   `ChannelHealth` says no higher tier is alive*, may move anything → `idle`. This is the direct
   fix for `harness-parity` D-2, where `screen_status()` returns `Idle` **during a running turn**
   on 2.1.270 because the working phrase it keys on no longer exists.
7. **`SubagentStop` is never turn evidence** (D-028 §3.1 [V]) — it fires with no subagent at all.
   Already honoured at `map.rs:155`'s catch-all arm; the phase mapper must not "fix" that.

### 2.3 `tool.progress`, and how the transcript supersedes without flicker

This is the mechanism that closes the 20.6-second dead window *and* removes the duplicate-node
risk, and it works because tools have something messages do not: **a shared id space**.

| channel | tool identity | message identity |
|---|---|---|
| hook | `tool_use_id` (`PreToolUse` / `PostToolUse`, claude-channels §1.3) | `message_id`, a UUID |
| transcript | the `tool_use` block's `id`; results link back via `sourceToolUseID` | `msg_vrtx_*` |
| joinable? | **yes — same string** | **no** (`native-pty-3` §1.3: `grep -c` the hook's id in the transcript → `0`) |

`crates/remuda-journal/src/claude.rs:20-52` already maps a native id to a stable `obj_` node id
(`NativeIds::tool`, `:45`) — but with `Id::new("obj")`, which is **random**, so the hook path and
the transcript path would allocate two different nodes for the same `tool_use_id`, and the web
would render two cards.

**Fix: make the node id a pure function of the native id.**

```rust
// crates/remuda-protocol/src/scalar.rs — additive, ~12 lines, no schema change
impl Id {
    /// Deterministic identity for a native object: the same `(scope, native)`
    /// always yields the same `Id`, so two channels observing one tool call
    /// converge on one node without sharing state.
    pub fn derive(prefix: &str, scope: &str, native: &str) -> Result<Self, WireValueError>;
}
```

Implementation note: the validator at `scalar.rs:67-90` requires a **canonical lowercase UUIDv7**
(version 7, RFC4122 variant). Build it with `uuid::Builder::from_unix_timestamp_millis(ts, rand)`
where both `ts` (48 bits) and `rand` (10 bytes) come from a BLAKE3 of `(scope, native)`. That is
a legal v7 *layout*; v7's monotonicity is a producer convention and **nothing in the workspace
sorts node ids** (`assemble.ts:74` `newerMutation` orders by `revision`, `compareMessages` by
`revision`/`seq`). A test asserts round-trip through `Id::try_from` and no collision over a
10⁶-seed fixture.

`scope` is the instance id, so node ids never collide across sessions.

**The flicker story, per node type:**

- **Tool nodes — same id, mutation chain, zero flicker.**
  `PreToolUse` → `ToolCall{mutation: open, state: Running, input: known(tool_input)}` on
  `Id::derive("obj", instance, tool_use_id)`.
  `PostToolUse` → `ToolResult{stage: Final, exitCode, outcome}` on the same id, carrying
  `tool_response` and `duration_ms` (claude-channels §1.3 — `duration_ms` excludes permission
  and hook time; label it as tool time, not wall time).
  The transcript's own `tool_use` / `tool_result`, 0.23–5.0 s later, lands on **the same node id**
  with a higher revision and is merged by the *existing* `newerMutation` guard
  (`assemble.ts:74`) — an in-place upgrade, not a second card.
  `ToolCard.tsx:40` already infers `running = !result || result.stage !== "final"`, so the
  running dot lights up with **no web change at all**.
- **Message nodes — two nodes in the journal, one in the projection.**
  Ids are disjoint and positional matching is a guess (`native-pty-3` §1.3 chose "two nodes where
  one is authoritative" for exactly that reason). Keep that on the wire: the journal is the audit
  record and must not lie. Collapse in the **projection** instead: within one `promptId`, a
  transcript-derived assistant message whose whitespace-normalised text has the streamed node's
  text as a **prefix** supersedes it; the streamed node stops rendering, the transcript node
  keeps the streamed node's scroll position. Pure function, new file, one call site (§3.3).
- **Never `replace` a streamed node with a transcript value.** D-028 §7 says
  «transcript 的完整块到达 → replace + close». As `native-pty-3` §1.3 found, that is not
  implementable as written because there is no shared identity. Supersede-in-projection is the
  honest substitute and is recorded here as an amendment to §7.

**What claude cannot do, and must not be made to look like it can.** There is no live tool-output
channel: no hook, no transcript record until the result, no OSC (claude-channels §0 row 4 —
"screen only"). `ResultStage::Partial` therefore stays unemitted for claude. **Do not scrape the
`⎿ <output tail>` row.** That row is *content*, and content from the screen is forbidden by the
product constraint; it is also rendered, wrapped and truncated, and re-ordered on final render
(claude-channels §3.6 layout caveat: in run `tui4` the tool row appeared at 5 612 ms and the text
*preceding* it only at 5 716 ms). The cell reports `unknown` — **not** `unsupported` — because
nobody has shown claude cannot stream it; there is simply no channel we have found
(`harness-parity` §6.3 honesty rule).

### 2.4 The screen / OSC tier: ephemeral status only, never content

**What the screen may contribute:** a busy/idle/needs-input *bit*, and the spinner phrase, and
nothing else. **What it may never contribute:** message text, tool output, tool input, dialog
options, exit codes, token counts.

**Elapsed time is not transported at all.** This is the single most important decision in this
section. `remuda-pipeline.md` §2 measured the failure mode: 20.6 s in which the journal was
silent while the TTY pushed **218 frames (10.6/s)** of spinner + a 1 Hz counter. Streaming that
counter would put ~20 events per tool call on a link that currently costs **0.39 s per extra
event**. Instead:

> `turn.live` carries `since`. The **browser** renders `now − since` at 1 Hz. One wire event per
> phase, zero per second.

The elapsed reading is then honest by construction: it is "how long since the harness told us
this started", anchored to a real event, and it greys out when `ChannelHealth` (§2.6) says the
channel that produced `since` has gone quiet.

**The spinner phrase** (`✽ Zigzagging… (14s · ↓ 103 tokens)`) is carried at most **once per phase
transition**, in `related_ids["phrase"]`, tier `screen`, and is rendered as secondary muted text
next to Remuda's own phase label. It is decoration: the verb is random from a ~200-word list and
rotates mid-turn (claude-channels §3.6). It must never be parsed for meaning. The
`(running stop hook · …)` variant *is* meaningful (a hook is blocking the turn) and is the one
phrase worth a rule.

**The OSC tier finally gets a consumer.** `emulator.rs:173` fills `OscState{title, progress}`,
`grid.rs:38-45` carries it on every `ScreenGrid`, and a workspace-wide grep finds **no reader**
(`harness-parity` D-3). Because `screen_status(grid: &ScreenGrid)` (`signature.rs:63`) already
receives the grid, it can read `grid.osc` with **no signature change and no caller edits** — which
matters, since its callers live in `shell_pty/promotion.rs` (c-hookgap) and `claude_pty.rs`
(r-effortsync). New precedence inside `screen_status`:

```
1. osc.progress starts with "3"  → Working      (raise-only, rule 6)
2. osc.title contains ✳ (U+2733) while osc.progress starts with "3" → Blocked / needs-input
3. BLOCKED_PHRASES                → Blocked      (unchanged, still checked first for dialogs)
4. WORKING_PHRASE                 → Working      (kept for older builds)
5. prompt glyph ❯ and no OSC evidence → Idle
6. otherwise                      → None (unknown — never Idle)
```

Two measured traps this must not fall into:

- **`9;4;3;` has an empty percent field.** `harness-parity` §2.1 saw exactly `9;4;3;`, while the
  emulator's own unit test (`emulator.rs:289`) asserts `"3;0"`. Match on the **state token**, not
  the whole payload.
- **`✳` U+2733 means idle *or* needs-input.** Disambiguate with `OSC 9;4` still being `3`
  (claude-channels §0 row 5). And `⚠ Action Required` flicker drops frames on blur — so
  **`blocked` latches** until a positive idle/working signal (D-028 §10 anchor ④).

Both OSC signals die if the overlay is wrong: `terminalProgressBarEnabled` and
`showStatusInTerminalTab` must stay pinned (D-028 §9.2), and `CLAUDE_CODE_DISABLE_TERMINAL_TITLE`
must stay out of the child env. That is `launch/overlay.rs` — **c-hookgap's file**. §3 records it
as a dependency, not a task.

**The trick we are *not* taking, and why it is worth writing down.** `claude-channels` §3.5: hook
output may include `terminalSequence` (allowed `Ps ∈ {0,1,2,9,99,777}`, ≤4096 B, sanitised), so an
emulator-only observer can make Claude Code inject **OSC 777 JSON event frames into its own PTY**
at hook latency. That is the right answer for an observer that owns *only* the terminal — a
future `remuda attach` to someone else's tmux, or a host where the UDS is unavailable. Remuda
already owns a per-instance UDS with a credential (`hook.sock`, 0600), which is strictly better:
no 4096-byte cap, no printable-only sanitisation, and it can carry a blocking reply for P5
approvals. **Record it as the documented fallback for the no-side-channel case; do not build it now.**

### 2.5 `message.delta` — what already works, and the two honesty constraints

`signal_messages.rs` is correct and stays. Two constraints it must keep advertising:

- **Line level, never token level.** `claude-channels` §1.4 measured the flush machinery:
  `Rn = 100 ms` minimum inter-flush interval, boundary at `raw.lastIndexOf("\n") + 1`, so a
  message with no newline produces **exactly one flush at `finalize()`**. `harness-parity` §3
  measured 3–5 chunks at 30–220 ms for a 20–30 line answer. `Completeness::Partial` is the honest
  tag; the parity whitelist's existing `granularity` rule already covers it (D-028 §12.1).
- **`MessageDisplay` is a display echo, not a model stream.** `harness-parity` §3: in a
  failed-model run the delta was the *client's own* error string
  (`There's an issue with the selected model …`). A consumer that treats it as "assistant tokens"
  puts client diagnostics in the assistant bubble. The streamed node must therefore be
  **supersedable by the transcript** (§2.3) rather than authoritative, and a streamed node that is
  never confirmed by any transcript message within the turn renders with the `Partial` chrome the
  web already has.

`--print` degrades this to one fire per finished message (`claude-channels` §1.4,
`harness-parity` probe A, `native-pty-3` §1.1 — three independent confirmations). With
`claude-print` retired, the only streaming test bed is a **real PTY driving `fake-harness
--kind claude`**, which §4 relies on.

### 2.6 `ChannelHealth` — derived, not transported

`harness-parity` §6.4 proposes one genuinely new field:
`{expected, last_record_at, reason: ok|never-materialised|stalled|disabled}`, motivated by D-4:
an inherited `CLAUDE_CODE_CHILD_SESSION` **silently disables transcript persistence**, so the file
tier looks exactly like an agent with nothing to say.

That motivation is right. The *transport* is not needed: every input is already in the journal.

```
expected(tier)  = tier ∈ the instance's RuntimeCapability / nativeRef.signalTier set
last_record_at  = max(observedAt) over observations with source.channel == tier, this run
reason          = !expected            → "disabled"
                  expected && none     → "never-materialised"
                  expected && stale    → "stalled"      (no record for > 3× that tier's cadence)
                  otherwise            → "ok"
```

Computed in the browser as a pure function of the event list. **No protocol change, no `gen:api`
regeneration, no collision with `r-effortsync` or `ux-c2`.** Promote it to a typed field the next
time W3 opens the protocol (P7), when `signalTier` / `RuntimeCapability` get their real producer.

This is what makes rule 6 (§2.2) decidable: the OSC tier is allowed to lower `busy → idle`
**only** when `hook` reports `never-materialised` or `stalled` — i.e. when there is no higher
authority to defer to. It is also what keeps the D-4 failure visible: the affected session would
have read `file: never-materialised` instead of looking quiet.

### 2.7 Per-harness fidelity on day one

Straight from `harness-parity` §6.3, reproduced so the UI has a table to render and the tests have
an oracle. `provision` / `tier` / `completeness` are all existing vocabulary.

| phase | claude (today, after batch A) | codex (hooks + rollout) | codex (+ app-server) | grok (files) |
|---|---|---|---|---|
| prompt-accepted | native / hook / structured | native / hook / structured | native / rpc / structured | native / hook / structured |
| thinking | **unknown** (§0.3) | unknown | native / rpc / partial | native / file / partial |
| tool-started | native / hook / structured | native / hook / structured | native / rpc / structured | native / file / structured |
| tool-output | **unknown** (no channel — not `unsupported`) | unknown | native / rpc / partial | native / file / partial |
| tool-finished | native / hook / structured | native / hook / structured | native / rpc / structured | native / file / structured |
| text-streaming | native / hook / **partial** (line level, display echo) | unknown | native / rpc / partial | native / file / partial |
| turn-ended | native / hook / structured | native / hook / structured (+`notify`) | native / rpc / structured | native / file / structured |
| blocked | native / hook / structured (answerable at P5) | native / hook / structured | native / rpc / structured | **emulated / screen / screen-derived** (D-028 §14 risk 15) |
| interrupted | emulated / file / partial (text marker) | native / file / structured | native / rpc / structured | native / file / structured |

---

## 3. Code changes by file, with owner boundaries

### 3.0 The contested map (what these batches must not touch)

| in-flight | owns | therefore off-limits |
|---|---|---|
| **c-hookgap** (`wt/c-hookgap/promoted-claude-hook-events`, merged @ `6f7dfe8`, rebase round open) | hook binding for promoted claude | `crates/remuda-driver/src/launch/{overlay,session,shim}.rs`, `promote.rs`, `shell_pty.rs`, `shell_pty/promotion.rs`, `crates/remuda-node/src/signal.rs`, `crates/remuda-node/src/runtime.rs`, `crates/remuda-hub/src/store.rs`, `crates/remuda-driver/tests/launch_shim.rs` |
| **r-p2-createlag** | create ack / reap / runtime-link serialization | `crates/remuda-node/src/runtime.rs`, `crates/remuda-node/src/transport/wss/runtime_wss.rs`, `crates/remuda-driver/src/{binary.rs,shell_pty.rs,shell_pty/lifecycle.rs}`, `crates/remuda-hub/src/{ws.rs,http.rs}` |
| **ux-e** | transcript identity / search | `web/src/features/session/{Transcript.tsx,assemble.ts,assemble.test.ts,TaskTrack.tsx,transcriptSearch.ts}`, `web/tests/e2e/session-virtual.spec.ts` |
| **ux-c2** | local bubble + observation correlation | `web/src/lib/store.ts`, prompt-observation correlation in `crates/remuda-node` (hook ingest + transcript join, `commandId`), `web/src/pages/SessionPage.tsx` *status label*, the LocalBubble component |
| **ux-w** | workflow card | `web/src/features/session/workflow/*`, the `WorkflowCard` branch of `ToolCard.tsx`, `toolPresenters.ts` |
| **r-effortsync** | effort read-back | `crates/remuda-driver/src/claude_pty.rs`, the shell-pty send path, `claude_print.rs`, `crates/remuda-claude-wire/*`, `web/src/features/session/Composer.tsx` effort section, the `store.ts` effort slice |

**Observation that makes this tractable:** `crates/remuda-signal/*` was **not touched** by
c-hookgap (`git show --stat 6f7dfe8`) and is not in any other brief. `crates/remuda-journal/*`,
`crates/remuda-screen/*` and `crates/remuda-protocol/src/scalar.rs` are likewise unclaimed.
That is where the whole Rust side of this design goes.

**The seam that makes it possible with zero `runtime.rs` edits.** `SignalBus::handle`
(`bus.rs:167`) already emits **two** observations for one event — see the synthesized
`interrupted` at `bus.rs:208-222`. The observation pump (`runtime.rs:787-878`) appends whatever
arrives on the channel. So a bus that emits `turn.live` + `ToolCall` + `ToolResult` alongside the
raw lifecycle evidence reaches the journal, the Hub and the browser **without one line changing in
`runtime.rs`**.

**Security boundary that must be preserved.** The Node gates *content* derived from hooks behind
`promoted_hooks.owns_hook(&committed)` (`runtime.rs:909`) so a second `claude` in a promoted
terminal cannot inject text into the wrong instance. Lifecycle evidence is journaled ungated (it
is raw evidence). The bus must therefore apply an equivalent gate **locally** before emitting
anything content-shaped:

```rust
// bus.rs — content requires the agent that owns this bus's session
fn owns(&self, ppid: i32) -> bool { self.binding().is_some_and(|b| b.pid == ppid) }
```

`turn.live` is lifecycle → ungated (consistent with today). `ToolCall` / `ToolResult` /
`message.delta` are content → gated. The Node's gate stays a strict superset (it also checks the
PTY foreground pgid); a test asserts a foreign `ppid` yields lifecycle-only, never content.

### 3.1 Batch A — `r-live-signal` (Rust: the hook tier becomes the live tier)

| file | change | size |
|---|---|---|
| `crates/remuda-signal/src/live.rs` | **new.** `Phase` (9 values), `LiveState` (open tools by `tool_use_id`, current phase + `since`, last `promptId`), `LiveState::observe(&HookEvent, &Mapped) -> Vec<ObservationPayload>`. Pure; no IO, no clock beyond an injected `now`. | ~220 |
| `crates/remuda-signal/src/map.rs` | add `MappedKind::{ToolStarted, ToolFinished, ToolFailed}`; `classify` (`:155`) routes `PreToolUse` / `PostToolUse` / `PostToolUseFailure` / `PostToolBatch` to them with **status unchanged (`"observed"`)** so `hook_activity` and `InteractionRuntime::ingest` see no new turn boundaries. Add `fn phase(&HookEvent, MappedKind) -> Option<Phase>`. `map_event`'s signature and its lifecycle payload are untouched. **Note:** `PostToolUseFailure` is classified here but is **not** in `HOOK_EVENTS` (`overlay.rs:81-97`); registering it is c-hookgap's overlay change, so the arm is inert until then and must not be tested as if it fires. | ~45 |
| `crates/remuda-signal/src/bus.rs` | `handle` (`:167`) calls `live.observe(...)` after the existing `emit`, and emits each returned payload through the existing `build`/`emit` path; add `owns(ppid)`; hold `LiveState` in the existing mutex slot next to `binding`. | ~40 |
| `crates/remuda-protocol/src/scalar.rs` | `Id::derive(prefix, scope, native)` (§2.3). **No schema change** → `gen:api` stays byte-identical. | ~12 |
| `crates/remuda-journal/src/claude.rs` | `NativeIds::{tool,message,thought}` (`:35-60`) take a `scope` and use `Id::derive`, so the transcript path lands on the **same node id** as the hook path for tools. | ~25 |
| `crates/remuda-signal/tests/live_map.rs` | **new.** Payload-level tests on recorded hook JSON. | ~200 |
| `crates/remuda-node/tests/live_latency.rs` | **new.** §4.1 budgets. | ~120 |
| `crates/remuda-node/src/lib.rs` | re-export only if `live::Phase` is needed web-side; otherwise untouched. | ≤2 |

**Touches no file in §3.0.** Delivers rows 1, 3, 4, 6, 8, 9, 10 of the §1.1 table.

### 3.2 Batch B — `r-live-screen` (Rust: OSC tier + the "no premature done" guard + proof)

| file | change | size |
|---|---|---|
| `crates/remuda-screen/src/osc.rs` | **new.** `OscStatus{busy, needs_input, title}` parsed from `OscState`; matches the **state token** of `9;4;…` (empty percent — `harness-parity` §2.1) and the `✳ / ◐ / ◑` glyphs, with the `✳`-is-ambiguous disambiguation (claude-channels §0 row 5). | ~120 |
| `crates/remuda-screen/src/signature.rs` | `screen_status` (`:63`) consults `grid.osc` **first** per §2.4's precedence; `WORKING_PHRASE` (`:48`) demoted to a legacy fallback; **no signature change, no caller edits**. Add the raise-only invariant + the `blocked` latch. | ~60 |
| `crates/remuda-screen/tests/osc_status.rs` | **new.** Golden bytes from `harness-parity` probe C (`0;◐ Claude Code`, `0;✳ …`, `9;4;3;`, `9;4;0;`) and from `claude-channels` §3.1 (`\x1b]0;\xe2\x97\x91 …`). Includes the regression for D-2: a 2.1.270 working row with **zero** occurrences of `interrupt` must not classify as `Idle`. | ~160 |
| `crates/remuda-testing/src/fake_harness/screen.rs` | add a `--dialect-version modern` claude variant that **omits** `esc to interrupt` and emits `OSC 9;4;3;`/`9;4;0;`, reproducing 2.1.270. Existing dialect kept so `golden_screens.rs` stays green. | ~60 |
| `crates/remuda-testing/fixtures/fake-harness/scenarios/live.json` | **new.** 1 tool with `duration_ms: 20000` (the 20.6 s window), 4 text chunks at 120 ms, one approval. | ~40 |
| `crates/remuda-node/tests/live_pipeline.rs` | **new.** Real PTY + fake-harness end-to-end, §4.2/§4.3. | ~260 |
| `docs/design/parity-whitelist.toml` | one rule: `kind="lifecycle"`, `lifecycle="turn"`, `allow-unmatched`, reason citing D-028 §12 (print is retired; `turn.live` has no print-side counterpart) — with the deletion condition written in, per §12.1. | ~6 |
| `docs/design/evidence/live-view-1.md` | the measured run backing §1.1's "target" column. | — |

**Touches no file in §3.0.** `screen_golden.rs` and `golden_screens.rs` stay untouched because the
new dialect is additive.

### 3.3 Batch C — `w-live-view` (web: render it, sequenced after ux-e / ux-w / ux-c2)

| file | change | owner note |
|---|---|---|
| `web/src/features/session/live/phase.ts` | **new.** Pure projection: `Observation[] → LivePhase{phase, since, tier, provision, toolCallId, phrase?}`. | free |
| `web/src/features/session/live/channelHealth.ts` | **new.** §2.6, pure. | free |
| `web/src/features/session/live/useElapsed.ts` | **new.** 1 Hz `requestAnimationFrame`-throttled timer off `since`; pauses on hidden tab; greys when health ≠ `ok`. **The only place elapsed exists.** | free |
| `web/src/features/session/live/LiveStatusStrip.tsx` | **new.** One line above the composer: phase label · elapsed · tier chip · muted spinner phrase · health warning. | free |
| `web/src/features/session/live/*.test.ts(x)` | **new.** §4.4. | free |
| `web/src/features/session/live/live.module.css` | **new.** | free |
| `web/src/pages/SessionPage.tsx` | **one line**: mount `<LiveStatusStrip/>`. | ux-c2 owns the *status label* only — declare the line in the commit body |
| `web/src/features/session/ToolCard.tsx` | **one line** in `BashCard`'s status span (`:40`'s `running` branch) to append `· {elapsed}`. | **sequenced after ux-w merges** |
| `web/src/features/session/assemble.ts` | **one line**: call `supersedeStreamed(nodes)` from `live/supersede.ts` before returning. | **sequenced after ux-e merges**; if ux-e's stable-identity work lands first, this may become zero lines |
| `web/tests/e2e/ux-live-view.spec.ts` | **new.** | free |

**Nothing else in `web/` is touched.** In particular `store.ts`, `Transcript.tsx`,
`assemble.test.ts`, `Composer.tsx`, `TaskTrack.tsx`, `toolPresenters.ts` are not.

### 3.4 Explicit non-tasks (dependencies, recorded so nobody duplicates them)

| item | owner | why not here |
|---|---|---|
| pipeline the Node→Hub uplink (`runtime_wss.rs:756`) | **r-p2-createlag** | the single largest measured latency (2.4 s of a 2.8 s batch); already in their brief |
| `http`-type hook to remove the 22 ms spawn (`launch/overlay.rs:81`) | **c-hookgap** | overlay is theirs; §1.2 records the win |
| register the full relay for `claude-pty` (`claude_pty.rs:1297`) | **c-hookgap** / D-028 P7 | `claude-pty` is a herdr driver being retired; the native path is `shell_pty.rs:500`, already wired |
| transcript poll 300 → 50 ms | nobody — **declined**, §1.3 | ~200 ms off a channel that is 0.24–6.7 s stale by construction |
| codex app-server deltas, grok ACP | **r-p6-adapters** | §0.4; this design gives them the shape to emit into |
| typed `ChannelHealth` on the wire | **W3 at P7** | §2.6 derives it today with no protocol change |

---

## 4. Tests

Two classes, both driven by `fake-harness` through a **real PTY** — the only way to get streaming
at all now that `claude-print` is retired and `-p` is proven not to stream (three independent
confirmations: `claude-channels` §1.4, `harness-parity` probe A, `native-pty-3` §1.1).

### 4.1 Node-side budget, deterministic — `crates/remuda-node/tests/live_latency.rs`

Same shape as the existing `hook_latency.rs` (`BUDGET_MS = 300`), extended to the new kinds.
Clock starts where the relay hands the payload to the bus — the first instant Remuda could
possibly know.

| assertion | budget | why that number |
|---|---|---|
| `PreToolUse` payload → `ToolCall{state: Running}` committed | **≤ 25 ms p99** | the fold is in-process arithmetic; measured 0 ms today for messages |
| `PostToolUse` → `ToolResult{stage: Final}` on the **same node id** | ≤ 25 ms | |
| `MessageDisplay` → readable text | ≤ 25 ms (existing 300 ms budget retained as the outer guard) | keeps `hook_latency.rs` green |
| `UserPromptSubmit` → `turn.live{prompt-accepted}` | ≤ 25 ms | |
| `Stop` → `turn.live{turn-ended, outcome: completed}` | ≤ 25 ms | |
| **no phase emits more than one observation per transition** | exact count | guards the 10 Hz spinner from ever reaching the wire |
| a 20 s tool emits **exactly 2** journal events (start, finish) | exact count | this is the 20.6 s window, priced |

### 4.2 End-to-end, real PTY — `crates/remuda-node/tests/live_pipeline.rs`

Drives `fake-harness --kind claude --dialect-version modern --script live.json` inside a real
`portable-pty`, with `REMUDA_PTY_HOOKS=1 REMUDA_PTY_EMULATOR=1`, and reads the local journal.

| assertion | budget |
|---|---|
| submit → `turn.live{prompt-accepted}` in the journal | **≤ 150 ms p50, ≤ 250 ms p99** |
| harness tool start (from `--events-out`) → `ToolCall{Running}` | ≤ 150 / 250 ms |
| **max gap between consecutive journal events during the 20 s tool** | **≤ 250 ms from tool start; no assertion of liveness thereafter** — the point is that the *first* frame is fast, not that we heartbeat |
| harness `Stop` → `turn.live{turn-ended}` | ≤ 150 / 250 ms |
| first `MessageDisplay` chunk → journal `message{status: streaming}` | ≤ 250 ms |
| **relative link cost**: a 6-event group must arrive within **1.5×** the single-event latency | *relative*, so it survives host load (the 61–67 load average that inflated `remuda-pipeline`'s absolutes) and **fails loudly if `forward_event`'s per-ACK serialization returns** |

The relative assertion is the contract with `r-p2-createlag`: it does not depend on their fix
landing to be meaningful, and it converts §1.2's assumption into a falsifiable guard.

### 4.3 Correctness — no premature done (the rules from §2.2 as tests)

| test | asserts |
|---|---|
| `screen_says_idle_mid_turn_does_not_end_the_turn` | with the `modern` dialect (no `esc to interrupt`), `screen_status` never yields `Idle` while `9;4;3;` is on. **This is `harness-parity` D-2 as a regression test.** |
| `osc_idle_alone_never_ends_a_turn_while_hooks_are_healthy` | `9;4;0;` with no `Stop` and `ChannelHealth.hook == ok` leaves `Activity` at `Working` (rule 6) |
| `osc_idle_may_end_a_turn_when_the_hook_tier_never_materialised` | the same bytes with `hook == never-materialised` **do** yield idle — the escape hatch is real and conditional |
| `a_tool_without_a_result_is_never_rendered_done` | `ToolCall{Running}` + no `ToolResult` → `diffState` `unknown`, no exit code, running dot (`ToolCard.tsx:40`) |
| `subagent_stop_is_not_turn_evidence` | `SubagentStop` with no subagent leaves phase untouched (D-028 §3.1) |
| `unknown_never_collapses_to_idle` | a phase nobody produced is absent; the previous phase latches |
| `blocked_latches_across_a_dropped_frame` | the flickering `⚠ Action Required` case (D-028 §10 anchor ④) |
| `a_foreign_ppid_produces_lifecycle_only` | the §3.0 content gate: no `ToolCall`, no message, from an unbound pid |
| `the_transcript_upgrades_the_tool_node_in_place` | hook `ToolCall` then transcript `tool_use`/`tool_result` on the same `tool_use_id` → **one** node, revisions increasing, no second card |
| `two_prompts_sharing_a_prompt_id_stay_two_turns` | D-028 §14 risk 6 — `promptId` groups, never dedupes |
| `derived_ids_round_trip_and_do_not_collide` | `Id::derive` output parses via `Id::try_from` (v7 + RFC4122 + canonical lowercase, `scalar.rs:67-90`); 10⁶ seeds, zero collisions |

### 4.4 Web — `web/src/features/session/live/*.test.ts` + `ux-live-view.spec.ts`

| test | asserts |
|---|---|
| `elapsed_is_derived_not_transported` | the fixture contains **one** `turn.live` for a 20 s tool; the strip renders 20 distinct second values |
| `elapsed_greys_when_the_channel_stalls` | health `stalled` → muted, and the label stops claiming freshness |
| `never_materialised_is_not_a_quiet_agent` | zero `transcript`-channel observations + expected `file` tier → an explicit note, not silence. **This is D-4 made visible.** |
| `the_screen_tier_contributes_no_text` | a fixture with screen-derived observations produces **zero** message/tool-result text nodes |
| `the_streamed_node_is_superseded_without_reflow` | streamed text then a transcript message with that prefix → one bubble, scroll position unchanged, node count decreases by one |
| `the_live_region_never_receives_streaming_text` | `Transcript` root stays `aria-live="off"` — **keeps ux-c1's existing assertion green** |
| e2e `ux-live-view.spec.ts` | fake node + fake-harness: tool card shows `running · 0:0N` within 1 s of the harness's tool start, flips to `exit 0` on `PostToolUse`, and **never** shows an exit code before one exists |

### 4.5 Parity gate

`remuda journal diff` (`crates/remuda/src/cmd/journal_diff.rs`) must stay green. Exactly **one**
new whitelist rule (§3.2), with a written deletion condition, because §12.1 makes an *expanding*
whitelist itself a regression signal. Since `claude-print` is retired, the gate's long-term role
shifts from `print` vs `pty` to `pty@N` vs `pty@N+1`; note that in the rule's `reason`.

---

## 5. Phased plan

Three batches. A and B are file-disjoint from each other and from everything in §3.0, so they run
in parallel. C is web-only and **sequenced** behind ux-e / ux-w / ux-c2 rather than overlapped —
sequence, don't collide.

### Batch A — `r-live-signal`: the hook tier becomes the live tier

> You are `r-live-signal`. Worktree on branch `wt/r-live/hook-live-layer` from `origin/main`
> (which already contains c-hookgap's promoted-claude hook binding). **Read FIRST:**
> `<coord-scratch>/realtime/design.md` §0, §2.2, §2.3, §3.1; `docs/design/native-pty-first.md`
> §4.3 and §7; `crates/remuda-signal/src/{map,bus}.rs`; `crates/remuda-node/src/signal_messages.rs`;
> `crates/remuda-journal/src/claude.rs:18-60`. **The measured problem:** for 20.6 s of a 108 s turn
> the journal emitted **zero** events while the TTY pushed 218 frames, because `ToolCallState::Running`
> (`enums.rs:530`) and `ResultStage::Partial` (`enums.rs:536`) have zero emitters and `map.rs:155`
> routes every tool hook to `Diagnostic`. **OWNERSHIP (exclusive):**
> `crates/remuda-signal/src/{live.rs (new),map.rs,bus.rs}`,
> `crates/remuda-signal/tests/live_map.rs (new)`, `crates/remuda-protocol/src/scalar.rs`
> (`Id::derive` **only** — no schema change, `gen:api` must stay byte-identical),
> `crates/remuda-journal/src/claude.rs`, `crates/remuda-node/tests/live_latency.rs (new)`.
> **Do NOT touch** `crates/remuda-node/src/{runtime.rs,signal.rs}` (c-hookgap + r-p2-createlag),
> `crates/remuda-driver/src/{shell_pty.rs,promote.rs,claude_pty.rs,launch/*}`,
> `crates/remuda-hub/*`, or anything under `web/`. **TASK:** emit `turn.live` (9 phases, `since`,
> `provision`/`tier`/`completeness` tags) plus real `ToolCall{state: Running}` / `ToolResult` from the
> existing `SignalBus` second-emit seam (`bus.rs:208-222` is the precedent); gate content — not
> lifecycle — behind `owns(ppid)` so a foreign `claude` in a promoted terminal cannot inject; make
> tool node ids deterministic so the transcript upgrades the hook's node in place instead of drawing
> a second card. `Activity` derivation and `hook_activity()` are **unchanged**: only
> `prompt-accepted` / `turn-ended` / `interrupted` may move it. **Acceptance:** (1) a 20 s tool
> produces exactly 2 journal events, the first ≤ 25 ms after the `PreToolUse` payload reaches the
> bus; (2) hook and transcript converge on one `toolCallId` node — asserted on the recorded
> transcript fixture; (3) `hook_latency.rs` still green; (4) an unbound `ppid` yields lifecycle-only;
> (5) `cargo test --workspace` green and `git diff --exit-code -- web/src/lib/api.generated.ts`
> clean. Checks: `cargo fmt --all`, `clippy --workspace --all-targets -- -D warnings`,
> `cargo test --workspace --locked`, `./scripts/ci/secret-scan.sh`.

### Batch B — `r-live-screen`: the OSC tier tells the truth, and the budgets are measured

> You are `r-live-screen`. Branch `wt/r-live/osc-tier-and-budgets` from `origin/main`. **Read
> FIRST:** `design.md` §0.2, §2.4, §4; `docs/design/native-pty-first.md` §10 (the five anchors —
> simplifying any of them is a regression); `docs/design/testing-fake-harness.md`;
> `crates/remuda-screen/src/{signature.rs,grid.rs,emulator.rs}`. **The measured problem, two live
> defects:** `signature.rs:48` keys `Working` off `"esc to interrupt"`, which occurs **zero times**
> in a 22,521-byte claude 2.1.270 PTY capture — so the screen tier reports `Idle` **during a running
> turn**; and the emulator's retained `OscState` (`emulator.rs:173`, `grid.rs:38-45`) has **zero
> consumers** workspace-wide, although claude emits `9;4;3;` / `9;4;0;` turn boundaries the whole
> time. **OWNERSHIP (exclusive):** `crates/remuda-screen/src/{osc.rs (new),signature.rs}`,
> `crates/remuda-screen/tests/osc_status.rs (new)`, the `modern` claude dialect in
> `crates/remuda-testing/src/fake_harness/screen.rs` + `fixtures/fake-harness/scenarios/live.json`,
> `crates/remuda-node/tests/live_pipeline.rs (new)`, one rule in `docs/design/parity-whitelist.toml`,
> `docs/design/evidence/live-view-1.md`. **Do NOT touch** `screen_status`'s signature (its callers
> live in c-hookgap's and r-effortsync's files), the existing fake-harness dialect or
> `golden_screens.rs` goldens (add, never edit), or anything in §3.0. **TASK:** read `grid.osc`
> inside `screen_status` with the §2.4 precedence; match the **state token** of `9;4;…` because
> claude sends `9;4;3;` with an **empty** percent and the emulator's own test asserts `"3;0"`;
> implement raise-only (OSC may set busy, never clear it while the hook tier is healthy) and the
> `blocked` latch; add a `modern` fake-claude dialect with no `esc to interrupt`; then measure the
> §1.1 target column end-to-end and write it up. **Acceptance:** (1) the D-2 regression test fails
> on `main` and passes on the branch; (2) `9;4;0;` with a healthy hook tier does **not** idle the
> instance, and **does** when the hook tier never materialised; (3) `live_pipeline.rs` meets the
> §4.2 budgets **and** the relative 1.5× link assertion; (4) `remuda journal diff` green with exactly
> one new whitelist rule carrying a deletion condition; (5) evidence doc contains the real numbers,
> not this design's targets. Checks as batch A, plus `cargo test -p remuda-testing --locked`.

### Batch C — `w-live-view`: render it, and never render more than we know

> You are `w-live-view`. Branch `wt/w-live/live-status-view`, **cut after ux-e, ux-w and ux-c2 have
> merged** — this batch is sequenced, not parallel, because the three files it needs one line from
> are theirs. **Read FIRST:** `design.md` §2.4, §2.6, §3.3, §4.4; `web/src/features/session/assemble.ts`
> and `ToolCard.tsx:40` **as merged**; `docs/design/workbench-ux-plan.md` §0 (file ownership).
> **The measured problem:** the structured view has no way to show that a tool is running, because
> elapsed time and the spinner exist only as TTY bytes — and the naive fix, streaming the 1 Hz
> counter, would put ~20 events per tool call on a link that measured **+0.39 s per extra event**.
> **OWNERSHIP (exclusive):** a new `web/src/features/session/live/` directory only
> (`phase.ts`, `channelHealth.ts`, `useElapsed.ts`, `supersede.ts`, `LiveStatusStrip.tsx`,
> `live.module.css`, their tests) plus `web/tests/e2e/ux-live-view.spec.ts`. **Exactly three
> one-line edits outside it**, each declared in the commit body: mount the strip in
> `SessionPage.tsx`; append `· {elapsed}` to `BashCard`'s running status in `ToolCard.tsx`; call
> `supersedeStreamed` in `assemble.ts`. **Do NOT touch** `store.ts`, `Transcript.tsx`,
> `assemble.test.ts`, `Composer.tsx`, `TaskTrack.tsx`, `toolPresenters.ts`, `workflow/*`, or
> `styles/ui.module.css`. **TASK:** elapsed is computed **in the browser** from `turn.live`'s `since`
> — it is never on the wire; derive `ChannelHealth` from the journal so a channel that never
> materialised (the silent `CLAUDE_CODE_CHILD_SESSION` failure) is distinguishable from a quiet
> agent; render the spinner phrase as muted decoration only, never as state; collapse the streamed
> message node into the transcript's authoritative one in the projection without reflow. **Acceptance:**
> (1) a fixture with **one** `turn.live` for a 20 s tool renders 20 distinct second values;
> (2) zero text nodes originate from a screen-derived observation; (3) no exit code is ever shown
> before a `ToolResult` exists; (4) `never-materialised` renders an explicit note; (5) the
> `aria-live="off"` assertion ux-c1 shipped stays green; (6) evidence doc with 1440/768/390
> screenshots from the synthetic fixture. Checks: `pnpm --dir web lint/typecheck/test`,
> `gen:api` diff clean, `test:e2e:hub` once under `flock`, secret-scan.

---

## 6. What this design refuses to do

- **Scrape the tool-output tail off the screen.** It is content; content from the screen is
  forbidden. It is also wrapped, truncated and re-ordered on final render (claude-channels §3.6).
  claude's `tool-output` cell stays `unknown` — not `unsupported`.
- **Treat the spinner verb or `↓ N tokens` as information.** Random from ~200 words, rotating
  mid-turn; the token number is `round(chars/4)`, eased, not `usage.output_tokens`.
- **Stream elapsed time.** One event per phase, a local timer, and a health flag that greys it.
- **Add an `ObservationPayload` variant.** Three of the four live kinds already have zero-emitter
  shapes waiting for a producer.
- **Promise live thinking for claude.** `MessageDisplay` is text-only, proven in code and by
  experiment (§0.3).
- **Claim codex parity from `strings` output.** [S] is not [V]; the app-server deltas stay
  `unknown` until someone watches one fire (§0.4).
- **Let `Stop` mean "the text has arrived".** Assistant text lands on disk **after** `Stop` in two
  of two measured runs. The transcript reconciles; it never gates the turn end.
