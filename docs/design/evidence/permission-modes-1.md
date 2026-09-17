# permission-modes-1 — Claude's real permission modes and in-session switching

The composer permission picker had four invented entries
(`manual / acceptEdits / dontAsk / bypassPermissions`) and the Chinese label
on `dontAsk` was **全自动** — the exact opposite of what that mode does.
Changing the picker also did nothing in a live session: the drivers honoured
`ClaudePermissionMode` only at launch. This doc records the real 2.1.273
vocabulary, the live wheel measurements, the in-session push-down, and the
per-harness launch sets.

Binary measured: **claude 2.1.273** (`claude --version`; the host updated
from 2.1.272 during the effort-sync-2 work). Host OS Linux 5.4, xterm
256-color, 40×120 PTY.

## 1. The real vocabulary

`claude --help --permission-mode <mode>` lists exactly six choices:

```
--permission-mode <mode>  (choices: "manual", "acceptEdits", "plan",
                           "auto", "bypassPermissions", "dontAsk")
```

`manual` is the CLI flag spelling; the TUI/transcript report it as
`default`. The shift+tab TUI status line paints a separate set of phrases
measured on a real TTY:

| wire id | TUI status phrase | what it really does |
| --- | --- | --- |
| `manual` (`default`) | `⏸ manual mode on` | 危险操作逐项询问（默认） |
| `acceptEdits` | `⏵⏵ accept edits on` | 自动批准文件编辑与常见文件命令 |
| `plan` | `⏸ plan mode on` | 只研究与提方案，不做改动 |
| `auto` | `⏵⏵ auto mode on` | 无例行弹窗；一个审查模型先筛动作 |
| `bypassPermissions` | `⏵⏵ bypass permissions on` | 不再弹窗，动作全部放行（需 bypass 允许） |
| `dontAsk` | `⏵⏵ don't ask on` | 任何会触发询问的动作一律拒绝（非交互） |

The TUI's own mode-metadata table (`Ko` in the 2.1.273 bundle) carries the
same title/indicator pairs, plus `indicator: "manual"` for default — the UI
word the picker must mirror, not an invented Chinese label.

## 2. The live shift+tab wheel

shift+tab is bound in the TUI `Chat` context (bundle `J` = `chat:cycleMode`)
and the mode transitions are in `EJe`:

```
default → acceptEdits → plan → (bypass?) → (auto?) → default
dontAsk → default                 // one press exits
```

### 2.1 Auto availability

`gT()` (`isAutoModeAvailable`) is what decides the `(auto?)` branch. On this
host (a human-attended TTY) auto is available, so the idle wheel is the
four-mode cycle `manual → acceptEdits → plan → auto → manual`. With auto
gated off, plan falls straight back to manual.

### 2.2 bypassPermissions is not normally in the wheel

The `(bypass?)` branch is gated by `vJe()`
(`isBypassPermissionsModeAvailable`). Cycling on an ordinary launch never
reaches bypass; the wheel is 4 modes. bypass joins the wheel (between plan
and auto) **only when the launch carried the bypass allowance** — the
`--allow-dangerously-skip-permissions` flag for the stream/PTY carrier, or
`--dangerously-skip-permissions` for the TTY (`apply_tty_bypass_flag`).

`dontAsk` never joins the wheel: `EJe` routes it directly to `default`, and
a launch in dontAsk shows `don't ask on` until the first press. Both
`dontAsk` and a non-allowed `bypassPermissions` are therefore **launch-only**
and the composer menu greys them with a `仅启动时` tag in a live session.

### 2.3 The bypass disclaimer and its default selection

Entering bypass at runtime (plan → bypass on an allow-flag launch) opens a
modal whose **default selection is "No, exit"**:

```
WARNING: Claude Code running in Bypass Permissions mode
…
❯ No, exit          ← highlighted by default
  Yes, I accept
Enter to confirm · Esc to cancel
```

A bare Enter on this modal exits the session — never a keystroke the driver
may send. The push-down accepts it only with the proven sequence **Down,
Enter, CR-gated on the active modal** (measurements below). The auto-mode
entry warning (`Sessions are slightly more expensive…`) is a non-blocking
toast, not a modal; it must not be mistaken for the disclaimer (it has no
confirm footer).

The disclaimer text stays in scrollback **after** it is accepted, so
presence of the words alone is not a valid gate: the active modal's last
painted line is the `Enter to confirm · Esc to cancel` hint, and once
accepted the settled status line paints below it. The engine matches the
modal positionally (`bypass_dialog_visible` in
`crates/remuda-driver/src/permission.rs`).

## 3. Keystroke and read-back measurements

Probes (raw bytes into a 40×120 PTY, the exact writes the driver makes):

