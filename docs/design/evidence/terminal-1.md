# Terminal-1 evidence

Isolated `remuda dev` on hub `127.0.0.1:58280` and node `127.0.0.1:58287` (`REMUDA_COOKIE_SECURE=0`, access-code file, not the live `:18080` demo). Headless Chrome through `web/node_modules/.bin/playwright` (no `npx`). Workspace for live creates was a throwaway `/tmp` git repo (not committed).

Contract: `docs/design/remote-terminal.md` (origin, D-016). Web speaks it; Hub follow `tty=1` binary replay is still x-term’s backend.

## What the UI now does

- `TerminalView` attaches over `/v1/follow?instanceId=&tty=1` (same-origin Vite proxy in dev). Output = 32-byte `tty.frame` channel `1`; input = channel `3` raw bytes from xterm `onData` + `onBinary`; resize = JSON `{type:"tty.resize",cols,rows}`. Snapshot JSON resets the emulator before live frames.
- xterm.js + fit + WebGL/canvas fallback + Unicode11 + Night Corral 16 + `extendedAnsi` 240. Container resize, **全屏** (`100dvh`, safe-area, chrome covered). Reconnect keeps the last paint. Output is rAF-batched; input is 8ms-batched, chunked at 4096.
- Input: every byte (keyboard, paste, mouse reports). Aux key bar: esc, tab, sticky ctrl/alt, arrows, pgup/pgdn, ctrl+c (`terminalTouch.ts` still owns phone scroll). Indicator **raw** (desktop default) vs **keys** (phone default, IME-safe local line).
- New Session kind **Terminal** → `driver=shell-pty`, cwd/worktree, empty prompt allowed. pty-backed kinds (`terminal` / `shell-pty` / `generic-pty` / `claude-pty` / codex·grok·agy) open the **终端** tab by default; **结构** is `/s/:id/structured`.

## Live path

| Step | Result |
|---|---|
| Login (bootstrap token from access-code file) | `/sessions` |
| New session kind=terminal, driver=shell-pty | `/s/ins_…` `data-view=tty`, raw indicator, key bar, follow WS live |
| Follow `tty=1` | journal snapshot JSON arrives; **no binary PTY bytes yet** (Hub follow still text-only; Node maps unknown `shell-pty` to claude-print) |
| New session kind=grok, driver=generic-pty | default view is 终端 tab (same follow attach) |

Mouse-in-vim / echo-to-PTY cannot be asserted until x-term fans binary `tty.frame` to follow sockets and implements `shell-pty`. Unit tests encode channel-3 input including CSI mouse-like bytes.

## Screenshots

Tokens omitted. No personal home paths in the frames.

| | |
|---|---|
| Mock lab ANSI + raw + key bar | [terminal-1-lab.png](./terminal-1-lab.png) |
| Mock New Session kind=terminal | [terminal-1-new-session.png](./terminal-1-new-session.png) |
| Live shell-pty tab (follow live, empty PTY) | [terminal-1-shell.png](./terminal-1-shell.png) |
| Live grok generic-pty defaults to 终端 | [terminal-1-grok.png](./terminal-1-grok.png) |

## Tests

- `pnpm --dir web test` (96)
- `pnpm --dir web build`
- Playwright mock: `new-session.spec.ts` (terminal kind), `session-terminal-lab.spec.ts`
- Playwright live: `pnpm --dir web test:e2e:terminal` against hub `:58280`
