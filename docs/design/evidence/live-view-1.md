# live-view-1 — the OSC screen tier, raise-only, and the end-to-end budgets

Evidence for the live structured view, batch **r-live-screen**:
[D-2 / D-3](../native-pty-1.md#2-channel-inventory) (the screen tier reported
`Idle` during a running turn because its only working anchor was a footer
phrase 2.1.270 removed) and the [live-view design §4](../native-pty-2.md)
target column.

- **When:** 2026-09-15 (UTC), r-live/osc-tier-and-budgets.
- **Where:** this worktree on the shared dev box; throwaway sessions under
  per-test temp dirs, each with its own cwd and a 0755 installed fake agent.
- **Agent under test:** `fake-harness --dialect-version modern`, the
  deterministic 2.1.270 TUI double (driven by
  [`live.json`](../../../crates/remuda-testing/fixtures/fake-harness/scenarios/live.json)).
- **Method:** real `portable-pty`, the production hook overlay and `remuda
  hook emit` relay, the Node observation pump (message fold, hook-gated
  activity, interactions), and a ground-truth `--events-out` channel with
  `wallMs` anchors. Measurements are wall-clock, anchor→committed-journal.

Nothing here is inferred from a mock. The fake emits the exact byte and hook
shapes [harness-parity](../../research/harness-parity.md) and
[claude-channels](../../research/claude-channels.md) measured from the real
2.1.270 binary; the pipeline around it is production code.

---

## 1 — The defect (D-2) and the fix

`screen_status` keyed *Working* off the literal `"esc to interrupt"`. A
22,521-byte probe-C capture contained the phrase **zero** times, while the
composer glyph `❯` is painted the whole turn — so the screen tier answered
`Idle` while a tool was running. Independently, the emulator captured and
retained OSC 0 / OSC 9;4 payloads on every `ScreenGrid`, but nothing read
them (D-3).

The fix ([`crates/remuda-screen/src/osc.rs`](../../../crates/remuda-screen/src/osc.rs)):

- **match the 9;4 state *token*, not the payload.** Claude emits
  `ESC ] 9 ; 4 ; 3 ; BEL` with an **empty** percent; the emulator's own
  unit test feeds `3;0`. Both parse to state 3.
- **disambiguate `✳` with progress.** The title glyph alone is idle/plain
  idle; `✳` while 9;4 stays 3 is the permission dialog (claude-channels §0
  row 5).
- precedence per design §2.4: `✳`+busy-progress → blocked; OSC busy →
  working; blocked phrases; the legacy working phrase (pre-2.1.270 builds);
  an active non-zero progress veto over the prompt glyph; prompt glyph →
  idle. **Unknown never collapses to idle** (D-028 §10).

Control that reproduces D-2 against the pre-fix logic, and its fix:

```
$ cargo test -p remuda-screen --test osc_status d2_
d2_a_modern_working_row_without_the_footer_phrase_stays_working_via_osc ... ok
```

Before this change the same working frame classified `Some(Idle)`.

---

## 2 — Rule 6 is now enforced on the wire

Design §0.2: *OSC may set busy, never clear it.* The library-level
`ScreenLatch` ([`osc_status.rs`](../../../crates/remuda-screen/src/signature.rs))
holds busy across poll boundaries and latches blocked across dropped frames,
but the production promotion poller still announced every raw screen verdict.
Batch wired the latch into
[`promotion.rs`](../../../crates/remuda-driver/src/shell_pty/promotion.rs):

- hook health is the poll's `HookSession::turn_active(pid)` (exact §2.6
  decision): `Some(true)` → hold busy; `Some(false)` → hook confirmed the
  turn end, screen may idle; `None` → never-materialised, screen is the
  authority;
- liveness is **sticky per promoted epoch**, so a transient `ps` miss or a
  hook-relay child momentarily taking the sampled row can't release the
  hold; a hook turn-end or demotion clears it.

### 2.1 Healthy hooks: the instance never idles on the screen tier before Stop

`live_pipeline.rs`, hooks configured. The screen raises working once (the
OSC edge), holds it through the 20 s tool and the approval dialog, and does
**not** emit `agent_status=idle` at turn end — the hook `Stop` is the only
idle authority:

