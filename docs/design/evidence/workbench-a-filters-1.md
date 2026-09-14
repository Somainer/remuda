# Workbench A — filters, scope, zero results and the shared overlay contract

Exploration §5 **P0-1** plus the overlay contract frozen in the execution plan §2.
Baseline `origin/main` = `3da158d`. Branch `wt/ux-a/filters-and-overlay-contract`.

All screenshots come from a **synthetic in-browser fixture** (mock mode,
`VITE_MOCK=1`, Playwright against `127.0.0.1:4177`): three invented hosts
(`demo-node-1/2/3`), invented directories and invented session titles. No live
Hub, no real model call, no personal path. The capture spec
(`web/tests/e2e/ux-filters-evidence.spec.ts`) asserts no real path or host name
appears in any frame before it writes the PNG.

## What landed

### 1. Shared overlay contract (plan §2 — frozen, B/D/F consume it)

`components/useFocusTrap.ts` + `components/Sheet.tsx` implement one signature:

```ts
{ open, onClose, labelledBy, initialFocusRef, returnFocusRef, variant: "popover" | "sheet" }
```

It guarantees the accessible name, `aria-modal`, initial focus, Tab cycling in
both directions, Escape, and focus return to the trigger. `components/Modal.tsx`
adopts the same hook, so the four existing dialog call sites (`AddHostForm` ×1,
`ProvidersPage` ×3) gain Escape and trapping without any caller change.

Two rules keep it clear of an attached terminal (plan §4 risk 1):

- the keydown listener is on the **overlay container**, never `window` — a unit
  test asserts no `window` keydown listener is registered at all;
- it returns early when `event.target.closest('.xterm')` matches, because a
  terminal owns Escape and Tab as bytes for the native process.

`lib/keyboardScope.ts` extracts the guard selector currently inlined at
`Shell.tsx:54` as `isTypingTarget` / `isTerminalTarget`. **Shell.tsx is
untouched** (batch C owns it); the helper carries a TODO naming the call site
for C to switch over.

### 2. Filtering, scope and conditions

`features/session/sessionFilters.ts` holds the model as pure functions — URL
round-trip, scope derivation, Space-switch pruning, empty-state classification —
so the rules are testable without rendering.

- The 筛选 button was **inert** before this batch (`SessionList.tsx:167–169`);
  it now opens the shared overlay with `aria-expanded` tracking it: popover on
  desktop, sheet on a phone.
- The default toolbar shows search, the current scope, the match count
  (`matched / total`) and each active condition as a removable chip.
- A Space-fixed scope **withholds host and directory conditions**, because the
  list is already pinned to one `hostId + workspaceId` and those conditions
  could only ever build a self-excluding query. A note in the panel says so and
  points at the global entry. Global scope offers both, each directory chip
  qualified by host when the name is shared.

### 3. URL as the source of truth (plan §4 risk 2)

Derivation runs one way: params → conditions. Nothing writes back into the Space
store from an effect.

| Action | History |
|---|---|
| Typing in the search box | `replace` — a keystroke is not a decision |
| Explicit condition change (chip, status, kind, host, directory, scope) | `push` — Back undoes exactly that choice |
| Space switch dropping inapplicable conditions | one `replace`, never a stack of them |

Switching Space drops host/directory conditions the new fixed scope cannot
honour, **keeps** text and status (they mean the same thing anywhere), and says
what it dropped rather than silently narrowing.

### 4. Four distinguished empty states (P0-1 rule 4)

| State | Cause | Offered next step |
|---|---|---|
| `no-hosts` | no host enrolled | 添加主机 → `/hosts` |
| `no-workspaces` | host online, no registered directory | 注册工作目录 → `/hosts` |
| `no-sessions` | the Space is genuinely empty | 新建会话 |
| `no-matches` | **the user's own conditions** | 清除筛选, plus 搜索所有空间 while still inside a Space |

Only `no-matches` offers to clear, and it states the count behind the filter
(“4 个会话在所有空间内，但都不匹配”) plus “不会创建或关闭任何会话”, so a
filtered-to-zero list never reads as data loss. The widen action appears only
when widening would actually help — inside a Space; once the search is already
global it is omitted rather than shown as a no-op. Clearing is a pure view
change — a unit test asserts `hubStore.close` and `hubStore.send` are never
called.

## Screenshots

