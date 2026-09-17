# effort-sync-3 — the second in-session effort switch never falsely degrades

Continuation of [`effort-sync-2.md`](./effort-sync-2.md). Owner report
(screenshot): in one session the first effort switch (to **ultracode**)
succeeded, but the next switch to a plain tier ended with the chip back on
`?` and the toast **「effort 切换被拒绝：未收到回读」** —
`effort-degraded:<word>:no-readback-within-window`. The suspicion was the
ultracode detection.

## 1. Reproduction attempts on this host (claude 2.1.273)

Two live probes were added under `crates/remuda-driver/examples/`:

- `effort_repro3.rs` — a raw PTY plus the **real** `TranscriptMapper` +
  `EffortBridge` pumped at the production 75 ms cadence;
- `effort_driver3.rs` — the **real `ShellPtyDriver`** (materialized launch,
  promotion-poller transcript binding, effort worker, event stream) driven by
  `DriverInput::ModelSwitch { effort }` exactly as the Node does.

Both walked `ultracode → high → ultracode → max`, once back-to-back and once
with a real model turn between every switch. Artifacts in
`/tmp/remuda-c-effort3-repro{1,2}/` and `/tmp/remuda-c-effort3-driver1/`.

**Result on 2.1.273: every switch settled Applied** (raw probe: slash + stdout
verdict 244–472 ms after the submit CR; real driver: effort observation
700–1191 ms after configure, all `source=Remuda`). Neither the generation
cursor nor the tracker carried stale state across switches, and the
`ultracode` stdout text parses on every occurrence. The `Notify` wait/notify
race was also ruled out: tokio 1.53.1's `Notified` snapshots the
`notify_waiters` generation counter at construction and completes on first
poll even when the broadcast happened before registration, so the bridge's
create-then-check pattern cannot lose a wakeup.

## 2. The fragile link: confirmation-dialog gating

What differs between the first and later switches is the **screen**, not the
transcript logic. A cached conversation makes Claude confirm every `/effort`
with a modal:

```
Change effort level?
This conversation is cached for the current effort level. Switching to <tier> means …
  ❯ 1. Yes, switch to <tier>
    2. No, go back
```

The pre-fix `perform_switch` sent its confirming Enter as soon as the visible
screen contained the generic phrase `"Change effort level"`. On the second
switch the previous switch's dialog can still be **rendered inside the
visible region** (a short Herdr grid, `ReadSource::Visible, lines: 40` on a
30-row terminal, or a slow Ink repaint):

1. submit CR for switch N → phase-1 poll immediately matches the *stale*
   dialog text;
2. the confirming Enter is sent ~120 ms later, racing the new modal's paint
   (measured paint 121–164 ms);
3. when it lands before the new modal reads input, the **real "Yes, switch"
   modal stays open** — the slash record is already on disk (it is written on
   submit) but no `<local-command-stdout>` verdict ever follows;
4. the bridge's 10 s window expires → `no-readback-within-window`, even
   though Claude had accepted the command bytes.

The first switch in a session cannot hit this: no dialog predates it. It is
also intermittent on the native carrier (the fullscreen TUI repaint usually
overwrites the stale cells), which is why the live probes on 2.1.273 did not
fault but the owner's herdr-backed Mac session did.

## 3. The fix (`crates/remuda-driver/src/effort.rs`)

`perform_switch` now has two dialog layers, while the transcript verdict
remains the only authority:

1. **Pre-submit snapshot + target gating.** The screen is read before typing;
   a dialog already present is marked stale. The phase-1 Enter only goes out
   for a dialog that names THIS switch's tier — `"switch to <tier>"` /
   `"Switching to <tier>"` (`EffortRequest::dialog_target_word()`, `xhigh`
   for an ultracode switch, matching the measured modal). A stale dialog
   naming the previous tier is waited out until the visible region goes
   dialog-free and the new modal paints.
2. **Rescue pass during read-back.** While waiting for the verdict (250 ms
   poll), a dialog still open and attributable to this command (names the
   target, or appeared after a dialog-free screen) gets at most two spaced
   Enters with a 700 ms gap. This heals both a late paint and an Enter that
   raced the paint; an extra Enter after the modal closed lands in the empty
   composer and is a TUI no-op.
3. **Early verdict.** Each phase-1 iteration non-blockingly checks the bridge
   (`wait(…, Duration::ZERO)`), so a dialog-free build that applies on the
   submit CR settles immediately instead of waiting out the 1.5 s dialog
   window.

Invalid-argument screens never receive an Enter in either phase.

## 4. Timing (claude 2.1.273, real `ShellPtyDriver`)

All times from `/tmp/remuda-c-effort3-driver1/timing.json` and the raw probe
timing files; ms relative to submit CR / configure.

| switch | slash + stdout verdict (raw PTY) | effort observation via real driver |
| --- | --- | --- |
| 1 ultracode | 436–438 ms | 1191 ms, Xhigh/ultracode=true, Remuda |
| 2 high (turn in between) | 320–472 ms | 699 ms, High/ultracode=false, Remuda |
| 3 ultracode | 244–396 ms | 700 ms, Xhigh/ultracode=true, Remuda |
| 4 max | 244–397 ms | 699 ms, Max/ultracode=false, Remuda |

Dialog paint in every case 121–164 ms after the submit CR; confirmation CR
fired once per switch. Back-to-back (no intervening prompt) settled with the
same timings (244–438 ms).

## 5. Tests and coverage

- driver unit tests (`effort.rs` `switch_tests`): stale dialog naming the old
  tier never receives the confirm CR (and the CR is asserted against the new
  dialog); a dialog painting after phase 1 is confirmed by the rescue pass and
  still reads back Applied inside the window; invalid argument gets no Enter
  and degrades with `invalid-argument`; existing applied/queued/bounded-degrade
  cases kept;
- driver integration (`tests/effort_transcript.rs`) replays the verbatim
  sanitized 2.1.273 fixture
  (`tests/fixtures/effort-21273/effort-multi-switch-21273.jsonl`,
  low → ultracode → high → ultracode → max with turns), arming a generation
  before each slash record and asserting every generation resolves Applied
  with the right tier and flag;
- the hub fake (`crates/remuda-hub/examples/hub_e2e.rs`) now stamps effort
  read-backs with a strictly increasing `observedAt` (the web store discards
  older-then-current observations, which rapid consecutive switches can
  trigger at equal millisecond resolution);
- hub e2e `effort-sync.hub.spec.ts` gains a four-switch test
  (ultracode → high → ultracode → max) asserting the chip settles on every
  switch with no `?` / pending left and one configure per stop.

### Reproduce

```sh
# live probes (gateway model; driver probe runs the real ShellPtyDriver)
PROBE_DIR=/tmp/remuda-c-effort3-repro1 \
  cargo run -p remuda-driver --example effort_repro3 --features test-stub
SCENARIO=turns PROBE_DIR=/tmp/remuda-c-effort3-driver1 \
  cargo run -p remuda-driver --example effort_driver3

cargo test -p remuda-driver --features test-stub
```
