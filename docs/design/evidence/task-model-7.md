# Task model · task 7 (`t-taskspace`): task space vs project space file panels

Date: 2026-09-21. Scope: the task-space panel beside the unchanged project
space, reusing the existing real-time file view with **no new Node or Hub
endpoint**. Plan: `briefs/plans/task-model.md` §(C) task 7 and §B.5;
design authority `docs/design/task-model.md` §7; decision
[D-050](../decisions.md) §7; contract `docs/design/files-view-contract.md`
§3.5–3.7. Base: `origin/main` @ `3d257d14` (all of M1 landed; the task
ledger, board projection and directory binding already exist).

## What landed

| Layer | File | Change |
|---|---|---|
| Web | `web/src/features/tasks/taskSpaceFilter.ts` (new) | pure client-side projection: owns[] glob matcher (byte-for-byte TS port of `remuda_protocol::glob_match`/`normalize_glob`), `filterTaskEntries`, §3.6 availability pass-through boundary (`taskSpaceEntries`), and scope builder over the placement ledger (`taskScopeFrom`) |
| Web | `web/src/features/tasks/taskSpaceFilter.test.ts` (new) | 24 vitest cases: glob parity, filtering, rename either-side, the empty projection, availability pass-through, session-set build |
| Web | `web/src/features/files/FilesView.tsx` | **one additive prop** `taskFilter?: {label, owns}` → a 项目空间 / 任务空间 tab pair; both tabs read the same live payload; the task tab renders 「还没有文件」 on an empty projection. No other file-view behaviour changed |
| Web | `web/src/features/tasks/TaskSpacePanel.tsx` (new) | resolves the open session's owning task through the existing `/v1/tasks` + `/v1/tasks/{id}/placements` routes and mounts `FilesView` with the task filter; a session with no owning task renders the unchanged project-space view |
| E2E | `web/tests/e2e/task-model-space.hub.spec.ts` (new) | 5 hub-live cases (incl. 390/1440 evidence renders) over the **default, ungated** fake Node — no fixture change, the g2 scenario workspaces already serve `workspace.scm.*` |

No Node, Hub, protocol, schema, migration, route or generated-client change.
No `playwright.hub.config.ts` / `sw.src.js` change. The e2e mounts the panel
from inside the already-loaded Remuda shell through the Vite module graph (a
same-page dynamic `import()` of `TaskSpacePanel.tsx` over an overlay node —
no lab HTML, no route, no second entry is committed); every screenshot pixel
is a Remuda component. Task 5 will mount the same panel in its app surface.

## Acceptance mapping (plan task 7)

1. **Project space stays the unchanged worktree tree on its existing axis.**
   `FilesView` without `taskFilter` is byte-similar to before; with the
   filter it defaults to the project tab, which renders the same
   `hostId + workspaceId` status rows, diffs and previews as before
   (e2e: all three fixture rows — `src/main.rs`, `notes/todo.md`,
   `assets/logo.bin`).