| Viewport | What it shows | File |
|---|---|---|
| 1440 | Default toolbar: search, Space-fixed scope naming its host, match count, 搜索所有空间 entry | [toolbar-1440](./workbench-a-filters-1-toolbar-1440.png) |
| 1440 | Filter popover in a fixed Space — status and type only, with the note explaining why host/directory are withheld | [popover-1440](./workbench-a-filters-1-popover-1440.png) |
| 1440 | Filter popover in global scope — host and directory now offered, `payments` qualified by host on both entries | [global-popover-1440](./workbench-a-filters-1-global-popover-1440.png) |
| 1440 | Zero results in global scope: `0 / 4`, the count behind the filter, and 清除筛选 (the widen action is correctly absent — the search is already global) | [zero-1440](./workbench-a-filters-1-zero-1440.png) |
| 768 | Toolbar at tablet width | [toolbar-768](./workbench-a-filters-1-toolbar-768.png) |
| 768 | Popover at tablet width | [panel-768](./workbench-a-filters-1-panel-768.png) |
| 390 | Toolbar on a phone | [toolbar-390](./workbench-a-filters-1-toolbar-390.png) |
| 390 | Bottom sheet, 44px touch targets | [sheet-390](./workbench-a-filters-1-sheet-390.png) |

The same-name case is visible in the sidebar of every shot: two Spaces named
`payments`, told apart only by `demo-node-1` and `demo-node-2`.

## Three defects the tests surfaced

Writing the e2e found three real races, all fixed in `d7f43dd`:

1. **Stale params on write.** Condition changes wrote back the `URLSearchParams`
   the render closed over, so two changes in quick succession clobbered each
   other. Both the toolbar commits and the pruning effect now update from the
   *current* params via the functional form of `setParams`.
2. **Pruning undid the scope switch.** The pruning effect rewrote the URL from a
   stale copy immediately after `scope=all` was set, so the global entry never
   worked at all — the URL said `scope=all` while the list stayed Space-fixed.
3. **The notice cleared before it could be read.** The “dropped conditions”
   message was keyed on a value that the pruning rewrite itself empties. It is
   now keyed to the Space that caused it, so it survives the rewrite and retires
   on the next Space.

A fourth, smaller one: the 44px phone targets used `height`, and under
`box-sizing: border-box` a fractional device-pixel border rounded the painted box
to 43.99997px. They now use `min-height`.

## Tests

| Suite | Count | Covers |
|---|---|---|
| `sessionFilters.test.ts` | 26 | URL round-trip, scope derivation, Space-switch pruning, matching incl. same-name directories, chips, four empty states |
| `useFocusTrap.test.tsx` | 15 | name/`aria-modal`, initial + explicit focus, Tab and Shift+Tab cycling, Escape, focus return, both variants, **xterm passthrough**, no `window` listener, scrim click |
| `SessionList.test.tsx` | 14 | four empty states, clearing touches no session, scope display, withheld conditions, `aria-expanded`, chips, popover vs sheet |
| `ux-filters.spec.ts` (e2e) | 10 | click + keyboard open/close, Tab containment, fixed vs global scope, same-name directories across hosts, zero results → clear, Space-switch pruning, deep link/refresh/Back, search not stacking history, 390px sheet at 44px |
| `spaces.spec.ts` (e2e) | +1 | deep link and reload agree on conditions and results; scope names its host |

Web unit suite: **473 passed / 72 files** (55 new; 418 pre-existing, no
regressions). Hub e2e (`pnpm --dir web run test:e2e:hub`, fake node +
`fake-harness`, no real model): **16 passed, 1 skipped, 0 failed**.

### Pre-existing failures, not from this batch

The mock-mode Playwright suite has 9 failures on this devbox. They reproduce
**identically on an unmodified `3da158d` worktree** (`agent-board` ×3,
`composer-effort` ×1, `new-session` ×1, `session-structured` ×3, `spaces` ×1),
so they are environmental, not caused by this diff. The comparison was run in a
throwaway worktree at the baseline commit and removed afterwards.

## Not done here

- `Shell.tsx` still has its inline keyboard guard; batch C owns that file and
  switches it to `lib/keyboardScope.ts`.
- The scope entry searches every top-level instance the hub has already loaded.
  It does not claim to search all history — that is P1-1's question, and this
  batch adds no remote index.
- `ui.module.css` and `tokens.css` are untouched (batch F owns them); the new
  overlay styles live in `components/overlay.module.css`.
