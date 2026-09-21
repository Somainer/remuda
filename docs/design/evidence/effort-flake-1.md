# Evidence: effort-flake-1

Date: 2026-09-21. Branch: `wt/c-effortflake/b-effortflake-md`. Base: main
`d2b99523`. Synthetic data only (the built-in fake Hub/Node, `e2e-fake-node`);
no hostnames, home paths or usernames appear here.

## Summary

The landing gate failed two tests in `effort-sync.hub.spec.ts` that the branch
did not touch. Both are the same load-sensitive product defect: after moving
the effort slider the chip can stay on **切换中 (`data-effort-effective="pending"`)
forever** even though the effort actually changed. The fix is in the product
(`web/src/lib/store.ts`), not the test; no assertion, retry, sleep, timeout or
`test.skip` changed. No six-tier picker semantics or effort default changed.

## Cause, per failing test (two sentences)

- **`effort-sync.hub.spec.ts:226` — ultracode read-back (assertion at :250,
  chip `pending`, expected `ultracode`).** `setEffort` stamps an optimistic
  pending entry that was cleared *only* by the effort event arriving on the
  live follow socket; under load that single frame is lost to the socket's
  gap/backpressure window (or buffered behind other frames), and nothing else
  ever cleared the pending, so the chip reported `pending` with
  `data-effort-source="unknown"` although the driver had already projected the
  settled level onto the durable instance record (journal append + projection
  commit in one transaction, before the configure ack). The push-down is now
  also settled from that durable projection, folded on the configure-ack
  refresh and the existing 2-second poll, so the settled effort always reaches
  the chip even when its live frame does not.

- **`effort-sync.hub.spec.ts:70` — Codex six-tier round-trip at 390px.** This
  was one of the two failures reported by the landing gate. It was **not
  independently reproduced in the load loop below**: the loop happened to
  surface the identical defect on the same test's **1440px** variant
  (`title="切换中：max"`, `data-effort-effective="pending"`, fourteen stable
  resolutions, `source=unknown`). The 390px attribution is inferred rather
  than separately reproduced, on the grounds that both widths are one
  parameterized test reading the exact same `effortPending` / `effortEffective`
  store fields (the 390px surface has no separate effort projection path), so
  a pending the store never settles must stick at either width; the six-tier
  ordering, labels and wire names are untouched and still asserted at 390px,
  so it is not a layout/picker defect. Both variants pass in the post-fix full
  suite.

## Why pending could become terminal (the product bug)

The driver writes the `effort` observation and the projected
`effortEffective` in one store transaction, then publishes the follow event,
then acks the configure command — the durable record is authoritative and
always current. The web client had two ways to learn the read-back:

1. the live follow socket → `noteEffortObservation`, which *cleared* the
   optimistic pending;
2. the durable record via `refresh()`/poll → `hydrateEffortEffective`, which
   folded `effortEffective` but **never touched `effortPending`**.

When path 1 missed a frame under load the client could go
`readonly-stale` mid-backfill, and because the fake fixture applies the level
synchronously there was no *later* live event to clear the pending that path 2
already knew the answer to. The chip therefore showed `pending` for the rest
of the session — exactly what an operator would read as "the switch never
landed." The 30-minute stale-pending expiry was the only escape, far past any
assertion.

The fix (`hydrateEffortEffective`) settles a pending push-down when the
durable projection reports a read-back strictly newer than the level the
push-down started from (so a queued/working switch is not cleared by a poll
returning the unchanged baseline); `setEffort` records that baseline and folds
one bounded refresh after the configure ack, with the existing poll as the
backstop (no new timer). A push-down settled this way is marked
(`settledEffortPushdown`) so that if the same edge later catches up on the
live socket it is still treated as *our* push-down — the slider stays where the
user put it and a native clamp keeps its `请求 max → 实际 xhigh` mismatch,
instead of being misread as a terminal-side switch that folds the slider to
the observed level.

## Reproduction protocol and counts

Ten sequential runs of `effort-sync.hub.spec.ts` (Playwright hub config,
workers 1, zero retries), fixed loopback ports, every loop and the build inside
one `flock` acquisition of the shared gate lock.

- Parallel load, warm host: a deliberate parallel cold `cargo build
  --workspace --all-targets` into a throwaway target directory alongside the
  runs reproduced **0 of 10** (host load ~14 was not enough to starve the
  browser/runner event loop on this 64-core box).
- Parallel load, pipeline pinned to two cores (the recipe that exposes these
  gate flakes): the browser, Playwright runner, dev server and fake Hub pinned
  to cores 0–1 with `taskset`, while a chained cold `cargo build -j 32` used
  the remaining cores — **before the fix 9 passed / 1 failed (run 7)** with the
  exact gate signature on the line-70 1440px chip; **after the fix 10 of 10
  passed** under the identical protocol.
- Unit coverage: three new store tests pin (a) poll-projected settlement with
  no live event, (b) a queued push-down surviving an equal-baseline poll and
  settling on a newer one, and (c) the live-frame handoff keeping a
  poll-settled clamp on the requested stop. They fail on the pre-fix store and
  pass with it; the full web unit suite is 1471/1471 green.
- Full web hub e2e suite once against a fresh disposable fake Hub/Node (the
  exact gate harness, real-Node case skipped): **170 passed, 20 skipped
  (pre-existing environment-gated `test.skip` cases), 0 failed** (25.1 m). The
  two non-effort failures seen in an earlier whole-suite attempt were caused by
  that run using a reused long-lived fixture with `HUB_E2E_EXTERNAL=1` (it
  enabled the operator-only real-Node test and had accumulated placement/space
  state from the morning's load loops); both passed on the fresh gate fixture.

**Product or test:** the fix is in the **product** —
`web/src/lib/store.ts` (pending settlement from the durable projection); the
test-file assertions are unchanged.
