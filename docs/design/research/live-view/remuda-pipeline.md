# Remuda structured-view latency: where the time actually goes

**Scope** — read-only trace of `<repo>`
at `eefcbba` (origin/main), plus one live measured turn on the demo Hub
(`http://127.0.0.1:18080`, Node env `REMUDA_PTY_EMULATOR=1 REMUDA_PTY_HOOKS=1`).
Repo was unmodified (verified `git status --porcelain` identical before/after).
All instances created during this work were deleted (`DELETE ...?force=1`).

## 0. Headline

Three findings dominate everything else, and two of them are not timers at all:

1. **The `claude-pty` driver registers exactly one hook — `SessionStart`.** The
   12-event relay (`MessageDisplay`, `PreToolUse`, `PostToolUse`, `Stop`, …) is
   built by `launch/overlay.rs` but is only ever started from `shell_pty.rs:476`.
   So the D-028 P3 delta-folding in `signal_messages.rs` **never executes for the
   driver the demo runs**, and `REMUDA_PTY_HOOKS=1` / `REMUDA_PTY_EMULATOR=1`
   are both inert for `claude-pty`. Measured: 1 hook event in a 108-second turn.
2. **The Node→Hub uplink is serialized one event per ACK round trip.** It is not
   batched and not timer-limited; it simply awaits each `journal.append` before
   sending the next. Measured **+0.39 s median per extra event in the same
   batch** — a 6-event transcript batch took **2.8 s** to fully reach the browser.
3. **A running tool has no journal representation.** Between the tool call and
   the tool result the journal emitted **0 events across 20.6 s**, while the TTY
   channel pushed **218 frames (10.6/s)** of spinner + elapsed-seconds counter.

## 1. Method

- One `claude` session, `kind=claude driver=claude-pty`, title `realtime-probe`,
  model `gemini-3.8-flash-tiered` (the demo gateway rejects `claude-fable-5.1`
  with `API Error: 400`), `permissionMode=bypassPermissions`.
- Prompt: one `Bash` call running `for i in 1 2 3 4; do echo tick-$i-$(date +%s.%N); sleep 5; done`
  (20 s, periodic echo), then a one-word reply.
- Three simultaneous observers, all timestamped against one wall clock `t0`:
  - `GET /v1/instances/{id}/journal?afterSeq=N` every 250 ms;
  - a `/v1/follow?instanceId=…&tty=1` WebSocket in its own process, recording
    every pushed journal event **and** every binary TTY frame;
  - the Node-side ground truth: each journal event's own `observedAt`, and the
    native transcript JSONL's own per-record `timestamp`.
- Host load average was **61–67** throughout (many other worktrees running). Absolute
  numbers are inflated; the *structure* (which hop, and how it scales per event)
  is not. Load-sensitive rows are marked ⚠.

**There is no HTTP tty snapshot endpoint.** The complete Hub route list
(`grep '\.route(' crates/remuda-hub/src/`) has no `screen`/`snapshot`/`tty` path.
The only terminal surface is the `/v1/follow?tty=1` WebSocket
(`crates/remuda-hub/src/tty.rs:8`), which replays a snapshot via a `tty.attach`
RPC to the Node (`crates/remuda-hub/src/ws.rs:1058`) and then streams live binary
frames. I used that.

## 2. Hop-by-hop table

