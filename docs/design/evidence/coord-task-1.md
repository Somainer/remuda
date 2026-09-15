# co-task — coordinator batch 4 evidence (2026-09-15)

Branch: `wt/co-task/task-ledger-and-ownership`
Design: `docs/design/coordinator-hierarchy.md` §2.2 (Tier 2 held state), §2.5
(mandate chain / recursive delegation tree), §4.3 (`TaskSpec.owns` /
`scopeCheck.diffMustStayWithin`), §5.3 (task states), §5.6 (placement ledger
as audit + bot card), §7 #8 (invariant I1: DONE is a claim, dependency edges
unlock only from a landed sha).
Builds on batch 1 (co-project), which is on `origin/main`.

## What landed

| Area | Change |
|---|---|
| protocol | `crates/remuda-protocol/src/task.rs` (new): `Task` entity (`id`/`projectId`/`parentTaskId` mirroring the §2.5 delegation tree/`title`/`mandate`/`class`/state/`owns[]`/`deps[]`/`budget`/placement ref/`landedSha`/`blockedReason`), `TaskState` + legal-transition table, `TaskClass`, `Mandate`/`MandateLink` (root-first inherited owner-intent chain), `TaskDep`, `TaskBudget`, `TaskPlacementRef`, `PlacementLedgerRow` + `PlacementRejection` + the four `PLACEMENT_KIND_*` constants. Pure gate logic: `glob_match` (`*`/`?`/`**`), `normalize_glob(s)`, `patterns_conflict`, `claim_conflicts`, `check_paths_within_owns`, `parse_diff_paths` (unified diff / `--stat` / `--name-only` / NUL-separated). `plc` id prefix registered. `schema.rs` registration + regenerated `schema/protocol.schema.json` and `web/src/types/generated.ts`. |
| store | Three additive tables in one self-contained `tasks::migrate` block: `tasks` (doc_json like `projects`, plus indexed project/parent/state/landed_sha columns), `task_paths` (the ownership registry, PK `(task_id, pattern)`), `placements` (ledger rows with kind + doc_json). All multi-table mutations run in one writer turn. |
| task HTTP | `POST /v1/tasks` (`add`), `GET /v1/tasks?project&state`, `GET /v1/tasks/{id}` (adds the live `lockedDeps` view), `PATCH /v1/tasks/{id}` (`set-state`; illegal transitions 409), `POST /v1/tasks/{id}/split` (child inherits mandate chain; depth checked against `policy.configurable.maxDelegationDepth`), `POST /v1/tasks/{id}/land` (`land` grant; 7–40 hex sha; running/done only). Deps must reference existing same-project tasks. Audit lines for `task.add/split/state/land`. |
| invariant I1 | Nothing unlocks from a worker status: `done` without a sha keeps every dep edge locked; only `landedSha` (set by the land step) unlocks. `GET /v1/tasks/{id}` reports `lockedDeps`; `Task::locked_deps` is the pure predicate. |
| own HTTP | `POST|DELETE /v1/tasks/{id}/own` (claim/release; conflicting *active* claims 409; `owns` at add/split seed the registry and conflict-check identically; terminal tasks release all claims), `GET /v1/own?project` (aggregated ownership map), `POST /v1/own/check` (pure path reconciliation; accepts explicit `paths` or a `diff` body). |
| placements | `POST /v1/tasks/{id}/placements` appends a row and drives the legal state transition: `dispatch` → placed, `park` → parked (running/stalled only), `unplace` → pending (clears slot), `switch-model` leaves state but requires a running task (§4.6 — explicit act on a live worker) and preserves host/instance/branch. Illegal state jumps 409; unknown kinds 400. `GET` lists oldest-first. Rows survive Hub restart. |
| agent_scope | Middleware admits agent callers on exactly the new route shapes (`task_read_target`/`task_write_target`, plus POST `/v1/own/check` as a read-shaped gate call). Handlers re-check scope and the `dispatch`/`land` grants — leaf workers reach the refusal and get 403. |
| CLI | `crates/remuda/src/cmd/task.rs`: `remuda task add/list/show/split/set-state` (`--intent`/`--intent-file -`, `--owns`, `--dep`, `--class`, budget flags). `crates/remuda/src/cmd/own.rs`: `remuda own claim/release/check/list`; `check` exits **0** within scope, **2** on a boundary crossing, accepts `--path`, inline `--diff`, or `--diff-file -` (the future gate pipe `git diff main...br | remuda own check T -`). |
| OpenAPI + generated | New task/own/placement paths and 20 schemas in `crates/remuda-hub/openapi/openapi.json` (additive; existing entries byte-identical, verified semantically). `pnpm --dir web gen:api` regenerated `web/src/lib/api.generated.ts`; re-running is diff-clean. |

