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
