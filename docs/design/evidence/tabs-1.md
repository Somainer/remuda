# D-024 addendum · Tab semantics: status, close, dismissal and exited sessions

2026-09-13 · `wt/x-tabs/tab-semantics` · rebased onto `288fde4`

Addresses two pieces of user feedback on the D-024 strip: the active tab and the active sidebar item were not distinguishable enough, and the exited-status glyph and the close button were both `×` sitting side by side.

## What changed

- **Status and close are separate.** The status indicator is a shape that is never an `×`: `⚠` blocked (amber), `●` working, `○` idle (green outline), `■` exited (grey), dashed outline unknown, each with a Chinese `aria-label` and `role="img"`. Shapes differ without colour. `×` now means only "close tab": on desktop it appears on hover or on the active tab; on touch a long press or a >48px horizontal swipe reveals it, at a 44px target.
- **Closing a tab never stops a session silently.** An exited tab is removed directly, with no sheet and no command. A running tab opens a two-choice sheet 「停止并关闭 / 仅关闭标签」 plus 取消. 「仅关闭标签」 sends nothing to the Hub.
- **Dismissal is a per-space device preference.** `closedTabs` grew from a list of ids to `{id, resurface}` records; a device that closed tabs before this change reads its plain ids back as `resurface: true`, so nothing re-opens unexpectedly and nothing is lost. A dismissed session keeps running, returns to the strip when it becomes `blocked`, and re-opens on a sidebar click. Dismissing it *while* blocked suppresses only that episode and re-arms once the episode ends; a stopped session never resurfaces.
- **Sidebar active states.** The current space and the current session both carry the brand left bar, tinted ground and bold title.
- **Exited group.** Each space gets 「已退出 (n)」, collapsed by default, with 恢复 (existing `hubStore.resume`) and 删除 (`DELETE /v1/instances/{id}`, confirmation 「删除会话及其记录？」; a running session is offered 停止并删除 instead).

## Delete is wired to the real endpoint

`DELETE /v1/instances/{id}` is on `origin/main` (`288fde4`, from x-ttyhub), and 删除 / 停止并删除 call it directly. There is no feature detection and no fallback path:

- A terminal-state session deletes outright.
- A live session is refused with `409` unless `?force=1`, which makes the **Hub** stop it and then delete it. 停止并删除 sends that one request; the client deliberately does **not** close the session itself first, which would be a second, racing stop.
- A repeated delete answers `404`, so the client treats it as an idempotent success rather than an error.
- The response's `nodePurge` reports the Node side only. The Hub record is deleted whatever it says, so anything other than `purged` produces 「已删除会话；该主机数据待其上线后清理」 instead of a bare success.
- A refused delete keeps the row listed and says 删除失败，请重试; it never shows a success message.

`InstanceDeleted` comes from the generated OpenAPI types rather than a hand-written shape, so a later change to the response is a typecheck failure here instead of a silent mismatch. The mock adapter mirrors the same contract — `409` on a live instance without force, `404`-idempotent, `nodePurge` reported — so the client paths are exercised without a Hub. This branch contains no Rust change.

An earlier revision of this branch predated the endpoint and carried a `405`/`501` → hide-the-row fallback, with `hiddenSessions` in the device preferences. Both are removed now that the route is on main; keeping them would have left an unreachable path and a persisted field nothing writes.

The delete target is held as `{spaceId, instanceId}` and re-resolved from current props each render, so a session that resumes while the sheet is open is offered 停止并删除 rather than the exited-only action.

## Verification

| Check | Result / boundary |
| --- | --- |
| `pnpm test` (web unit) | PASS: 251 tests in 56 files |
| Dismiss/resurface and exited grouping unit tests | PASS: 13 store tests, including legacy `closedTabs` migration, blocked-episode suppression and re-arming, and exited grouping |
| `SpaceTabs` component tests | PASS: 6 tests — exited tab closes with no sheet and no command; 仅关闭标签 sends no close; cancel is inert; dismissed tab returns on blocked; plus the three pre-existing async-close cases |
| `SpacesPanel` component tests | PASS: 6 tests — default-collapsed group, resume, confirm-before-delete, a refused delete keeping the row, `force=1` for a live target with no separate close, and the non-`purged` `nodePurge` message |
| Mutation checks | Each new assertion was re-run against a deliberately broken implementation (resurface disabled, exited routed through the sheet, 仅关闭 wired to stop, group defaulting open, `force` dropped, `nodePurge` ignored) and failed in every case |
| `pnpm lint` | PASS: 4 pre-existing warnings in hosts/providers/NewSession, none in spaces |
| `pnpm exec tsc -b` | PASS |
| `./scripts/ci/secret-scan.sh` | PASS |
| Tab-semantics browser run (mock fixture) | PASS: 2 tests, desktop 1440×900 and phone 400×860, both themes |
| Full Hub suite `pnpm run test:e2e:hub` | PASS: 8 passed in 1.3m (standalone Hub + fake Node), including the extended spaces scenario |

