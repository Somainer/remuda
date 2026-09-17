# Evidence: gate-log-1

Date: 2026-09-17. Branch: `wt/c-gatelog/gate-failure-evidence`.
Context: on 2026-09-17 the first real verification through the Node gate lane
(`remuda gate wt/c-watchfailed/... --lane sg-lane1`, job
`gjb_01a0ae4f-077a-73ba-89e4-0cec27ed05e8`) streamed all seventeen steps and
failed at cargo-test after 39 minutes, but the job and the CLI only recorded
`cargo-test failed, exit status 101, attempts 2, retried`. Nothing kept the
test output, so the coordinator had to re-run the old shell gate just to read
the log — the shell gate's per-branch `.err` file existed precisely for this.

What shipped: the lane runner captures every step's output into bounded
rings; on failure it extracts a step-specific summary and hands the Hub a
`GateRunLog`, which the Hub stores as an `obj_…` **gate log object** — never
inline in the job row. The job carries only `failedStep`, the one-line
`reason`, and `logObjectId`; the same fields ride the terminal `gate.event`,
so `remuda gate`/`land`/`watch`/`report` render the failure without a second
channel, and `remuda gate log <gjb|obj>` fetches the bounded evidence.

## Surface

- Protocol `remuda-protocol/src/gate.rs` (all additive):
  - `GateRunLog` — `{step, kind: failed|kept, attempts, headline, summary[],
    tail[], capturedLines, truncated}` — the bounded evidence envelope.
  - `GateJob` gains `failedStep`, `reason`, `logObjectId`, `keepLogs`.
  - `GateRunResult` (the `gate.run` reply / `finished` event) gains
    `failedStep`, `reason`, and the in-transit `runLog: GateRunLog?`.
  - `GateRunParams` gains `keepLogs`; the `finished` event now boxes its
    result so the event enum does not grow to the size of the full verdict.
- Node `remuda-node/src/gate.rs`:
  - `RunTrace` tails the consumed merge CLI's stderr (gate.sh streams every
    step there) into a global ring plus one ring per step, attributed by the
    exact `gate: <step>` / `gate: <step> (retried)` attempt markers. Detail
    lines (`gate: cargo-test: …`) and the `gate: Rust tests: …` banner never
    match. Per-step ring 2 000 lines / 256 KiB; global ring 400 lines /
    128 KiB; lines clipped at 4 096 chars.
  - On failure: the failed step's last **400 lines** (96 KiB) plus an
    extracted `summary[]`:
    - **cargo-test** — the libtest `failures:` section (through
      `test result:`, ≤160 lines), every `test <name> … FAILED` line and
      every `panicked at …` line (≤40), de-duplicated against the section.
    - **Playwright web steps** (`web-*`) — the numbered failure titles
      (`  1) [chromium] › …`, ≤20) and the first failure block (≤100 lines).
    - other steps keep the bounded tail as their evidence.
  - Runner-level deaths (timeout, unparseable merge report) get a `gate`
    entry from whichever step was active; cancellations and stale-tip/base
    moves carry no log.
  - Passing runs produce nothing; `keepLogs` (`--keep-logs`) stores a `kept`
    whole-run log even on green.
- Hub `remuda-hub/src/gatequeue.rs` (additive):
  - New `gate_log_objects` table (`obj_…` id, job, project, JSON bytes,
    digest, 30-day TTL) and `Store::insert_gate_log`/`get_gate_log`. One log
    per job: a CAS re-verify attempt replaces the previous row, so
    `gate log <gjb>` always reads the latest run.
  - `apply_result` persists the `runLog` out-of-band **before** the job
    transition and stamps `failedStep`/`reason`/`logObjectId`; a store
    failure logs and never loses the verdict. The duplicate
    `finished`-event/RPC-reply arrival is now a read-guarded no-op so it
    cannot replace a stored object. Re-queued base-moved jobs clear the
    evidence fields for the fresh attempt.
  - `GET /v1/gate/logs/{id}` accepts a `gjb_` job id (latest log) or the
    `obj_` object id, project-scoped to the caller, with lazy expiry.
  - Enqueue accepts `keepLogs`; the audit journal detail gains
    `failedStep`/`reason`/`logObjectId`.
- CLI `crates/remuda/src/cmd/`:
  - `gate.rs`: `remuda gate <branch>` / `remuda land <branch>` gain
    `--keep-logs`; on failure they print the failed step, the one-line
    reason, the extracted summary lines, and
    `full log: remuda gate log <gjb> (<obj>)`, exiting 1. New
    `remuda gate log <gjb|obj>` renders headline / summary / tail.
    `remuda gate list` gains a `FAILED_STEP` column.
  - `report.rs`: `== gate queue failures ==` rows read
    `<step>: <reason>` and print the `gate log` pointer; JSON digest gains
    `failedStep`/`reason`/`logObjectId`; `--for-owner` asks carry the step.
  - `watch.rs`: columns only, as owned — the active-gate table is unchanged.
- Untouched, per ownership: merge/gate scripts, dispatch and supply code,
  and `web/` application code (only generated `api.generated.ts` /
  `generated.ts` were regenerated).

## Automated gates (this host)

