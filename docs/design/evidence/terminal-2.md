# Terminal-2 evidence — scroll and mouse dead in the web terminal

User report: in the web **终端** view "scroll and mouse do nothing". Diagnosis judge
(three independent lenses) put the byte transport in the clear — `ws.rs` relays raw and
`shell_pty.rs` is byte-exact — and located four separate web-layer faults, all reproduced.
This branch fixes the four web-side ones (A1–A4) plus B5. Hub-side items (B2/B3/B4) are
another worker's; B1 (herdr driver has no mouse/scroll contract) needs a product decision
and is not implemented here.

Isolated `remuda dev` on hub `127.0.0.1:60280` / node `127.0.0.1:60287`
(`REMUDA_COOKIE_SECURE=0`, `REMUDA_ALLOWED_ORIGINS` for the Vite origin,
`REMUDA_MAX_INSTANCES=60` so the suite can create its own sessions; access-code file, not
the shared demo). Playwright channel `chrome` via `pnpm --dir web test:e2e:terminal`.
Throwaway `/tmp` git workspace, not committed.

Contract: `docs/design/remote-terminal.md` (D-016).

---

## A1 — `disableStdin` stuck true after a client-side session switch

`TerminalView` constructed the Terminal with `disableStdin: true`, and the only code that
cleared it was an effect with deps `[directInput, frozen]`. But the Terminal is destroyed
and rebuilt by the effect keyed on `[instance.id]`, and switching sessions from the
sidebar (`/s/A` → `/s/B` both render `SessionPage`, so `TerminalView` never unmounts)
rebuilds it while `directInput`/`frozen` are unchanged. The clearing effect never re-ran,
so the new Terminal kept `disableStdin: true` forever.

xterm 6 gates *everything* on that one flag — `CoreService.triggerDataEvent` drops mouse
and wheel reports before `onData`, local scrollback wheel is cancelled while the app has
tracking on, and the alt-screen wheel→arrow conversion is skipped. So keys, clicks and
scroll all die together while the toolbar still says 直连 / `data-tty-io=raw` /
`status=live`. Toggling 本地输入→直连 (or reloading) re-ran the effect and restored it —
which is why it read as "intermittent".

| | before | after |
|---|---|---|
| `.xterm-helper-textarea.readOnly` after sidebar A→B→A | `true` | `false` |
| typing after the switch | no echo | `TERMUI_AFTER_SWITCH` echoes |
| wheel over a tracking app after the switch | 0 bytes | `CSI <64;…M` |
| click after the switch | 0 bytes | `CSI <0;…M` |

Fix: derive the policy at construction from the current refs and re-apply it on prop
change, both through one pure helper (`stdinPolicy.ts`). `disableStdin` now means only
"frozen" — `keys` mode no longer uses it (see A3). The `[instance.id]` effect also resets
per-instance view state (`ready`/`status`/`preview`/`rawTail`/`mouseMode`/`mouseReports`)
so nothing leaks across a switch, and the async `attachTerminalRenderer` resolution is
guarded against landing on a disposed terminal.

Regression test: `terminal-live.spec.ts` › *stdin, mouse and scroll survive a sidebar
session switch (A→B→A)*. Creates two shell-pty sessions, switches via the sidebar (never
`goto`, so the component stays mounted), then asserts `readOnly === false`, that typing
echoes, and that a wheel + click over `printf '\033[?1000h\033[?1006h'; cat -v` produce
SGR reports. Run against the pre-fix tree it fails exactly at the `readOnly` assertion
(`Expected: false / Received: true`); after the fix it passes.

Unit: `stdinPolicy.test.ts` (9 cases — the direct/keys/frozen matrix and the
"clears a stale flag on a rebuilt terminal" case).

## A2 — sticky DECSET mouse tracking had no indicator and no escape hatch

An app that sets `?1000h`/`?1006h` and dies without the DECRST leaves the PTY in tracking
mode, and the attach snapshot replays the `?1000h`, so it survives a reload for every
device that attaches. While tracking is on, xterm cancels local wheel scrolling and ships
each tick to the PTY, where it lands as `zsh: command not found: 64;77;22M`. Shift+wheel
is not a workaround (xterm's `consumeWheelEvent` bails on `shiftKey` and cancels the
event). Before this change `data-tty-mouse` was a bare data attribute — no indicator, no
toggle, no reset.

Added to the toolbar, shown only while `data-tty-mouse !== none`:

- **鼠标上报** — highlighted while on. Off ⇒ pointer reports are dropped at the input
  gate and a custom wheel handler scrolls the local scrollback instead.
- **重置终端模式** — writes `ESC[?1000l ?1002l ?1003l ?1006l` into the emulator *and*
  sends it to the PTY. Both halves matter: writing locally clears the common sticky case
  where the app that armed tracking is long gone and nothing downstream would ever send
  the DECRST back; sending it on stops an app that is still tracking from re-arming.

Regression test: *wheel scrolls locally without tracking and reports with it (A2)* —
(a) no tracking: wheel moves the slider and sends 0 bytes; (b) tracking on: the same
gesture emits `CSI <64|65;…`; (c) 鼠标上报 off: the tail stops growing and the wheel
scrolls locally; (d) 重置终端模式: `data-tty-mouse` returns to `none`, the toggle retires,
and the wheel scrolls again.

## A3 — narrow viewport silently discarded every mouse event

