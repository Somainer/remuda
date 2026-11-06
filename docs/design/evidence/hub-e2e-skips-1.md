# Hub e2e skip audit (2026-11-06)

Every self-skipping spec in the Hub live suite, why it skips, and where it
runs. The gate's live Hub checks are two steps
([coordinator guide](../coordinator-guide.md)):

- `web-hub-e2e` — `pnpm -C web run test:e2e:hub`, the main pass. It starts the
  in-process Hub + fake Node with no `HUB_E2E_*` trigger, so 31 tests behind a
  harness trigger (none of them had ever run in a pass) and 14 Main-seating
  tests skip there, plus the capture-only evidence tests.
- `web-hub-e2e-main` — `pnpm -C web run test:e2e:hub:main`
  (`web/scripts/e2e-hub-main.mjs`), the opt-in pass. Before this audit it ran
  only the `HUB_E2E_MAIN_UI` specs (`main`, `home-extras`, `owner-notify`,
  `routed-to`, `seat-profile`). It now also runs every other spec that
  self-skips on a harness trigger no pass set (`fake-node`,
  `effort-sentinels`, `task-bind`, `project-switcher`, `dir-picker`,
  `cardsettle`, `api-route`), one Hub per trigger group.

CI runs the same two steps (`.github/workflows/ci.yml`), in the same order.

The inventory below is every `test.skip`/`test.fixme`/`describe.skip` site in
the Hub specs. `pnpm exec playwright test -c playwright.hub.config.ts --list
--reporter=json` reports 446 tests and 71 collection-time skip annotations; the
rest of the gate's ~103 skipped come from runtime guards inside tests
(evidence capture and fixture probes), which only a real run reports.

## Disposition key

| Key | Meaning |
| --- | --- |
| **runs** | Runs in a gate pass today (say which). |
| **wired** | Used to skip everywhere; this branch adds it to the opt-in pass. |
| **justified** | Deliberately not a gate test; the reason is in the table. |
| **deleted** | Obsolete. |

## Trigger-gated and Main-gated specs

These are the skips that hid real coverage. Counts are `--list` tests per
gated block. The main pass sets none of these flags.

| Spec:line | Condition | Tests | Gate before | Disposition |
| --- | --- | --- | --- | --- |
| `api-route.hub.spec.ts:35` | `HUB_E2E_API_ROUTE !== "1"` | 1 | never ran (main pass skips; opt-in does not set the flag) | **wired** — opt-in group `api-route` |
| `cardsettle.hub.spec.ts:26` | `HUB_E2E_CARDSETTLE !== "1"` | 2 | never ran | **wired** — group `cardsettle` |
| `dir-picker.hub.spec.ts:24` | `HUB_E2E_DIR_PICKER !== "1"` | 2 | never ran | **wired** — group `dir-picker` |
| `effort-sync.hub.spec.ts:444` | `HUB_E2E_FAKE_NODE !== "1"` | 3 | never ran (nothing in the repo set the flag) | **wired** — group `effort-sentinels` (grep: only this describe) |
| `ux-permmode.hub.spec.ts:68` | `HUB_E2E_FAKE_NODE !== "1"` | 4 | never ran | **wired** — group `fake-node` |
| `cmd-queue.hub.spec.ts:84` | `HUB_E2E_FAKE_NODE !== "1"` | 3 | opt-in pass (cb-command-queue r2, beside the Main specs) | **runs** — group `fake-node` |
| `task-model-bind.hub.spec.ts:30` | `HUB_E2E_TASK_BIND !== "1"` | 8 | never ran | **wired** — group `task-bind` |
| `task-model-boardui.hub.spec.ts:39` | `HUB_E2E_TASK_BIND !== "1"` | 4 | never ran | **wired** — group `task-bind` |
| `task-model-project.hub.spec.ts:44` | `HUB_E2E_PROJECT_SWITCHER !== "1"` | 7 | never ran | **wired** — group `project-switcher` |
| `home-extras.hub.spec.ts:25` | `HUB_E2E_MAIN_UI !== "1"` | 2 | opt-in pass (since 2026-11-05) | **runs** — group `main` |
| `main-ui.hub.spec.ts:26` | `HUB_E2E_MAIN_UI !== "1"` | 4 | opt-in pass | **runs** — group `main` |
| `owner-notify.hub.spec.ts:244` | `HUB_E2E_MAIN_UI !== "1"` | 4 | opt-in pass | **runs** — group `main` |
| `routed-to.hub.spec.ts:28` | `HUB_E2E_MAIN_UI`/`REMUDA_DELEGATED_DECISIONS`/`HUB_E2E_LINEAGE_FENCE` `!== "1"` | 2 | opt-in pass | **runs** — group `main` |
| `seat-profile.hub.spec.ts:23` | `HUB_E2E_MAIN_UI !== "1"` | 2 | opt-in pass | **runs** — group `main` |

