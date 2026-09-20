# Evidence: flakes-3

Date: 2026-09-21. Branch: `wt/c-flakes3/b-flakes3-md`. Base: main
`5e11f1eb` (contains the c-flakes2 fixes).

The landing gate runs the full web hub e2e suite against one shared fake
Hub/Node on a loaded host (workers 1, observed load average up to ~16 while
other workers build and test). Two tests in `web/tests/e2e/hub-live.spec.ts`
that had passed three gate runs earlier on 2026-09-20 failed there, both
load-sensitive. Each was reproduced with a deliberate load loop; this file
records the signature, root cause, fix and before/after loop counts. All data
is synthetic (built-in fake-node scenarios, `wsp_e2e`, `e2e-fake-node`); no
hostnames, home paths or usernames appear below. No retries, sleeps, blanket
timeouts or `test.skip` were added; one product file changed (Flake C) and
the reason is given in its section.

## Reproduction protocol

10 sequential runs of `hub-live.spec.ts` (Playwright hub config, workers 1,
zero retries) with a deliberate parallel `cargo build --workspace
--all-targets -j 16` into a fresh throwaway target directory for sustained
host load. Every command — the whole loop and the build — ran inside one
`flock` acquisition of the shared gate lock so it serialized against the
landing gate, which holds that lock for its entire run. Fixed loopback
ports were used throughout.

- Before the fix: **1 of 10 runs failed** (run 1). The failure was
  `hub-live.spec.ts:528` — `expect(getByTestId("composer-interrupted-chip"))
  .toBeVisible()` found no element — the literal gate signature. Because the
  file runs in serial mode, the two tests after the failing test (the
  hook-carried approval test at line 561 and the structured-stream test at
  line 637) were skipped in that run; the other 7 tests in the file passed.
- After the fix: **10 of 10 runs passed** under the same load protocol.
- The full web hub e2e suite was also run once to completion: **165 passed,
  20 skipped (pre-existing environment-gated `test.skip` cases such as the
  real-external-Node flow), 0 failed** — including the two tests after Flake
  C's point in the serial file (hub-approval-card and structured-stream),
  which ran and passed as full-suite tests 22/185 and 23/185.

## Flake C — interrupted chip missed after a steer on a working native session

**Signature (gate and load loop).** `hub-live.spec.ts:528`,
`expect(getByTestId("composer-interrupted-chip")).toBeVisible()` — element
not found — right after the 插队 (steer) POST was observed on the wire.

**Root cause.** The 已打断 receipt is raised only after the steer POST
resolves, while the fake Node journals the interrupted turn's idle status
*before* answering the send RPC (the real driver reports turn end before the
RPC result as well); under host load the follow socket's working→idle status
commits to React in the same burst the POST resolution lands in, and the
Composer's held-flush effect unconditionally cleared the receipt on that
edge, so the chip was un-rendered before it ever painted. The flush effect no
longer touches the receipt; the receipt's existing 4-second timer is its sole
bound, so batched event order on a loaded host cannot erase it while an
instant subsequent turn still clears it.

**Fix (product race).** `web/src/features/session/Composer.tsx` — removed the
`setInterrupted(false)` call from the working→idle / blocked→answered
held-flush effect and left the badge bounded only by the timer already
documented as covering a missed phase transition. The flush effect's job is
to deliver the held queue; clearing an interrupt receipt that is raised
asynchronously (POST resolution) on the same turn-end edge was coupling two
unrelated transitions. A regression test in
`web/src/features/session/ComposerSteerQueue.test.tsx` resolves the steer
POST and commits the idle phase in one burst and pins the badge visible; it
fails on the old code (`Unable to find an element by:
[data-testid="composer-interrupted-chip"]`, the gate's exact error) and
passes on the fixed code.

## Flake D — strict-mode violation: three "echo e2e" approval rows

**Signature (gate).** `hub-live.spec.ts:88` —
`getByTestId("approval-row").filter({ hasText: "echo e2e" })` resolved to 3
elements, a strict-mode violation, when the test navigated to `/approvals`.

**Root cause.** Every create on the single long-lived fake Node raises an
identically labelled approval card (title `Bash`, description `echo e2e`),
and the CI Playwright config retries a failed test once against that same
still-running fake Node while neighbouring serial specs share it too; a card
left pending by a retried attempt (or by a neighbour whose delete/answer
raced the queue read) therefore sat in the approvals queue and made the
text-only locator match three rows. The test now owns exactly its own row by
also requiring the row's session link (`a[href="/s/<createdId>"]`), which
foreign cards never carry, so a retry or a neighbour can neither make the
match strict nor be answered by this test; the answered-row wait then asserts
on the same instance-scoped locator.

**Fix (test owns its rows; no product or fixture change).**
`web/tests/e2e/hub-live.spec.ts` only. The fixture already purges a deleted
instance's pending cards and every neighbouring spec force-deletes its
instances; the remaining seam was a locator that assumed the shared queue
contained only this test's card, so the assertion was scoped to the instance
the test itself created. Departed rows in the approvals UI carry no session
link, so the scoped locator matches only the live row of this instance.

## Verification

- `pnpm --dir web test` — all 1454 unit tests pass (including the new
  Composer receipt-vs-flush-edge regression test).
- `pnpm --dir web typecheck` and `pnpm --dir web lint` — clean.
- `hub-live.spec.ts` × 10 under the load protocol above — 1/10 failing
  before (Flake C, line 528; lines 561 and 637 serial-skipped), 10/10
  passing after.
- Full web hub e2e suite once (185 tests, serial) — 165 passed, 20 skipped
  (pre-existing environment gates), 0 failed; the two tests after Flake C's
  point in the serial file ran (full-suite tests 22/185 and 23/185) and
  passed.
- `hub_e2e.rs` was not modified, so the `cargo test -p remuda-hub` condition
  did not apply.
- Repository secret scan — clean.