| # | Hop | Mechanism + code location | Constant | Measured |
|---|---|---|---|---|
| 1 | Claude writes transcript JSONL | native, outside Remuda | — | reference point |
| 2 | Transcript → Observation | **polling**, no notify/fsevents. `spawn_transcript_pump` `crates/remuda-driver/src/claude_pty.rs:894-961`; reader `TranscriptTail::poll` `crates/remuda-driver/src/claude_transcript.rs:521-546` (open + `read_to_end` from byte offset, split on `\n`, keep partial) | `TRANSCRIPT_POLL = 300 ms` `claude_pty.rs:885` | **median 230 ms, max 250 ms** (5 records) |
| 2b | Mapper flush | `mapper.flush()` appended after every batch, `claude_pty.rs:936` | none (per tick) | folded into hop 2 |
| 3 | SessionStart meta file watcher | separate 200 ms poll of `launch/session-meta.json`, `claude_pty.rs:840,879` | `200 ms` | fires once/session |
| 4 | Hook relay → Observation | `launch/overlay.rs:82-99` (12 events) → UDS `hook.sock` → `signal.rs` | no timer (event-driven) | **NEVER RUNS for `claude-pty`** — see §3 |
| 5 | MessageDisplay delta fold | `crates/remuda-node/src/signal_messages.rs:118-178`, driven from `runtime.rs:849-857` | none (in-process) | **0 invocations** (no input) |
| 6 | Observation → local journal | `spawn_observation_pump` `crates/remuda-node/src/runtime.rs:787-878`; `store.append_driver_observation`. Unbuffered, one at a time | none | sub-ms (per `native-pty-3.md` §2) |
| 7 | Local journal → Hub (**the big one**) | `pump_live` `crates/remuda-node/src/transport/wss/runtime_wss.rs:711-753` is broadcast-woken (no tick); `forward_event` `:756-790` does `journal.append_seq(...).await` — **one request, awaits its ACK, then the next** | `retry` interval `250 ms` `:727` only on replay; no batch size | **first event of batch: median 2.06 s ⚠ (min 0.16 s); each extra event: +0.39 s median, +0.61 s max** |
| 7' | (other transport, not used here) | `crates/remuda-node/src/daemon.rs:497` `interval(50 ms)`; `forward_journals` `:625-658` caps in-flight at **16** and shares that budget across all instances | `50 ms` tick, `16` window | n/a (dev uses WSS, `crates/remuda/src/cmd/dev.rs:174`) |
| 8 | Hub ingest + fan-out | `crates/remuda-hub/src/ws.rs:518-525`: `append_journal` then `publish_journal(&state.bus, …)` — **no batching, no timer** | — | included in hop 7 |
| 9 | Hub → browser push | **WebSocket**, `follow_session` `crates/remuda-hub/src/ws.rs:907-1030`, `broadcast::Receiver` forwarded immediately; overflow → `resync_after_gap` | channel cap `follow_buffer_events = 256` `crates/remuda-hub/src/config.rs:119` | ~0 (measured inside hop 7) |
| 10 | Browser journal client | `web/src/lib/store.ts:392-480` — full history page-read, then `api.eventsSubscribe` → real `WebSocket` `web/src/lib/api.ts:1410` | no poll | n/a |
| 10' | Browser instance-list poll | `web/src/lib/store.ts:308` `setInterval(…, 2000)` — list/hosts only, not transcript | `2000 ms` | affects status chips only |
| 11 | Assemble + render | `web/src/features/session/assemble.ts:160+`, `Transcript.tsx` | none | n/a |
| — | HTTP journal polling (alternative to 9) | `GET /v1/instances/{id}/journal` `crates/remuda-hub/src/instances.rs:22` | client-chosen | **+0.35 s median over WS**; endpoint RTT median 0.44 s / max 1.52 s ⚠ |
| — | TTY path (parallel, much faster) | `spawn_tty_pump` `claude_pty.rs:967+` → `handle_tty_binary` `ws.rs:244` → binary WS frames | none | **10.6 frames/s sustained, sub-second** |

### Measured turn timeline (seconds from `t0` = instance create)

| Event | native (transcript ts) | Node journal `observedAt` | browser (WS) | native→browser |
|---|---|---|---|---|
| prompt record | 39.79 | 40.02 | 42.88 | 3.09 s |
| thinking | 65.40 | 65.65 | 67.02 | 1.62 s |
| **tool_call (Bash)** | 65.42 | 65.65 | 67.97 / 68.47 (open/close) | **2.55 / 3.05 s** |
| *(tool running)* | — | **nothing** | **nothing** | **20.6 s gap** |
| **tool_result** | 86.08 | 86.22 | 86.68 | **0.60 s** |
| final text | 107.56 | 107.75 | 108.34 | 0.78 s |
| turn end (`agent_status` idle, `channel: herdr`) | — | 108.17 | 108.80 | — |

