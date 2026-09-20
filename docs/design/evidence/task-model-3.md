# Task model · task 3 (`t-bind`): task directory binding selector wired into dispatch

Date: 2026-09-21. Scope: the per-task `reuse | pool` directory choice at
task creation, its lease acquisition, and its fold into the existing
dispatch fields. Plan: `briefs/plans/task-model.md` §(C) task 3 and §B.3;
design authority `docs/design/task-model.md` §1.1/§2/§3.4; decision
[D-050](../decisions.md). Base: rebased onto the merged t-pool /
t-board-api lines (`worktree.lease`/`worktree.return`, the
`worktree_leases` table, `archived_at`).

Like task 2 this task opens no ports, installs no tunnel and ships no new
visual surface (the binding parser is pure; the selector UI belongs to a
later product task), so screenshots are not part of this evidence — the
proof is the hub e2e transcript over the composed Hub + gated fake Node.

## What landed

| Layer | File | Change |
|---|---|---|
| Protocol | `crates/remuda-protocol/src/task.rs` | `TaskBindingMode` (`reuse`/`pool`) and `TaskSpaceBinding {mode, hostId, workspaceId, worktreeName?, branch?, leaseRefIds[]}`; `Task.workspaceBinding: Option<_>` with serde default + skip-if-none (zero migration, byte-safe); `dir_key`/`is_root`/`lease_path_name` helpers |
| Protocol | `crates/remuda-protocol/src/schema.rs` + regenerated `schema/protocol.schema.json`, `web/src/types/generated.ts` | new wire types in the schema document |
| Hub | `crates/remuda-hub/src/tasks.rs` | `POST /v1/tasks` accepts `workspaceBinding`, validates mode/segment/project membership, acquires the lease, records it on the task, rolls the row back on refusal (`delete_task_cascade`), and returns the serial-sharing projection |
| Hub | `crates/remuda-hub/src/http.rs` | lease route refactored into shared `lease_on_host` + `lease_refusal`; dispatch (`POST /v1/instances`) folds the binding into existing `hostId`/`workspaceId`/`cwd`/`worktree` fields and stamps the attach-lock holder; conflicting explicit directory fields are refused, never overridden |
| Hub | `crates/remuda-hub/src/store.rs` | `attach_worktree_lease_holder` (dispatch fold) and `delete_task_cascade` (refusal rollback) |
| Hub | `crates/remuda-hub/openapi/openapi.json` | `TaskSpaceBinding`/`TaskBindingMode`/`WorktreeSharing`; `Task.workspaceBinding`; `TaskCreate.workspaceBinding`; `TaskCreateResult` |
| Web | `web/src/features/tasks/binding.ts` (new) | pure selector parsing: out-of-tree boundary mirroring `resolve_instance_cwd`, dispatch fold, `与 N 个 task 共用` footer, per-project choice memory (D2) |
| Web | `web/src/features/tasks/createTask.ts` (new) | `createTask` POST client; remembers the last choice per project |
| Web | `web/src/features/tasks/binding.test.ts` (new) | 15 vitest cases |
| Web | `web/src/lib/api.generated.ts` | regenerated only; `rest` exported from `api.ts` for feature code |
| E2E | `crates/remuda-hub/examples/hub_e2e.rs` | **gated** `HUB_E2E_TASK_BIND=1` fake: in-memory worktree catalog/lease (reuse refcount/queue, pool `<pool>-s<n>` + per-task branch, dirty refusal, full-pool deferral) plus one branded workspace; default runs keep the previous unknown-method behaviour |
| E2E | `web/tests/e2e/task-model-bind.hub.spec.ts` (new) | 6 cases over real HTTP, one per acceptance item plus the host-pin conflict |

## Acceptance mapping (plan task 3)

1. **reuse admission follows `resolve_instance_cwd`.** Root (no
   `worktreeName`) folds to *no* cwd override, so the Node applies
   `resolve_instance_cwd` to the registered root exactly as an unbound
   launch; a sibling folds the catalog-recorded absolute path into the
   existing `cwd`. Segment admission is the same boundary
   (`safe_segment`, `[a-z][a-z0-9_-]{0,31}`): `../escape`, `/abs/path`,
   `a/b`, `.hidden`, `-dash` and space-bearing names are all rejected
   400 before any lease call. The sibling must already exist in the
   Node catalog and may not be a pool slot (a pooled name must bind with
   `mode: pool`).
