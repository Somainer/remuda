# Merge queue evidence — two-lane optimistic verification

Date: 2026-09-15. Branch: `wt/r-mergequeue/optimistic-queue`
at `eefcbbab…` + the merge-queue change (`remuda 0.1.0`, rustc 1.94.1).

Everything ran in a throwaway clone under `/tmp/remuda-mq-evidence` (a
coordinator checkout, a bare `origin.git`, and two `git worktree` branches
`wt/a/work`, `wt/b/work`) — never the coordinator's live checkouts. The
gate was the real `scripts/ci/gate.sh` with a test-seam executable
(`REMUDA_MERGE_GATE_COMMAND`) replacing the build/test commands, exactly
like `crates/remuda/tests/merge_cli.rs`; secret/no-tunnel scans were
no-op stubs. No paid models were used.

## Scenarios

The queue summary for each run (exit code, per-verification lane/base/merge
verdict/speculation/reuse, landed sha):

```text
$ passpass: exit 0 status queue_ok
  wt/a/work   lane=1 base=ddae6f6be3 merge=943523c0ca passed speculative=False reused=True
  -> landed landed=943523c0ca
  wt/b/work   lane=2 base=943523c0ca merge=f3cabb0b61 passed speculative=True reused=True
  -> landed landed=f3cabb0b61

$ semantic: exit 1 status queue_gate_failed
  wt/a/work   lane=1 base=f3cabb0b61 merge=ba66c2e152 passed speculative=False reused=True
  -> landed landed=ba66c2e152
  wt/b/work   lane=2 base=ba66c2e152 merge=685c0e7357 failed speculative=True reused=False
  -> gate_failed landed=-

$ failpass: exit 1 status queue_gate_failed
  wt/a/work   lane=1 base=ba66c2e152 merge=3cb335b7eb failed speculative=False reused=False
  -> gate_failed landed=-
  wt/b/work   lane=2 base=3cb335b7eb merge=b4b56c7ed6 passed speculative=False reused=True
  wt/b/work   lane=1 base=ba66c2e152 merge=0f93abec99 passed speculative=False reused=True
  -> landed landed=0f93abec99
```

### 1. pass/pass — lane 2 verified on `(main+b1)` and both landed (exit 0)

`wt/a/work` adds `docs-a.md`, `wt/b/work` adds `docs-b.md`. Lane 1 verified
a onto main (`ddae6f6b…`); lane 2 verified b speculatively onto a's
constructed merge `943523c0…` **before a landed**. After a landed, b's
verification base *was* the new main (`943523c0…`), so b landed with no
second gate (`reused=true`). Final history:

```text
f3cabb0 merge: wt/b/work into main
943523c merge: wt/a/work into main
2d96b10 b
```

Lane isolation from the gate trace (per-step env): lane 1 built in
`tgate` with `HUB_E2E_LISTEN=127.0.0.1:58980`; lane 2 in `tgate-lane2`
with `127.0.0.1:58990` — separate Cargo target locks and per-lane port
pairs, sharing the one configured Playwright endpoint.

### 2. semantic conflict — the merged-tree failure is caught (exit 1)

b1 (`wt/a/work`) adds `expect-ok.txt` ("a test": cargo-test fails unless
`state.txt` says `ok`); b2 (`wt/b/work`) changes `state.txt` to `bad`.
Each branch is green alone. In the queue, b1 verified and landed
(`ba66c2e1…`). b2's speculative gate ran on the merged
`(main + b1 + b2)` tree `685c0e73…` and **failed there**, so b2 never
landed. Main stopped at b1 (`git show main:state.txt` is still `ok`), and
the command exited 1. This reproduces the 2026-09-14 class of failure the
queue must keep catching.

### 3. fail/pass — b2 is re-verified onto main after b1 fails (exit 1)

b1 carries `gate-deny.txt` and fails cargo-test on main; b2 is a docs
change. b2's speculative verification onto b1's tentative merge passed
(reused=false — its base never became main); once b1 failed, the queue
re-verified b2 onto the unchanged real main `ba66c2e1…`, that passed, and
b2 landed (`0f93abec…`). Exactly one extra verification, as designed.

### 4. `--onto` verify-only and `base_moved` on `--land` (exit 2)

`remuda merge wt/a/work --gate --onto main --no-push` reported status
`verified`, `base=0f93abec…`, main unchanged, and persisted
`.git/remuda/merge-reports/wt-a-work/0f93abec….json`. A third party then
advanced main (`git commit-tree … update-ref`). The subsequent

```sh
remuda merge wt/a/work --land --onto 0f93abec… --no-push --json
```

exited **2**, status `base_moved`, with `currentMain=1688b9c0…`, and
stderr:

```text
main moved from 0f93abec998af1df1829665a4ed43e7856ab4441 to
1688b9c0018791009f73103d66daf3b7bee52841 since verification;
re-verify onto 1688b9c0018791009f73103d66daf3b7bee52841
```

No gate ran inside `--land` (its report steps are only
`repository/preflight/land`).

## Automation

The same four cases are encoded in
`crates/remuda/tests/merge_queue_cli.rs` (plus `--onto` never advances
main, one-shot `--gate --land`, and missing-report rejection), and the
queue state machine's pure transitions (pass/pass, fail/pass re-verify,
pass/fail, third-party base move, conflict) are unit-tested in
`crates/remuda/src/cmd/merge/queue.rs`. The pre-existing
`merge_cli.rs` suite (26 tests) stays green: the single-branch
`--gate` flow is byte-compatible.

## Cleanup

The throwaway repo, worktrees, target dirs, and gate stub lived entirely
under `/tmp/remuda-mq-evidence` and were removed after the run; only this
report is repository state.
