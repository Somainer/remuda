# Evidence: gate-lane-1

Date: 2026-09-17. Branch: `wt/co-gate/gate-land-through-node`.
Design: [coordinator-hierarchy.md](../coordinator-hierarchy.md) §3.2/§8 row 6;
decision: [D-034](../decisions.md).

What shipped (batch 6 co-lanes): the verify/landing run goes through the Node on
the project's gate lane, driven by the Hub queue. The coordinator no longer
`scp`s `remote-gate.sh` or pushes main from the Mac; the lane host fetches,
fast-forwards (stale-tip refusal), runs the consumed `remuda merge --gate` CLI in
its lane checkout, and — for a land — CAS-pushes main with its own credentials.

## Surface

- Protocol `remuda-protocol/src/gate.rs`: `GateJob` (`gjb_…`), `GateStep`
  (same JSON shape as `remuda merge --gate --json`), `GateMode`
  verify/land, `GateWebMode` auto/always/never, `GateJobState`
  queued/running/passed/failed/landed/canceling/canceled, and the
  `gate.run` / `gate.cancel` / `gate.then` (Hub→Node) plus `gate.event`
  (Node→Hub) wire types. `ProjectGateLane` gains additive `env`, `lockPath`,
  `pwEndpoint`, `toolchainPath`, `timeouts` (per-step wall-clock budgets, step
  name → seconds, `0` = no cap; overrides the project-level `ProjectGate.timeouts`
  per key).
- Hub `remuda-hub/src/gatequeue.rs`: `gate_jobs` table, routes
  `POST|GET /v1/projects/{id}/gate`, `GET /v1/projects/{id}/gate/jobs/{jobId}`,
  `POST …/cancel`, `GET /v1/gate/jobs`; FIFO scheduler (parallel verify across
  lanes, serialized land per project, one job per lane); global-cas re-verify
  (re-queue on `base-moved`, ≤3 attempts); queued/running cancel; every
  transition journaled to `audit_log` (subject = job id).
- Cancel is authoritative: a `canceling` job never lands. `apply_result` skips
  the home-land handoff for a canceling job and finishes it `canceled`, keeping
  `mergeSha`/`mergeRef`/steps (retention drops the ref later, like any unlanded
  verify). The single case where main did move — a `pushFrom: lane` land whose
  Node already pushed and reports `landed`, or a `pushFrom: home` push that lands
  main before the cancel reaches its terminal write — keeps `landed` and records
  the cancel arrived too late (honesty over tidiness). `reconcile` finishes a
  `canceling` job `canceled` on restart rather than re-queueing it. A cancel
  never waits out `GATE_RUN_TIMEOUT`: `cancelRequestedAt` is stamped on the first
  request and a scheduler tick finishes the job `canceled` after a bounded grace
  (`gateCancelGraceMs`, default 30 s), after which a late run reply is ignored;
  the grace is suspended while a home push is in flight (`homeLandInFlight`), so
  the longer `HOME_LAND_TIMEOUT` push writes the honest outcome itself.
- Node `remuda-node/src/gate.rs`: lane runner — fetch, main sync, branch
  fast-forward with the stale-tip refusal (held worker worktree ff'd in place,
  diverged tip rejected), runs its **own `remuda` binary** with
  `merge <branch> --onto main --gate [--land] --json`; per-lane lock,
  process-group cancel (`SIGTERM`→`SIGKILL`), whole-run timeout, env plumbing
  (`CARGO_TARGET_DIR`, `VITE_NO_WATCH`, `PW_*`, `HUB_E2E_*`,
  `REMUDA_E2E_LOCK`, `REMUDA_GATE_STEP_TIMEOUTS`, toolchain PATH prefix); step
  results tailed from `<tmp>/remuda-mq-*/gate.jsonl` and streamed as
  `gate.event`; land pushes from the lane host; `gate.then` runs the post-land
  command on the project home host. Wired into all four dispatch sites
  (`server.rs`, `hubnode_codec.rs`, `wss/runtime_wss.rs`, `stdio.rs`) with
  carrier event pumps for both outbound WSS and ssh-stdio.
- CLI `remuda/src/cmd/gate.rs`: `remuda gate <branch> [--web auto|always|never]
  [--lane] [--no-wait]`, `remuda land <branch> [--then "<cmd>"]`,
  `remuda gate list [--state] [--branch]`,
  `remuda gate cancel <gjb|branch> [--no-wait] [--json]`.
  Wait mode prints live `name: status (duration ms)` step lines; exit code =
  outcome (0 passed/landed, 2 canceled, 1 otherwise). `remuda gate cancel` now
  requests the cancel and then waits for the real terminal state by default:
  exit 2 on a clean cancel, exit 1 if the job landed anyway (the cancel raced a
  push), 0 otherwise; `--no-wait` returns the accepted `canceling` row without
  waiting and `--json` emits the job as JSON. `remuda project set --gate
  @file.json` configures lanes. `remuda watch` prints active gate rows;
  `remuda report [--for-owner]` adds active/failed gate jobs and the last landed
  sha. `remuda dispatch --carrier native|herdr|print` prefers the advertised
  native shell-pty, then herdr; print is never silent.
- The consumed merge CLI is untouched (`crates/remuda/src/cmd/merge.rs`,
  `crates/remuda/src/cmd/merge/` — the gate step authority stays
  `scripts/ci/gate.sh` in the merged worktree).