`HUB_E2E_FAKE_NODE=1` is the one flag the harness itself does not read: it is
the spec-side announcement that the in-process fake Node is the backend (the
opt-in Hub always runs one). `web-hub-e2e-main` sets it for both groups.

`web/scripts/e2e-hub-main.test.mjs` keeps these lists honest: it scans every
`*.hub.spec.ts` for a `process.env.FLAG !== "1"` guard and fails unless the
spec appears in exactly one group whose env sets every non-exempt flag it
needs. A new trigger-gated spec cannot silently skip the gate again.

## Capture-only and perf skips (justified)

These tests exist to write committed evidence images or perf numbers, not to
assert behaviour in a gate run. They are refreshed by hand. No gate pass (main,
opt-in, or CI) sets `REMUDA_EVIDENCE`, `HUB_E2E_HOME_EVIDENCE` or
`REMUDA_PERF`, so all of them skip in every pass today.

| Spec:line | Condition | Gate today | Disposition |
| --- | --- | --- | --- |
| `home-evidence.hub.spec.ts:58` | `!REMUDA_EVIDENCE` | skipped | **justified** — the home evidence matrix writes PNGs only |
| `home-evidence.hub.spec.ts:62` | `HUB_E2E_HOME_EVIDENCE !== "1"` | skipped | **justified** — the harness must also start the showcase/laptop Nodes; evidence only |
| `uo1-evidence.hub.spec.ts:126`, `uo2a:16`, `uo2b:22`, `uo8:20`, `uo9:19`, `uo13:19`, `ui-tokens-evidence.hub.spec.ts:145` | `!REMUDA_EVIDENCE` | skipped | **justified** — screenshot capture specs |
| `theme-boot:164`, `task-model-space:318`, `m-inbox:456`, `m-shell:263,315`, `ux-attach-files:376`, `ux-comment:230`, `ux-wfdrill:197`, `task-model-annotations:284`, `effort-sync:138`, `task-model-project:365`, `uo6a:392,401` | `!REMUDA_EVIDENCE` / `!== "1"` | skipped | **justified** — individual screenshot tests inside otherwise-gated specs |
| `uo5-evidence.hub.spec.ts:468` | `!instanceId` | skipped (evidence spec) | **justified** — serial evidence spec: later shots reuse the session the first test created |
| `uo6a-evidence.hub.spec.ts:218` | `REMUDA_PERF !== "1"` | skipped | **justified** — idle-commit perf probe, run by `playwright.perf.config.ts` |

## Engine- and project-specific skips (justified)

| Spec:line | Condition | Gate today | Disposition |
| --- | --- | --- | --- |
| `composer-popover:352`, `memory-ui:412`, `uo6b:544` | no `PW_TEST_CONNECT_WS_ENDPOINT` and no bundled WebKit | main pass runs them on the devbox lane (which sets the shared run-server endpoint); CI has no WebKit and skips | **justified** — iPhone/WebKit cases; a host without a WebKit build skips rather than fails |
| `uo6a:409` | WebKit launch failed (`!REMUDA_EVIDENCE` also holds) | never runs in the gate — its enclosing test is evidence-only | **justified** — evidence capture, and WebKit is absent on CI |
| `new-session.spec.ts:312,399,438,439` | `info.project.name !== "chromium"` | the hub config has one chromium project, so these run | **justified** — desktop-chromium-only geometry/evidence cases |
| `new-session.spec.ts:105,353,463` | `hubLive` / `!hubLive` | each case runs in exactly one of the mock and Hub configs | **justified** — the same file runs under both configs |
| `m-realdevice.hub.spec.ts:991` | engine surfaces no install offer | main pass runs it; non-WebKit engines take the skip | **justified** — the install-banner case is WebKit-only by design; other engines skip explicitly rather than pass without the banner |
| `m-realdevice.hub.spec.ts:379,736` | fake Node journal lacked the sentinel | main pass runs it; the current harness emits both sentinels | **justified** — old-fixture guard |

## External-Node mode (justified; not skipped in the gate)

