# steer-1 — 插队 (interrupt-and-send) end to end

Date: 2026-09-17. Branch `wt/c-c-steer/steer-and-busy-detection`.

Scope: the owner finding — a message typed while AskUserQuestion/approval was
pending sat 排队 forever because the pending interaction was classified as
"working"; and the ask for an explicit 插队 gesture (Cmd/Ctrl+Enter + visible
button): interrupt the current turn and deliver the message first, ahead of
the held queue, journaled with origin + reason.

Synthetic fixture only (in-process fake node + fake PTY in the Rust worker
tests); no real model, no real PTY. The native keystroke facts for claude
2.1.270 come from [`claude-queue-steer-1.md`](./claude-queue-steer-1.md);
this change implements the Remuda side on top of them.

## What was wrong (before)

- The web composer hard-disabled itself whenever an interaction was pending
  (`disabled={… || pending.length > 0}`), while the instance activity model
  already distinguished `waiting-interaction` (`blocked`) from `working`. A
  prompt sent into the D-022 PTY queue behind the approval was marked
  `waiting-interaction` too, so every queued message looked like it was stuck
  behind "work" and never surfaced a reason.
- There was no jump-ahead path anywhere: the node's `pty_queue` was strictly
  FIFO, `instance.cancel` cleared the **whole** queue, and the node dropped
  `PromptMode` at its RPC edge (`runtime_link.rs` / `runtime_wss.rs` /
  `hubnode_codec.rs`) — `mode:"steer"` reached nothing.
- Plain Enter while working held a message web-only with no transcript row
  describing why it waited or its position, and the only "interrupt and send"
  control (`打断并发送`) fired two separate commands from the browser, so a
  page that lost focus between them could cancel without sending.

## Design (this change)

### Busy classification

- `waiting-interaction` (AskUserQuestion, PermissionRequest, Elicitation,
  plan review) is **not working** end to end. The web projects it to the
  composer phase `blocked`; the composer stays enabled, offers plain 打断 but
  never 插队 (Esc into a dialog would answer/dismiss the question), and Enter
  holds the message with reason `answer` — tag 「待回答后送出」.
- The node's PTY queue no longer paints queued prompts over a
  `WaitingInteraction` activity (`remark_after_enqueue` keeps a real
  question's evidence), and a queued follow-up behind a *running* turn keeps
  the turn's `Working` evidence. Previously every queued prompt forced
  `waiting-interaction`, which is exactly what made a question and a running
  turn indistinguishable.

### 插队 (mode = `steer`)

One command, atomic interrupt-then-deliver inside the node instance worker:

1. The web posts a normal `instance.send` with `mode:"steer"`
   (Cmd/Ctrl+Enter, or the visible 插队 button with a confirm dialog).
2. The node queue worker (`crates/remuda-node/src/runtime/pty_queue.rs`):
   - journals the prompt as a `queued` user row carrying `promptMode:steer`
     and inserts it at the **front** of the deque (FIFO prompts keep their
     order behind it);
   - when the instance was `working`, executes `DriverRequest::Cancel` —
     `esc` through the driver's own key path (`claude_pty`/`generic_pty`
     send_keys(["esc"]); `shell_pty` per-harness `interrupt_bytes`) — and
     appends a `turn-interrupted` native lifecycle
     (`relatedIds: {origin, reason:"user-steer", commandId}`);
   - holds the steer at the head until the turn ends: activity leaves
     `working`/`waiting-interaction` (Stop/turn-ended/idle evidence from the
     harness), then writes the prompt on the first delivery tick;
   - bounds the wait at **2 s** (`STEER_INTERRUPT_BUDGET`); past it, a missed
     idle signal no longer blocks delivery (`wait_control` still probes), so
     the prompt can never sit 排队 forever — a harness that natively absorbs
     mid-turn text (claude's tool-boundary queue) does the right thing with it;
   - on completion appends `prompt-steer`
     (`reason:"interrupted-current-turn"`) and completes the queued user row,
     which keeps its `promptMode:steer` through the revision replace.
- Plain Enter while working holds the message web-side (reason `turn`),
  shown as 「排队中 · 第 n 条 · 回车后送出」with per-row cancel; on
  working→idle the rows flush in order as ordinary `new-turn` sends. A
  harness-native queue (codex Tab) still posts `mode:queue` immediately and
  the chip is a non-cancelable ledger mirror.
- A pending-question hold (reason `answer`) flushes on
  blocked→idle/working when the interaction is answered.

## Measurements

From the Rust worker test
`pty_queue::tests::steer_interrupts_the_running_turn_then_jumps_the_queue`
(fake PTY: a long tool whose control closes, harness reports idle after the
interrupt — `cargo test -p remuda-node --lib pty_queue -- --nocapture`,
3 runs, dev build, 2026-09-17):

| run | Esc dispatched | turn-ended evidence | steer written | steer after turn-ended |
|----:|---------------:|--------------------:|--------------:|-----------------------:|
| 1 | 6.3 ms | 257.2 ms | 391.0 ms | **133.8 ms** |
| 2 | 6.2 ms | 257.1 ms | 390.7 ms | **133.5 ms** |
| 3 | 6.3 ms | 257.5 ms | 392.6 ms | **135.1 ms** |

(The fixed ~257 ms "turn-ended" column is the test's own 250 ms dwell proving
the steer does NOT write while the turn is still `working`; the product
number is *steer after turn-ended*: one ≤200 ms delivery tick, versus the
2 s live budget.)

Observed order for three prompts (`first` already delivered, `second` and
`third` queued, `jump` 插队): PTY writes
`first → Esc → jump → second → third`; queued user rows complete in the same
order; the two FIFO commands keep ordinals 1/2 and settle completed.

E2E (Playwright, in-process fake node, `tests/e2e/ux-steer.hub.spec.ts`):
3/3 passing, ~36 s total:

- queue two → cancel one → 插队: `mode:steer` POST precedes the surviving
  held row, which flushes after idle with no `mode`; no row keeps a held tag;
- pending-question (`blocked-question` sentinel in `hub_e2e.rs`): composer
  stays enabled, no 插队 button, Enter shows 待回答后送出, zero commands POSTed
  until the interaction is answered, then the message flushes as a plain send;
- Ctrl+Enter POSTs `mode:steer`.

## What is deliberately NOT in scope

- The AskUserQuestion card itself and its "Type something" free-text row are
  c-question's file (`wt/c-question`); the blocked-composer change here only
  stops misclassifying the pending interaction and queues around it.
- The live strip (r-live-signal) renders the native phases; this branch reads
  instance activity for the composer and does not alter the strip.
- Capabilities matrix: claude `interrupt`/`queue` cells stay as the D-028
  task left them; the 插队 control follows the reported `interrupt`
  capability provision (native Esc / emulated cancel sequence / unknown),
  never guessing.
