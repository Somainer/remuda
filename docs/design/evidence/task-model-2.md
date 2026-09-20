# Task model · task 2 (`t-pool`): warm worktree pool — lease / return / refcount

Date: 2026-09-20. Scope: the Node worktree pool layered above
`provision_record`, the Hub `worktree_leases` table and lease/return routes,
and the lease-aware reclaim guards. Plan: `briefs/plans/task-model.md` §B.3
and task 2 acceptance (1–9); design decision
[D-050](../decisions.md) (merged by `c-tspec`) fixes the table,
`mode`/`dir_key` vocabulary and the lease-aware reclaim requirement this
implements.

No ports, no tunnels, no web suite: the pool touches only git worktrees, the
Node catalog and Hub SQLite. Screenshots are not part of this task's
evidence; all transcripts below come from temporary git repositories under
the per-test scratch directory.

## What landed

| Layer | File | Change |
|---|---|---|
| Node | `crates/remuda-node/src/worktree_pool.rs` (new) | `lease` / `return_slot` / `reset_park` / `reconcile`, isolated from `worktree.rs` |
| Node | `crates/remuda-node/src/worktree.rs` | `WorktreeRecord.state` (`free`/`leased`/`parked`) + `leasedBy: Vec<TaskId>` with serde defaults; explicit `worktree.lease`/`worktree.return` dispatch; `remove_record` refuses while leased; catalog helpers exposed `pub(crate)` |
| Node | `crates/remuda-node/src/worker.rs` | `worker.remove` honors `ReclaimOutcome::Retained` (skips the target-dir delete too, answers `retained/refcount`) |
| Node | `crates/remuda-node/src/server.rs`, `runtime_link.rs`, `stdio.rs`, `transport/wss/runtime_wss.rs` | new methods route through `is_worktree_method` → `worktree_rpc_capped` on every carrier; dev server gets explicit match arms |
| Hub | `crates/remuda-hub/src/store.rs` | new `worktree_leases` table (`mode`, `(host_id, workspace_id, dir_key)` unique key, nullable `worktree_name`, `holder_instance_id`, `task_ids_json`); record/release/held-by-instance methods; `delete_instance` clears the attach lock |
| Hub | `crates/remuda-hub/src/http.rs` | `POST /v1/worktrees/{name}/lease` and `/return`, forwarding only `{hostId, workspaceId, name, base, taskId}`; full pool → 429 `SUPPLY_DEFERRED`; `delete_instance` returns held leases before purge |
| Hub | `crates/remuda-hub/src/workers.rs` | `retire_core` checks the lease table before `worker.remove`: shared slot → 409, sole holder → return/park, no lease → legacy reclaim |
| Wire | `crates/remuda-hub/openapi/openapi.json`, `web/src/lib/api.generated.ts` (regenerated), `crates/remuda-protocol/src/scalar.rs` (`wtl_` id prefix) + regenerated schema/types | additive only |

Pool size is per (host, repo), default **4**, configurable with
`REMUDA_WORKTREE_POOL_SIZE` (clamped 1–16; D3). Eviction removes only
**clean + idle + refcount 0** slots parked on a different base; a dirty slot
is never evicted.

## Lease/return/share transcript (Node JSON-RPC, redacted)

Cold lease provisions detached (fetch-first), then switches a per-task
branch in place; a second task on the same slot shares by refcount and is
**queued/blocked**, never run concurrently; the ordinary reclaim path refuses
while shared; returns decrement to zero and park detached.

### 1) Cold lease — provisions, fetch-first

```json
{
  "name": "pool-s1",
  "path": "<scratch>/remuda-wt/pool-s1",
  "branch": "wt/pool-s1/tsk-01a0bf05-3aaa-7450-b236-4168d6fade3e",
  "baseOid": "794245f16828cb6c36b8b74d60a9dc952019edc4",
  "mode": "pool",
  "state": "leased",
  "refcount": 1,
  "warm": false,
  "queued": false,
  "dirKey": "pool-s1"
}
```

