# UI PTY-1 evidence

Hub `127.0.0.1:38080`, Node `127.0.0.1:38787` via `remuda dev` (`REMUDA_COOKIE_SECURE=0`, access-code file, not the live `:18080` demo). Headless Chrome through `web/node_modules/.bin/playwright` / Playwright `channel=chrome` (no `npx`).

## What the UI now does

- New Session offers **claude / codex / grok / agy**. Non-claude kinds use driver `generic-pty` and show the Node yolo preset (`grok --always-approve`, `codex --dangerously-bypass-approvals-and-sandbox`). Claude still defaults to `claude-print` (optional `claude-pty`).
- cwd is an existing host directory or **new worktree `<name>` from main** (`POST /v1/worktrees` → Node `git worktree add -b wt/<name>/…`).
- Create POST matches Hub `InstanceCreate` (`kind`, `driver`, `prompt`, `cwd`, `worktree`, `name`).
- generic-pty session page is a screen-derived journal view + lifecycle + prompt `instance.send` + keys `enter` / `esc` / `ctrl+c` (`tty.write`).
- Sessions list shows Hub `lifecycle` (`running`) × `activity` (`idle`/`working`/…) instead of activity only.

## Live path

| Step | Result |
|---|---|
| Login (bootstrap token from access-code file) | `/sessions` |
| New session: kind=grok, driver=generic-pty, worktree `uipong` from main, brief `Reply with the single word PONG` | `ins_01a09692-…` |
| Wait | `lifecycle=running` |
| Screen | assistant **PONG** |
| Prompt `Now reply DONE` | assistant **DONE** |
| Same for kind=codex, worktree `uicodex` | `ins_01a09693-…`; screen `• PONG` then `• DONE` |

Grok 4.6 (xhigh) · `--always-approve`. Codex v0.154.0 gpt-6-astra max · YOLO. Both idle after the second turn. List rows: `running · idle · connected` · `generic-pty`.

## Screenshots

Tokens omitted. Local host cwd appears in TUI chrome (not copied here).

| | |
|---|---|
| New grok + worktree | [ui-pty-1-new-grok.png](./ui-pty-1-new-grok.png) |
| Grok running | [ui-pty-1-grok-running.png](./ui-pty-1-grok-running.png) |
| Grok PONG | [ui-pty-1-grok-pong.png](./ui-pty-1-grok-pong.png) |
| Grok DONE | [ui-pty-1-grok-done.png](./ui-pty-1-grok-done.png) |
| New codex + worktree | [ui-pty-1-new-codex.png](./ui-pty-1-new-codex.png) |
| Codex running | [ui-pty-1-codex-running.png](./ui-pty-1-codex-running.png) |
| Codex PONG+DONE | [ui-pty-1-codex-done.png](./ui-pty-1-codex-done.png) |
| List lifecycle | [ui-pty-1-sessions-final.png](./ui-pty-1-sessions-final.png) |

`ui-pty-1-codex-pong.png` is the same frame as `ui-pty-1-codex-done.png` (both replies visible).

## Tests

- `cargo test -p remuda-node --lib worktree`
- `cargo test -p remuda-hub --test openapi --test worktree`
- `cargo clippy -p remuda-node -p remuda-hub --tests -- -D warnings`
- `pnpm --dir web test` (87) + `pnpm --dir web build`
- Playwright mock: `new-session.spec.ts`, `agent-board.spec.ts` (claude flow + board keys)
