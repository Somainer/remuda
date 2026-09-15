# tty-mode-1 — 渲染方式 badge on herdr-backed sessions

**Date:** 2026-09-15
**Worktree:** `wt/c-ttymode/herdr-alt-screen-observation`
**Spec:** `web/tests/e2e/ux-ttymode.hub.spec.ts`

## Problem

On herdr-backed sessions the terminal header badge stayed at
「渲染方式待检测」 even while a full-screen task was visibly running. The
badge leaves "unknown" only on a `tty.mode` observation, and the only
producer of that observation was the native-carrier terminal emulator
(`shell_pty.rs`). The herdr carrier (`claude_pty.rs`) returned the trait
default `None` ("no trustworthy mode observation"), so the web never learned
the pane's real mode and the requested-fullscreen mismatch hint could never
fire.

## Change

A tiny deterministic byte-stream scanner tracks the DEC alternate screen on
the relay every herdr consumer already uses
(`crates/remuda-herdr/src/alt_screen.rs`):

- `ESC [ ? 1049 h/l` (save-cursor alt buffer; what Claude Code uses)
- `ESC [ ? 1047 h/l` and `ESC [ ? 47 h/l` (legacy alt-buffer switches)
- `ESC c` (RIS — a full reset always returns to the primary screen)
- mixed private-mode sequences are split parameter by parameter; OSC/DCS
  strings cannot spoof the mode; a sequence split across relay chunks is
  resumed from its ESC byte.

It is deliberately not a terminal emulator: one state bit, one pending
sequence suffix bounded at 256 bytes. The scanner lives in the shared
`TerminalObserver` frame reader and its reading rides each `TerminalFrame`;
the Node's herdr TTY pump emits `TtyEvent::Mode` the moment the bit flips
and reports the current value in `tty.attach` (`altScreen`), so a follower
attaching to an already-full-screen TUI gets the right badge immediately.
The `claude-pty` driver's attach relay tracks the same value and exposes it
through a new `Driver::alt_screen()` method.

Verified baseline on herdr 0.8.2: its `terminal.session observe|control`
frames carry only `seq/width/height/full` and server-side rendered damage —
there is no mode field — so the byte stream is the right source on current
herdr; a future herdr mode API can be added as a second source behind the
same `alt_screen()` surface.

## Tests

- Scanner unit tests (`remuda-herdr`): split sequences (every cut point),
  1049 vs 1047 vs 47, non-private `47`, RIS, batched `?1049;25h`, OSC
  spoofing, unterminated garbage resync, byte-by-byte vs whole-frame parity.
- Driver test (`remuda-driver` `tests/claude_pty.rs`): a claude-pty session
  reports `None` before attach, `Some(false)` on the inline frame, flips to
  `Some(true)` when the scripted fake harness enters `?1049`, and returns to
  `Some(false)` on leave.
- Hub-backed spec (`ux-ttymode.hub.spec.ts`): badge goes
  待检测 → 行内渲染 on attach, → 全屏渲染 after the harness enters the alt
  screen (raw `?1049h` frame + `tty.mode`, as the Node relay emits it),
  → 行内渲染 on leave.

## Screenshots

Captured only with `REMUDA_EVIDENCE=1`; otherwise they land under
`web/test-results/evidence/`:

- `tty-mode-1-inline.png` — 行内渲染 right after attach.
- `tty-mode-1-fullscreen.png` — 全屏渲染 after `TTYMODE_ALT_ON`.
- `tty-mode-1-back-inline.png` — 行内渲染 after `TTYMODE_ALT_OFF`.