2. **Task space = the same view filtered by the task's session set +
   owns[] globs; empty state, never synthesised entries.** The session set
   comes from `GET /v1/tasks/{id}/placements` (the wire form of
   `instances.task_id`) and fixes the workspace axis (the panel only opens
   over a workspace the task's sessions ran in); the rows are narrowed by
   the task's `owns[]` globs with Hub gate semantics. `src/**` +
   `notes/**` project to two rows; `docs/**` projects to zero and renders
   「还没有文件」 (e2e asserts no `files-entry` rows in either case). A
   rename stays visible when either side is owned.
3. **No new endpoint; client-side filtering over the existing reads.** Both
   tabs are driven by one `workspace.scm.status` answer
   (`filesApi.ts` `fetchChanges`); switching tabs emits **zero** additional
   `/changes` requests (asserted in the e2e), and opening a row reuses the
   existing `workspace.scm.diff` / `.file` read unchanged. Task metadata
   uses the task-ledger routes that shipped with M1.
4. **Non-git / unavailable workspaces fall through to the six availability
   states; no faked baseline.** The filter boundary only narrows a resolved
   `changes` view — `loading / not-collected / clean / unsupported /
   forbidden / offline / missing / failed` all pass through untouched
   (vitest one-case-per-phase; e2e drives `wsp_g2_nogit` → 不支持 in both
   tabs and a 409 `HOST_OFFLINE` proxy → 离线 in both tabs).

## Verification

- `pnpm --dir web test` — full vitest suite, 151 files / 1507 tests pass
  (24 new).
- `pnpm --dir web typecheck` and `pnpm --dir web lint` — clean (pre-existing
  warnings only).
- `playwright test -c playwright.hub.config.ts task-model-space.hub.spec.ts`
  — run **three times** under lock slot `e2e.lock-c` with
  `HUB_E2E_LISTEN=127.0.0.1:59320`, `HUB_E2E_WEB_PORT=59329`,
  `HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59321`; results recorded below.
- Full web hub e2e suite once under the same lock slot; recorded below.
- `bash scripts/ci/secret-scan.sh`, `bash scripts/ci/no-tunnel-scan.sh`
  PASS.

## Evidence renders (Remuda only)

All screenshots are Remuda renders against the synthetic fake-Node g2
fixture workspaces (`/tmp/remuda-…` scratch paths only); no real
repository, no models.

| Render | File |
|---|---|
| 1440 desktop, project space (full worktree changes) | `task-model-7-project-1440.png` |
| 1440 desktop, task space (owns[] projection, two rows) | `task-model-7-task-1440.png` |
| 390 phone, task space in the shared `/files` route | `task-model-7-task-390.png` |

## Compliance

Generic wording only in committed files; no reference-product names, no
internal documents, no hostnames/usernames/home paths; screenshots show
only Remuda renders. No tunnel, no new listening surface, nothing under
`deploy/`.

## Verification results

- `pnpm --dir web test`: **151 files / 1507 tests pass** (24 new in
  `taskSpaceFilter.test.ts`).
- `pnpm --dir web typecheck`: pass. `pnpm --dir web lint`: pass
  (warnings are pre-existing compiler/lint advisories in untouched files).
- `task-model-space.hub.spec.ts` × **3 consecutive runs** under
  `flock …/locks/e2e.lock-c` with
  `HUB_E2E_LISTEN=127.0.0.1:59320 HUB_E2E_WEB_PORT=59329
  HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59321` (plus `PW_CHANNEL=chromium`,
  the config-documented fallback on a host without Google Chrome):
  **5 passed each time** (52.1s / 52.3s / 52.2s), exit 0 all three.
- Full web hub e2e suite (`playwright test -c playwright.hub.config.ts`)
  once under the same lock slot and ports: **175 passed, 28 skipped, 0
  failed** (26.2m). The 28 skips are the self-skipping gated specs
  (`HUB_E2E_TASK_BIND` and peers absent in a default run).
- `bash scripts/ci/secret-scan.sh`: pass.
  `bash scripts/ci/no-tunnel-scan.sh`: pass.

## Merge gate note

This task adds `web/src/features/tasks/**` and a new hub spec
`web/tests/e2e/task-model-space.hub.spec.ts`, which are not in the
automatic `--web-e2e` list; `remuda merge` must pass `--web-e2e`
explicitly for this change (task-model plan §(C) common convention). The
new spec is **not** gated — it runs in every default full-suite run
(unlike the bind spec, it needs no fake-node trigger), which the full run
above proves.

## The two default-run guards (same problem class)

A hub spec has to behave in a default gate run; two distinct guards make
that true here:

1. **Gated-fixture self-skip** (the bind-spec rule): a spec that needs a
   fake-Node trigger (`HUB_E2E_TASK_BIND`-style) must self-skip when the
   trigger is absent, or the default full-suite run fails the landing
   gate. This spec needs **no** trigger — it uses the default fake's g2
   workspaces — so it deliberately has no such skip and always executes.
2. **Evidence capture guard** (`REMUDA_EVIDENCE=1`, the m-keybar /
   cua-media idiom): the three committed PNGs were regenerated on every
   run, and even a one-pixel font/timing difference dirtied the gate's
   working tree (this was found at the web gate). The evidence describe
   block now calls `test.skip(!evidence, …)` and all writes go through a
   `shot()` helper that is a no-op without the flag. A default run
   executes the behavioral assertions — including an always-on 390px
   test that keeps the phone narrowing covered — and touches **no** file
   under `docs/`; `REMUDA_EVIDENCE=1` runs the two capture tests and
   rewrites only the three intended PNGs.

### Round 3 verification (evidence-guard fix)

- Default run (no flag): **5 passed, 2 skipped, working tree unchanged**
  (only the spec source is modified pre-commit; zero files under
  `docs/design/evidence/` written).
- `REMUDA_EVIDENCE=1` run: 7 passed; only
  `task-model-7-{project-1440,task-1440,task-390}.png` are eligible to
  change, and the committed blobs were verified to match the fresh
  captures (PNGs restored to/kept at the committed round-1 renders).
- `pnpm --dir web typecheck` / `lint`: clean.


Post-rebase: branch rebased onto `origin/main` `fd37d714` (effortflake
merge; web store-only delta, no file/task overlap). After rebase:
vitest **151 files / 1511 tests pass**, typecheck/lint clean, and
`task-model-space.hub.spec.ts` rerun **5/5 passed**; evidence PNGs are
byte-identical after the rerun.