2. **pool obtains its own branch through the existing worktree field.**
   Task creation calls task 2's `worktree.lease`; the Node returns the
   concrete `<pool>-s<n>` slot and branch `wt/<slot>/<task-slug>`, both
   stored on the binding. Dispatch folds the slot path into `cwd` and
   the slot name into the existing `CreateInstanceBody.worktree` — no
   new dispatch field (D-050 §3.4).
3. **no schema change beyond `archived_at`.** The binding lives in the
   task `doc_json`; rows written before t-bind decode as unbound and
   serialise byte-identically (`workspace_binding_lives_in_doc_json_…`
   protocol test asserts the key is absent on an old/new unbound task).
4. **sharing and refuse-never-reroute.** A second task binding a leased
   reuse directory bumps `refcount` and receives
   `queued: true` + `blocked.reason: "dir-busy: …"` (footer `与 N 个 task
   共用`). A dirty tree / branch conflict returns 409 blocked, a full
   pool returns 429 `SUPPLY_DEFERRED`; in every refusal the half-written
   task row is deleted and no other directory is chosen (D-035).

## Transcripts

### Unit tests

```text
$ cargo test -p remuda-hub -p remuda-protocol
…
test result: ok. 167 passed; 0 failed   # remuda-hub
test result: ok. 121 passed; 0 failed   # remuda-protocol (incl. the
                                        # workspace_binding doc_json case)
$ cargo run --locked -p remuda-protocol --example gen_types -- --check
Checked crates/remuda-protocol/schema/protocol.schema.json
Checked web/src/types/generated.ts
$ pnpm --dir web run gen:api      # api.generated.ts matches openapi.json
$ cd web && npx vitest run src/features/tasks/binding.test.ts
 Test Files  1 passed (1)
      Tests  15 passed (15)
$ pnpm --dir web test               # whole vitest suite
 Test Files  150 passed (150)
      Tests  1468 passed (1468)
$ pnpm --dir web typecheck && pnpm --dir web lint   # clean
$ cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings  # clean
$ bash scripts/ci/secret-scan.sh && bash scripts/ci/no-tunnel-scan.sh
secret-scan: pass
no-tunnel-scan: passed
```

The vitest file covers: reuse root/sibling parsing, pool name + base,
the out-of-tree list (`../escape`, `/abs/path`, `a/b`, `.hidden`,
`-dash`, `remuda-wt/agent`, space), the reserved `-s<n>` suffix,
missing host/workspace, the dispatch fold for all three directory
kinds, no-reroute on a missing catalog path, the refcount footer,
per-project D2 memory (remember/forget/corrupt storage), and body
assembly that rejects an invalid choice before POST.

### Hub e2e — `task-model-bind.hub.spec.ts`, three consecutive runs

Command (lock slot `c`, ports from the task brief, bundled chromium via
`PW_CHANNEL=chromium`, gated fake Node):

```bash
flock <locks-dir>/e2e.lock-c \
  env PW_CHANNEL=chromium \
      HUB_E2E_LISTEN=127.0.0.1:59280 \
      HUB_E2E_WEB_PORT=59289 \
      HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59281 \
      HUB_E2E_TASK_BIND=1 \
  bash -c 'cd web && pnpm playwright test -c playwright.hub.config.ts task-model-bind'
```

Result, three runs in a row:

```text
Running 6 tests using 1 worker
  ✓  reuse root: binding round-trips in doc_json and the session starts at the registered root
  ✓  reuse sibling: cwd folds to the existing remuda-wt directory
  ✓  reuse sharing: a second task on the same directory bumps refcount and is queued 与 N 个 task 共用
  ✓  pool: the lease allocates a slot with its own wt/<slot>/<task> branch and folds into worktree
  ✓  refusals: out-of-tree, dirty directory and a full pool block without creating a task
  ✓  a binding outside project membership is refused and dispatch cannot override the bound host
  6 passed   # run 1 (47.5s), run 2 (1.2m), run 3 (46.2s)
```