| measurement | value |
| --- | --- |
| shift+tab encoding | `ESC [ Z` (`\x1b[Z`) |
| reliable cycle gap | 80 ms gaps work; single batch of 4 presses also lands |
| status-line repaint after a press | **~80–110 ms** (stable indicator within ~1 s) |
| hint persistence | `(shift+tab to cycle)` ~4 s, then the settled phrase |
| min read-back window used | 2.5 s/step, stable indicator read twice (80 ms apart) |
| shift+tab during a streaming turn | honoured (~104 ms repaint while output streams) |
| modal accept | Down (800 ms) → Enter → bypass line within 2 s |
| mid-turn `/effort` | swallowed if submitted while a turn runs; shift+tab is not |

The transcript writes a top-level record whenever the mode changes:

```json
{"type":"mode","mode":"normal","sessionId":"…"}
{"type":"permission-mode","permissionMode":"plan","sessionId":"…"}
```

Records are lazy-flushed: rapid cycles coalesce (3 presses at 1.2 s produced
`acceptEdits → default` plus the earlier launch record), and a turn
boundary flushes the current mode. To pin verbatim records for each mode the
capture submitted a cheap prompt per mode. Verbatim records for all six
words (`default, acceptEdits, plan, auto, bypassPermissions, dontAsk`) live
in `crates/remuda-driver/tests/fixtures/permission-21273/`.

`/plan` is the one slash command that changes mode at runtime (there is no
`/permissions` mode setter): its stdout verdict is `Enabled plan mode` and
the following `permission-mode` record reads `plan`; the tailer and mapper
attribute that edge to the terminal (`source: slash`).

## 4. The implemented push-down

* Both PTY carriers (`claude-pty` via Herdr, `shell-pty`) walk the wheel
  closed-loop: send `ESC [ Z`, read the status line each poll, stop the
  moment the requested phrase paints **stably** (two reads 80 ms apart), up
  to 6 presses. It never assumes a start mode — that is what makes a
  mid-turn cycle and an unknown wheel layout safe.
* The bypass step detects the active disclaimer and sends Down then Enter,
  never a bare CR; on failure it Esc-cancels and journals
  `permission-degraded:<mode>:bypass-dialog-kept`.
* Queue-until-idle like the effort worker: a switch while a turn runs
  journals `permission-queued:<mode>` and the worker types it at the next
  idle. Launch-only targets return `CapabilityUnsupported` before any
  keystroke.
* Read-back is dual: the TUI status line settles the closed loop, and the
  transcript `permission-mode` record drives the `permission` observation
  (`source launch | slash | remuda | unknown`). A hand-typed shift+tab /
  `/plan` folds into the chip locally and never calls configure back (no
  ping-pong). The Hub projects `permissionEffective` onto the instance spec.

Journal lifecycles: `permission-applied | permission-queued |
permission-degraded:<mode>:<reason> | permission-unsupported-in-session:<mode>`.

## 5. Non-Claude harnesses — their own sets, never Claude's list

The picker used to force every non-Claude kind to `bypassPermissions`. Each
harness now shows its native vocabulary; runtime switching today is the
Claude wheel, the others render read-only:

* **Codex** (measured `codex --help`, installed CLI): two axes — approval
  policy `-a/--ask-for-approval <untrusted|on-request|never>` and sandbox
  `-s/--sandbox <read-only|workspace-write|danger-full-access>`; the vendor
  yolo flag is `--dangerously-bypass-approvals-and-sandbox`.
* **Grok**: `native-prompt | auto | always-approve`
  (`--always-approve`), per `docs/design/protocol.md`.
* **agy**: `native | accept-edits | plan | always-proceed` (`--yolo`).

The Node builds the typed `PermissionMode` union from the create request
(plus `sandbox` for Codex) and the materializer emits the real argv.

## 6. Evidence and tests

* Driver unit tests: wheel transition table, status-indicator parser,
  active-modal detection (scrollback after accept is not a modal), mock-I/O
  closed-loop walks incl. gated Down/Enter, launch-only refusal, degrade.
* `crates/remuda-driver/tests/permission_transcript.rs` (feature
  `test-stub`) replays the verbatim 2.1.273 fixtures through the real
  mapper: every mode, `/plan` slash attribution, launch attribution, remuda
  correlation settling the bridge.
* Journal tailer and Hub projection tests; materializer argv tests for the
  Codex/Grok/agy flags.
* `web/tests/e2e/ux-permmode.hub.spec.ts` against the fake Node: six-mode
  menu with dontAsk greyed, a live switch posting `permissionMode` and
  settling the chip, a terminal-side fold with zero configure calls, and
  queued → degraded lifecycle handling.
* Screenshots (REMUDA_EVIDENCE=1):
  `permission-modes-1-menu-1440.png`,
  `permission-modes-1-chip-auto-1440.png`.
