# c-mfix real-device defects: soft keyboard, terminal, effort/model rows

Date: 2026-09-23. Scope: task c-mfix — the three defects the owner hit on an
iPhone (Safari) against the demo build:

1. tapping the session composer and raising the soft keyboard left an empty
   page (「点击输入框弹出键盘后啥都没了」);
2. the terminal segment rendered nothing on the phone;
3. known `model` / `effort` observations showed as 「未识别事件」.

The earlier mobile acceptance only used Playwright/Chromium render screenshots
with a fake Node — it never simulated the soft keyboard's visualViewport
behaviour and never exercised a WebKit engine. Verification for this task runs
on Playwright **WebKit with an iPhone device descriptor**
(`tests/e2e/m-realdevice.hub.spec.ts`, config
`playwright.mrealdevice.config.ts`).

## 1. Soft keyboard emptied the page

### Root cause (reproduced first)

Compact layout sets `--workbench-height` from `window.visualViewport.height`
(`web/src/lib/viewport.ts`). When iOS Safari opens the keyboard it keeps the
**layout** viewport full-size and exposes the visual viewport as a sub-rect:
height shrinks AND `offsetTop` grows (the page scrolls to reveal the focused
input). The shell was a top-anchored `height: var(--workbench-height)` box, so
once the keyboard opened:

- shell height = short visual height (e.g. 410px of 664px);
- shell top stayed at layout y=0;
- the header/transcript/composer therefore painted in the area **above** the
  visible band (y 0..410 while the band was y 254..664), and iOS had scrolled
  that region off-screen — an empty screen.

A second contribution: `update()` forced `window.scrollTo(0, 0)` on every
viewport event, fighting iOS's own focus scroll mid-gesture.

Measured before the fix with the keyboard emulated (height 410 / offsetTop
254): shell `top=0 bottom=410`, header at y=61 — entirely outside the visible
band; the composer at y=315 was the only thing near the band's edge.

### Fix

- The shell now lives inside the whole visualViewport band: a new
  `--workbench-top` carries `offsetTop`, and both shells are
  `position: fixed; top: var(--workbench-top); height: var(--workbench-height)`
  (`Shell.module.css`, `phoneShell.module.css`; set/cleared in
  `lib/viewport.ts`). Fixed positioning also removes document-level scroll,
  which iOS otherwise hijacks while revealing the focused input; the forced
  `scrollTo(0,0)` was removed.
- Consumers of the same var model were updated to band math: the new-session
  bottom sheet anchors at `100dvh - --workbench-top - --workbench-height`
  (`NewSessionPage.module.css`), and tty fullscreen fills the band instead of
  `inset:0; height:100dvh` (`TerminalView.module.css`).

After the fix the same probe reports shell `top=offsetTop`,
`bottom=offsetTop+height`, and status label, transcript and composer all
non-empty and fully inside the band.

## 2. Terminal did not render on the phone

Two distinct risks were checked against WebKit:

