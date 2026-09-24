# UO-10 终端外框（terminal chrome）

Date: 2026-09-24. Scope: UO-10 (S) — the terminal frame around xterm in the
UI overhaul. Design: `docs/design/visual-system.md` §3.4/§4, ADR D-053 item 4,
ui-spec §2.3/§4.7.

## What changed

- **Always-dark instrument.** `tokens.css` now carries the full
  `--term-*` chrome palette (`--term-bg` `#1a1917`, `--term-fg`
  `#e4dfd6`, cursor/selection, raised surface, hairline, muted text) in the
  mode-independent token block. `TerminalView.module.css` references only
  `--term-*` tokens — no ink/paper/dust role tokens and no colour literals —
  so the pane, toolbar, dock, key bar and badges resolve identically in
  light and dark. The xterm canvas keeps `TERMINAL_THEME`
  (`TerminalView.tsx` imports that name instead of the transition alias;
  ANSI output and the standard 256-colour cube are never recoloured).
- **No pane/canvas seam.** Pane `.lab`, `.viewport` and the xterm host all
  resolve `rgb(26, 25, 23)`; the host itself is transparent over the same
  pane background, so there is no boundary between frame and canvas.
- **Keyboard freeze (zero PTY resize).** While
  `<html data-keyboard="1">` is stamped, `applyFit()` returns immediately —
  no xterm fit and no `sessionRef.resize`. The existing
  `data-keyboard` CSS freezes the grid: the viewport becomes a
  bottom-aligning clipping flex box and the pre-keyboard xterm host sits at
  the bottom of the band, upper rows clipped. A single `sendPtyResize`
  dedupes by the last grid (`{cols,rows}`), so keyboard close that restores
  the same grid also sends nothing. `window.__ttyLab.resizeCount()` /
  `resetResizeCount()` expose the counter for tests. Keyboard close
  re-enables fit normally.
- **Type and spacing tokens.** Local input is `--text-input` (14px fine /
  **16px coarse pointer**), field height 36px fine / 44px coarse, send
  control matched. All chrome text uses `--text-meta` (12px) or larger; the
  old 10.5px / 11px literal sizes are gone. Toolbar is a 32px desktop strip;
  AuxKeys keys 28px with 12px mono; progress bar 2px; mode pill is a
  radius-pill capsule. Stale badge shapes follow the two evidence kinds
  (dashed neutral 「画面可能过期」 for `node-link-unavailable`, regular
  frame text 「会话已结束」 for `instance-gone`).

## Verification

The dev-only `pnpm test:e2e:terminal` gate needs `remuda dev` panes on a
herdr server (which carries every worker session and the owner's own panes);
per the task coordinator that command is NOT run here. Terminal coverage
uses the hub-e2e fake Node plus `m-realdevice`, under the gate e2e lock
with per-worker ports, servers in their own process groups with trap
cleanup.

- `tests/e2e/uo10-evidence.hub.spec.ts` (new):
  - same dark instrument in explicit **dark** and **light** appearance on a
    1440 desktop: `--term-bg` resolves `#1a1917`, pane and viewport both
    `rgb(26, 25, 23)` (no seam), chrome ink stays light;
  - 393px keyboard (323px visible band): local input and phone key bar fully
    inside the band, xterm viewport/canvas height > 0; zero
    `__ttyLab.resizeCount()` calls and identical rows/cols across the full
    open/close cycle; the minimum font size on the terminal frame is ≥12px.
- `tests/e2e/m-realdevice.hub.spec.ts` (b) drives the terminal at a 393px
  phone width on every engine and asserts zero PTY resize calls with the
  same fitted rows/cols through keyboard open and close.

Screenshots (390/1440, Remuda only):

- `uo10-terminal-dark-1440.png` / `uo10-terminal-light-1440.png` — the dark
  terminal is pixel-identical in both appearances;
- `uo10-terminal-keyboard-390.png` — keyboard up, input + key bar in band,
  xterm bottom-aligned.

## Results

- WebKit iPhone 13 (mrealdevice config): `m-realdevice` + `uo10-evidence`
  9 passed.
- Chromium hub config: `uo10-evidence`, `m-realdevice`,
  `session-terminal-lab`, `terminal-live`, `ux-ttymode` — passed with the
  pre-existing skips.
- Unit: StaleScreenBadge wording tests updated; full `pnpm test` green.
- Perf scenario B (5000-line terminal flood + scrollback paging, chromium,
  same host, two runs each):

  | run | this branch wallMs | origin/main wallMs |
  |---|---|---|
  | 1 | 13 515 | 12 946 |
  | 2 | 13 009 | 13 171 |
  | median | 13 262 | 13 059 |

  Both: 0 long tasks, renderer `webgl`, 0 context losses. Median delta
  ~+1.5 %, inside run-to-run noise; **no regression**.
