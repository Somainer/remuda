# Workbench D — QuickFind: cross-Space finder on the loaded cache

Exploration §5 **P1-1（跨空间快速查找）**, execution plan §1 row D and §4 risks
1–2. Baseline `origin/main` = `eefcbba` (batches A and C1 merged).
Branch `wt/ux-d/quickfind`.

All browser verification ran against the in-process **fake node + fake
terminal harness** in `crates/remuda-hub/examples/hub_e2e.rs` through
`playwright.hub.config.ts`. No real model, no real PTY, no remote index.

## What landed

### 1. Pure ranking — `features/search/quickFind.ts`

One pure function, `rankQuickFind`, over the instance/Space/host lists the Hub
has already loaded. It never calls the network and never reads journal bodies;
the "search all history" question is explicitly out of scope (exploration §8
row 1).

Match priority is fixed by the spec and asserted in tests:

1. session **title**
2. **Space** name
3. **host** name
4. **instance id** — a secondary hit, always behind any title/Space/host hit

Within a field: exact → prefix → word-start (`pay` in `payments api`) →
substring. Equal scores break on most recent `updatedAt`, then id.

Every result row carries the title, the Space name and the host name. When two
instances share a title (`ambiguousTitle`), both rows keep their Space + host
qualifiers — the same-name case the acceptance calls out — plus status and last
activity time. With an empty query the panel lists the whole cache by recency,
so opening it shows what is available before the user types.

When the Hub connection is not `live` (`offline`/`reconnecting`), the same
cache is searched and the result is stamped `cacheOnly`; the panel shows
"Hub 离线或重连中：只搜索本机已缓存的会话，可能不是完整列表。" instead of
presenting stale rows as complete.

### 2. The overlay — `features/search/QuickFind.tsx` + `quickfind.module.css`

- **Shortcut**: ⌘K / **Ctrl+K**, the hint agreed in the exploration doc and
  UI spec §2.3. The global listener goes through batch A's
  `isTypingTarget`: it never opens while focus is in an
  `input/textarea/select/[contenteditable]` or inside `.xterm` (risk 1).
  Opening it from a session page therefore cannot steal the attached
  terminal's keystrokes — including ⌘K, which a TUI may bind itself.
- **Overlay contract**: rendered through batch A's `Sheet`
  (`variant="popover"` on desktop, `"sheet"` under 768 px), so name,
  `aria-modal`, initial focus, Tab containment, Escape and trigger-focus
  return all come from `useFocusTrap`. No second scrim was written.
- **Navigation**: ArrowUp/ArrowDown move the active row (wrapping at both
  ends), the input tracks it with `aria-activedescendant`, Enter calls
  `spaceStore.selectTab(...)` + React Router `navigate('/s/<id>')` — a client
  route change, never `location.assign`/a reload. Enter is the **only** thing
  that changes navigation state.
