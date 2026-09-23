# Evidence: flakes-4

Date: 2026-09-24. Branch: `wt/c-authrace/b-hublive-md`. Base: main
`35f14e33`.

Two `web/tests/e2e/hub-live.spec.ts` tests failed two unrelated landing gates
on 2026-09-23 — the 18:00 c-inboxperf run and the 23:37 c-uo0 docs-only run
(so the failures are pre-existing on main):

- `:425` "native PTY default from the host matrix, with both projections" —
  after `new-session-start` the URL stayed `/sessions/new` past 20 s
  (`:447`); passed on retry.
- `:462` "composer queue / steer / interrupt states on a working native
  session" — `composer-interrupted-chip` not visible within 5 s (gate
  `:538`); failed even after retry.

Reproduction on clean main with no induced load
(`--repeat-each 10 --workers 1`, each test filtered alone, one long-lived
fake Hub/Node): **both tests failed 2 of 10**, in both cases at the create
`toHaveURL(/\/s\//)` (repeats 9–10). The failure is deterministic given a
server that has run long enough, not a CPU-load artifact.

## Flake E — leaked live instances hit the shared fake host's fixed `maxInstances: 8`

**Signature.** The sheet never leaves `/sessions/new`; the start button is
back to "开始" and an inline alert is rendered:

```
PLACEMENT_UNSATISFIABLE · placement unsatisfiable · hst_…: at maxInstances 8
(8 live); raise it with PATCH /v1/hosts/hst_…
```

(Playwright error-context snapshot captured on repeat 9.)

**Root cause (test hygiene).** The in-process fake Node advertises a fixed
`maxInstances: 8` (`crates/remuda-hub/examples/hub_e2e.rs`); placement
(`crates/remuda-hub/src/placement.rs`) refuses an over-cap create with 422
once 8 rows are in a live lifecycle. `hub-live.spec.ts` creates an instance
in seven of its eight tests but only two of them delete theirs, and the five
gate files that run before it leave more. Instances only leave the live set
on an explicit `DELETE …?force=1` (the fake's idle `agent_status` does not
exit the lifecycle), so the count is monotonic within a server lifetime. By
the second half of the file the next create is refused; the New Session
sheet shows the definite 4xx inline (per `isDefiniteFailure`, 400–499
re-enables the form) and never navigates — which Playwright attributes to
the spec's `toHaveURL` line. Retries fail identically once the cap is
actually full; the gate's "passed on retry" runs hit Flake F below while
headroom still existed.

**Fix (test owns its instances; no product or fixture change).**
`hub-live.spec.ts` gains a `beforeAll`/`afterEach` sweep that force-deletes
every instance on the `e2e-fake-node` host — the same hygiene shape
`m-realdevice`, `m-inbox`, `ux-steer` and `cua-media` already use — scoped to
that label so the external-Node flow is never touched (the hooks early-return
under `HUB_E2E_EXTERNAL=1`). The fixture's `maxInstances: 8` is a real
product ceiling other specs deliberately exercise (ux-touchhit,
session-virtual, task-model-boardui PATCH it deliberately), so it is not
raised.

## Flake F — UI waits for post-landing HTTP reconciliation instead of the landing itself

**Signature.** The sheet transiently stays on `/sessions/new` past 20 s even
though the create landed (retry then passes, with the new instance visible
in the list); and after a 插队 steer the 已打断 chip is not visible within
5 s even though the `mode=steer` POST returned — the receipt is raised only
when that POST's promise resolves in the UI.

**Root cause (product latency coupling, not a race in the chip).** The
09-21 flakes-3 fix stopped the held-flush phase edge *clearing* the chip; a
second seam remained in *raising* it. Both promises the UI awaits include
HTTP work that happens strictly after the server action landed:

- `store.create` awaited `this.refresh()` (instance list + interaction list
  GETs) before returning, and `NewSessionPage` calls `navigate(/s/:id)` only
  after that promise settles. The create response already carries the
  instance the route mounts.
- `store.send` awaited `catchup()` (journal resync read + refresh's two
  GETs), bubble settle and `refreshScreen()` (a `tty.screen` node RPC with
  its own 5 s budget) before resolving. SessionPage raises the 已打断
  receipt on this resolution, so the receipt waited on the whole chain.

