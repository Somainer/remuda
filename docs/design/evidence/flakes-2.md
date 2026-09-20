# Evidence: flakes-2

Date: 2026-09-20. Branch: `wt/c-flakes2/b-flakes2-md`. Base: main
`6d211bb1` (main advanced from this item's starting base `a221553d` while
the work was open; the intervening commits were docs-only and left both
specs unchanged, so the fix context is identical).

The landing gate runs the full web hub e2e suite against one shared fake
Hub/Node on a loaded host (workers 1, observed load ~5–9, other workers
running cargo and Playwright). Two specs kept failing there while green in
isolation; each failure was a different symptom on the gate's single retry,
which is why a normal re-run could not converge. This file records each
signature, the root cause proven by a deterministic or load-loop
reproduction, the fix, and before/after loop counts. All data is synthetic
(built-in fake-node scenarios, `wsp_e2e`); no hostnames, home paths or
usernames appear below. No retries, sleeps or blanket timeouts were added to
any spec; the product change is zero files (both defects were a test harness
seam and a test-fixture mis-set option).

## 1. Flake A — ux-nextstep.hub.spec.ts: geometry reads a card unmounted by the D-049 /sessions → /m redirect

**Signatures (gate).**

- Attempt 1, line 265: `expect(geometry.headline.oneLine).toBe(true)` failed
  with `headline {"h":0,"lineH":null,"oneLine":false}` — the headline
  element measured zero height and had no computed px line-height.
- Attempt 2, line 187: `expect(getByTestId("board-card").filter({ has:
  a[href="/s/<workflowId>"] })).toHaveAttribute("data-status","working")`
  timed out at 90 s; the new card never appeared.

**Root cause of the line-265 symptom (proven deterministically).** The test
shrinks the page from 1440 px to 390 px right before the geometry measurement.
D-049's `ViewportGate` (`web/src/app/router.tsx`) watches
`COMPACT_WORKBENCH_QUERY` (max-width 767 px) and renders
`<Navigate to="/m">` the instant it matches: the desktop `SessionList`
unmounts and the phone `HomeList` (a different component with its own rows)
mounts. Playwright's locator `.evaluate()` resolves a card element, then the
resize-driven redirect replaces the tree; the resolved element is now
detached — `getBoundingClientRect().height === 0` and computed line-height is
`normal` (parses to `null`). The exact failure value was reproduced with a
temporary spec that pinned the resolved handle across the resize:
`STALE {"h":0,"lineH":null,"connected":false}`. The gate hit the window where
the evaluation ran during the navigation (~one frame); idle machines schedule
the navigation fast enough that the card was usually already replaced and the
locator re-resolved onto the `/m` tree's unrelated markup or the measurement
beat the redirect. A load loop on the original spec (8 ×
`ux-mobile-new` + `ux-nextstep` under a loaded host, the suite's real
execution order) reproduced the family three times, including the literal
gate value at line 265.

The redirect was mounted in `9d250353` (feat(web): mount the /m route tree
behind the viewport redirect layer) minutes AFTER the geometry test landed
in `57be317c`; the test had been written against a desktop-only shell.

**Fix (test harness seam; no product file changed).** The geometry gate
proves the FULL desktop board row under the 390 px *CSS* rules (media
queries still key off the real viewport), not the phone `HomeList`. A new
`holdDesktopShellAtPhoneWidth(page)` seam intercepts
`MediaQueryList.prototype.matches` for the exact compact query string and
pins it to non-compact while the layout runs at a real 390 px: CSS media
rules genuinely apply, the `/m` redirect never fires, and every `board-card`
stays mounted. Per-instance patching cannot work — Chromium hands each
`matchMedia()` call a fresh wrapper — so the prototype getter is the single
choke point; the fresh browser context per test restores it. The measurement
then waits on a real layout condition (`offsetParent !== null` plus a
resolved px line-height for both headline and sentence) instead of reading a
node that might be mid-navigation. An adversarial variant inserting a 150 ms
post-resize delay fails before the fix and passes with it.

**On the line-187 symptom (desktop-width, before any resize).** Line 187
runs at 1440 px before the viewport change, so the /m redirect cannot
explain it; two other mechanisms, both addressed here, can make the new
workflow card not satisfy its assertion there:

