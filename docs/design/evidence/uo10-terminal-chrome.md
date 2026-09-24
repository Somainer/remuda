# UO-10 终端外框（terminal chrome）

Date: 2026-09-24. Scope: UO-10 (S) — the terminal frame around xterm in the
UI overhaul. Design: `docs/design/visual-system.md` §3.4/§4, ADR D-053 item 4,
ui-spec §2.3/§4.7.

## What changed

- **Always-dark instrument.** The only new token on `:root` is the UO-1
  pair `--term-bg` `#1a1917` / `--term-fg` `#e4dfd6` (tokens.css is UO-1's
  file; round-2 review asked that no further tokens be added there). The
  chrome shades (`--tty-raised`, `--tty-border`, `--tty-muted`) are derived
  locally with `color-mix` and scoped to `.lab`, so they never leak and
  never swap with the page appearance. `TerminalView.module.css` references
  only those and type/radius tokens — no ink/paper/dust role tokens and no
  component colour literals. The pane, toolbar, dock, key bar, badges,
  search field and history panel resolve identically in light and dark.
  xterm keeps `TERMINAL_THEME`; ANSI output and the standard 256-colour
  cube are never recoloured.
- **No pane/canvas seam.** Pane `.lab`, `.viewport` and the xterm host all
  resolve `rgb(26, 25, 23)`; the host is transparent over the pane.
- **Keyboard freeze with a three-state fit gate.** `applyFit` classifies
  each visual-viewport event:
  - `freeze` — keyboard open, height-only change: clear any pending 40ms
    resize timer, skip the fit, and if an earlier sub-threshold frame
    already changed the xterm grid, roll it back to the frozen snapshot
    (no PTY resize);
  - `defer` — compact viewport in the first <120px of the keyboard opening:
    commit no grid (the gesture may still reach the freeze threshold), so
    a real open ANIMATION cannot schedule a resize that fires while frozen;
  - `fit` — normal layout, including a WIDTH change with the keyboard open
    (rotation): keeps the frozen ROWS, refits COLS to the new width, and
    sends exactly one resize via an `allowResizeWhileKeyboard` flag that
    lets the deferred timer past the still-stamped keyboard attribute.
  A grid-deduping `sendPtyResize` means an open/close that settles to the
  same grid sends nothing. `__ttyLab.resizeCount()` /
  `resetResizeCount()` expose the counter.
- **Cursor-anchored crop.** The frozen host is positioned with one
  `translateY` computed from the active cursor row
  (`clamp(cursorY - visibleRows + 1, 0, gridRows - visibleRows) ×
  cellHeight`): a fresh shell prompt in the first rows stays at the top of
  the band; a tall scrollback keeps its newest bottom rows. No xterm/PTY
  resize; recomputed as frames arrive.
- **Search field + history panel.** The desktop terminal search input gets
  terminal roles (`--tty-raised` bg, `--term-fg` ink, hairline border,
  terminal focus ring) instead of the page input theme. The history Sheet
  renders in a page portal outside `.lab`, so a `historySheet` class gives
  it the terminal background/border/foreground — prompts, heading and
  close never render pale-on-white in light appearance.
- **Type and spacing tokens.** Local input `--text-input` (14px fine /
  **16px coarse**), height 36/44; all chrome text `--text-meta` (12px) or
  larger (old 10.5/11px literals removed); toolbar 32px; AuxKeys 28px;
  progress 2px; mode pill a radius capsule. Stale badge: dashed neutral
  「画面可能过期」 for `node-link-unavailable`, regular 「会话已结束」 for
  `instance-gone`.

## Verification

The dev-only `pnpm test:e2e:terminal` gate needs `remuda dev` panes on a
herdr server; per the task coordinator it is never run here. Coverage uses
the hub-e2e fake Node plus `m-realdevice`, under the gate e2e lock with
per-worker ports, servers in setsid process groups with trap cleanup.

- `tests/e2e/uo10-evidence.hub.spec.ts`:
  - same dark instrument in **dark** and **light** appearance (1440):
    `--term-bg #1a1917`, pane/viewport `rgb(26,25,23)`, chrome ink resolves
    from the `--tty-muted` shade (canvas-resolved sRGB), distinct from and
    darker than `--term-fg` but still ≥4.5:1;
  - 393px keyboard (323px band): local input + phone key bar inside the
    band, xterm visible; **zero** resize calls and identical rows/cols
    across the animated open/close; minimum frame font ≥12px; a fresh
    shell's early rows remain inside the band (cursor-anchored crop);
  - **rotation with keyboard open**: width 393→700, exactly one resize,
    cols increase, rows stay at the frozen value;
  - history sheet dark in both appearances (`rgb(26,25,23)` panel, light
    heading) with light/dark screenshots.
- `tests/e2e/m-realdevice.hub.spec.ts` (b) drives a real animated keyboard
  open (sub-threshold frame then full height) at 393px on every engine and
  asserts zero resize calls with unchanged rows/cols through open+close.

Screenshots (390/1440, Remuda only):

- `uo10-terminal-dark-1440.png` / `uo10-terminal-light-1440.png`;
- `uo10-terminal-keyboard-390.png` — keyboard up, input + key bar in band;
- `uo10-history-sheet-dark-390.png` / `uo10-history-sheet-light-390.png`.

## Results

- WebKit iPhone 13: `m-realdevice` 9 passed.
- Chromium hub config (`uo10-evidence`, `m-realdevice`,
  `session-terminal-lab`, `terminal-live`, `ux-ttymode`): 13 passed, 3
  pre-existing skips.
- Unit 1738/1738 (StaleScreenBadge wording tests updated).
- Perf scenario B (two runs each vs origin/main):

  | run | this branch wallMs | origin/main wallMs |
  |---|---|---|
  | 1 | 13 515 | 12 946 |
  | 2 | 13 009 | 13 171 |
  | median | 13 262 | 13 059 |

  0 long tasks, `webgl`, 0 context losses both — ~+1.5% median, within
  noise; **no regression**.
