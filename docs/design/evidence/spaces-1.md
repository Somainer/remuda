# D-024 · Spaces and agent tabs

2026-09-13 · `wt/x-space/spaces-and-tabs` · rebased onto D-023 at `9bd89f3`

**PASS: registered-workspace live acceptance is complete.** The full Hub browser suite passed against the owned `remuda dev` Node and against the standalone fake Node. `web/tests/e2e/spaces-hub-live.spec.ts` runs in both modes without a skip or grep exclusion.

## Behavior and scope

- Spaces use exact `(hostId, workspaceId)` identity from registered Workspace inventory. Unmatched instances, including children, appear in `其他`; an unregistered root is never inferred from a similar path or name.
- Local device preferences retain space names, manual order, group collapse, panel collapse, hidden tabs and independent last-selected tabs. Desktop collapse leaves a space-initial rail and a persistent toggle. New Session entry points and the minimal page hook select the current space's host/workspace; cwd defaults to its root.
- Tabs display title, harness glyph, activity and close. A successful close request hides its local tab; rejection retains it. This is command acceptance, not proof of native exit. A deep link reopens a hidden tab without resuming its process. Delayed closure cannot pull the user away from another project.
- Desktop shortcuts are scoped to session routes: Cmd/Ctrl+B, 1–9, and brackets. They do not intercept composition, text inputs or terminal input. The phone workbench has scrollable space chips, a tab strip and a focus-contained drawer.
- Fleet/approvals pages, HostsPage and composer implementation files are unchanged. NewSessionPage retains only the defaults hook; D-023's registered workspace ID is preserved. SessionPage keeps request-busy state per instance so one project's request does not disable another project's composer.

## Verification

| Check | Result / boundary |
| --- | --- |
| Grouping, ordering and local persistence unit tests | PASS: 11 tests, including duplicate workspace IDs on different hosts, children, stale selection, invalid/denied storage and close/reopen |
| Tab component tests | PASS: 3 tests for delayed close while switching projects, rejected close, and successor keyboard focus |
| Concurrent journal follow regression | PASS: 2 tests; same-journal subscription count failed before the fix and passed after it, while distinct journals remain independent |
| Web unit suite | PASS: 185 tests in 49 files |
| `pnpm lint` and `pnpm exec tsc -b` | PASS; existing host/provider/NewSession warnings, no new spaces warnings |
| `pnpm build` | PASS; existing bundle-size advisory remains |
| `cargo test -p remuda-hub --locked` | PASS: 97 tests, 0 failed |
| `cargo clippy -p remuda-hub --all-targets --locked -- -D warnings` | PASS |
| `cargo fmt --all -- --check` | PASS |
| Mock browser workbench | PASS: 3 consecutive runs after keyboard timing fix; Chrome channel, desktop 1440×900 and phone 400×860 |
| Full Hub suite, owned native Node | PASS: 7 passed / 1 conditional skip in 47.1s; spaces scenario passed in 18.6s |
| Full Hub suite, standalone fake Node | PASS: 7 passed / 1 conditional skip in 41.4s; spaces scenario passed in 11.3s |
| Secret/path hygiene | PASS: `./scripts/ci/secret-scan.sh`, `git diff --check`, zero internal-registry entries in the pnpm lockfile |

The native suite's conditional skip is the legacy fake-engine command/approval flow; the standalone suite runs it. The standalone suite skips D-023's native register/create/close/unregister scenario, which passed against the real Node. Neither mode skips the spaces scenario.

The native spaces test reads the real Hub workspace registry, requires a nonzero revision and distinct IDs for the two registered roots, then creates two shell sessions in `project-alpha` and one in `project-beta`. It verifies each instance's host/workspace/cwd fields and actual `SPACE_CWD` output from the native PTY, with `kind=terminal`, `driver=shell-pty` and `lifecycle=running`. It checks isolated tab strips, independent selection, deep-link restoration, New Session defaults, desktop shortcuts, panel collapse across reload, phone chips/drawer, no horizontal page overflow at 400px, and close persistence. Cleanup waits for all three owned sessions to reach `lifecycle=exited`; no model was invoked. The owned dev and test servers were stopped afterward, with ports 60180/60187/60188/60189 released.

The first native run exposed `FOLLOW_CONNECT_FAILED`: concurrent route mounts could both finish history reads and replace the same pending journal subscription. A second ownership check after history loading prevents the duplicate subscription and stale history overwrite. The deterministic regression failed before this change; both regression cases and the full native suite then passed. The final native spaces run recorded zero browser `pageerror` events. Vite still logged `EPIPE` during WebSocket teardown on navigation; terminal reconnection, actual cwd output and all assertions passed. Raw logs, DOM dumps and traces are not published as sanitized evidence.