Turn end is **not** a `Stop` hook — no `Stop` was ever journaled. The only
end-of-turn signal is a screen-derived `agent_status` observation on
`channel: "herdr"` (`claude_pty.rs:1061`, `Completeness::ScreenDerived`).

## 3. Why the hook channel is empty

The demo's `claude-pty` driver writes its own settings overlay with a single
hook. Live instance `launch/settings.json` contained exactly:

```json
"hooks": { "SessionStart": [ { "hooks": [ { "type": "command",
  "command": "'…/launch/session-start.sh' '…/launch/session-meta.json'" } ] } ] }
```

and there was **no `hook.sock`** in the instance directory.

- Written by `inject_session_start_hook` / `merge_session_start_hook`,
  `crates/remuda-driver/src/claude_pty.rs:1297-1390` — `SessionStart` only. Its
  payload is reduced to `{session_id, transcript_path, cwd, hook_event_name}` by
  `launch/session-start.sh`; its sole job is to name the transcript file.
- The full relay lives in `crates/remuda-driver/src/launch/overlay.rs:82-99`
  (`HOOK_EVENTS` = SessionStart, UserPromptSubmit, Stop, StopFailure, SessionEnd,
  Notification, PreToolUse, PostToolUse, PostToolBatch, **MessageDisplay**,
  PermissionRequest, Elicitation) and is started by `HookSession::start`, whose
  **only** caller is `crates/remuda-driver/src/shell_pty.rs:476`.
- `REMUDA_PTY_HOOKS` is consumed at `shell_pty.rs:142`; `REMUDA_PTY_EMULATOR` is
  read at exactly one site, `shell_pty.rs:198` (`emulator_enabled()`).

So both env flags set on the demo Node apply only to the *promoted shell* driver.
For `claude-pty`, **every** structured node in the measured turn came from the
300 ms transcript poll (`channel: "transcript"`) or the herdr screen watcher
(`channel: "herdr"`).

## 4. What has NO representation in the journal today

Verified by grep for emitters (excluding tests/enum definitions):

| Intermediate state | Protocol support | Emitters | Reality |
|---|---|---|---|
| **Tool is running** | `ToolCallState::Running` `crates/remuda-protocol/src/enums.rs:530-533` | **zero** | Measured payload was `"state": "proposed"` on both the `open` and the `close` mutation. The web *infers* running from result absence: `const running = !result \|\| result.stage !== "final"` — `web/src/features/session/ToolCard.tsx:40`. |
| **Elapsed / "Wandering… 7s"** | no field anywhere | — | Exists only as TTY bytes. Captured frames: `ESC[18;3H…Wandering…` with a seconds counter at col 16 incrementing 2,3,4,… The structured view cannot show elapsed time even in principle. |
| **Spinner / phase label** | — | — | TTY only (`✻ ✶ ✳ ✢ ·` glyph cycle at ~10 Hz). |
| **Partial tool output** | `ResultStage::Partial` `enums.rs:535-538` | **zero** | Every result is `"stage": "final"`. The 4 `tick-*` lines landed as one atomic block at t=86.2 even though the first was echoed at t≈66.0. |
| **Streaming assistant text** | `ContentStatus::Streaming` `enums.rs:505-511` | only `signal_messages.rs:174` (dead for `claude-pty`, §3) and `claude_print_stream.rs:54` (retired `claude-print` path) | Measured turn: every message arrived `status: "complete"`. No incremental text. |
| **Turn end** | `Stop` hook | not registered (§3) | Inferred from screen-derived `agent_status`. |
| **`SourceChannel::Screen` tier** | `enums.rs:459` | **zero** emitters in the whole workspace | Screen-derived signals are actually tagged `channel: "herdr"` with `Completeness::ScreenDerived` (`claude_pty.rs:1051,1786`). The `Screen` variant is vestigial. |
| **Queue operations** | design §6 says `queue-operation` is journaled | — | none observed this turn (no steering occurred). |