Under gate load the Vite proxy / single fake node / loaded host can make
that multi-HTTP chain longer than the UI's 5 s receipt budget (or, for the
create, the test's 20 s navigation window), although the POST itself
returned quickly and the live follow socket was delivering the same state.

**Fix (product; wait on the real signal).** `web/src/lib/store.ts`:

- `create` emits the created instance from the POST response and returns it;
  the list refresh is fire-and-forget (the follow socket + the background
  refresh converge the list).
- `send` settles the bubble from events the live socket already delivered
  and resolves on the POST; journal catch-up and the screen read run in the
  background. Both background jobs are pre-existing bounded convergence
  paths (catch-up applies its REST backfill through the same
  `onEvents → settleBubbles` handler), so final state is unchanged — only
  the latency the callers wait on is. What the tests assert is unchanged; no
  timeouts were raised and nothing is skipped.

Two unit tests in `web/src/lib/store.localBubble.test.ts` pin both
resolutions with every reconciliation read hanging forever; they fail on the
old awaited code (2 s timeouts) and pass on the new code.

### Round 2 — making the fire-and-forget reconciliation race-safe

The codex+grok acceptance review rejected round 1: detaching the
reconciliation exposed three real races. All fixed in `web/src/lib/store.ts`
(`screen.ts` exposes a screen snapshot's source seq), each with a unit test
in `web/src/lib/store.reconcile-race.test.ts` (five tests, each verified red
on round-1 code):

1. **Stale list poll dropping the just-created instance.** A `refresh()`
   started before the create could resolve after it; `incoming.map(…)`
   replaced the whole list, so `/s/:id` rendered 会话不存在 on mount. List
   fetches now take a monotonic seq at fetch START (`listReqSeq` /
   `listOutstanding`); `create` pins its optimistic id against the seq in
   flight. `mergeInstanceSnapshots` keeps a pinned id missing from a
   response while any request at/before the pin is outstanding, and the pin
   releases only after a response newer than the create AND every older
   request have settled — the newer-response-witness + in-flight sweep is
   needed: either condition alone either releases too early on the stale
   response itself or retains the pin forever.
2. **Swallowed background errors / latched 重连中.** The detached
   `.catch(() => undefined)` calls hid failures and a rejection between
   `markReconnecting()` and the `live` emit latched the global connection
   indicator. `catchup()` now never leaves the latch: the live emit runs on
   every path, and failures (resume / list refresh / screen read) go through
   a `reconcileToast` advisory on the existing toast mouth — the POST action
   still resolved on its landing, and the 2 s poll self-heals.
3. **Stale `tty.screen` overwriting a newer journal screen.** Each read now
   takes a per-instance generation (a newer read supersedes a late one) and
   the journal-derived screen carries its source seq in the screen state
   (`journalSeq`); a live RPC read that began before a newer journal frame
   commits nothing when the frame landed while it was in flight (the next
   scheduled poll re-reads the buffer). Journal-derived empty fallbacks are
   deduped the same way.

## Verification

- 10x loops, `--repeat-each 10 --workers 1`, each test filtered alone, under
  `flock gate-e2e.lock`, per-worker ports
  (before 58790/58791/58799, after 58810/58811/58819), `PW_CHANNEL=chromium`,
  every server started inside the flock'd shell in its own process group
  with an EXIT-trap kill:
  - before (main): pty **8/10 pass, 2 fail**; steer **8/10 pass, 2 fail**
    (both at the create URL; repeats 9–10, PLACEMENT_UNSATISFIABLE).
  - after: pty **10/10**; steer **10/10**.
- `pnpm -C web test` — all unit tests pass including the two new pins
  (1640); `pnpm -C web typecheck` and oxlint on the changed files — clean.
- Round 2: `pnpm -C web test` — 1645 pass (five additional race pins,
  `store.reconcile-race.test.ts`, each failing on round-1 code);
  10x loops after the race fixes — pty **10/10**, steer **10/10**; whole
  hub-live file **10 passed, 1 skipped** (external-Node). Same lock/ports
  protocol (58830/58831/58839).