## Automated gates (this host)

- `cargo test -p remuda-hub` — all 27 integration binaries green, incl. the new
  `tests/gate_queue.rs`: parallel verify on two lanes, land serialization with
  the second job landing after, CAS loss → re-verify onto new main → landed,
  FIFO on one lane, queued cancel (never dispatched), running cancel
  (`gate.cancel` observed), lane/web/mode input validation.
- `cargo test -p remuda-node` — green, incl. 7 lane-runner tests with a fake
  merge binary over a real bare-origin/lane-clone git fixture: step streaming,
  land argv (`--gate --land`, no `--no-push` when pushing), stale-tip refusal
  on a diverged held worktree, second concurrent run → `lane-busy`, cancel
  kills the process group, whole-run timeout kills and fails.
- `cargo test -p remuda` — green, incl. 5 `tests/gate_cli.rs` tests through the
  in-process Hub + scripted lane Node: live step table, JSON shape matching the
  merge report steps, land printing the merge sha (with a `--then` hook),
  `gate list`, help surface.
- `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo fmt --all --check` clean; `just gen-types` and
  `pnpm --dir web run gen:api` regenerated, `pnpm --dir web run typecheck`
  clean; OpenAPI route-coverage test green for the five new routes.

## Real proof on THIS host (local Hub + WSS lane Node + scratch bare repo)

The Node enrolled over outbound WSS on this host (the ssh-stdio bridge is the
same code path; both carriers run the same runner and event pump, covered by
the node/hub test suites).

### Notes / follow-ups

- `web: never` is accepted and stored; the consumed merge CLI has no web
  suppression flag, so on a web-touching diff the lane runner behaves like
  `auto` (documented on the enum). Explicit live Hub e2e is `web: always`.
- The Hub keeps the `gate.run` RPC open up to 90 min; the Node enforces its own
  default 1 h cap (overridable via `gateTimeoutSecs`). Per-step budgets ride
  `REMUDA_GATE_STEP_TIMEOUTS` from the lane env and stay the gate.sh
  supervisor's existing `reason: timeout|child-lost` reporting.
- Feishu intake/three-card rendering from batch 6's original plan remains a
  follow-up; the queue/CLI/Node surface it would consume is complete (M2 minus
  the bot card UI).

### Actual run (2026-09-17, this host)

Script: `/tmp/remuda-mq-gate-proof.sh` (scratch-only root
`/tmp/remuda-mq-gate-proof`, a fresh bare `origin.git` — never the real origin).
Local Hub on 127.0.0.1:58480, a Node enrolled with a single-use Hub enroll
token over outbound WSS, one lane (`lane1`) whose env carries a
`REMUDA_MERGE_GATE_COMMAND` stub gate (same seam as `merge_cli.rs`) plus the
real `scripts/ci/gate.sh` in the scratch tree. The verify printed steps as they
streamed (`name: ok (N ms)`), passed without moving main; the land re-verified,
CAS-pushed from the lane host, and `--then` ran:

```text
host=hst_… workspace=wsp_…
project=prj_…
=== 1) remuda gate (verify, no push) with streamed steps ===
secret-scan/no-tunnel-scan/cargo-fmt/cargo-check/cargo-clippy/cargo-test/
web-install/web-build/web-test: ok (~200 ms each, streamed)
web-hub-e2e: skipped        # no pwEndpoint on the lane — gate.sh rule
gen-api-current: ok (30 ms); verify-tree/record-gate/cleanup: ok
verify: passed
origin/main before land: e51a35e4bdab544f35d4fad92c4a20b615b9d783
=== 2) remuda land --wait --then (push from the lane host) ===
…same step stream re-verified onto current main…
land: ok (3 ms)
push: ok (19 ms)
landed: 00e4e215ea45a362e807658a5bc53cc2812ce26c
origin/main after land:  00e4e215ea45a362e807658a5bc53cc2812ce26c   # bare repo main moved
=== 3) gate list + final job state ===
passed verify wt/co-gate/proof merge=89bc8b6625bd then=-
landed land   wt/co-gate/proof merge=00e4e215ea45 then=demo-refresh-ran
=== gate steps the lane host actually executed ===
cargo-check cargo-clippy cargo-fmt cargo-test gen-api-current
no-tunnel-scan secret-scan web-build web-install web-test
```

Timings on this devbox: enqueue→running within the 1 s scheduler tick;
stubbed steps streamed at ~200 ms/step (gate supervisor overhead; real steps
match `remote-gate.sh` today); full verify run ~2 s, land verify+push ~2 s in
the stubbed fixture. The pushed sha in the scratch bare repo equals the job's
`mergeSha`, and the post-land hook output is persisted on the job
(`thenOutput: "demo-refresh-ran\n"`). The stale-tip refusal, cancel-kill and
timeout paths were exercised separately against the same lane runner with a
real git fixture (node tests above), since a healthy run here has no stale tip.

### Timings

- Enqueue → lane dispatch: ≤1 s (scheduler tick).
- Step events arrive per gate step as `gate.jsonl` is flushed (~400 ms poll).
- Land re-verify path (synthetic): re-queue on `base-moved` and a fresh
  verify-onto-new-main within the next tick — covered deterministically in
  `gate_queue.rs::land_losing_the_cas_is_reverified_onto_new_main_then_lands`.