## Reproduction

Use a fresh task-owned data directory and an owned `remuda dev` with Hub `127.0.0.1:60180`, Node `127.0.0.1:60187`, web origin `http://127.0.0.1:60188`, and that origin included in `REMUDA_ALLOWED_ORIGINS`. The public browser bootstrap fixture is `e2e-bootstrap-token`, supplied through an access-code file. Set `REMUDA_MAX_INSTANCES=8`, disable the Herdr orphan sweep for this isolated run, and use generic existing directories under `/private/tmp/remuda-x-space/d024`:

- `--workspace-root /private/tmp/remuda-x-space/d024`
- `--workspace /private/tmp/remuda-x-space/d024/project-alpha`
- `--workspace /private/tmp/remuda-x-space/d024/project-beta`
- Create `registration-check` under the same root for the independent D-023 registration scenario.

Set `HUB_E2E_HOST_ID` to this owned Node's live host ID. From `web/`, run the complete native suite:

```sh
HUB_E2E_EXTERNAL=1 HUB_E2E_LISTEN=127.0.0.1:60180 \
HUB_E2E_WEB_PORT=60188 VITE_HUB_URL=http://127.0.0.1:60180 \
HUB_E2E_HOST_ID="$HUB_E2E_HOST_ID" \
HUB_E2E_WORKSPACE=/private/tmp/remuda-x-space/d024/registration-check \
HUB_E2E_SPACE_PRIMARY=/private/tmp/remuda-x-space/d024/project-alpha \
HUB_E2E_SPACE_SECONDARY=/private/tmp/remuda-x-space/d024/project-beta \
pnpm run test:e2e:hub
```

After stopping that dev process, omit `HUB_E2E_EXTERNAL` and run the standalone gate on the same owned ports. Its config starts a disposable Hub and fake Node advertising both `wsp_e2e` and `wsp_e2e_second`; fake screenshots use a separate `fake-` prefix and cannot overwrite native evidence.

```sh
HUB_E2E_LISTEN=127.0.0.1:60180 HUB_E2E_WEB_PORT=60188 \
VITE_HUB_URL=http://127.0.0.1:60180 pnpm run test:e2e:hub
```

## Playwright screenshots

The following PNGs come from the passing **real Node** run, using desktop 1440×900 and phone 400×860. Before each capture, the test waits for the terminal connection, renderer readiness and actual cwd output. The shell clears screen/scrollback and replaces personal startup customizations with a clean environment. Images are generated directly by Playwright Chrome, inspected for personal paths, and checked against the 300,000-byte limit.

| View | Screenshot | Bytes |
| --- | --- | ---: |
| Desktop · Night Corral | [desktop-dark.png](./spaces-1/desktop-dark.png) | 73,488 |
| Desktop · light | [desktop-light.png](./spaces-1/desktop-light.png) | 75,139 |
| Desktop · persisted collapsed rail | [desktop-collapsed-dark.png](./spaces-1/desktop-collapsed-dark.png) | 52,014 |
| Phone 400px · Night Corral | [phone-dark.png](./spaces-1/phone-dark.png) | 42,424 |
| Phone 400px · light | [phone-light.png](./spaces-1/phone-light.png) | 42,489 |
| Phone 400px · space drawer | [phone-drawer-dark.png](./spaces-1/phone-drawer-dark.png) | 29,368 |

Earlier **mock fixture** evidence remains separate. It covers agent conversation rendering, rename/manual-order persistence, keyboard switching and drawer navigation; it does not prove native execution. Reproduce with `pnpm exec playwright test -c playwright.spaces.config.ts`.

| View | Screenshot | Bytes |
| --- | --- | ---: |
| Desktop · Night Corral | [mock-desktop-dark.png](./spaces-1/mock-desktop-dark.png) | 141,756 |
| Desktop · light | [mock-desktop-light.png](./spaces-1/mock-desktop-light.png) | 144,213 |
| Desktop · persisted collapsed rail | [mock-desktop-collapsed-dark.png](./spaces-1/mock-desktop-collapsed-dark.png) | 101,643 |
| Phone 400px · Night Corral | [mock-phone-dark.png](./spaces-1/mock-phone-dark.png) | 63,024 |
| Phone 400px · light | [mock-phone-light.png](./spaces-1/mock-phone-light.png) | 63,825 |
| Phone 400px · space drawer | [mock-phone-drawer-dark.png](./spaces-1/mock-phone-drawer-dark.png) | 45,040 |