- **No side effects on cancel**: opening, typing and Escape never write the
  URL and never change the selected Space; Escape returns focus to the
  trigger (the Sheet's return-focus contract). There is no URL `q` history
  stack — the finder is an action, not a list filter (risk 2 stays owned by
  the session list).
- The desktop scrim portals to `document.body` (the sidebar is a scrolling
  container); inside the phone Spaces drawer it renders **inline**, because
  that drawer stacks at z-index 45 while the shared scrim is 40 — a portaled
  sheet would have painted *under* the drawer and its clicks would have
  landed on drawer content (this was caught by the 390 px e2e).
- Styles live only in `quickfind.module.css`; `ui.module.css`, `tokens.css`
  and batch A's files are untouched. 44 px touch targets use `min-height`
  (the fractional-border lesson from batch A).

### 3. SpacesPanel entry + Sheet — `features/spaces/SpacesPanel.tsx`

- A "搜索所有空间… ⌘/Ctrl+K" trigger sits under the panel header (and a `⌕`
  glyph in the collapsed rail; the same trigger is inside the phone drawer).
- The delete-session confirmation no longer uses the bespoke
  `ActionSheet` backdrop; it renders through `Sheet` (popover/sheet by
  viewport) with the same testids (`delete-session-sheet`,
  `delete-session-confirm`, `delete-session-stop`,
  `delete-session-sheet-cancel`) and the same `busy`-locked cancellation.
- The delayed Node-purge branch is byte-for-byte the C1 behaviour:
  `hubStore.deleteInstance` → `hubStore.toast("已删除会话；该主机数据待其上线后清理")`
  when `nodePurge !== "purged"`. The bridge in `Shell.tsx` (owned by C)
  surfaces it; this batch changed nothing there.
- `ActionSheet.tsx` itself is untouched — SpaceTabs (not in this batch's
  ownership) still uses it.

### 4. Fake terminal harness — `crates/remuda-hub/examples/hub_e2e.rs`

The e2e needs an attached terminal whose process demonstrably receives a raw
ESC byte. The fake node previously modeled only print sessions, so a small
PTY double was added (test fixture only, no production code):

- `instance.create` with `kind: "terminal"` registers a `TtyFake` instead of
  synthesizing prompt/approval journal turns;
- `tty.attach` returns a stable `tty_<uuidv7>` stream id and a
  base64 snapshot (`fake-harness terminal\r\n$ `);
- `tty.write` records every raw byte, and an inbound **ESC (0x1b)** is
  acknowledged on screen with `QUICKFIND_ESC_RECEIVED`, so a browser test can
  assert the byte reached the process rather than being swallowed by a panel.

The Hub's existing tty frame relay (`ws.rs`) carries the bytes in both
directions; the fixture adds no protocol surface.

## Tests

| Suite | Count | Covers |
|---|---|---|
| `features/search/quickFind.test.ts` | 8 | title > Space > host priority; exact/prefix/word/substring order; recency tie-break; id is a secondary hit and loses to a title; same-title rows flagged and kept distinct by Space+host; offline/reconnecting → `cacheOnly`; empty-query recency list; `total` counted before the limit |
| `features/search/QuickFind.test.tsx` | 7 | trigger opens with input focused; ⌘K opens, suppressed in input/xterm; type + Enter navigates to the right instance without URL filters; arrow wrap + `aria-activedescendant`; Escape returns focus and leaves `/sessions` untouched; zero-results clear path; cache-only label |
| `features/spaces/SpacesPanel.test.tsx` | 6 (existing, updated harness) | resumed/delete/nodePurge flows through the new Sheet-backed confirmation |
| `tests/e2e/ux-quickfind.spec.ts` (hub, fake node) | 4 | ⌘/Ctrl+K → arrows → Enter lands on the exact selected instance; Escape restores trigger focus and keeps Space + `/sessions`; attached terminal: ⌘K does not open over xterm and the ESC byte reaches the harness (`QUICKFIND_ESC_RECEIVED`) with no panel; 390 px bottom sheet inside the viewport, ≥44 px rows, tap opens the session |

Web unit suite: **614 passed / 80 files** (21 new for this batch; the
remainder pre-existing, no regressions). `pnpm lint` exits 0 (the two
fast-refresh notices on `QuickFind.tsx`'s non-component exports match the
repo's existing pattern, e.g. `HostProviderBinding.tsx`), `tsc -b` clean,
`gen:api` leaves `api.generated.ts` unchanged.

Hub e2e (`pnpm --dir web run test:e2e:hub`, fake node + harness, no real
model, under `flock /tmp/remuda-agents/e2e.lock` on devbox-sg):
**26 passed, 1 failed, 2 not run**. All four `ux-quickfind` tests pass (and
all of batch C1's `ux-status`, `hub-live`, `spaces-hub-live`, `pairing`,
`passkey-hub-live`). The single failure is
`providers-discovery` ("provider-model-row Expected 5 Received 0", a
fake-upstream catalog timing failure); it **fails identically with this
batch's `hub_e2e.rs` change stashed** (rebuilt baseline and re-ran the same
spec), so it is the documented shared-devbox flake, not a regression — the
two sibling tests in that file did not run because Playwright aborts the file
after the first serial failure. The devbox flake is tracked in the runner
notes for this environment.

## Three defects the tests surfaced

1. **The phone sheet rendered under the drawer.** The Spaces drawer is
   z-index 45 and the shared Sheet scrim is 40; a portal-to-`body` scrim sat
   *beneath* the open drawer, so Playwright clicks were intercepted by the
   panel's group list (`_groups_`). Fix: portal on desktop, inline inside the
   drawer.
2. **The fake terminal opened on the structured tab.** A `shell-pty` row
   without a structured signal tier defaults to the conversation projection,
   so `data-tty-lab` never appeared. The e2e navigates to `/s/<id>/tty`
   explicitly — it is testing the raw projection, not the default.
3. **`getBoundingClientRect` mid-animation.** The sheet's 140 ms rise uses a
   translateY entry transform; measuring geometry immediately read 847.9 px
   bottom in an 844 px viewport. The e2e polls until the transform settles.

## Not done here

- No remote search index and no body/history search — phase 1 is metadata over
  the loaded cache by design (exploration §8).
- `Shell.tsx`, `store.ts`, `SessionList.tsx`, `ActionSheet.tsx`,
  `SpaceTabs.tsx`, `ui.module.css`, `tokens.css` untouched, per the ownership
  matrix. The global shortcut therefore lives in `QuickFind` (always mounted
  via the panel) rather than in Shell.
- No new Space preference: the open/closed state is component-scoped and is
  not persisted.
