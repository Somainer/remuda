# Terminal-3 evidence — replayed queries, fullscreen fit, and the 直连 dock

Three follow-ups after [terminal-2.md](./terminal-2.md) (merged as `46e937a`):

1. after a TUI exits, the shell prompt fills with leaked terminal-query *answers*;
2. starting a TUI in 全屏 renders a clipped, offset grid until a page refresh;
3. the raw input box in 直连 mode was rendered disabled instead of hidden.

Isolated `remuda dev` on hub `127.0.0.1:60280` / node `127.0.0.1:60287`
(`REMUDA_COOKIE_SECURE=0`, `REMUDA_ALLOWED_ORIGINS` for the Vite origin,
`REMUDA_MAX_INSTANCES=60`, `REMUDA_WORKSPACE_ROOTS` pointing at a throwaway `/tmp`
workspace — new on main, which now refuses workspaces outside the configured roots).
Playwright channel `chrome` via `pnpm --dir web test:e2e:terminal`. Nothing committed from
the throwaway workspace.

Contract: `docs/design/remote-terminal.md` (D-016).

---

## 1. Replayed history re-answered terminal queries

### Symptom

After `claude` exited, the prompt showed runs of junk that the shell then tried to execute:

```
2RR0;276;0c11;rgb:1212/1616/1c1c10;rgb:e7e7/dcdc/c8c812;2$y
```

Those are *replies*, not output: CPR (`ESC[…R`), DA1/DA2 (`…c`), OSC 10/11 colour reports
(`rgb:…`), DECRQM (`…$y`).

### Root cause

The Node keeps a raw ring buffer of past PTY bytes (`crates/remuda-driver/src/shell_pty.rs:157`
— `snapshot()` returns the ring verbatim). On every attach the hub replays it
(`crates/remuda-hub/src/ws.rs:925` `send_tty_snapshot`), and `TerminalView` writes it into
xterm. If that history contains the queries a TUI issues at startup, **xterm answers them
again** — and the answers go out as PTY input. The app that asked is long gone, so they
land on whatever is at the prompt now.

Verified against the wire rather than inferred. Counting outbound binary frames on the
follow socket across a reconnect, on the pre-fix tree:

```
frame 0103…551b5b323b31521b5b
       ^^ channel 3 = TTY input        payload @32 = ESC[2;1R   (a CPR reply)
```

| | before | after |
|---|---|---|
| channel-3 frames sent during a reconnect replay | 2 | **0** |
| bytes sent | `0;276;0c` `rgb:e7e7/dcdc/c8c8` `rgb:1212/1616/1c1c` `$y` | none |

### Fix

Every one of xterm's ~33 automatic replies funnels through
`CoreService.triggerDataEvent` → `onData` (verified in the 6.0.0 bundle; it is also what
`disableStdin` gates). So the whole class is caught by dropping `onData`/`onBinary` while
*replayed* bytes are in the parser — no sequence-by-sequence stripping, and live queries
are still answered normally.

Two details decide whether that is correct:

- **`term.write()` does not parse synchronously.** `WriteBuffer` defers via
  `setTimeout(() => this._innerWrite())` and `_innerWrite` yields every 12 ms. A
  `flag = true; write(x); flag = false` wrapper would clear the flag long before the bytes
  are parsed. What xterm *does* guarantee is that each chunk's write callback runs right
  after that chunk is parsed, in FIFO order — so the guard is raised when a replay chunk is
  queued and lowered in that chunk's callback. It is a counter, not a boolean, because
  several replay chunks can be in flight at once.
- **`flushOut` merged the whole queue into one `term.write`.** A snapshot and the first
  live output can arrive in the same animation frame, so a single flag per write would
  smear one decision across both — either leaking historical answers or swallowing a live
  app's legitimate reply. The queue now carries the origin per chunk and
  `groupByOrigin` emits one write per run, still collapsing the common all-live case into
  a single write.

The replay boundary itself is client-side: the hub sends the snapshot as an ordinary
binary frame, indistinguishable on the wire, and as a *single* `encode_output_frame` call
(`ws.rs:670`) — so "the first delivery after an attach/reset is the replay" is exact.
`client.ts` arms that flag on snapshot, on `gap`, and on every reconnect.

New module `replayGuard.ts` + `replayGuard.test.ts` (12 unit cases), including a fake
emulator that answers asynchronously like xterm. The decisive ones: replayed queries send
nothing; **the same queries arriving live are still answered** (suppressing those would be
a worse bug than the leak); a mixed batch suppresses only the replayed half.

## 2. Fullscreen rendered a clipped grid until refresh

### Root cause — a font-size ratchet, not a transition race