1. **Journal-frame projection lag.** The fake node fire-and-forgets the
   create's journal frames (user node, native `working` status, workflow
   scenario) ahead of the create RPC ack; on a loaded shared node the web
   list snapshot right after `create()` resolves can predate the frames
   that move the row out of `requested` and set its activity, so the board
   briefly has no `data-status="working"` href row. The old test raced that
   window with its full assertion budget. `createAgentSession` now calls
   `waitForProjectedInstance`, which polls `GET /v1/instances` until the
   row is present with `connectivity:"connected"` and the exact expected
   activity (`working`/`blocked`) before the test proceeds.
2. **Implicit-default workspace.** The three sessions were created with
   whatever workspace the picker defaulted to. The board filter is
   Space-scoped and the rows are asserted by href, so a default other than
   the other two rows' Space hides the card for the whole budget.
   `pinWorkspace` forces `wsp_e2e` for every created session.

The /m redirect remains the sole explanation for the reproduced 390 px
failures: line 249 (approval card, ×2 in the load loop), line 265
(`headline {"h":0,"lineH":null,"oneLine":false}`, the exact gate value)
and line 268 (terminal card, ×1) — all evaluated after the resize, every
reproduced "card never found" at those lines was the detached/redirected
family.

## 2. Flake B — grok-structural.hub.spec.ts:507 — settled tool card never appears

**Signature (gate).** Both attempts failed at `settleStructuredToolCard`
(line 434 → called at line 630): `Error: structured tool card never
settled`, host load ~10 — twice across separate gate runs, so a first
hardening was not enough; two independent defects compose.

**Root cause 1 (fixture option mis-set; proven).** The test rewrites the
fixture's `quit_after_turns` to keep the fake harness alive past turn end —
but it set the field to `0`. The field is a turn COUNT
(`crates/remuda-testing/src/fake_harness/engine.rs`:
`.is_some_and(|n| self.turn_count >= n)`): `0` means "exit after zero
turns", so the harness exits at the FIRST turn end just as surely as the
committed `1`; only an ABSENT field keeps the binary up. When the harness
exits, the shell-pty promotion poller's next ~800 ms sample finds no
foreground agent and emits `agent_demoted`; the Node resets the instance to
kind terminal / mode native and clears `nativeRef.signalTier`, and the Hub
removes `nativeSignalTier` on that frame. The web gate
(`hasStructuredSignal`, `web/src/features/session/tty/gate.ts`) then renders
the raw `ScreenView` ("(waiting for screen snapshot…)") instead of the
`Transcript`, so no `tool-card` can ever settle. Verified against a
long-lived hub: after a full-file run every grok instance was left
`kind=terminal, mode=native, signalTier=null` (`transcript=false`, raw
screen fallback), and a fresh-hub full-file run failed identically before
the fix. A diagnostic probe on the second failure mode while the pane stayed
mounted returned `{transcript:true, cards:0, foldOpens:0, strip:"回合结束 ·
完成 file"}` — pane up, turn ended, zero cards.

**Root cause 2 (the settle helper's click/reload race; proven by that
probe).** Transcript compact mode defaults ON (`hub.compact` defaults true
in `web/src/lib/store.ts`); `compactTranscript` groups the turn's tool and
thought rows into ONE closed `compact-fold` group whose children are not
mounted until opened. The old helper clicked the fold and synchronously
asked `card.count()` in the same iteration: under load React has not mounted
the children yet, so the count is 0, and the next line **reloaded the page**
— which discards the fold-open local state. Every iteration repeated
click-open → 0 → reload-reset, so the card could never settle even though
the Final frame was durable. The probe's DOM was the group summary ("▸ 2
次工具 · 1 段思考") with zero mounted tool cards.

**Fix (test-fixture + test helper; no product file changed).**
`patchedScenario` now deletes `quit_after_turns` instead of setting it to 0;
the harness genuinely stays up, promotion/file-tier survives to the settled
assertions, and `finishHarness` stops the node in teardown (no leaked
processes or scratch dirs observed across the loops). The settle helper now
(1) opens the closed compact *group* via its real `compact-fold` trigger
(and any per-card D-041 `tool-fold-open` inside it), (2) then waits IN PLACE
on the card's terminal condition — the Final `exit 0` badge — for up to 2.5 s
per iteration instead of reading `count()` immediately, and (3) reloads only
while the structured pane itself is ABSENT (the hydration-lag case), never
while the pane is up but a card is merely unscrolled/unmounted, so a reload
can no longer undo the fold it just opened; after a reload it waits for the
pane to become visible (condition) instead of a fixed `waitForTimeout(800)`,
so the net sleep count in the spec drops by one. The test's invariant
(partials replaced once, each of one/two/three a whole line exactly once)
is unchanged: the Final close replaces the partial tail outright, which is
what the settled-card line counts assert.