- `cargo test -p remuda-node -p remuda-hub -p remuda` — all green. New:
  - Node lane-runner (`remuda-node/src/gate.rs`): a fake merge binary that
    fails **cargo-test with a known panic line across two attempts**; the
    test asserts `failedStep`, the headline (`cargo-test failed, exit
    status 101, attempts 2, retried`), the extracted summary (both
    `test … FAILED` names, the `failures:` section, the exact
    `panicked at …:42:9:` site) and the bounded tail; plus green-run cheapness
    (no `runLog`), `--keep-logs`, and unit tests for marker parsing, the
    cargo/Playwright extractors, the bounded ring and the per-step trace.
  - Hub (`remuda-hub/tests/gate_queue.rs`): the scripted lane Node returns a
    failed verdict with a `runLog`; the job row carries `failedStep`,
    `reason`, `logObjectId`; the log is fetchable by `gjb_` and by `obj_`
    (`200`), bad prefixes are `400`, unknown ids `404`, and a green job has
    no log object (`404`).
  - CLI (`remuda/tests/gate_cli.rs`): the failing wait prints step + reason +
    extracted summary + both log ids and exits 1; `gate log <gjb>` renders
    summary and the bounded tail; `gate list` shows the `FAILED_STEP`
    column; help lists `log` and `--keep-logs`.
- `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo fmt --all` clean; `cargo run -p remuda-protocol --example gen_types
  -- --check` and `pnpm run gen:api` regenerated, `pnpm run typecheck`
  clean; the OpenAPI route-coverage test covers the new
  `/v1/gate/logs/{id}` route.

## Synthetic failing run (in-process Hub + scripted lane Node, real CLI)

The harness is the same one `tests/gate_cli.rs` uses; the lane Node returns a
failed cargo-test verdict carrying a `GateRunLog` whose summary/tail mirror a
real libtest failure. The commands below are the actual `remuda` binary:

```text
$ remuda gate wt/c-gatelog/feature --project prj_… ; echo "exit=$?"
secret-scan: ok (11 ms)
cargo-test: failed (2340000 ms) [retried]
verify: failed
cargo-test: cargo-test failed, exit status 101, attempts 2, retried
failures:

---- remuda_hub::gatequeue::tests::evidence_flows stdout ----
thread 'remuda_hub::gatequeue::tests::evidence_flows' panicked at crates/remuda-hub/src/gatequeue.rs:742:17:
assertion `left == right` failed
  left: Failed
 right: Passed

failures:
    remuda_hub::gatequeue::tests::evidence_flows

test remuda_hub::gatequeue::tests::evidence_flows ... FAILED
thread 'remuda_hub::gatequeue::tests::evidence_flows' panicked at crates/remuda-hub/src/gatequeue.rs:742:17:
full log: remuda gate log gjb_01a0aec3-cace-7207-b079-ab05c4624cc6 (obj_01a0aec3-cad4-774e-ace7-cd88681d4d84)
exit=1
```

The list view gained the failed-step column:

```text
$ remuda gate list --project prj_…
JOB                                           MODE       BRANCH                          STATE      FAILED_STEP     MERGE_SHA
gjb_01a0aec3-cace-7207-b079-ab05c4624cc6      verify     wt/c-gatelog/feature            failed     cargo-test      -
```

The bounded evidence is one command away, by job id or object id:

```text
$ remuda gate log gjb_01a0aec3-cace-7207-b079-ab05c4624cc6
== cargo-test (failed) obj_01a0aec3-cad4-774e-ace7-cd88681d4d84 ==
cargo-test failed, exit status 101, attempts 2, retried

-- summary --
failures:

---- remuda_hub::gatequeue::tests::evidence_flows stdout ----
thread 'remuda_hub::gatequeue::tests::evidence_flows' panicked at crates/remuda-hub/src/gatequeue.rs:742:17:
assertion `left == right` failed
  left: Failed
 right: Passed

failures:
    remuda_hub::gatequeue::tests::evidence_flows

test remuda_hub::gatequeue::tests::evidence_flows ... FAILED
thread 'remuda_hub::gatequeue::tests::evidence_flows' panicked at crates/remuda-hub/src/gatequeue.rs:742:17:

-- last 4 lines --
test result: FAILED. 142 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 61.20s

error: test failed, to rerun pass `-p remuda-hub --lib gatequeue::tests::evidence_flows`
gate: cargo-test: exit status 101
```

The same evidence lands in the fleet digest for the coordinator/owner:

```text
$ remuda report --project prj_…
== fleet ==
{}

== gate queue failures ==
  wt/c-gatelog/feature [verify] cargo-test: cargo-test failed, exit status 101, attempts 2, retried
      log: remuda gate log gjb_01a0aec5-7d6d-7577-a599-7db78b499297 (obj_01a0aec5-7d74-756d-9cfe-1a51a77f3d73)
```

The persisted object envelope (`GET /v1/gate/logs/gjb_…`) is:

```json
{
  "objectId": "obj_…",
  "jobId": "gjb_…",
  "projectId": "prj_…",
  "expiresAt": "2026-10-17T…Z",
  "log": {
    "step": "cargo-test",
    "kind": "failed",
    "attempts": 2,
    "headline": "cargo-test failed, exit status 101, attempts 2, retried",
    "summary": ["failures:", "    …", "test … FAILED", "… panicked at …"],
    "tail": ["… last 400 lines …"],
    "capturedLines": 412,
    "truncated": true
  }
}
```

## Notes / follow-ups

- The job row stays small: `gate_log_objects` is the only place bytes live,
  rows are replaced per attempt, and lazy expiry plus a 30-day TTL bound
  storage. Green runs write nothing (`--keep-logs` excepted).
- Attribution relies on gate.sh's existing `gate: <step>` banners; if a step
  prints output before its first banner (e.g. secret-scan), those lines stay
  only in the global ring — a runner-level fallback that still produces a
  400-line tail even when the marker never arrives.
- The web client receives the additive job fields and the new route through
  generated types; rendering a failure panel in the web UI is a follow-up and
  needs no protocol change.
- Logs of a queued cancel / stale-tip refusal / base-moved re-queue are not
  stored: none of those states ran a failing step.
