# co-project — coordinator batch 1 evidence (2026-09-15)

Branch: `wt/co-project/project-entity-and-scope`
Design: `docs/design/coordinator-hierarchy.md` §§2.0–2.5, §3.1–§3.2, §3.4 step 1–2, §6.

## Scope change during the batch

Owner review (design §2.5, fetched from
`wt/coord/coordinator-and-live-view-design`) replaced the three-tier role
enum **before the schema was built**. Instead of baking tiers in, this batch
implements a recursive, scoped **delegation tree**:

- `scope = {projectIds[], hostIds[], workspaceIds[], supplyGrants[]}` on each
  instance, validated as a **subset of the parent scope** at child create.
- `grants` = verbs from `{dispatch, land, spend, address-owner}`; empty set =
  leaf worker.
- `role` survives only as a **preset name** (`worker` /
  `project-coordinator` / `top-coordinator`) — a named scope+grant bundle
  applied at create, stored for display, **never read by enforcement**.
- `projectId` is the convenience view of `scope.projectIds` when it has
  exactly one entry.
- `taskId` is a plain instance column.

## What landed

| Area | Change |
|---|---|
| protocol | `crates/remuda-protocol/src/project.rs` (new): `Project` entity (id/name/homeHost/members reusing the Space key, per-host quotas, placement/provider refs, modelRoles, gate placeholder, D-031 enforced + configurable policy), `InstanceScope`, preset expansion (`preset_grants`), subset/`allows_*` predicates, `ProjectLaunchDefaults`. `GrantVerb` enum, `ProjectId`/`TaskId` brands, `prj`/`tsk` prefixes. Additive only. |
| store | `projects` table (skeleton later batches extend); instances gain `role`, `scope_json`, `grants_json`, `task_id` via `ensure_column`; `InstanceDelegation` + `insert_instance_delegated`; writer-thread checks for scope subset, grant subset, DAG (ancestor walk with visited set), depth (default 3), fan-out (default 8), and the two seat rules. Non-delegating paths (fleet/resume/SSH) pass `enforce_tree: false`. |
| uniqueness | At most one active `address-owner` holder per Hub; at most one active `dispatch` holder per project unless `policy.configurable.allowMultipleDispatchers`. Both map to HTTP 409. |
| projects HTTP | `/v1/projects` (GET/POST), `/v1/projects/{id}` (GET/PATCH/DELETE), `/v1/projects/{id}/members` (POST/DELETE). D-031 enforced policy is immutable after creation. Members must reference registered host workspaces. Audit lines on every mutation. |
| agent_scope | Enforcement reads scope+grants, never `role`: leaf workers get 403 on `/v1/projects` and `/v1/providers*`; project-scoped agents only see their projects/instances; child create requires `dispatch`, in-scope host/workspace; middleware admits only the route shapes §2.4 lists. `/v1/caller` returns role/projectId/scope/grants. |
| placement | `Placement::Project {projectId}` resolves to the project's member hosts + per-host quotas/`requires`; project `maxInstances` clamps (never raises) the host ceiling; **`HostRecord.resources` (`cpuPct`/`memPct`) is finally read** — ≥90% excludes a host with a machine-readable reason; missing snapshots never exclude. |
| defaults fold | `merge_project_defaults` next to the existing host fold; order explicit > project > host > global (model/effort/permission posture). |
| provider waterfall | New project layer between explicit request and host binding (`SOURCE_PROJECT`); a project profile is host-scope-checked and missing profiles are `Unsatisfiable`, not silently skipped. |
| CLI | `remuda project list/show/create/set/add-member/remove-member`; generic `PATCH`/`DELETE` added to the hub client. |
| OpenAPI + generated | `crates/remuda-hub/openapi/openapi.json` documents all five project routes + 15 schemas; `InstanceCreate` gains role/scope/grants/projectId/taskId. `pnpm gen:api` and `just gen-types` regenerated. |

## Test evidence (fake node + fake harness only)

- `cargo test -p remuda-protocol` — preset expansion golden, scope subset
  semantics.
- placement unit tests — member-host restriction + `requires`, project quota
  clamp, cpu/mem saturation exclusion, `Placement::Project` parsing.
- `crates/remuda-hub/tests/projects.rs` (new, 12 tests):
  CRUD/members across **two fake hosts** with acknowledged workspace
  snapshots; restart persistence; bad workspace pair → 400; revision bumps;
  default folding; `Placement::Project` dry-run; saturated resources picking
  the idle host; cross-project 403 (token bound to A cannot read B);
  scope-filtered project list; leaf-worker 403 on projects/providers/create;
  subset violation → 403; dispatch-without-grant → 403; depth >3 → 409 and
  policy raise unblocks; fan-out 2 → third child 409; address-owner second
  holder → 409; second per-project dispatch holder → 409 (relax flag
  unblocks); tampered-column delegation cycle rejected.
- `crates/remuda/tests/project_cli.rs` (new, 3 tests): help wiring, full
  create/list/show/set round-trip, client-side `--member` validation, Hub
  rejection of unknown hosts/workspaces.
- Pre-existing `hub.rs` agent-create test updated for §2.5 (the parent now
  needs an explicit `dispatch` grant); one resume regression avoided by
  scoping tree validation to delegating inserts.

## Not done here (later batches, per §8)

Tasks/ownership (`co-task`), gate lanes consumption (`r-mergequeue`),
supply accounting (`co-supply`), dispatch/watch/retire verbs and the
launch-shim denylist (`co-loop`). The projects table is the skeleton those
batches extend; gate config and enforced policy are stored but not yet
enforced beyond the create-path folds.