- **Renderer.** `tty/renderer.ts` preferred WebGL, then canvas, then DOM. On
  headless WebKit (iPhone 13 descriptor) the effective renderer is recorded as
  **webgl** (a 425×324 canvas paints the grid), so a hard WebGL blacklist would
  have been wrong. The real-device hazard is a WebGL *context loss*
  (backgrounding, memory pressure): the old handler only disposed the addon,
  which silently drops to the DOM renderer. `attachTerminalRenderer` now takes
  an `onEffective` callback and, on context loss, disposes WebGL (which
  re-installs xterm's own DOM renderer as an immediate safety net) and then
  chains the canvas renderer, reporting whichever engine is actually painting
  so the toolbar pill / `data-tty-renderer` cannot lie.
- **Geometry.** The terminal never used `height:100dvh`; its box comes from the
  flex chain shell → main → `.pane` → `.lab` → `.viewport` (min 240px). With
  defect 1's band fix the container measures > 0 with the keyboard up and the
  responsive grid refits (the evidence run fitted 51×12 in the short band).

## 3. effort / model rendered as 未识别事件

`model` and `effort` are known `ObservationKind` values (protocol §5.1,
`ObservationPayload::Model` / `::Effort`) but `assemble.ts` has no dedicated
node type for them, so they fell through to the generic `opaque` node and
`OpaqueRow` stamped them 「未识别事件」.

The only `Transcript.tsx` change is the opaque dispatch site (assembly and
windowing logic are untouched per the c-perfaudit ownership boundary): for
`kind === "model" | "effort"` it renders the new `ObservedChangeRow`
(`web/src/features/session/ObservedChangeRow.tsx`) — one light line,
`▸ 模型 → <id>` / `▸ 档位 → <tier>` (with `· ultracode` when that flag is
set), plus a `原始事件` disclosure keeping the raw payload one click away
(provenance honesty). Genuinely unknown kinds still render through
`OpaqueRow` (D-052); `session-workflow`'s opaque-row coverage still passes.

## Verification

- Spec: `web/tests/e2e/m-realdevice.hub.spec.ts` — Playwright **WebKit**,
  `devices["iPhone 13"]`, fake Node (`hub_e2e`):
  - (a) focus composer, emulate the keyboard (shrink visualViewport + raise
    offsetTop + fire resize/scroll), assert the shell is fixed to exactly the
    band and the status label, transcript (non-empty text) and composer are
    non-zero and fully inside it;
  - (b) open the tty, assert viewport > 0, fitted cols/rows, a painted canvas
    or DOM rows, log the effective renderer, then repeat with the keyboard up;
  - (c) drive an `effort` observation via `instance.configure` and a `model`
    observation via the `/model:` send sentinel; assert the change records,
    the values (xhigh / e2e/fast), no 未识别事件 anywhere in the transcript,
    and the raw-payload disclosure. Sentinels whose observation never reaches
    the journal `test.skip` instead of failing.
- Result: **3 passed on webkit-iphone** (27s; ~52s with evidence screenshots).
  The host devbox is Ubuntu 20.04, which Playwright 1.63 does not ship WebKit
  for; the WebKit runs were executed in `mcr.microsoft.com/playwright:v1.63.0-jammy`
  with `--network=host` against Hub/Vite started on the host
  (`HUB_E2E_EXTERNAL=1`). On host Chromium the same spec is 3/3 green.
- Local gate: `pnpm test` 1605/1605, `typecheck` clean, `oxlint` warnings
  only (all pre-existing). Regression sweeps on chromium: m-shell, m-home,
  m-keybar, effort-sync, session-terminal-lab, session-workflow, new-session,
  mobile-qa, session-structured, approvals, pwa-shell, theme-boot,
  tabs-semantics — all green.

### Evidence screenshots (390 CSS px, Remuda UI only)

- `m-realdevice-1-keyboard-390.png` — structured view with the keyboard band:
  status dot + view switch, transcript content and composer all visible.
- `m-realdevice-2-terminal-390.png` — tty segment, renderer pill webgl.
- `m-realdevice-3-terminal-keyboard-390.png` — tty inside the keyboard band
  (`51×12 · keys · webgl · responsive`, painted rows visible).
- `m-realdevice-4-observations-390.png` — `▸ 模型 → passthrough/auto`,
  `▸ 档位 → xhigh`, `▸ 模型 → e2e/fast`, each with 原始事件 disclosure, and
  no 未识别事件.

Keyboard-up shots are clipped to the visualViewport band (the off-band strip
of the layout viewport is not part of what the phone displays).

## Round 2: keyboard up still hid the conversation and most of the composer

The band pinning fixed "everything disappears", but on the owner's real
session (re-verified in WebKit iPhone 15) the band is only 323 px tall
(393×659 viewport, 336 px keyboard) and fixed chrome ate all of it: install
banner ~60 + session header ~42 + the exited-session resume row ~45 + the
运行详情/加批注 row ~40 + a multi-line live status strip ~90, leaving the
transcript at 0 px and only ~20 px of the composer visible above the
keyboard. The round-1 fixture session had none of that chrome.

### Fix: keyboard-compact chrome collapse

`lib/viewport.ts` now stamps `<html data-keyboard="1">` whenever a compact
viewport loses ≥120 px of visual height (both acceptance keyboards clear it;
small browser-chrome moves and rubber-banding do not). While stamped,
`src/styles/keyboardCompact.css` (imported once in `main.tsx`, all selectors
scoped to the attribute, stable testids only):

- hides the install banner (`install-bar`) AND the waiting-worker update bar
  (`update-bar` — InstallBar renders that instead when a new service worker
  is waiting), the exited-session resume row, the 运行详情 disclosure, the
  annotation dock, task track and session notifications;
- **round 3:** also hides the transcript toolbar (全部折叠 / 搜索正文), an open
  transcript search and the gap-backfill journal banner — these sat ABOVE the
  message scroller and were why the real SCROLLPORT kept only ~29 % of the
  band while the transcript ROOT passed the 40 % assertion;
- compresses the live status strip to one clipped line — phase dot + tool
  name + the hook health note; timer/token/tier/spinner phrase/Esc fold away
  until the keyboard closes;
- tightens the dock wrapper padding/gaps (the composer's own 44 px touch rows
  are untouched).

The composer and the transcript are never hidden; with the keyboard down the
attribute is removed and nothing in the stylesheet applies.

### Fixture and assertions

The fake Node gained the `mfix-chrome-combo` sentinel
(`crates/remuda-hub/examples/hub_e2e.rs`, reached from both
`instance.create` initial input and `instance.send`): a ready→exited
resumable instance (`signalTier: hook`, native session id) with a user
message, a still-**running** AskUserQuestion tool_call + a hook
`tool-started` phase whose last hook record is backdated 30 s (past the
3× cadence stall budget → the `hook · 通道静默，计时可能不准` note), an
858-output-token usage snapshot, and a fresh screen `live.status` spinner
(keeps the turn decision `working`, so Esc 打断 and the running verbs
render). The spec self-skips when the sentinel is absent (the gate runs
without a fake Node; headless Chromium also does not surface the install
banner, which the spec treats as a precondition rather than a requirement).

The keyboard is emulated exactly as iOS and the coordinator's verifier do —
the layout viewport is NOT resized:

```js
Object.defineProperty(vv, "height", { configurable: true, get: () => innerHeight - KB });
Object.defineProperty(vv, "offsetTop", { configurable: true, get: () => KB });
window.scrollTo(0, KB); vv.dispatchEvent(new Event("resize"));
vv.dispatchEvent(new Event("scroll")); window.dispatchEvent(new Event("resize"));
```

Cases: iPhone 15 393×659 / KB 336 AND iPhone SE 375×667 / KB 260. Each device
has TWO cases:

- **geometry** — the exited full-chrome session (force-tap only; its composer
  is disabled by design). Assertions against the exact band
  `[offsetTop, offsetTop + height]`:
  1. composer form, textarea, and the 发送 control are entirely inside the band;
  2. the message SCROLLPORT `[data-testid="transcript-scroller"]` — not the
     transcript root — keeps ≥ 40 % of the band: measured **147/323 ≈ 45.5 %**
     on iPhone 15 (the toolbar collapse returned ~50 px to the scrollport);
     the scroller is bottom-pinned (`scrollTop + clientHeight >=
     scrollHeight - 4`) and the painted elements just above its bottom edge
     belong to the tail `transcript-row` wrappers (the AskUserQuestion tool
     card + usage row of the fixture). Short conversations that fit the band
     are exempt from the bottom-edge paint check — blank space below the last
     row is correct there.
- **focus** — an enabled, idle composer: a real (non-forced) click asserts
  `toBeFocused()` BEFORE the keyboard geometry is applied; the composer stays
  fully in the band, focus survives the geometry change, and the enabled
  composer returns when the keyboard closes. The scroller measured
  **173/323 ≈ 53.5 %** in this case.

Honesty guards: trigger presence is read from the fake Node's JOURNAL
(`obj_mfix_ask` tool call + the exited entity) and missing trigger SKIPS; once
the trigger exists, missing chrome is a hard failure. The install banner is
required in the WebKit acceptance cases (iPhone surfaces the offer; if an
engine never does, the case skips explicitly). `fakeHostId` returns only the
`e2e-fake-node` host — a registered real host skips the hub-backed cases
rather than erroring. Keyboard close restores every collapsed piece; with the
keyboard down the layout is unchanged.

### Round-2 evidence (WebKit iPhone 15, 390 CSS px)

- `m-realdevice-5-chrome-keyboard-down-390.png` — keyboard DOWN: unchanged,
  all chrome present.
- `m-realdevice-7-chrome-keyboard-up-nocollapse-390.png` — mechanical
  BEFORE: same geometry with the collapse attribute removed; banner, resume
  row, run details and the wrapped multi-line strip fill the band, the
  composer is clipped at the band edge (owner's report).
- `m-realdevice-6-chrome-keyboard-up-390.png` — AFTER: one-line status strip,
  the full transcript block with the AskUserQuestion row and 858-token usage,
  and the entire composer (textarea + effort chip + 发送) inside the band.

Result (round 3): **7/7 passed on webkit-iphone** (a–c plus geometry/focus
on both devices); the geometry/focus cases also pass on host Chromium. Full
`m-*.hub.spec.ts` sweep (m-chrome, m-home, m-inbox, m-jumpto, m-keybar,
m-push, m-shell, m-realdevice) green on Chromium against a fresh fake Node.
All e2e runs were wrapped in
the gate e2e lock (`flock` on the shared gate lock) with per-worker ports.


## Round 4: pin-on-shrink, and search focus under the keyboard

Two more acceptance findings, both verified as real:

1. **Pin held only on node/size changes, not on scroller resize.** When the
   keyboard opened, the scroller's `clientHeight` shrank but the pin effect
   (`el.scrollTop = el.scrollHeight`, keyed on `nodes.length`/`sizes`) never
   reran and the scroller's ResizeObserver only stored the new height — so an
   overflowing, pinned transcript stayed scrolled above its tail and the
   newest message vanished while typing. Fix (`Transcript.tsx`): the scroller
   ResizeObserver samples `pinRef` BEFORE the shrink and re-applies the
   bottom pin in the same frame only while the user was already pinned; a
   reader who scrolled up keeps their position. Covered by a jsdom unit test
   (pinned → re-pinned from 720 to 323; scrolled-up → offset preserved at 100
   when the viewport shrinks to 240). The `mfix-chrome-combo` fake-Node
   fixture now journals twelve tall history turns so the transcript
   OVERFLOWS the band, and the e2e assertion requires overflow and hits the
   exact LAST `transcript-row` (the running AskUserQuestion card) just above
   the scroller's bottom edge — no "last two", no skip when content fits the
   overflow case.
2. **Opening transcript search hid the search box.** The collapsed toolbar
   hosts the search input, so focusing search raised the keyboard and hid the
   box. `lib/viewport.ts` now tracks the focused surface on focusin/focusout
   (`data-keyboard-focus="search"|"composer"` on `<html>`);
   `keyboardCompact.css` keeps the toolbar/searchbar mounted while search
   owns focus and collapses them only when the composer is focused. New e2e
   case per device: open search, focus the input, apply the keyboard
   geometry, assert `data-keyboard-focus="search"`, the toolbar + searchbar +
   input stay visible inside the band and keep focus.