The phone assertions run under Playwright touch emulation (`hasTouch`/`isMobile`), because only that makes `(pointer: coarse)` and `(hover: none)` match — the media queries the hover-free close affordance depends on. The test asserts this is in effect before relying on it. An earlier revision captured the phone screenshots without emulation and produced files byte-identical to the desktop-media ones; every published PNG is now distinct. The focus-ring case likewise reaches the close control with a real `Tab` press, since programmatic `.focus()` does not satisfy `:focus-visible`, and asserts a non-zero outline rather than only a screenshot.

## Playwright screenshots

Generated by Chrome at desktop 1440×900 and phone 400×860 from the mock fixture, whose host labels and workspace roots are replaced with generic demo inventory before capture; each render is checked for `/Users/` and for the original labels, and each file against the 300,000-byte limit.

| View | Screenshot | Bytes |
| --- | --- | ---: |
| Desktop · Night Corral | [desktop-dark.png](./tabs-1/desktop-dark.png) | 140,629 |
| Desktop · light | [desktop-light.png](./tabs-1/desktop-light.png) | 142,805 |
| Desktop · close-control focus ring | [desktop-close-focus-dark.png](./tabs-1/desktop-close-focus-dark.png) | 140,811 |
| Desktop · 停止并关闭 / 仅关闭标签 · Night Corral | [desktop-close-sheet-dark.png](./tabs-1/desktop-close-sheet-dark.png) | 130,042 |
| Desktop · 停止并关闭 / 仅关闭标签 · light | [desktop-close-sheet-light.png](./tabs-1/desktop-close-sheet-light.png) | 144,110 |
| Desktop · 已退出 group · Night Corral | [desktop-exited-group-dark.png](./tabs-1/desktop-exited-group-dark.png) | 141,576 |
| Desktop · 已退出 group · light | [desktop-exited-group-light.png](./tabs-1/desktop-exited-group-light.png) | 143,837 |
| Desktop · 删除会话及其记录？ | [desktop-delete-sheet-dark.png](./tabs-1/desktop-delete-sheet-dark.png) | 123,390 |
| Phone 400px · Night Corral (close hidden) | [phone-dark.png](./tabs-1/phone-dark.png) | 62,370 |
| Phone 400px · light | [phone-light.png](./tabs-1/phone-light.png) | 63,121 |
| Phone 400px · close revealed by long press | [phone-close-revealed-dark.png](./tabs-1/phone-close-revealed-dark.png) | 62,509 |
| Phone 400px · close sheet · Night Corral | [phone-close-sheet-dark.png](./tabs-1/phone-close-sheet-dark.png) | 59,742 |
| Phone 400px · close sheet · light | [phone-close-sheet-light.png](./tabs-1/phone-close-sheet-light.png) | 66,047 |
| Phone 400px · space drawer | [phone-drawer-dark.png](./tabs-1/phone-drawer-dark.png) | 44,772 |

## Reproduction

```sh
cd web
pnpm exec playwright test -c playwright.tabs.config.ts   # evidence screenshots, mock fixture
pnpm test                                                 # unit suite
```

The Hub suite is unchanged in shape; `web/tests/e2e/spaces-hub-live.spec.ts` gained the dismiss-without-stopping, exited-tab-close and exited-group assertions and runs the same way as in [spaces-1.md](./spaces-1.md). It passed in the standalone mode (disposable Hub + fake Node). The exited-tab and exited-group assertions there are conditional: the fake Node acknowledges a close without settling an `exited` lifecycle, so that mode skips them rather than asserting against a state it cannot produce; they run against a real Node. The Hub suite was run once, at the end, as required.

## Scope

Only the spaces panel, the session list rows inside it and the tab strip changed, plus the shared `StateDot` (whose exited glyph was the reported `×`) and the API/store plumbing for delete. `SessionPage`'s header and composer, `tty/*` and all Rust code are untouched.
