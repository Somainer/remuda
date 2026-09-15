# Merge queue evidence — two live failures from the first production night

Date: 2026-09-15. Branch: `wt/r-mergequeue2/queue-robustness` (rebased onto
`e2c919e` after the worktree loss; design identical to the `3542b24`
implementation, verified on both bases). Everything ran
in a throwaway clone under scratch storage, a bare `origin.git`, and two git
worktrees (`wt/a/work`, `wt/b/work`, `wt/c/work`) — never the coordinator
checkout, live worktrees, or target-gate* dirs. The gate was the real
`scripts/ci/gate.sh` with `REMUDA_MERGE_GATE_COMMAND` replacing each step
with an executable test seam, exactly like
[merge-queue-1.md](merge-queue-1.md).

## Live evidence that prompted this change

1. **Gate driver parked in `wait4`.** On 2026-09-15 ~04:07–05:30 UTC a lane
   driver process (`python3 -B -`, the embedded gate) sat for 1 h 19 min in
   syscall `wait4`, state S, with zero child processes — last recorded step
   `cargo-clippy ok`, report stuck at `<base>.preparing.json`. Another gate
   process on the same host was terminated around then; the step's child
   (`cargo clippy`) had been killed externally while the driver blocked
   forever waiting for a reap that would never arrive.
2. **Queue parent parked in a futex.** After the coordinator killed that
   driver, the parent `remuda merge --queue` (5 branches, 2 lanes) ended up
   sleeping on syscall 202 (`futex`) with zero children and never returned:
   every lane verdict existed but no further lane could be signalled. A
   re-run lost all speculative lane work.
3. **Two independent gates on one checkout.** A scripting error ran two
   gates against the same `--repo`/`--target-dir`; failure surfaced deep in
   the gate as "local main is behind" and "webServer exited early" instead
   of as an obvious "another merge is running" refusal.

## Fixes

1. `scripts/ci/gate.sh` + `scripts/ci/gate_supervisor.py`: per-step
   wall-clock timeouts (`cargo-*` 30 min, `web-*` 20 min, `web-hub-e2e`
   30 min; `REMUDA_GATE_STEP_TIMEOUTS` override, `0` disables), steps in
   their own process groups, a 200 ms child-loss poll (`/proc` or
   `kill(pid, 0)`), SIGTERM→3 s→SIGKILL group teardown, and
   `reason: timeout|child-lost` in `gate.jsonl`. A `PR_SET_PDEATHSIG` guard
   tears the active step group down when the parent lane is killed.
2. Queue driver: a signal death, a missing/unusable final report, or a lane
   whose monitor goes silent while its pid is gone is a failed verification
   (`"lane died: …"`), not a lost channel. A watchdog observes lane pids
   (and `/proc` zombie state) on a timed receive, kills the whole lane
   descendant tree after a reap grace, and synthesises the verdict. The
   driver loop additionally settles terminal failures *before* dispatching
   so a dependent branch re-verifies onto the real main in the same
   iteration (the exact ordering that parked with zero in-flight lanes) and
   refuses to wait when nothing is in flight.
4. Scratch safety (post-incident hardening): isolated gate worktrees
   now live in `std::env::temp_dir()/remuda-mq-<pid>-<n>-<nanos>/`, never
   under the checkout, and every deletion goes through a containment-checked
   `scratch::remove_within(root, path)` that refuses `..`, symlink escapes,
   sibling scratch roots, and caller-supplied ancestors. The killed-lane
   sweep is bounded to the exact pids the queue spawned and to worktrees
   registered against that repo, so two queues sharing a host can never
   delete each other's scratch. Unit-tested in `scratch.rs`.
3. Advisory locks: `<git-common-dir>/remuda/merge.lock` and
   `<target-dir>/.remuda-merge.lock` via `flock(2)`; contention exits 3
   `locked` with `another remuda merge is running on this repo (pid N,
   since T)`; `--wait` blocks. The queue parent holds the repository lock
   and lanes (`REMUDA_MERGE_QUEUE_WORKER`) lock only their per-lane target
   directories.

## Scenario A — externally SIGKILLed lane: branch fails, others land

`wt/a/work` carries `queue-hang.txt` so its gate parks inside
`cargo-test`; b and c are docs-only. A three-branch, two-lane queue is
started, and once lane a is inside the hanging step the test sends
`SIGKILL` directly to the lane's `remuda` process (no report can be
written; the Python parent-death guard takes the gate group down). The
queue returns exit 1 with:

