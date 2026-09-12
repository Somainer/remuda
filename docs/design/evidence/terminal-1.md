# Terminal-1 evidence

Isolated `remuda dev` on hub `127.0.0.1:58280` and node `127.0.0.1:58287` (`REMUDA_COOKIE_SECURE=0`, access-code file, not the live `:18080` demo). Headless Chrome through `web/node_modules/.bin/playwright` (no `npx`). Live cwd was a throwaway `/tmp` git repo named `remuda-x-termui-ws` (not committed).

Contract: `docs/design/remote-terminal.md` (D-016), Hub/Node frames from `70924b6` / merge `677a908`.

## What the UI does

- `TerminalView` attaches over `/v1/follow?instanceId=&tty=1` (same-origin Vite proxy in dev). Output = 32-byte `tty.frame` channel `1`; input = channel `3` raw bytes from xterm `onData` + `onBinary` (offset `0`); resize = JSON `{type:"tty.resize",cols,rows}`. Journal snapshot then binary ANSI snapshot; emulator resets before live frames.
- xterm.js + fit + WebGL/canvas fallback + Unicode11 + Night Corral 16 + `extendedAnsi` 240. Container resize, **全屏** (`100dvh`, safe-area). Reconnect keeps the last paint.
- Input: **raw** (desktop default) vs **keys** (phone default, IME-safe local line). Mobile key bar: esc, tab, sticky ctrl/alt, arrows, pgup/pgdn, ctrl+c; 44px tap targets.
- New Session kind **Terminal** → `driver=shell-pty`. Pty-backed kinds open **终端** by default; **结构** is `/s/:id/structured`.

## Live path (after x-term `70924b6`)

| Step | Result |
|---|---|
| Login (bootstrap token from access-code file) | `/sessions` |
| New session kind=terminal, driver=shell-pty | `/s/ins_…` `data-view=tty`, `lifecycle=running`, follow WS live, login-shell prompt |
| `echo TERMUI_ECHO` | typed command + echo visible in xterm |
| `printf '\033[?1000h\033[?1006h'; cat` then click the screen | SGR mouse report echoed: `CSI <0;11;3M` (button 0 at col 11, row 3) reached the PTY and came back |
| New session kind=grok, driver=generic-pty | default view is 终端 tab (herdr TUI still starting in the shot) |

## Screenshots

Tokens omitted. No personal home paths in the frames.

| | |
|---|---|
| Mock lab ANSI + raw + key bar | [terminal-1-lab.png](./terminal-1-lab.png) |
| Mock New Session kind=terminal | [terminal-1-new-session.png](./terminal-1-new-session.png) |
| Mobile key bar (44px; esc/tab/ctrl/alt/arrows/pgup/pgdn/ctrl+c) | [terminal-1-keybar.png](./terminal-1-keybar.png) |
| Live shell-pty tab | [terminal-1-shell.png](./terminal-1-shell.png) |
| Live echo | [terminal-1-echo.png](./terminal-1-echo.png) |
| Live mouse SGR round-trip | [terminal-1-mouse.png](./terminal-1-mouse.png) |
| Live grok generic-pty defaults to 终端 | [terminal-1-grok.png](./terminal-1-grok.png) |

## Tests

- `pnpm --dir web test` (98)
- `pnpm --dir web build`
- Playwright mock: `new-session.spec.ts`, `session-terminal-lab.spec.ts`, `mobile-qa.spec.ts` (44px key bar)
- Playwright live: `pnpm --dir web test:e2e:terminal` against hub `:58280` (echo + mouse round-trip)