Net effect: for 20.6 of the turn's 108 seconds the structured view could show
nothing beyond a static "running" dot with no exit code, while the terminal view
next to it was fully live.

## 5. Top 3 changes, by latency removed

### ① Pipeline the Node→Hub journal uplink — `crates/remuda-node/src/transport/wss/runtime_wss.rs:756-790`

`forward_event` awaits each `journal.append_seq(...)` ACK before the caller loops
to the next event, so a burst costs `n × RTT` instead of `RTT`.

> Measured: same-`observedAt` batch of 6 events (seq 22–27, the thinking +
> tool_call group) arrived at +0.38, +0.80, +1.36, +1.73, +2.32, +2.82 s.
> **+0.39 s median per extra event; 2.4 s of the 2.8 s was pure serialization.**

Fix: keep an in-flight window and reconcile ACKs asynchronously — the sibling
transport already does exactly this (`crates/remuda-node/src/daemon.rs:625-658`
keeps a `pending` map up to 16 deep), or add a `journal.appendBatch` method.
Watermark logic in `record_watermark` already tolerates out-of-order confirmation.
**Expected saving: ~2.4 s on every multi-event group** (i.e. on every tool call
and every assistant turn, which are always ≥2 events).

### ② Register the real hook overlay for `claude-pty` — `crates/remuda-driver/src/claude_pty.rs:1297-1390` → reuse `crates/remuda-driver/src/launch/session.rs:84` / `launch/overlay.rs:82`

Replacing the bespoke single-hook injection with `HookSession::start` (already
written, tested, and gated by `REMUDA_PTY_HOOKS`, currently reachable only from
`shell_pty.rs:476`) buys three things at once:

- `PreToolUse` / `PostToolUse` → a tool-start node **at the moment it starts**,
  instead of 230 ms of transcript poll + 2.5 s of uplink, and an anchor to hang
  elapsed time on — this is what closes the **20.6 s** dead window.
- `MessageDisplay` → activates `signal_messages.rs` and the `ContentStatus::Streaming`
  open/append/close chain the web assembler already merges — line-level text
  instead of one 21 s-late block. `native-pty-3.md` §2 measures this fold at 0 ms.
- `Stop` → a real turn-end signal instead of a screen guess.

Note `native-pty-3.md` §1.4 measured hooks arriving ~48–72 ms *after* the
transcript record, so this is not a preview — but it is 12 events/turn of
structure the journal currently has no other source for.

### ③ Stop polling the transcript — `crates/remuda-driver/src/claude_pty.rs:885` + `crates/remuda-driver/src/claude_transcript.rs:521-546`

`TRANSCRIPT_POLL = 300 ms` with a `File::open` + `read_to_end` per tick and no
`notify`/FSEvents watcher. Measured cost **median 230 ms, max 250 ms** — textbook
uniform-over-the-interval. A file watcher (or simply 300→50 ms; the read is
offset-bounded and cheap) removes ~200 ms from *every* structured node.
Smaller than ① and ②, but it is the one hop that is a pure constant.

## 6. Caveats

- Host load average 61–67 during the run. It inflates the absolute uplink numbers
  (⚠ rows); it does not create the per-event serialization, which is visible in
  the code and in the linear +0.39 s/event slope.
- One turn, one harness. The per-hop constants are from source and are exact; the
  timings are n=1 for the turn and n=16 batches for the uplink slope.
- `native-pty-3.md` §7 already flags browser paint timing as uncovered; this
  report stops at the WebSocket frame arriving in a client, not at the frame painted.
- Raw data: `<coord-scratch>/realtime/scratch-probe/journal2.ndjson`,
  `<coord-scratch>/realtime/scratch-probe/ws2.ndjson`,
  `<coord-scratch>/realtime/scratch-probe/created2.json`.