Slot branch after lease: `wt/pool-s1/tsk-01a0bf05-…` (branch per task,
directory reused — `wt/<slot>/<task>`).

### 2) Second task shares the slot — serial reuse, queued

```json
{
  "name": "pool-s1",
  "path": "<scratch>/remuda-wt/pool-s1",
  "branch": "wt/pool-s1/tsk-01a0bf05-3aaa-7450-b236-4168d6fade3e",
  "base": null,
  "mode": "pool",
  "state": "leased",
  "refcount": 2,
  "warm": true,
  "queued": true,
  "dirKey": "pool-s1",
  "blocked": {
    "reason": "directory is held by another attached task; queued for serial reuse"
  }
}
```

### 3) The existing reclaim main path refuses a shared slot

`worker.remove` (the RPC `retire_worker` / `delete_instance` drive) with
refcount 2:

```json
{
  "name": "pool-s1",
  "worktreeRemoved": false,
  "targetRemoved": false,
  "reclaimedBytes": "0",
  "retained": true,
  "refcount": 2,
  "worktreePath": "<scratch>/remuda-wt/pool-s1"
}
```

The directory still exists afterwards. The Hub guards earlier
(`retire_core` 409 for a multi-task slot; `delete_instance` returns the held
lease) make this the on-disk root-cause guard rather than the only one.

### 4) Both tasks return — decrement then detached park

```json
{ "name": "pool-s1", "mode": "pool", "state": "leased", "refcount": 1, "parked": false, "dirKey": "pool-s1" }
{ "name": "pool-s1", "mode": "pool", "state": "parked", "refcount": 0, "parked": true, "dirKey": "pool-s1" }
```

`git symbolic-ref` on the slot exits non-zero after parking — the slot rests
on a **detached HEAD** at the pool base, so git's one-branch-per-worktree
rule never blocks re-leasing it.

## Offline scenario: warm hit never fetches

After parking, the fixture points `origin` at an unreachable
(`file:///nonexistent/offline`) remote. A fresh task leasing the same pool
still succeeds — the parked slot is switched to a new per-task branch with
**no `git fetch`** (the unbounded fatal fetch in `provision_record` runs
only when a brand-new slot is provisioned):

```json
{
  "name": "pool-s1",
  "path": "<scratch>/remuda-wt/pool-s1",
  "branch": "wt/pool-s1/tsk-01a0bf05-3aaa-7450-b236-41694f714a18",
  "baseOid": "794245f16828cb6c36b8b74d60a9dc952019edc4",
  "mode": "pool",
  "state": "leased",
  "refcount": 1,
  "warm": true,
  "queued": false,
  "dirKey": "pool-s1"
}
```

The same fixture fails if the warm path accidentally fetches, because
`provision_record` treats a failed fetch as fatal by design.

## Reuse mode returns byte-identical (acceptance #3)

`reuse_return_leaves_the_tree_byte_identical` creates a **standalone**
worktree via the non-pool path, writes tracked + nested untracked content,
leases it twice (`mode:"reuse"`, refcount 2, second task queued), then
returns both. The test compares a recursive `(relative path, bytes)`
fingerprint before leasing and after returning: the sequence is exactly
equal — no `clean`/`reset`/`remove` ran, and `notes.txt` / `scratch/data.bin`
are untouched. Reset/clean/park only ever run for `mode=pool`.

### Task-archive boundary

D-050 §2 gates the pool `clean -fd`/park on the last refcount returning *and*
the task being archived. The Node `worktree.return` RPC deliberately knows
nothing about task ledger state, so in this task the last return parks
immediately (the directory is clean by the refuse-on-dirty guard); wiring
"park only once the task row is done/archived" is the binding task's
(`t-bind`) Hub-side responsibility. The blocked/queued reason carries the
stable token `dir-busy:` (D-050 §2) so the board can discriminate it.

## Acceptance cross-reference