```
pty   agent_status working   (once, on the OSC busy edge)
hook  UserPromptSubmit working
hook  PreToolUse running
… 20 s, zero screen events …
pty   agent_status blocked   (approval dialog; legitimate)
hook  Stop idle              ← the turn ends here, activity → Idle
```

### 2.2 No hook tier: the escape hatch

Same scenario with `REMUDA_PTY_HOOKS` unset. There is no hook channel at
all, and the screen OSC edge legitimately carries the whole turn including
the idle transition. Both behaviors asserted in the same test file.

---

## 3 — The dead window carries no spinner

The remuda-pipeline report measured 218 KB pushed through the TTY during
the 20 s tool and **zero** structured events. The modern fake repaints at
~9 fps, like the real TUI. The test runs the 20 000 ms tool for real and
asserts the journal interior (after a 1 s promotion grace, before the
300 ms finish phase) contains **zero** pty-channel events:

```
tool_start_hook → tool_finish_hook anchor gap: ~20 000 ms
pty/agent_status events strictly inside that window: 0
```

The screen tier announces working once; it never streams the 10 Hz
`Grooving… (N s)` repaint.

---

## 4 — Measured end-to-end latencies (design §4.2 target column)

`cargo test -p remuda-node --test live_pipeline`
(real PTY → Node observation pump → durable local journal). The clock
starts at the harness anchor written microseconds before the relay spawns
(the first instant Remuda could know) and stops when the observation is
committed. Representative run, dev box:

| §1.1 edge | target p50 | measured |
|---|---|---|
| submit → `UserPromptSubmit` | ≤ 150 ms | **−4 ms**¹ |
| tool start (`PreToolUse`) → running | ≤ 150 ms | **7 ms** |
| first `MessageDisplay` chunk → streaming message | ≤ 250 ms | **6 ms** |
| harness `Stop` → turn-ended | ≤ 150 ms | **5–6 ms** |

¹ the anchor and journal timestamps are written on different cores within
the same millisecond window; the assertion is a one-sided upper bound, so a
small negative is "sub-ms", not time travel. Repeated runs were 5–8 ms for
the positive edges. The ~22 ms command-hook spawn floor measured in
[claude-channels](../../research/claude-channels.md) is fully hidden by
the in-process fold on this machine; the 400 ms test budgets leave slack
for loaded CI hosts.

These are **local-journal** numbers (native→journal). The design's
native→browser column adds one Node→Hub RTT; batch r-p2-createlag owns that
link (pipelined journal uplink).

### 4.1 Relative link cost

The design's §4.2 relative assertion is *"a 6-event group must arrive
within 1.5× the latency of a single event — it fails loudly if forward_event
serialisation returns."* At the local pump, the turn tail (PostToolUse,
four MessageDisplay chunks + derived messages, Stop) is one tight emission
burst:

```
tail burst: 9 events over 14–15 ms; single-event latency 1 ms; budget 301 ms
```

9 events commit in ~15 ms — there is no per-event serialization in the
observation pump. The guard (`span ≤ 1.5 × single + 300 ms local slack`)
fails loudly only if a regression stacks await-per-commit there. The
Node→Hub uplink half of the same contract is gated in r-p2-createlag's
`wss.rs` against the real Hub ACK ordering.

---

## 5 — Verification

```
cargo test -p remuda-screen                 # incl. osc_status D-2/rule-6 suite
cargo test -p remuda-testing                # fake-harness + golden_screens untouched
cargo test -p remuda-driver                 # promotion poller / latch integration
cargo test -p remuda-node --test live_pipeline   # the §4.2 budgets + rule 6
cargo test -p remuda --test journal_diff    # new turn.live whitelist rule
cargo clippy --workspace --all-targets -- -D warnings
```

- The modern dialect and the checked-in golden screens are additive: the
  legacy dialect's grids are byte-stable and `golden_screens.rs` is
  unmodified.
- The modern fixture timeline
  (`tests/fixtures/modern-claude-turn.bin`) classifies
  idle → working → blocked → working → idle through the real VT parser with
  zero occurrences of `"interrupt"`.