## 3. Verification (loaded host, single shared fake Hub/Node per slot)

Loop counts and loop logs are synthetic-only. `CI=1` loops honour the gate's
one-retry shape; plain loops run with no retries. Background load was a
parallel `cargo build --workspace --tests` plus the host's ambient workers,
holding load average ~7–12 (the gate band).

- Flake A original spec, BEFORE: an 8× execution-order pair loop (not a
  10× loop — each iteration ran `ux-mobile-new` then `ux-nextstep`, the
  suite's real order, under load) reproduced 4/8 failures: line 249 (×2),
  line 265 with the literal `headline {"h":0,"lineH":null,
  "oneLine":false}` (×1, the exact gate symptom), line 268 (×1). AFTER
  (fixed spec): the same pair loop 8/8 green, and a separate dedicated 10×
  CI=1 loop was 10/10 green.
- Flake A deterministic check: stale-handle probe reproduces
  `{"h":0,"lineH":null,"connected":false}` on the original code; the fixed
  spec passes both normally and with an adversarial 150 ms post-resize
  delay.
- Flake B: BEFORE, full-file run failed at "structured tool card never
  settled" on two fresh hubs (0/2), and a runtime diagnostic returned
  `{transcript:true, cards:0, foldOpens:0}` — the closed compact group.
  AFTER: full-file runs green; a 10× loop under sustained
  `cargo build --workspace` plus ambient worker load (load ~7–12) was
  10/10, and the previous mixed-load 10× loop was 9/10 with the one miss an
  unrelated environmental startup (`native node exited before enrollment`
  while the load rig had saturated the host's SQLite writer; it passed
  immediately in isolation and never reached the settle helper).
- Shared WebSocket loop hypothesis (the gate brief's "fake node simply
  takes longer than 90 s" branch): the fake node serves every spec through
  one connection and one frame-processing loop, so a stall from another
  spec's parked frames was considered. No such stall was reproduced — the
  pair/10× loops showed no late card once the projection guard was added,
  and direct `GET /v1/instances` probes during concurrent create load never
  observed a permanently missing row. More importantly, the client-side
  `waitForProjectedInstance` poll makes attempt 1 deterministic on its own:
  it blocks until the server projection converges (connected + exact
  activity), so attempt 1 can no longer time out and hand a
  state-inheriting retry the missing-row window — the retry path the gate
  saw is not entered at all, regardless of WS-loop scheduling.
- Full hub e2e suite once on the fixed tree (CI=1, fresh hub, 173 tests,
  one worker, ambient host load ~4–6): 132 passed, 34 skipped, 5 did not
  run; 2 failed, both pre-existing environment-config tests that require an
  external operator dev Node and env vars that are not set on this machine —
  `hub-live.spec.ts:261` needs HUB_E2E_WORKSPACE/HUB_E2E_HOST_ID,
  `spaces-hub-live.spec.ts:165` needs HUB_E2E_HOST_ID/HUB_E2E_SPACE_PRIMARY/
  HUB_E2E_SPACE_SECONDARY (they assert those vars truthy and fail identically
  on clean main; neither file is touched by this change). Both fixed specs
  passed in the suite (artifact dirs contain only native-node.log, no
  error-context).
- `pnpm --dir web typecheck` green; lint green; `pnpm --dir web test`
  1380/1380 green; `cargo test -p remuda-hub` 423/423 green;
  `bash scripts/ci/secret-scan.sh` pass.

## 4. Final numbers

- ux-nextstep, AFTER: 8× execution-order pair loop 8/8; dedicated 10×
  CI=1 loop 10/10.
- grok-structural, AFTER: gate-like 10× loop 10/10 (full-file, both tests).
- Full hub suite: 132 passed / 2 pre-existing env-var-only failures
  (external dev Node), see §3.

