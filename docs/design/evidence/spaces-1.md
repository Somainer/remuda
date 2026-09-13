# D-024 · Spaces and agent tabs

2026-09-13 · `wt/x-space/spaces-and-tabs`

The project workbench is implemented. **The registered-workspace live test is pending D-023**, as explicitly deferred by the user: merge the Node change, then workspace registration, then send `REBASE`. This report does not claim that the two-workspace live scenario passed on the pre-D-023 Hub.

## Behavior and scope

- Spaces use exact `(hostId, workspaceId)` identity from the Workspace inventory. Unmatched instances, including children, appear in `其他`; an unregistered root is never inferred from a similar path or name.
- Local device preferences retain space names, manual order, group collapse, panel collapse, hidden tabs and independent last-selected tabs. Desktop collapse leaves a space-initial rail and a persistent toggle. New Session entry points and the minimal page hook select the current space's host/workspace; cwd defaults to its root.
- Tabs display title, harness glyph, activity and close. A successful close request hides its local tab; rejection retains it. This is command acceptance, not proof of native exit. A deep link reopens a hidden tab without resuming its process. Delayed closure cannot pull the user away from another project.
- Desktop shortcuts are scoped to session routes: Cmd/Ctrl+B, 1–9, and brackets. They do not intercept composition, text inputs or terminal input. The phone workbench has scrollable space chips, a tab strip and a focus-contained drawer.
- Fleet/approvals pages and composer implementation files are unchanged. SessionPage keeps request-busy state per instance so one project's request does not disable another project's composer.

## Verification

| Check | Result / boundary |
| --- | --- |
| Grouping, ordering and local persistence unit tests | PASS: 11 tests, including duplicate workspace IDs on different hosts, children, stale selection, invalid/denied storage and close/reopen |
| Tab component tests | PASS: 3 tests for delayed close while switching projects, rejected close, and successor keyboard focus |
| Web unit suite | PASS: 172 tests after rebase onto `68684d5` |
| `pnpm lint` and `pnpm exec tsc -b` | PASS; lint has existing warnings in host/provider/NewSession code, no new spaces warnings |
| `pnpm build` | PASS; existing bundle-size advisory remains |
| `cargo test -p remuda-hub --locked` | PASS: 96 unit/integration tests, 0 failed |
| `cargo clippy -p remuda-hub --all-targets --locked -- -D warnings` | PASS |
| `cargo fmt --all -- --check` | PASS |
| Mock browser workbench | PASS: 3 consecutive runs after keyboard timing fix; Chrome channel, desktop 1440×900 and phone 400×860 |
| Existing Hub live regressions | PASS: 6 tests in 20.7s; owned `remuda dev` Hub 60180 / Node 60187, web 60188, fake Node |
| D-023 registered-space live scenario | **PENDING**: `web/tests/e2e/spaces-hub-live.spec.ts` |
| Secret/path hygiene | PASS: `./scripts/ci/secret-scan.sh`, `git diff --check`, zero internal-registry entries in the pnpm lockfile |

The first external Hub attempt lacked the web origin in the Hub allowlist (HTTP 403); it was rerun with `REMUDA_ALLOWED_ORIGINS`. A subsequent attempt reused an old fake-host row; the final run uses a fresh task-owned data directory. Neither setup error required product changes. The old base also failed the existing Hub message test in `assembleTranscript`; rebasing onto the already-merged message normalization fix resolved that baseline dependency.

## Browser reproduction and screenshots

Run from `web/`:

```sh
pnpm exec playwright test -c playwright.spaces.config.ts
```

The browser test uses **mock fixtures**, with demo host labels and generic project roots. These screenshots prove rendering and client navigation, not Node registration or native agent execution. The script selects sessions across spaces, checks independent selection and deep links, exercises keyboard switching, reloads a collapsed panel, checks mobile drawer navigation, and verifies rename/manual-order persistence. It records browser `pageerror` events and checks for horizontal page overflow at 400px. Each screenshot is generated directly by Playwright and must be no larger than 300,000 bytes.

| View | Screenshot | Bytes |
| --- | --- | ---: |
| Desktop · Night Corral | [mock-desktop-dark.png](./spaces-1/mock-desktop-dark.png) | 141,756 |
| Desktop · light | [mock-desktop-light.png](./spaces-1/mock-desktop-light.png) | 144,213 |
| Desktop · persisted collapsed rail | [mock-desktop-collapsed-dark.png](./spaces-1/mock-desktop-collapsed-dark.png) | 101,643 |
| Phone 400px · Night Corral | [mock-phone-dark.png](./spaces-1/mock-phone-dark.png) | 63,024 |
| Phone 400px · light | [mock-phone-light.png](./spaces-1/mock-phone-light.png) | 63,825 |
| Phone 400px · space drawer | [mock-phone-drawer-dark.png](./spaces-1/mock-phone-drawer-dark.png) | 45,040 |

For the existing live regressions, attach the fake Node example to the owned dev Hub with `HUB_E2E_EXTERNAL=1` and `HUB_E2E_LISTEN=127.0.0.1:60180`, then run:

```sh
HUB_E2E_EXTERNAL=1 HUB_E2E_LISTEN=127.0.0.1:60180 \
HUB_E2E_WEB_PORT=60188 VITE_HUB_URL=http://127.0.0.1:60180 \
pnpm run test:e2e:hub --grep-invert 'registered spaces'
```

Use the public fake-engine bootstrap fixture and allow `http://127.0.0.1:60188` on the dev Hub. The fake Node advertises `wsp_e2e` at `/tmp/remuda-e2e` and `wsp_e2e_second` at `/tmp/remuda-e2e-second` using D-023's `workspaceRevision/workspaces` fields and `workspace.list` response. These fields compile on the old Hub; publishing them as registered inventory requires D-023.

After `REBASE`, run the complete `pnpm run test:e2e:hub` against a fresh fixture. The prepared spaces spec creates two agents in the first registered workspace and one in the second, verifies isolated strips and last-selected tabs, deep-link restoration, New Session defaults, keyboard shortcuts, persistent collapse, mobile chips/drawer and close persistence, and writes live screenshots without the `mock-` prefix. It must pass before this evidence can be described as registered-workspace live acceptance.