`HUB_E2E_EXTERNAL=1` runs the suite against an operator's real Node instead of
the in-process fake Node. Those guards (`=== "1"` in 18 specs: `api-route:31`,
`composer-popover:18`, `font-swap:28`, `journal-window:28`, `native-progress:34`,
`node-inventory:28`, `question-alert:32`, `resume-overlay:5`, `settings-tui:19`,
`task-model-annotations:27`, `ux-attach-files:40`, `ux-comment:34`,
`ux-imgref:16`, `ux-toolfold:28`, `ux-ttymode:33`, `ux-wfdrill:23`,
`ux-workflow-card:13`, `hub-live:142,230,406,505,542,657,727`) only skip in
that operator mode; the gate leaves `HUB_E2E_EXTERNAL` unset, so they run.

`hub-live.spec.ts:325` is the mirror case (`HUB_E2E_EXTERNAL !== "1"`): it
needs the operator's own `remuda dev` workspace and skips in the gate.

## Fixture-absence guards (justified; run in the gate)

These skip only when the request-time fixture is missing, which the in-process
fake Node always provides in the gate. They stay as the safety net for
`HUB_E2E_EXTERNAL` runs:

- `needsFakeNode(page)` probes: `home-freshness:123,167,227`,
  `home-queue:291,432,463,544`, `home-glance:261,324,470,529,589`,
  `home-seen:117,161,192,235`.
- host/session probes: `memory-ui:197,299,341,426`, `home-polish:315,400,491,540`,
  `owner-notify:255`, `uo10-evidence:191,317,416,493,623,924,1006,1082,1172`,
  `m-realdevice:432,508,638,954,1038,1079`.
- built-binary probes: `grok-structural:552,696`, `ux-usage-native:128` — the
  specs build the binaries in their own `beforeAll` (and CI prebuilds
  `-p remuda-testing --bins`), so the skips do not fire in the gate.

## Obsolete skips

None. Every skip above either runs somewhere or names a real, current
constraint.

## One product conflict surfaced by the audit

`task-model-boardui.hub.spec.ts:561` ("fetches /v1/projects on mount only,
never on a poll interval") fails once the spec runs: a 31 s idle window on
`/board?project=…` issues one extra `/v1/projects` read (6 -> 7). That read
comes from the shared task-ledger poller (`PROJECT_POLL_MS = 30_000`,
`web/src/features/tasks/taskLedger.ts`), which `da8a1f1df "fix(home): one
task-ledger poller per scope"` (2026-11-01) made deliberate for the phone
surfaces. The board spec still asserts the old "mount only" discipline.
Per the audit brief this is not fixed on this branch: the single test is
marked `test.fixme` with the reason so the gate stays green and the conflict
stays visible. Resolve by either moving the board off the ledger's project
poll or narrowing the assertion to "at most one 30 s poll".

## Budget

Measured on the shared devbox with the gate env (`CI=1`,
`PW_CHANNEL=chromium`, the shared Playwright run-server, the gate-e2e flock,
ports 59740/59741/59749), see `scratch/proof/`:

| Pass | Groups | Tests | Wall |
| --- | --- | --- | --- |
| before this branch | 1 (one Hub) | 14 | **209 s** (includes the first `cargo run --example hub_e2e` build in this worktree) |
| after this branch | 8 (one Hub each) | 45 (44 pass, 1 `test.fixme`) | **692 s** measured with `web/dist` absent |

Per group in the after pass (each includes its own Hub + Vite start):
`main` 174 s, `fake-node` 66 s, `effort-sentinels` 66 s, `task-bind` 132 s,
`project-switcher` 72 s, `dir-picker` 58 s, `cardsettle` 60 s, `api-route`
55 s.

**Read the 692 s with the gate's build order in mind.** This worktree had no
`web/dist`, so `crates/remuda-hub/build.rs` (`rerun-if-changed=../../web/dist`)
reran on every `cargo run`, recompiling `remuda-hub` once per group: the log
records 43-46 s of `Compiling remuda-hub` before each of the eight groups. The
gate runs `web-build` (`pnpm build`) before the e2e steps, so in the gate that
per-group rebuild does not happen; subtracting the eight measured rebuilds
puts the after pass at ~340 s, with `main` ~130 s. The before number keeps one
such rebuild because the example had not yet been built for this single-package
feature set.

At ~340 s the pass is comfortably inside the 900 s step budget and the ~600 s
guidance. **Headroom proposal (not done here; it changes the gate plan, CI and
the merge-plan tests):** if more specs join the trigger groups, split them into
a second opt-in step `web-hub-e2e-triggers` (same script with a group
selector), leaving `web-hub-e2e-main` with the `main` group.