`keys` mode set `disableStdin = true`, which in xterm also kills mouse reports — so under
768px a click or wheel sent 0 bytes while the dock's `LocalInput` kept the keyboard
working. That is precisely "keyboard fine, mouse and scroll dead". Worse, the compact
query (`max-width: 767px`) matches a **narrow desktop window**, which has a real mouse and
a real keyboard.

Two changes:

- `keys` mode no longer touches `disableStdin`. Keyboard and pointer are gated separately
  at the `onData` boundary (`mouseReports.ts`: mouse reports are recognised by their SGR /
  X10 / urxvt shape, everything else is keyboard).
- The direct-input default is now `(pointer: coarse) and (hover: none)`
  (`COARSE_POINTER_QUERY`) instead of the layout query, so only a touch device hands the
  keyboard to the dock. Layout still follows `COMPACT_WORKBENCH_QUERY`.

`mobile-qa.spec.ts` gained `hasTouch: true` for the same reason: at 390px without touch
the terminal now correctly stays in 直连 and has no 发送 button, so the phone QA fixture
has to actually emulate a phone.

Regression test: *narrow viewport keeps the mouse alive in keys mode (A3)* — 700px
viewport, click still produces `CSI <0;…M`.

## A4 — `applyFit` measured the wrong box; `.viewport` stole the scroll

`applyFit` measured `.viewport`, but xterm is mounted in `.host`, which carries
`padding: 16px 20px`. Three compounding problems, all fixed:

| | before | after |
|---|---|---|
| `.viewport` `scrollHeight − clientHeight` @1440×900 | `15` | `0` |
| `.xterm-screen` height vs. its content box | `680` in `649` (31px clipped) | `640` in `649` |
| rows | 68 (bottom row cut off) | 64 |

- measure `.host` and subtract its computed padding;
- `.host` gets `box-sizing: border-box` (with `height: 100%` the padding was otherwise
  added *outside* the 100%);
- the visually-hidden `.preview` `<pre>`s get `top: 0; left: 0` — unanchored they sat at
  their static position below `.host` and stretched the scroll box by 15px;
- `.viewport` is `overflow: hidden` (the terminal owns its scrollback; a wheel near the
  bottom edge used to scroll the outer div);
- after `fit.fit()`, trim rows against the painted `.xterm-screen` height — FitAddon's CSS
  cell estimate can round one row larger than the renderer actually paints.

Asserted in *New Session kind terminal opens the terminal tab* (`scrollHeight −
clientHeight === 0`).

## B5 — renderer-dependent line height and an unbounded preview

- Touch/wheel scrolling measured `.xterm-rows > div`, which does not exist under the
  webgl or canvas renderers — it always fell back to a hardcoded 16px. Now measured from
  `.xterm-screen` height ÷ `term.rows`, which is renderer-independent (`data-tty-renderer`
  is `webgl` on this host).
- `setPreview` had no cap while `rawTail` was capped at 4000; a long session grew an
  unbounded `<pre>`. Both are 4000 now.

---

## Drive-by: deterministic terminal focus in the live spec

`terminal-live.spec.ts` focused the terminal by clicking `.xterm-helper-textarea`, a
3×10px element xterm parks at the cursor; Playwright frequently rejected it with
`Element is outside of the viewport` (3/3 failures on the unmodified tree). All four call
sites now go through `focusTerminal()`, which focuses the element directly and polls
`document.activeElement`. A readOnly textarea still swallows the keystrokes, so the A1
regression stays visible — verified by re-running each new test against the pre-fix tree:

| test | pre-fix failure |
|---|---|
| switch-session-keeps-stdin | `readOnly` `Expected: false / Received: true` |
| wheel with/without tracking | `tty-mouse-reports` element not found |
| narrow-viewport-mouse | `tty-raw-tail` never contains `[<0;` |

## Screenshots

Tokens omitted. No personal home paths in the frames.

| | |
|---|---|
| Live after sidebar A→B→A: typing echoes, wheel + click report SGR | [terminal-2-after-switch.png](./terminal-2-after-switch.png) |
| Tracking on: 鼠标上报 / 重置终端模式 in the toolbar | [terminal-2-tracking-on.png](./terminal-2-tracking-on.png) |
| After 重置终端模式: `data-tty-mouse=none`, controls retired | [terminal-2-tracking-reset.png](./terminal-2-tracking-reset.png) |
| 700px viewport: click still reaches the PTY | [terminal-2-narrow-mouse.png](./terminal-2-narrow-mouse.png) |

## Checks

- `pnpm --dir web test` — 177 passed (44 files; +19 new in `stdinPolicy.test.ts` and
  `mouseReports.test.ts`)
- `pnpm --dir web lint` — no new findings
- `pnpm --dir web build` (tsc -b + vite build)
- `pnpm --dir web test:e2e:terminal` — 6 passed against the isolated `remuda dev`
- `pnpm --dir web test:e2e:hub` — 6 passed
- `pnpm --dir web test:e2e --project=chromium` (mock) — 50 passed
- `pnpm --dir web test:e2e` (mock, both projects) — the `mobile-webkit` project stalls with
  `Test timeout … while setting up "page"` on a handful of specs, hitting a different set
  on each run. Reproduced identically on the unmodified tree (stash the branch, rerun), so
  it is a pre-existing browser-launch flake on this machine, not a regression here. The
  chromium project is green, and `mobile-qa.spec.ts` passes on both projects when run
  alone (12/12).
- `./scripts/ci/secret-scan.sh` — pass