The hypothesis was a fit/resize race to be solved with `transitionend`. It is not: there
is **no CSS transition** on `.lab[data-tty-fullscreen="1"]`, so `transitionend` would never
fire. Sampling geometry every frame across the toggle showed the real fault — `applyFit`
was not idempotent:

```
FITDBG {"w":1072,"h":635,"font":13.17,"cols":138,"rows":39}
FITDBG {"w":1072,"h":635,"font":12.72,"cols":143,"rows":41}
FITDBG {"w":1072,"h":635,"font":12.19,"cols":150,"rows":44}    ← drifting before fullscreen
...
FITDBG {"w":1424,"h":835,"font":6.66,"cols":475,"rows":94}     ← toggle
FITDBG {"w":1424,"h":835,"font":3.32,"cols":1426,"rows":212}
FITDBG {"w":1424,"h":835,"font":1.66,"cols":1426,"rows":212}   ← collapsed
```

`fit.fit()` derives cols/rows from the font currently in effect; `fittedTerminalFont` was
then fed *those* cols/rows and shrank the font to fit them. Smaller font → wider grid →
smaller font. Each call ratcheted, and a fullscreen toggle (several fits in a row) drove
14px to 1.66px and 1426 columns, painting a tiny grid in a large box. This was a
pre-existing bug — the drift is visible before fullscreen is ever touched.

### Fix

Size the font against the grid the box yields at the **base** font (`BASE_FONT_SIZE = 14`,
now the single source for construction and fitting) rather than against the current grid,
making `applyFit` idempotent. Alongside it, `settleFit` re-fits across the frames a
React-driven container change needs, and re-fits once more after `document.fonts.ready`.

Two things that did *not* survive contact with the hardware, both caught by probing:

- `clearTextureAtlas()` + `refresh()` after a resize **corrupted the WebGL renderer** —
  `.xterm-screen` collapsed to width 0 and mouse reports stopped. Dropped.
- Running the multi-frame settle on *mount* raced the renderer's own first sizing and left
  the screen collapsed the same way. It now runs only on an actual `mode`/`fullscreen`
  transition; mount keeps the plain single fit.

| | before | after |
|---|---|---|
| fullscreen grid @1440×900 | 212×1426, screen 424×**0** px | 46×178, screen 828×1424 px |
| painted vs. container height | 288 px in a 561 px box | 828 px in an 835 px box |
| font after one toggle | 1.66 px | 14 px |
| after 3 toggles + full mode cycle | drifts every pass | returns to exactly 137×37 |

## 3. 直连 no longer renders a disabled input box

In 直连 the dock rendered a `raw` label plus two dashed placeholder divs standing in for
the input and the send button. `.dock` itself carries `padding: 12px 18px` and a
`border-top`, so hiding only the children would still reserve a strip — the whole
container is now unrendered, and the ghost CSS (`.dockLabel`, `.directGhost`,
`.directGhostSend`) is deleted. 本地输入 renders the real `LocalInput`, enabled.

## Screenshots

Tokens omitted. No personal home paths in the frames.

| | |
|---|---|
| Clean prompt after a reconnect replay of query-heavy history | [terminal-3-replay-clean.png](./terminal-3-replay-clean.png) |
| Fullscreen with a TUI running, fitted on the first try | [terminal-3-fullscreen.png](./terminal-3-fullscreen.png) |
| 直连: no dock, no reserved space | [terminal-3-direct-no-dock.png](./terminal-3-direct-no-dock.png) |
| 本地输入: the real input, enabled | [terminal-3-keys-dock.png](./terminal-3-keys-dock.png) |

## Tests

Three new live cases in `terminal-live.spec.ts`, each verified to fail on the pre-fix tree
(`46e937a`) and pass after:

| test | pre-fix failure |
|---|---|
| replayed history does not answer terminal queries | `sent` was `0;276;0cgb:e7e7/…$y…`, expected `""` |
| fullscreen fits the container without a refresh | painted height `288`, expected `> 504` |
| 直连 hides the local input dock entirely | `tty-dock` not found in 本地输入 mode |

The replay test asserts on **outbound channel-3 frames**, not screen text: the query bytes
are legitimately present in the scrollback as history, so only a sent frame proves the
emulator answered them again.

## Checks

- `pnpm --dir web test` — 223 passed (52 files; +12 in `replayGuard.test.ts`)
- `pnpm --dir web lint` — no new findings
- `pnpm --dir web build` (tsc -b + vite build)
- `pnpm --dir web test:e2e:terminal` — 9 passed against the isolated `remuda dev`
- `./scripts/ci/secret-scan.sh` — pass