| # | Acceptance | Where it is pinned |
|---|---|---|
| 1 | Three-way lease: parked warm (no fetch) → provision new (fetch-first) → defer, never reroute/dirty reuse | Node: `lease_and_return_round_trip_warms_a_slot`, `warm_hit_does_not_fetch_while_offline`, `full_pool_without_a_clean_slot_defers_instead_of_rerouting`, `eviction_reclaims_only_a_clean_idle_slot_on_another_base`. Hub: `full_pool_defers_with_429_and_records_no_lease` |
| 2 | Refcount +1/−1; force-remove refused while shared; reclaim main path lease-aware incl. `worker.remove` on a multi-task slot | Node: `refcount_sharing_blocks_force_remove_until_returned`, **`worker_remove_does_not_delete_a_shared_slot`**. Hub: `retire_of_a_shared_slot_is_refused_and_never_reaches_remove`, `retire_of_a_sole_holder_returns_and_parks_instead_of_removing`, `retire_without_a_lease_uses_the_legacy_remove_path` |
| 3 | Reuse return is a zero-op, byte-identical tree; reset/clean/park pool-only | `reuse_return_leaves_the_tree_byte_identical` |
| 4 | Dirty tracked tree refuses return/reset; attach lock; second reuse task queued/blocked | `dirty_tracked_changes_refuse_pool_return_and_reset`, `second_task_shares_refcount_and_queues` (Hub), shared-slot transcripts above |
| 5 | Detached-HEAD parking dodges one-branch-one-worktree and "branch exists" | `detached_parking_survives_branch_name_reuse` |
| 6 | reuse-to-root row: `worktree_name=NULL`, `dir_key='.'`, counted | Node `reuse_to_root_lease_keys_the_registered_root`; Hub `reuse_to_root_lands_a_null_name_dot_key_row` |
| 7 | path/repo never cross the wire | Node `lease_refuses_path_and_repo_overrides` (defensive reject); Hub `lease_forwards_only_the_safe_params_and_records_a_pool_row` (asserts the forwarded JSON contains exactly the whitelisted keys) |
| 8 | Explicit dispatch, no `{ok:true}` silent no-op; unregistered method errors | `lease_return_dispatch_real_payloads_and_unknown_method_errors` |
| 9 | Catalog path/name/branch consistency keeps the pre-trust flag | `native::tests::a_pooled_worktree_keeps_the_trust_flag_across_lease_and_park` (extends `native.rs:3189`-family coverage through a lease → park → warm re-lease cycle) |
| — | Catalog reconcile (missing rows dropped, stuck leases healed) | `reconcile_drops_missing_rows_and_heals_stale_leases` |
| — | Deleting the holder instance returns its lease and releases the attach lock | Hub `deleting_a_holder_instance_returns_its_lease_and_clears_the_lock`; store transaction clears `holder_instance_id` |

### Root addressing over HTTP

WHATWG URL clients collapse literal and percent-encoded `.` path segments
(`/v1/worktrees/%2E/lease` normalises to `/v1/worktrees/lease`), so the root
cannot be addressed as a `.` path parameter over HTTP. The HTTP edge
reserves the token `-` (invalid as a real worktree name, since
`safe_segment` requires a leading letter) and maps it to the Node/root key
`"."`; the Node JSON-RPC and the store still use `"."`. This is documented
on the OpenAPI path parameter.

## Verification

- `cargo test -p remuda-node -p remuda-hub` — all suites pass. One
  pre-existing timing flake in `tests/daemon::outbound_reconnect_replays_offline_completion_without_a_new_command`
  failed once under a fully parallel run and passed consistently in
  isolation (×3); it is unrelated to this change (no pool code on that path).
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --all --check` clean.
- `bash scripts/ci/secret-scan.sh` and `bash scripts/ci/no-tunnel-scan.sh` — PASS.
- `pnpm --dir web run gen:api` regenerated `api.generated.ts`;
  `just gen-types` regenerated the protocol schema/types (only the new `wtl`
  id prefix differs). The hand-maintained OpenAPI document covers the two new
  routes (`tests/openapi.rs` passes).
