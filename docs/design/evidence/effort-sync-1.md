# effort-sync-1 — in-session effort, both directions (D-028 §9.1)

Live measurements behind implementing `switch.effort` on the native shell/PTY
claude driver in the worktree `wt/r-effortsync`, **2026-09-14/15**, on
**claude 2.1.221** (host binary on the dev box). The owner's demo bug was on
claude 2.1.270; 2.1.221 is what this machine has, and every shape below is
also asserted against the checked-in 2.1.270-shaped fixtures.

## 1. The two defects (live TUI, `claude` under a PTY)

1. Moving the effort slider sent `instance.configure`; on `claude-pty` the
   driver returned `CapabilityUnsupported("claude-pty has no runtime
   model/effort command …")` and dropped the switch.
2. Nothing read the level back. The slider was the only source of truth: a
   clamp, an org cap, or a user typing `/effort` in the terminal could never
   change what Remuda displayed.

## 2. In-session `/effort` vocabulary — measured

Typed by hand into a `claude` TUI (`pty.fork`, stdin/stdout captured,
transcript read from
`~/.claude/projects/-tmp-remuda-r-effortsync-probe*/<session>.jsonl`).

Submitting `/effort bogus` shows and journaled:

```
Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, ultracode, auto
```

So the **in-session vocabulary is `low | medium | high | xhigh | max |
ultracode | auto`** — wider than the launch `--effort` help (which lists the
five levels; `ultracode` is documented but undocumented-in-help; `auto` is an
in-session mode rather than a launch value).

### Every switch opens a confirmation dialog

With a cached conversation, after typing `/effort xhigh` + Enter the TUI
opens (captured viewport):

```
Change effort level?
Your nxt response will be slower and use more tokens …
This conversation is cached for the current effort level. Switching to
xhigh means the full history gets re-read on your next message.
❯ 1. Yes, switch to xhigh
  2. No, go back
```

It needs **a second Enter**; one Enter submits the command and leaves the
question on screen. The driver therefore writes the body, then CR
(§5.2 two-segment write), waits briefly, reads the viewport, and sends one
more CR only when the dialog text is present. `Esc` cancels, so the extra CR
is gated on the actual screen text rather than sent unconditionally.

`/effort ultracode` is accepted in-session too:

```
Set effort level to ultracode (this session only): xhigh + dynamic workflow
```

A switch back shows the same shape (`Set effort level: high (saved as your
default for new sessions)`).

## 3. Transcript record shapes — measured

A probe sequence `/effort high → prompt → /effort xhigh → prompt →
/effort max (confirm) → prompt → /effort low → prompt` produced (excerpt):

```json
{"type":"user",
 "message":{"role":"user","content":
   "<command-name>/effort</command-name>\n            <command-args>xhigh</command-args>"}}
…
{"type":"user","message":{"content":"Set effort level to xhigh (saved as your default …)"}}
…
{"type":"assistant","effort":"high","perTurnEffort":null,
 "message":{…"stop_reason":"end_turn"…}}
```

Findings the implementation relies on:

- assistant records carry a **top-level `effort` string** with one of the five
  level names, plus `perTurnEffort: string | null`;
- the slash command and its stdout land as **two `user` records sharing one
  `promptId`** — the command markup (`<command-name>/effort</command-name>`,
  `<command-args>…</command-args>`) and a `<local-command-stdout>…</local-
  command-stdout>` line; an invalid argument is journaled as
  `<local-command-stdout>Invalid argument: …</local-command-stdout>`;
- **`ultracode` reads back as `effort:"xhigh"`**; the workflow flag is not on
  the assistant record, so the mapper only reports `ultracode: true` after a
  positively observed `/effort ultracode` slash record;
- `/effort auto` sets a mode ("Effort level set to auto"), not a level — it is
  deliberately not mapped to an effective tier.

## 4. Print mode (`claude -p`) does not support a runtime effort switch

Measured against `claude -p --output-format stream-json --input-format
stream-json`:

- a control request `{"type":"control","subtype":"set_settings",
  "settings":{"effortLevel":"xhigh"}}` is **silently ignored** — no
  `control_response`, and the next assistant frame does not change;
- assistant frames emit `effort: null` / `perTurnEffort: null` even when
  launched `--effort xhigh`.

There is no `set_settings` variant in the stream-json control enum
(`remuda-claude-wire`), and probing confirms it is not a hidden channel. The
`claude-print` carrier therefore returns an honest
`CapabilityUnsupported("claude-print cannot switch effort in-session: relaunch
with --effort …")` instead of silently accepting. Effort on print is launch-
time only (`--effort`, §12 keeps `settings.json` out of the path).

## 5. What the implementation does

- `claude-pty` and shell-pty (both a Remuda-launched agent and a promoted
  hand-typed one) handle an effort-bearing `ModelSwitchInput` by typing
  `/effort <level>` through the composer, accepting the confirmation dialog,
  and **waiting for the transcript read-back** (`EffortBridge`, 45 s bounded
  window). A switch while the agent is working is journaled `effort-queued`
  and applied at the next idle; a missing read-back journals
  `effort-degraded:<level>:no-readback-within-window` at `warning` and never
  reports applied.
- Both transcript mappers (the live `TranscriptMapper` and
  `remuda-journal::ClaudeJsonlTailer`, now sharing one
  `remuda_protocol::EffortTracker`) emit an `effort` observation on level
  edges, deduped, with source `launch | slash | remuda | unknown`.
- The Hub projects the payload onto the instance record as
  `effortEffective {name, ultracode, source, observedAt}` without touching the
  requested `effort`. The web chip and session row render the **effective**
  level, `?` greyed until first read-back, and `请求 max → 实际 xhigh` on a
  mismatch; they never fall back to the requested value.
- `CLAUDE_CODE_EFFORT_LEVEL` and `CLAUDE_CODE_CHILD_SESSION` are stripped from
  every child env (the first pins the level, the second disables transcript
  saving — both defeat §9.1).

## 6. Tests

- driver unit: `/effort` body + CR + confirmation-CR write ordering, queued-
  while-working → applied-at-idle, bounded-window degrade, tracker edges;
- `tests/effort_transcript.rs`: slash detection, ultracode→xhigh, per-turn
  fallback, dedupe;
- `remuda-journal`: effort observations from a synthetic transcript;
- hub store: requested `max` survives while effective projects `xhigh/slash`;
- web: 599 unit tests, including the effective/`?`/mismatch chip;
- hub Playwright (`effort-sync.hub.spec.ts`): fake node accepts the slider
  configure and echoes a clamped read-back; the chip shows `?`, then `xhigh`,
  then the `请求 max → 实际 xhigh` mismatch.

## Environment notes

- Probes ran with `CLAUDE_CODE_*` stripped from the env: an inherited
  `CLAUDE_CODE_CHILD_SESSION` marker (present when Remuda itself runs inside a
  Claude child session) turns transcript saving **off**, which initially
  produced no JSONL at all. That leak is now denied in `child_env.rs`.
- One live confirmation flow (probe2) accepted `Yes, switch` from an
  apparently idle composer: the dialog is driven whenever its text is
  observed; no assumption is made about when Claude chooses to show it.