## Boundary to the later merge gate

The scope check is deliberately a **pure function** in `remuda-protocol`
(`check_paths_within_owns(owns, changed)` + `parse_diff_paths`) with a thin Hub
wrapper (`POST /v1/own/check`) and a CLI that already returns a gate-shaped
exit code. Batch 5/6 (co-loop/co-lanes) wires this into `remuda merge
--gate`/`remuda land` without touching `cmd/merge.rs` (untouched here). The
`/v1/tasks/{id}/land` ledger entry is the gate-side call that records the sha.

## Test evidence (Hub-only state; no node, no models)

Protocol unit golden (`cargo test -p remuda-protocol --lib`):

- state machine golden: full legal lifecycle and representative illegal
  jumps (`pending→running`, `done→running`, `failed→pending`,
  `stalled→done`, `deferred→done`);
- **mandate chain golden to depth 2**: the grandchild's chain carries the
  owner's exact Chinese root words verbatim, then each edge's own words, with
  depth stamps and `parentTaskId`/project inheritance;
- deps stay locked under a `done` status bit and unlock only when
  `landed_sha` is set; missing dep never silently unlocks;
- glob matching (`*`, `?`, `**` across/within segments), directory-glob
  normalisation, claim-conflict matrix;
- `parse_diff_paths` over a synthetic unified diff crossing the boundary;
  `--stat` / `--name-only` shapes.

Hub integration (`cargo test -p remuda-hub --test tasks`, 7 tests):

- legal/illegal transitions over HTTP (409 + message, 400 bad state, 404,
  state filter on list, `failed` reason round-trip);
- **dependency unlock only by landed sha**: worker `done` leaves `lockedDeps`
  populated; landing a 40-hex sha clears it; malformed sha 400; landing a
  pending task 409; cross-project and unknown deps 400;
- **own claim conflict**: `owns` at add time and later claims both detect a
  file inside another active task's subtree (409 names the held pattern);
  unrelated trees coexist; release frees the pattern;
- **own check on the synthetic diff**: the diff touching
  `crates/remuda-hub/src/tasks.rs` (owned) and `crates/remuda/src/cmd/merge.rs`
  (not owned — the file co-loop must wire the gate against, deliberately not
  touched here) reports `within:false`, `checked:2`, one violation; explicit
  paths inside the claim are clean;
- terminal (landed) tasks release claims, which another task can immediately
  claim;
- **placement ledger round-trip**: dispatch/switch-model/park/unplace drive
  exactly the legal states, early switch-model 409, bad kind 400,
  pending→park 409; `reasons[]`/`rejected[]` survive a Hub restart;
- split mandate chain over HTTP + project depth policy (cap at 1 → 409 with
  a depth message, raise to 3 → grandchild ok and still carrying the root
  intent);
- scope/grant enforcement with a store-inserted leaf worker scoped to project
  A: task writes and land 403, project B's task 403/absent from list,
  `/v1/own/check` allowed for A and refused for B.

CLI (`cargo test -p remuda --test own_cli`, 2 tests): help lists every
subcommand; full `project → task add → list/show → split → set-state → own
claim/list/check/release` lifecycle against an in-process Hub; illegal
transition exits non-zero with the Hub's message; conflicting claim fails;
`own check` exit codes 0 vs 2; **stdin diff piped through `--diff-file -`**
succeeds.

## Commands run

```text
cargo fmt --all -- --check
cargo clippy -p remuda-protocol -p remuda-hub -p remuda --all-targets -- -D warnings
cargo test -p remuda-protocol -p remuda-hub -p remuda
cargo run --locked -p remuda-protocol --example gen_types
pnpm --dir web run gen:api   # idempotent / diff-clean
./scripts/ci/secret-scan.sh
```

## Not in this batch

- `cmd/merge.rs`, runtime/node, signal/screen, supply/placement (co-supply
  ownership), dispatcher/dispatch verbs, web UI — all untouched; `web/src`
  changes are generated files only.
- The merge gate itself does not call `/v1/own/check` yet; the pure function,
  HTTP wrapper, CLI exit code, and `/land` ledger entry are the seams co-loop
  plugs into.
