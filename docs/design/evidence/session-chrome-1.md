# Session chrome: view switch, fixed harness, files route

Mock UI (`VITE_MOCK=1`, Playwright `channel: chrome` against `127.0.0.1:4177`). Night Corral tokens. No live Hub ports, no personal paths.

Three reports, one pass over the session header and composer control bar:

- **A.** 终端 / 结构 是二选一 — one segmented switch, not two buttons.
- **B.** 已经在会话中的话，harness 切换没有意义 — the composer's harness dropdown is gone inside a session.
- **C.** 点击「文件」后就回不去了 — the files route was a dead end.

## What landed

### A — one segmented switch

`ViewSwitch` (`web/src/features/session/ViewSwitch.tsx`) replaces the two `终端` / `结构` header links with a single two-state control.

- `role="radiogroup"` wrapping two `role="radio"` segments; exactly one carries `aria-checked="true"`.
- Roving tabindex: the selected segment is `tabIndex=0`, the other `-1`. `←` `↑` `→` `↓` move between the two states, `Home` / `End` jump to an end. Clicking the already-selected segment is a no-op.
- The choice is remembered **per instance** in `localStorage` under `runtime.session-view.<instanceId>` (`web/src/lib/viewPref.ts`), so the bare `/s/:id` route reopens on the view last used for that session. Unknown stored values are ignored.
- **Hidden entirely** when the session has no terminal: `canShowTerminal()` is false for `claude-print` and other structured-only drivers, and then no switch renders at all (not a disabled one).

### B — harness is a label inside a session

The harness cannot change for a live instance, so the composer no longer offers it as a menu. `harness-chip` is now a `<span data-readonly="1">` carrying the glyph (`C` / `X` / `G` / `A` / `$`) and the harness name — `Claude Code` on desktop, `Claude` at phone width. The `harness-menu` popover and `harness-option-*` rows are gone from the composer along with the now-dead `onHarness` / `hostLabel` / `hostCli` props and the local harness-override state.

Harness selection stays on **New Session** (`new-session-kind-*`), which is the only place it is a real choice.

### C — the files route has a way back

The `文件 / diff` placeholder keeps the full session header (title, status, meta row, composer dock) and gains four ways out:

1. `← 返回会话` button in the pane (`files-back`) — `navigate(-1)` when there is history, otherwise a replace to the session.
2. The header `文件` control is a **toggle**: `aria-pressed` reflects the active state and a second click returns to the session. `原始事件` got the same treatment.
3. Browser back.
4. `Esc` — armed in a `useLayoutEffect` (live the moment the route commits) and in the **capture** phase, so it still fires when focus sits on the header button that swallows the bubble. An open composer popover eats the first `Esc`.

Deep-linking straight to `/s/:id/files` keeps the session context: the header, meta row, and back button are all present, and `← 返回会话` lands on the session's remembered view rather than a blank history entry.

`原始事件` also gained `white-space: nowrap` on `.headBtn` — it was wrapping to two lines at 400px.

## Screenshots

Desktop 1440×900 and phone 400×844, `prefers-reduced-motion: reduce`.

| | 1440 | 400 |
|---|---|---|
| Switch on 终端 | [switch-tty-1440](./session-chrome-1-switch-tty-1440.png) | [switch-tty-400](./session-chrome-1-switch-tty-400.png) |
| Switch on 结构 | [switch-structured-1440](./session-chrome-1-switch-structured-1440.png) | [switch-structured-400](./session-chrome-1-switch-structured-400.png) |
| Structured-only: no switch, static harness label | [harness-label-1440](./session-chrome-1-harness-label-1440.png) | [harness-label-400](./session-chrome-1-harness-label-400.png) |
| Files route with header + back | [files-1440](./session-chrome-1-files-1440.png) | [files-400](./session-chrome-1-files-400.png) |
| New Session keeps the harness picker | [new-session-1440](./session-chrome-1-new-session-1440.png) | — |

## data-testid contract

| id | where |
|---|---|
| `view-switch` | segmented control; `role=radiogroup`, `data-view=tty\|structured`; absent when there is no terminal |
| `view-switch-tty` / `view-switch-structured` | the two segments; `role=radio`, `aria-checked` |
| `harness-chip` | now always `data-readonly="1"` inside a session; no `aria-expanded` |
| `files-toggle` / `events-toggle` | header toggles; `aria-pressed` |
| `files-pane` | the files / diff route body |
| `files-back` | `← 返回会话` |

`harness-menu` and `harness-option-*` no longer exist in the composer.

## Tests

- `pnpm --dir web test` — 222 passed, 53 files. New: `ViewSwitch.test.tsx` (segmented state, click semantics, arrow-key roving), `viewPref.test.ts` (per-instance recall, junk rejection), two `Composer.test.tsx` cases for the static harness label.
- `pnpm --dir web exec playwright test` — mock suite, chromium + mobile-webkit. `session-structured.spec.ts` gained a `session chrome` describe covering the single segmented control, keyboard movement, per-instance recall, the absent switch on `claude-print`, no harness menu in-session, all four ways back from 文件, deep-link context, and a 400px back affordance. `composer-effort.spec.ts`, `new-session.spec.ts`, `session-terminal-lab.spec.ts`, and `terminal-live.spec.ts` were retargeted from the removed `终端` / `结构` links to `view-switch-*`.

Rebased across the effort-slider work (`22e9c3c`, then the codex-style slider in `b7b3b5a`) and the spaces/tabs shell (`bb935f0`): the slider, its `effort-slider` / `effort-slider-panel` / `effort-open-list` testids, `effortDisabled`, and the spaces `useSpaceWorkbench` wiring in `SessionPage` are kept as-is. The two `composer-effort.spec.ts` cases that reached the codex and grok effort tables *through the harness menu* now reach them through the codex and grok mock sessions instead, so that coverage survives the menu's removal.
- `pnpm --dir web lint`, `pnpm --dir web exec tsc -b`, `./scripts/ci/secret-scan.sh`.

The codex effort table is asserted as a unit test rather than e2e: `codex-worker` lives in a non-default space, so `/sessions` no longer lists it. The grok table keeps its e2e case because `Grok 会话` is in the default space.

Pre-existing failures, unrelated and untouched here: `agent-board.spec.ts` (3 cases) and `spaces.spec.ts` reach `codex-worker` / `grok-canary` through `/sessions` and hit the same space-scoping change; they fail on a clean `origin/main` checkout too. A full `--workers=1` run on a loaded machine also times out mobile-webkit at browser launch ("while setting up page"); those are load artifacts, not assertions — the same specs pass on chromium.