```text
branches[0] wt/a/work  gate_failed
  verifications[0] base=<main0> lane=1 status=failed
  reason="lane died: killed by signal 9"
  why="lane died: killed by signal 9; branch not landed"
  landedSha=null
branches[1] wt/b/work  landed   (speculative pass, then real pass on main)
branches[2] wt/c/work  landed   (speculative pass, then real pass on main)
refs/remuda/merge/* : (none — pins and preparing sidecars swept)
OS temp dir        : (no remuda-mq-* scratch roots for the queue's own pids)

Captured output of the real-binary throwaway run (distinct b.md/c.md so
both followers land):

```text
queue pid=1152153 killed lane pid=1152172
queue rc=1
exit 1 queue_gate_failed
wt/a/work gate_failed - | lane died: killed by signal 9; branch not landed
     failed lane 1 lane died: killed by signal 9 real
wt/b/work landed e2e3f367 | re-verified on main after main moved; landed
     passed lane 2 None spec
     passed lane 2 None real
wt/c/work landed bdc5e006 | re-verified on main after main moved; landed
     passed lane 1 None spec
     passed lane 1 None real
queue: lane died: killed by signal 9
pins: 0  data: absent  scratch: 0
```
```

Captured by
`queue_killed_lane_is_a_failed_branch_while_others_land` in
`crates/remuda/tests/merge_queue_cli.rs` (7.5 s, real SIGKILL on a real
lane pid). The queue process itself exits; it is never left sleeping in a
futex.

## Scenario B — wedged lane reaped by the watchdog

With the same hanging branch on one lane and
`REMUDA_QUEUE_WATCHDOG_SECS=3` / `REMUDA_QUEUE_REAP_GRACE_MS=1000`, the
watchdog notices the lane has exceeded the deadline, kills the lane group
and its descendant gate/stub processes, and records the failure itself
after the reap grace:

```text
queue: lane 1 (pid <pid>) exceeded the watchdog deadline; waiting 1s for its monitor
queue: lane died: lane process was gone or killed past the grace period and its monitor never reported
exit 1, branches[0] gate_failed, main unchanged at <main0>
```

Captured by `queue_watchdog_kills_a_hung_lane_and_the_queue_completes`
(~3.3 s wall clock; without the watchdog the pre-change driver parked
indefinitely, reproduced against this same fixture before the fix).

## Scenario C — concurrent run exits 3; --wait proceeds

A first `remuda merge --gate` hangs holding the repository lock. A second
invocation exits immediately:

```json
{"exitCode":3,"status":"locked",
 "error":"another remuda merge is running on this repo (pid <pid>, since 2026-09-15T..Z)"}
```

The same invocation with `--wait` blocks; when the holder is killed the
waiter acquires the lock, verifies its docs-only branch, and lands it
(exit 0). Captured by
`concurrent_merge_exits_three_and_wait_runs_after_the_lock_frees`.

## Scenario D — gate step timeout and child-loss at the driver

`scripts/tests/test_gate_supervisor.py` (run with
`python3 -B -m unittest discover -s scripts/tests`):

- `test_timeout_kills_the_whole_step_process_group` — a step that ignores
  SIGTERM and hangs is killed by the group SIGKILL escalation at a 0.5 s
  budget; no process remains in the step group.
- `test_timeout_reaps_grandchildren_inside_the_group` — a forked
  grandchild in the same group is reaped too.
- `test_child_lost_when_pid_vanishes_without_a_reap` — a fake `Popen`
  whose direct pid is gone but whose `poll()` never returns yields
  `child-lost` immediately instead of an indefinite wait.
- `test_hanging_step_times_out_and_fails_the_gate` — the real
  `gate.sh --web-only` with a hanging `web-install` stub (1 s override)
  records `web-install failed reason=timeout`, skips later steps, and exits
  1; the setsid'd grandchild orphan does not hold the gate's pipes open.
- Timeout-config unit tests cover the default table, JSON overrides, and
  the `0 = disabled` semantics.

## Automation

The four scenarios plus the [merge-queue-1.md](merge-queue-1.md) cases
(pass/pass speculation and reuse, semantic conflict, fail/pass
re-verification, `--onto`/`--land`, base moves) all run as
`cargo test -p remuda --test merge_queue_cli` (10 tests) and
`--test merge_cli` (26 tests) from both a `/tmp` checkout and a `$HOME`
checkout. The optimistic state machine's pure transitions stay unit-tested
in `queue.rs`. No gate build, worktree, merge pin, or report state touched
the coordinator checkout; the throwaway clones and gate target dirs were
removed after the run.
