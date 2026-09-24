# UO-10 终端外框（terminal chrome）

Date: 2026-09-24. Scope: UO-10 (S) — the terminal frame around xterm.
**Change of direction (owner 2026-09-24): the terminal FOLLOWS the
appearance** — two palettes (dark/light), live switching. Supersedes the
earlier always-dark rule in D-053 item 4. Design:
`docs/design/visual-system.md` §3.4/§4, ADR D-053 item 4, ui-spec §2.3/§4.7.

## What changed

- **Two xterm themes, live switch.** `theme.ts` exports
  `DARK_TERMINAL_THEME` and `LIGHT_TERMINAL_THEME` plus
  `terminalThemeFor(appearance)`. A `useTerminalAppearance` hook resolves
  the current mode (explicit `data-appearance` or live `prefers-color-scheme`)
  and watches both; switching assigns `term.options.theme` on the EXISTING
  Terminal, so the WebGL/canvas renderer repaints in place — no rebuild, no
  scrollback loss. Verified by a live-switch test (settings choice changes,
  grid stays, background repaints, switches back).
- **Light ANSI palette.** 16 named colours designed for `#faf9f6`; every
  chromatic colour and both greys clears WCAG AA 4.5:1 on the light
  background (unit-tested with the same contrast helper as the dark set).
  `white`/`brightWhite` are dark greys (`#5a554c`/`#2b2a27`) because TUIs
  commonly paint default text on "white"; true white remains available via
  truecolor or 256-cube slot 231. The 256-colour cube and truecolor pass
  through unchanged.
- **Terminal roles follow the mode.** `tokens.css` carries dark (on `:root`)
  and light (both light branches) values for `--term-bg/-fg/-raised/-border/
  -muted/-danger-fg/-selection`. Every terminal surface — toolbar, key bar,
  local input, history sheet, search field, stale badge, progress — uses
  these roles, so light mode has no dark islands and dark mode has no light
  ones. The history Sheet (rendered in a page portal) gets a
  `historySheet` class with the mode's terminal background/border/fg.
- **Round-2 fixes retained:** three-state fit gate (`freeze`/`defer`/`fit`)
  gives **0** PTY resize calls across an animated keyboard open/close;
  rotation while keyboard open keeps frozen rows, refits cols, sends
  exactly one resize; cursor-anchored `translateY` crop keeps a fresh
  shell's top prompt visible.
- **Type and spacing tokens** unchanged from round 2 (16px coarse input,
  ≥12px chrome text, 32px toolbar, etc.).

## Verification

The dev-only `pnpm test:e2e:terminal` gate needs `remuda dev` panes on a
herdr server; per the task coordinator it is never run here. Coverage uses
the hub-e2e fake Node plus `m-realdevice`, under the gate e2e lock with
per-worker ports, servers in setsid process groups with trap cleanup.

- `tests/e2e/uo10-evidence.hub.spec.ts`:
  - terminal follows **dark** and **light** appearance (1440): mode
    `--term-bg` hex, pane/viewport rgb match, no seam, chrome ink contrast
    correct for the mode;
  - **live switch**: settings choice changes with no reload, pane repaints,
    fitted rows/cols and the xterm node preserved, switches back;
  - 393px keyboard (323px band): input + key bar in band, xterm visible;
    **zero** resize calls and identical rows/cols across animated
    open/close; frame font ≥12px; fresh-shell early rows stay in band;
  - **rotation with keyboard open**: 393→700 width, exactly one resize,
    cols increase, frozen rows retained;
  - history sheet follows both appearances (mode bg + contrasting heading).
- `tests/e2e/m-realdevice.hub.spec.ts` (b): animated keyboard open
  (sub-threshold frame then full height), zero resize calls, unchanged
  rows/cols, every engine.

Screenshots (390/1440; copied outside git to
`~/Projects/remuda-agents/scratch/c-uo10/screenshots/`):

- `uo10-terminal-dark-1440.png` / `uo10-terminal-light-1440.png`;
- `uo10-terminal-keyboard-390.png`;
- `uo10-history-sheet-dark-390.png` / `uo10-history-sheet-light-390.png`.

## Results

- WebKit iPhone 13: `m-realdevice` 9 passed.
- Chromium hub config (`uo10-evidence`, `m-realdevice`,
  `session-terminal-lab`, `terminal-live`, `ux-ttymode`): 15 passed, 3
  pre-existing skips.
- Unit 1729/1729, incl. the dark+light theme contrast suite.
- Perf scenario B: steady-state render path unchanged beyond the theme
  option; round-2 numbers stand (0 long tasks, webgl, 0 context losses,
  median within noise of origin/main — no regression).