Representative request/response shapes (ids redacted; `/tmp/remuda-e2e`
is the fake Node's registered root):

```jsonc
// POST /v1/tasks — reuse sibling
{ "projectId": "prj_…", "title": "reuse-sibling …", "intent": "…",
  "workspaceBinding": { "mode": "reuse", "hostId": "hst_…",
                        "workspaceId": "wsp_…", "worktreeName": "agent-one" } }
// 200 — the binding round-trips from doc_json with a lease row id
{ "id": "tsk_…", "state": "pending",
  "workspaceBinding": { "mode": "reuse", "hostId": "hst_…",
    "workspaceId": "wsp_…", "worktreeName": "agent-one",
    "branch": "wt/agent-one/work", "leaseRefIds": ["wtl_…"] },
  "sharing": { "dirKey": "agent-one", "refcount": 1, "queued": false, "blocked": null } }
// POST /v1/instances {taskId} → GET /v1/instances/{id}
// instance.cwd == "/tmp/remuda-e2e/remuda-wt/agent-one", worktree absent

// POST /v1/tasks — pool
{ "workspaceBinding": { "mode": "pool", "hostId": "hst_…",
                        "workspaceId": "wsp_…", "worktreeName": "alpha" } }
// 200 — operator's pool name resolves to the Node-assigned slot/branch
// workspaceBinding.worktreeName == "alpha-s1"
// workspaceBinding.branch == "wt/alpha-s1/tsk-…"
// instance.cwd == "/tmp/remuda-e2e/remuda-wt/alpha-s1" (worktree field)

// Second task on a leased reuse directory
"sharing": { "dirKey": "agent-two", "refcount": 2, "queued": true,
             "blocked": "dir-busy: directory is held by another attached task; queued for serial reuse" }

// Refusals (task row rolled back; list shows no dirty/full task)
POST /v1/tasks { worktreeName: "../escape" }  → 400 BAD_REQUEST
POST /v1/tasks { worktreeName: "dirty" }     → 409 directory binding blocked: … dirty …
POST /v1/tasks { worktreeName: "fullpool" }  → 429 SUPPLY_DEFERRED (pool full)
POST /v1/instances {taskId: <root-bound>, hostId: "hst_not_a_member"}
  → 409 "task is bound to host hst_…; refusing to launch on hst_not_a_member"
```

The full hub suite (`pnpm playwright test -c playwright.hub.config.ts`)
is run once under the same lock in both trigger positions:

- **default (unset):** all 165 pre-existing specs pass; the new
  `task-model-bind` spec's `beforeEach` cannot find the gated branded
  workspace, so it is the only failing file — proof the gated arms are
  inert by design;
- **`HUB_E2E_TASK_BIND=1`:** the same full suite including all six
  binding cases passes together (transcript recorded in this file).

With the trigger unset the new match arms do not fire and the fake Node
keeps its previous unknown-method answers, so no other spec changes
behaviour.

## Boundary notes

- **Attach-lock.** Dispatch stamps `holder_instance_id` on the lease
  row and checks it before launch: a session attached for *another*
  task makes the launch fail 409 `dir-busy` (the queued task waits);
  later sessions of the same task (its tabs) are allowed. Instance
  delete (task 2) clears the holder and returns the lease. Sharing is
  serial — the second task never starts concurrently; the sharing e2e
  deletes the holder's session and then launches the queued task in the
  same cwd.
- **No path/repo over wire.** Binding reuse still performs no git RPC;
  pool goes through task 2's whitelisted
  `{hostId, workspaceId, name, base, taskId}` lease.
- **D-035.** Every conflicting dispatch input (host, workspace, cwd,
  worktree) and every lease refusal is an explicit 4xx; nothing falls
  back to the main checkout or substitutes a directory.
- **Compliance.** Evidence uses generic terms and Remuda's own render
  only; no internal product names, internal documents, hostnames or
  usernames appear (the `/tmp/remuda-e2e` path is the fake harness's
  documented scratch root).
