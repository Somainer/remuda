# Evidence: flakes-1

Date: 2026-09-20. Branch: `wt/c-flakes/b-flakes-md` (item 2 shipped first on
`wt/c-flakes/effortslider` as `b285602c`, then merged into this branch).

The landing gate runs the full web hub e2e suite and the Rust suites on a
loaded shared host (one hub process for all specs, workers 1). Over
2026-09-19/20 several tests failed intermittently and each passed on re-run or
in an idle worker environment. This file records the failure signature, the
root cause, the fix and the verification result per item. No blanket timeouts,
no config retries, no skipped tests: every assertion now waits on the state it
documents, and the two product defects (fence-node remount, accepted-event
drain) have product fixes.

## 1. ux-code evidence screenshot loops — code-block node replaced after the theme switch

**Signature.** `ux-code.hub.spec.ts` both evidence loops (1440px "fenced code:
hover toolbar, copy, highlighting and wrap" and 390px "toolbar stays visible
with 44px targets and no page-wide overflow") failed at
`locator.screenshot` with `Element is not attached to the DOM` /
`element is not stable` right after the `data-theme` switch, immediately after
`expect(toBeVisible)` passed. Seen at 21:34 (390px loop) and 22:09 (1440px
loop) on main d1f4fe95, and still at 00:26 on main 83ad6d8e after the
two-equal-heights settle helper landed.

**Cause.** A jsdom probe showed the `[data-testid="code-block"]` DOM node was
a *different element* after every `MarkdownText` re-render
(`BLOCK_SAME false`); re-rendering `CodeBlock` alone kept identity
(`A_SAME true`). `MarkdownText.renderWithFileMentions` passed react-markdown
a `components` object rebuilt per render whose `pre` override was an inline
arrow function, also rebuilt per render. react-markdown renders overrides by
their function identity, so the new `pre` type made React unmount and remount
the whole fence subtree (`FencedCode` → `CodeBlock`) on every transcript
re-render. The remount restarted `CodeBlock`'s lazy highlighter effect; when
its async token HTML landed, the block node swapped — and any transcript
re-render while the screenshot was being prepared detached the node that
`toBeVisible` had just resolved. The two-height poll could not help: it held a
locator (re-querying) but the final screenshot used the same handle after a
remount could have started. (The theme switch itself is not observed by app
code; it only coincides with the steady stream of live transcript renders.)

**Fix (product + test).**
- `MarkdownText.tsx`: the `pre` override is now a stable module-level
  component (`FenceMarkdownPre`), so the fence subtree and the `CodeBlock`
  instance (highlight state, wrap/copied state) survive re-renders.
- `MarkdownText.test.tsx`: a unit test renders the two-fence fixture, waits
  for highlighting, re-renders twice and asserts both code-block nodes keep
  DOM identity. It fails on the pre-fix code (`expected <div …> to be <div
  …>`).
- `ux-code.hub.spec.ts` `settleAndShootCodeBlock`: re-location, toolbar
  visibility, height settle and the screenshot now run inside a single
  `expect(...).toPass({ timeout: 20_000 })`; a detaching node or late layout
  wave re-arms the whole attempt instead of pinning a stale handle.

**Five-run result under load.** `ux-code.hub.spec.ts` (both tests) passed
5/5 under the b lock with the dedicated ports while the cargo-build load loop
ran (host load average 10–26): 2 passed each run at 38.7–39.7 s.

## 2. EffortSlider "Enter picks the focused model row" — End raced the open-list focus ride

**Signature.** `EffortSlider.test.tsx` tall catalog case "Enter picks the
focused model row; Escape closes back to the slider": `onChange`/`onModel`
received no `gateway/model-79` call (0 calls), once per 1194-test run, and it
failed three of four landing gates on 2026-09-19/20 (vitest has no retry).

**Cause.** Opening the list schedules the initial focus ride on
`requestAnimationFrame` (`EffortSlider.tsx`), focusing the selected tier row.
The test pressed End then Enter immediately after the opening click. Under
load the frame callback landed *after* End: with focus still on `document.body`,
the listbox's `onKeyDown` never saw End, the late focus ride then focused the
tier row, and Enter activated that tier row — a tier pick calls `onChange`
and closes the list, so the model row's `onModel("gateway/model-79")` never
fired. A real user cannot key before the next paint; only the test could win
that race. The sibling arrow-key test already waits for the initial focus.

**Fix (test).** Wait for the documented focus state after each key:
`effort-tier-high` focused after open, `model-option-model-79` focused after
End, then Enter. No component change needed — focus cannot legitimately land
anywhere else (the ride targets the selected tier, and every list keystroke is
driven by `focusRow`).

**Five-run result.** `pnpm --dir web test run src/features/session/EffortSlider.test.tsx`
passed 5/5 (20/20 each) before the branch was pushed; the full web unit suite
passes (1233 tests). Shipped first as `b285602c` on
`wt/c-flakes/effortslider`.

## 3. dispatcher restart test — accepted prompt dropped because events closed before the readers flushed

**Signature.**
`crates/remuda/src/cmd/dispatcher.rs`
`recorded_inbound_restarts_consume_and_drains_accepted_dispatches`:
assertion "the accepted next prompt must drain before shutdown" failed
`left: 0, right: 1` on the loaded gate.

**Cause.** The fake consume child writes every replayed NDJSON line (the
accepted `/status`, prompt and `continue`) and only then installs the state
the test waits on, so the bytes are already flushed before shutdown. But
`supervise()` sent the `stopping` watch (which made `drive()` call
`mpsc::Receiver::close()`) **concurrently** with `supervisor.shutdown()`,
which SIGTERMs the children and joins the reader tasks. On an idle host the
readers were usually already scheduled and had enqueued everything; on a
loaded host a reader could still be unscheduled when `drive()` drained the
buffer and saw the channel end — the accepted `continue` send was never
processed. The only thing reliably pushing it through in the test was the
20 ms follow interval passed to `supervise` (a wall-clock nudge, not a
signal): the 20 ms deadline occasionally beat the buffered `continue` into
the select arm. `left 0 right 1` was the lost send.

**Fix (product + test).** Shutdown now drains accepted stdout structurally:
- `ConsumeSupervisor` (`remuda-feishu/src/consume.rs`) gains `terminate()`
  (SIGTERM every child without waiting) and `join()` (wait for the reader
  workers, 30 s loaded-host budget); `shutdown()` is now terminate + join.
- On the shutdown signal a reader SIGTERMs its child, **drops the child's
  stdin**, and keeps reading its stdout to EOF — bounded at
  `SHUTDOWN_DRAIN` = 8 s so a child ignoring SIGTERM cannot stall shutdown —
  enqueuing every accepted line before its worker exits. stdin is closed
  only *after* the SIGTERM syscall has been issued (the consume contract is
  "SIGTERM first, never EOF first", covered by the
  `sigterm_does_not_close_stdin_first` integration test); closing it before
  the drain rather than after the reaping means a child that exits on stdin
  EOF still leaves promptly instead of holding the drain for 8 s.
- The drain races ONLY `read_line` against the 8 s deadline: a `child.wait()`
  arm was removed because `terminate()` has already sent SIGTERM, and a
  biased `wait()` arm would win on an already-reaped child and return with
  buffered pipe bytes unread — which was the residual form of the original
  flake. `Ok(0)` is reached only when the child has exited and closed its
  stdout, i.e. after every buffered byte was consumed. The child is then
  reaped unconditionally with a 2 s wait bound.
- `supervise()` arms the draining state (follow polling stops; receiver left
  OPEN), terminates the children, then `join()`s the reader workers before the
  drive worker can finish: all reader senders drop only after the channel
  already holds every accepted event, which `drive` then processes before
  `recv()` returns `None`.
- The test no longer passes a 20 ms interval (1 s; it only bounds idle
  follow polling now), and the direct-`drive` unit test drops its event
  sender on stop, mirroring the real shutdown contract.

This is deliberate **production** behaviour in `remuda-feishu`, not a test
nudge: accepted Feishu events must survive a shutdown. Named budgets:
post-SIGTERM stdout drain 8 s per child, reader `join()` 30 s, post-drain
reap 2 s — only a child ignoring SIGTERM or stuck in the kernel can consume
them; a well-behaved child hits EOF immediately and close is not delayed
beyond the old behaviour except for the (now-correct) flush of its accepted
lines. The flaky test itself no longer times any of this.

**Result under load.** The test was repeated 30 times back-to-back while
`cargo build --workspace --tests` hammered the host: 30/30 passed (slow runs
~5–9 s, proving the drain is exercised, not skipped); after the round-2
drain rewrite (read-only drain arm) another 25/25 under load. Full
`cargo test -p remuda -p remuda-feishu` passes.

## 4. hub-live / ux-steer-queue / ux-modelsync — waits that could pass before the state existed

**Signatures (gate history).** `hub-live.spec.ts` queue/steer test (line ~426
on d1f4fe95) and `ux-steer-queue.hub.spec.ts` (line ~144) intermittently
timed out waiting for working/idle session state or queued rows; the
ux-modelsync typed/read-back assertions (pending clears, current model,
mismatch text, toast, queued tag) failed when the picker opened before the
fake node's verdict reached the web's journal window.

**Cause — approval helpers.** `answerPendingApprovals` (hub-live) and
`answerPending` (ux-steer-queue) polled `/v1/interactions` and finished as
soon as the pending count read 0. The launch approval is journaled
concurrently with the create response: an immediate empty read satisfied the
wait **before the approval existed**, so nothing was ever answered and the
fake node never sent the idle/working lifecycle the rest of the test waited
on.

**Cause — model read-back.** Three modelsync cases read picker attributes
immediately after the configure POST or the `/model:` send with a plain 5 s
attribute timeout; under gate load the verdict/observation could still be in
flight, so `data-model-current`/the toast/`data-model-pending` were read too
early (the mismatch case already polled the journal; the others did not).

**Fix (tests only).**
- The approval helpers (hub-live `answerPendingApprovals`, ux-steer-queue
  `answerPending`, and touchhit `clearApprovals` in item 6) now wait until an
  interaction for the instance EXISTS in ANY state (20 s) before proceeding,
  answer whatever is still pending, then wait for the pending list to clear.
  "Exists, not merely pending" matters: the hub-live hook-approval test
  answers its launch card on `/approvals` before calling the helper, so at
  that point the interaction is already `answered` — the first revision of
  this fix waited for *pending > 0* and failed the full suite at that test;
  `GET /v1/interactions` returns answered rows, so existence is the right
  "the launch approval was created" predicate and tolerates a pre-resolved
  card.
  Round 2 made the clear self-healing: the answer step now runs INSIDE the
  clearing poll, which re-lists the pending set every attempt and answers
  anything still open (an id is retried until its POST succeeds), so a
  late-populated options array, a rejected answer, or a second approval
  cannot strand the launch until the 20 s timeout.
- `ux-modelsync` gains `waitForJournalFragment`, which waits on the
  `/journal` state the picker renders from: `model-queued` before the queued
  case, `model-degraded` before the not-found toast, and the `"slash"`
  model observation before the terminal-/model case opens the list. The toast
  and read-back attributes carry explicit 10 s budgets.

**Five-run result under load.** Under the b lock with the dedicated ports
while the cargo-build load loop ran: `ux-steer-queue.hub.spec.ts` passed 5/5
(1 test, 30.3–31.6 s); `ux-modelsync.hub.spec.ts` passed 5/5 (5 tests,
49.5–53.7 s); `hub-live.spec.ts` passed 5/5 clean (10 tests — the spec file
also runs the passkey and spaces-hub-live tests — 1.4–1.5 min each). The
hub-live 5× was repeated on the final code after the first three matrix
repetitions exposed the hook-approval exception (a card answered on
`/approvals` leaves `interaction.list` entirely, so the generic helper's
existence wait timed out and the failed test's uncleaned instance then 422'd
spaces-hub-live within the same invocation); the fix removed that redundant
helper call, and runs 4–5 plus the clean 5× all passed 10/10. A first
full-suite run on the interim "pending must appear" version of the hub-live
helper exposed this; it led to both the existence-in-any-state predicate
recorded above and the hook-test correction.

**Full hub suite once.** With all fixes in place, the full hub suite passed
under the b lock with the dedicated ports while the cargo-build load loop ran:
**131 passed, 16 skipped, 0 failed, 0 did-not-run** (~18.4 min).

## 5. Report only — mock Playwright suite failures on clean main

Report only: no fixes here, this is the input census for the separate mock-suite
task. Census taken on clean main **83ad6d8e** with `web/playwright.config.ts`
(two projects, `CI` unset → zero retries, fully parallel, mock vite on 4177).
Conditions match the gate host: the box was under a background
`cargo build --workspace --tests` loop during the chromium run. chromium used
the bundled browser (`PW_CHANNEL=chromium`, Chrome is not installed at
/opt/google/chrome on this host); mobile-webkit ran in the
`mcr.microsoft.com/playwright:v1.63.0-jammy` container (Playwright cannot
install webkit on this Ubuntu 20.04 host) against a host vite for the clean
tree. Single runs: **chromium 23 unexpected / 91 passed / 13 skipped**,
**mobile-webkit 25 unexpected / 78 passed / 24 skipped** (48 failing specs
total; consistent with the "about 45" the earlier c-sessionchrome run saw).

### chromium (23, 8 files) — first failure line per spec

- `agent-board.spec.ts:8` — shows kind badges, worktree, status triple, snippet, and DONE — `expect(locator).toBeVisible() failed`
- `agent-board.spec.ts:23` — quick send, keys, and stop — `Test timeout of 30000ms exceeded`
- `agent-board.spec.ts:37` — fleet toolbar broadcasts to selected instances — `Test timeout of 30000ms exceeded`
- `composer-effort.spec.ts:188` — effort popover is a snapping six-stop slider… — `expect(locator).toHaveAttribute(expected) failed`
- `composer-effort.spec.ts:248` — the ultracode stop sits past max… — `expect(locator).toHaveAttribute(expected) failed`
- `composer-effort.spec.ts:383` — approval card and expanded effort menu do not overlap — `expect(received).toBeTruthy()`
- `composer-effort.spec.ts:407` — New Session permission chips stay single-line at 390 and 1440 — `expect(locator).toHaveCount(expected) failed`
- `composer-effort.spec.ts:492` — structured Grok session shows four chips… — `expect(locator).toContainText(expected) failed`
- `new-session.spec.ts:125` — kind terminal uses shell-pty and opens the terminal tab — `expect(locator).toHaveAttribute(expected) failed`
- `new-session.spec.ts:146` — effort is the inline layout-A slider… — `expect(locator).toContainText(expected) failed`
- `new-session.spec.ts:219` — the ultracode stop creates with the ultracode wire name… — `expect(locator).toContainText(expected) failed`
- `session-structured.spec.ts:213` — the files route still has a back affordance at phone width — `expect(received).toBeGreaterThanOrEqual(expected)`
- `session-structured.spec.ts:61` — blocked session pins approval on composer — `expect(locator).toBeDisabled() failed`
- `session-structured.spec.ts:72` — AskUserQuestion form takes over composer — `expect(locator).toBeDisabled() failed`
- `session-structured.spec.ts:82` — composer send on idle session — `Test timeout of 30000ms exceeded`
- `session-structured.spec.ts:93` — terminal sessions get one segmented switch… — `expect(locator).toHaveAttribute(expected) failed`
- `session-structured.spec.ts:120` — the segmented switch moves with the keyboard — `expect(page).toHaveURL(expected) failed`
- `session-structured.spec.ts:152` — 文件 is a toggle and the files route goes back to the session — `expect(page).not.toHaveURL(expected) failed`
- `session-virtual.spec.ts:221` — reading position and follow state restore after navigating away and back — `expect(received).toBeLessThan(expected)`
- `session-virtual.spec.ts:251` — failed and denied tools stay visible inline, immune to collapse-all — `expect(locator).toHaveCount(expected) failed`
- `spaces.spec.ts:52` — mock spaces remember tabs, names, order and panel state… — `expect(received).not.toContain(expected) // indexOf`
- `ux-status.spec.ts:202` — a blocking error stays visible after a later success — `expect(locator).toBeVisible() failed`
- `workflow-card-evidence.spec.ts:25` — 1440 / 768 / 390 captures — `expect(locator).toBeVisible() failed`

### mobile-webkit (25, 10 files) — first failure line per spec

- `agent-board.spec.ts:8` / `:23` / `:37` — same three chromium agent-board failures (`toBeVisible() failed`; two `Test timeout of 30000ms exceeded`)
- `composer-effort.spec.ts:188`, `:248`, `:383`, `:407`, `:492` — same five chromium composer-effort failures (`toHaveAttribute` ×2, `toBeTruthy`, `toHaveCount`, `toBe(expected) // Object.is equality`)
- `new-session.spec.ts:125`, `:146`, `:219` — same three chromium new-session failures (`toHaveAttribute`, `toContainText` ×2)
- `session-structured.spec.ts:213`, `:61`, `:72`, `:82`, `:93`, `:120` — same six chromium session-structured failures (the `:152` URL case passed on webkit)
- `session-virtual.spec.ts:204` — searching performs no network writes — `Test timeout of 30000ms exceeded` *(webkit only)*
- `session-virtual.spec.ts:221`, `:251` — same two chromium session-virtual failures (`toBeLessThan`, `toHaveCount`)
- `session-workflow.spec.ts:26` — raw events drawer filters by kind — `Test timeout of 30000ms exceeded` *(webkit only)*
- `spaces.spec.ts:52` — same chromium spaces failure (`not.toContain // indexOf`)
- `tabs-semantics.spec.ts:43` — status and close are distinct… — `Test timeout of 30000ms exceeded` *(webkit only)*
- `ux-filters-evidence.spec.ts:103` — workbench A filter evidence at 1440 / 768 / 390 — `Test timeout of 30000ms exceeded` *(webkit only)*
- `ux-status.spec.ts:202` — same chromium ux-status failure (`toBeVisible() failed`)

Shared core (fails on both projects): agent-board (3), composer-effort (5),
new-session (3), session-structured (6), session-virtual (2), spaces (1),
ux-status (1) = 21 specs. WebKit-only: session-virtual:204,
session-workflow:26, tabs-semantics:43, ux-filters-evidence:103 (4).
Chromium-only: session-structured:152, workflow-card-evidence:25 (2).

## 6. ux-touchhit — failed retry plus the fake node's 8-instance budget

**Signature (01:35, main 83ad6d8e + the EffortSlider fix).**
`ux-touchhit.hub.spec.ts:327` "Stop is a reachable 44px target on the 390px
viewport": after `createClaudeSession` navigated to `/s/<id>/structured`, the
`session-page` testid was not found within 5 s, on both the attempt and its
CI retry; the header test at line 251 passed only on retry; two later specs
did not run.

**Cause — confirmed mechanism.** The fake node advertises `maxInstances: 8`
and every spec in the hub config shares one hub process. When the host is
full, `POST /v1/instances` answers 422 `PLACEMENT_UNSATISFIABLE`, but the app
still navigates to `/s/<id>` — the session never mounts, surfacing minutes
later as a missing `session-page` testid. With `CI=1` the hub config retries
once: a failed attempt's afterEach teardown can land after the retry's
re-create, and leftovers from earlier specs accumulate toward the cap.
(ux-status and ux-code already isolate against this by raising the cap;
touchhit did not.)

**Fix (tests only).**
- `createClaudeSession` waits for and asserts the create response and reads
  its body ONCE into a string (the assertion message and the id parse share
  it — `res.text()`/`res.json()` consume the stream), so the id is tracked
  for cleanup even if the navigation afterwards fails.
- The file raises the fake-node cap to 24 in beforeEach (same fixture-level
  isolation as ux-status/ux-code) and restores the previous value in afterAll,
  after a force-delete sweep.
- `clearApprovals` waits for an approval for the instance to exist (in any
  state) before resolving the pending one.

**Five-run result under load.** `ux-touchhit.hub.spec.ts` (all four tests)
passed 5/5 under the b lock with the dedicated ports while the cargo-build
load loop ran: 4 passed each run (46.8 s–1.5 min).

## 7. remuda-driver close ladder — SIGKILLed child unreaped when the host is loaded

**Signature (04:13, Rust step retry on main 54d825ae, host load average
> 10).** `crates/remuda-driver/tests/claude_sdk_process.rs:601`
`close_kills_a_child_that_ignores_both_eof_and_sigterm` panicked with
`pid <n> survived a close that had to reach SIGKILL`; the test read
`process_alive(pid)` immediately after `driver.close()` returned.

**Cause.** The stop ladder (`shell_pty/lifecycle.rs`,
`shell_pty.rs::stop_tree`) reaches group SIGKILL, then waits for the direct
child to become reapable — a SIGKILLed child sits as a zombie until the
parent `wait`s, and a zombie still answers `kill(pid, 0)`. That wait was
capped at `REAP_EXITING_GRACE` = 3 s. On the loaded gate host the child can
take more than 3 s to be scheduled through the kernel exit path and become
a zombie the reaper reaches: the deadline expired, `close` returned, and the
immediate liveness assertion saw a still-present pid. A fixed 150 ms sleep
before the grandchild assertion had the same shape.

**Fix (product + test).**
- `REAP_EXITING_GRACE` is 30 s, matching the c-testbudget loaded-host
  shutdown/reap budgets. The wait still returns the instant `try_wait`
  succeeds (the normal transition is milliseconds), so a healthy close is
  unchanged; only a genuinely slow exit gets the room it needs instead of
  being abandoned as an unreaped child for the Node's lifetime.
- The test now waits on the observable state — both the direct child and the
  SIGKILLed grandchild are polled with `process_alive` on a 25 ms cadence up
  to a 30 s bound instead of an immediate read / fixed 150 ms sleep. The
  outer close timeout is 60 s (the configured rungs sum to ≈34.5 s worst
  case: 2 s + 2 s signal rungs, 0.5 s SIGKILL window, 30 s reap), and the
  "ladder is bounded" assertion names that ceiling.

**Result under load.** `cargo test -p remuda-driver --test
claude_sdk_process` passes (11 tests); the kill test was repeated 25 times
alongside 25 repetitions of the dispatcher drain test (50 reps total) while a
`cargo build --workspace --tests` loop held the host at load average 10–20:
50/50 passed, zero failures.

## Verification environment

Hub specs five times each under the shared b lock with dedicated ports
(127.0.0.1:59190 hub, 59199 web, 59191 upstream) while a
`cargo build --workspace --tests` loop loaded the host; bundled Chromium
(`PW_CHANNEL=chromium`). Five-by-five: ux-code 2/2, ux-touchhit 4/4,
ux-steer-queue 1/1, ux-modelsync 5/5 and hub-live 10/10, each 5/5 green.
The full hub suite once on the final code: 131 passed, 16 skipped, 0 failed.
Round 2: the three hub specs re-touched in review passed 3/3 each under the
same lock/ports/load — hub-live 10/10 ×3, ux-steer-queue 1/1 ×3,
ux-touchhit 4/4 ×3.
Rust: `cargo test -p remuda -p remuda-feishu` (all green),
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo fmt --all -- --check`. Under load the two shutdown tests were each
repeated 25 times after the round-2 rewrites (dispatcher drain and
remuda-driver SIGKILL ladder): 50/50, zero failures, on top of the earlier
30/30 dispatcher run. Web: `pnpm --dir web test` (1233 passed),
`pnpm --dir web typecheck`, `pnpm --dir web lint` (no new warnings).
