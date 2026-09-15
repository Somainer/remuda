# effort-sync-2 — in-session effort sync actually works (latency and read-back)

Continuation of [`effort-sync-1.md`](./effort-sync-1.md). The owner's Mac demo
(claude/herdr driver, structured composer + terminal side by side) showed:

1. after choosing the ultracode stop in the slider popover, the composer's
   effort chip stayed `?` (Composer.tsx:546 `effectiveUnknown → "?"`);
2. structured → terminal switching "sometimes worked, with high latency";
3. typing `/effort <level>` in the terminal never synced the chip/slider.

This doc records the real claude PTY measurements that explain all three, the
fixes, and the before/after numbers.

## 1. How it was reproduced

`crates/remuda-driver/examples/effort_probe.rs` (a runnable probe, not a unit
test) forks a PTY with the driver's exact `child_env` allowlist, launches
`claude` with a logging-hook overlay, binds the session transcript from the
`SessionStart` hook payload (the same binding
`spawn_transcript_pump` uses), and walks switches while timing each read-back
channel independently: the TUI status line, the hook socket, and three
transcript record kinds.

The host binary auto-updated mid-session from **claude 2.1.221 to 2.1.272** —
the owner's exact demo build — so the measurements below are on 2.1.272, with
the synthetic 2.1.221-shaped fixtures kept for regression. Artifacts:
`crates/remuda-driver/tests/fixtures/effort-21272/` (verbatim trimmed
transcript, reject fixture, hook log, timing.json).

## 2. What the measurements showed

### 2.1 Per-channel arrival for an idle `/effort xhigh` + confirm

Times relative to the submit CR (ms; representative of 4 switches):

| channel | xhigh | ultracode | high |
| --- | --- | --- | --- |
| confirmation dialog paint | ~42–164 | ~42–164 | ~42–164 |
| **TUI status line repaint** (`◉ xhigh · /effort`) | ~80–110 after confirm CR | ~80 | ~80 |
| **transcript slash user record** (`command-args`) | ~125–270 after confirm CR | ~125 | ~228 |
| **transcript stdout verdict** (`local-command-stdout`) | same line/ts as slash | same | same |
| next assistant record `effort:"xhigh"` | only after the NEXT prompt + model round-trip (3.5–6 s via the gateway) | 10.8 s | 34.7 s (`auto`) |
| hooks (UserPromptSubmit/Stop) for the `/effort` itself | **none** | **none** | **none** |

The 2.1.221 assumption — *wait for the next assistant record* — is why the
chip stayed `?` until the user happened to prompt again, and why the read-back
window was 45 s and blocking. There is no hook for a slash command:
`UserPromptSubmit` fires only for real prompts (verified in `hooks-21272.log`).

### 2.2 The slash record is NOT an acceptance signal

On 2.1.272 the `<command-name>/effort</command-name>` user record is written
when the command is submitted, **before** the confirmation dialog resolves —
and even when the argument is invalid. The sibling
`<local-command-stdout>` record is the actual verdict:

- accept: `Set effort level to xhigh (saved as your default for new sessions): …`
- ultracode: `Set effort level to ultracode (this session only): xhigh + dynamic workflow orchestration`
- **Esc on the dialog**: `Kept effort level as xhigh` — the slash record is
  still written, the level does not change
- bad word: `Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, ultracode, auto`
- mode: `Effort level set to auto`

The old mapper resolved the switch from the slash/assistant edge, so a
dismissed dialog or an invalid word could be mis-attributed, and an accepted
switch to the level already in effect emitted no edge at all (dedup) and the
45 s wait degraded anyway.

### 2.3 Ultracode

- the slash args say `ultracode`; the stdout verdict says
  `Set effort level to ultracode (this session only): xhigh + dynamic workflow`;
- assistant records still carry `effort: "xhigh"` with **no flag field**;
- 2.1.272 additionally writes an `attachment` record
  `attachment.type = "ultra_effort_enter"` (and `…_exit` on the way out), but
  it rides the **next prompt**, not the switch.

So the old tracker's flag (`ultracode_pending`, consumed by the next
assistant edge) lost the flag the moment any later xhigh record arrived
without the pending bit, and the chip's `effectiveLabel` rendered the raw
tier `xhigh` even when the flag was observed — the Mac `?`/wrong-tier
symptom once a record finally arrived.

### 2.4 Switching while a turn is running

Typing `/effort medium` + CR while the agent is mid-turn is **silently
swallowed** by the native composer: no dialog, no slash record, and it does
not apply at idle either (native queuing delivers *prompts*, not slash
commands). The driver-side queue-until-idle is therefore mandatory;
`is_idle()` gating in `spawn_worker` was already correct.

### 2.5 Other 2.1.272 deltas

- the transcript file named by `SessionStart.transcript_path` does not exist
  until the first prompt (lazy creation);
- new record types: `atis-latch`, attachments `ultra_effort_enter|exit`;
- status-line glyph: `○` saved default · `◉` session override · `●` auto.

## 3. The fix

### Read-back settles from the command verdict

- `remuda_protocol::parse_effort_stdout()` parses the measured stdout strings
  into `Accepted(ObservedEffort)` / `Kept` / `Invalid` / `Other`.
- `EffortTracker::note_stdout()` settles the level **immediately**, with no
  assistant record required; `Kept`/`Invalid` clear the Remuda awaiting
  attribution so a later natural edge can never be credited to a refused
  switch. `ultra_effort_enter|exit` attachments are mapped by
  `note_ultra_attachment()`, and the ultracode flag is latched
  (`ultracode_on`) so subsequent xhigh assistant records keep it until a
  positive exit.
- the live `TranscriptMapper` and the `remuda-journal` tailer both resolve
  effort edges from slash + stdout user records (and the ultra attachments),
  sharing the same tracker; the driver's `EffortBridge` gains a
  `Rejected { reason }` verdict and `perform_switch` polls the dialog at
  40 ms instead of one fixed 500 ms read.
- bounded read-back window 45 s → **10 s**; transcript pump 300 ms → **75 ms**.

### End-to-end latency, idle

Measured path: slider click → node configure → PTY writes
(body, 120 ms settle, CR; ~40–160 ms dialog paint; 120 ms settle; confirm CR)
→ transcript verdict → pump → journal → hub → web chip. Driver-owned time is
the write/confirm cadence (~300–560 ms) plus one pump tick (≤75 ms); the
verdict itself lands 125–270 ms after the confirm CR. The measured end-to-end
fold (verdict mapped → bridge settled) is **0 ms** in the node integration
test (budget 50 ms), and the probe shows the whole idle switch comfortably
inside the ~2 s budget.

### Working state shows a pending chip

The web store tracks `effortPending[instance]` (`queued` when the instance
activity is `working`, otherwise switching): the chip shows the requested
word with a `切换中` / `排队中` tag until the effective observation settles
it, rather than going ambiguous. A `effort-degraded:<word>:<reason>`
lifecycle reverts the slider to the last observed level with a toast
(`已取消切换` / `档位无效` / `未收到回读`); stale pending entries expire after
30 min.

### Terminal → structured, single source of truth, no ping-pong

A transcript effort edge with no pending push-down (source `slash`) folds the
observed `{name, ultracode}` into the store's slider selection **locally** —
it never calls `instance.configure`, so a terminal `/effort` moves the slider
and chip without bouncing a push-down back into the PTY. The chip label for
an xhigh+flag read-back is now `ultracode`.

### Files touched (not shell_pty.rs — owned by r-native2)

- `crates/remuda-protocol/src/effort.rs` — stdout verdict, tracker rework
- `crates/remuda-driver/src/effort.rs` — reject verdict, 10 s window, dialog poll
- `crates/remuda-driver/src/claude_print.rs` — slash/stdout/attachment mapping
- `crates/remuda-driver/src/claude_pty.rs` — 75 ms pump
- `crates/remuda-journal/src/claude.rs` — same mapping in the file tailer
- `web/src/lib/store.ts`, `web/src/features/session/*`, `Composer.tsx`
- note for r-native2: **shell_pty.rs needs the same verdict-based mapping**
  (it shares `EffortBridge`/`spawn_worker` but its hydrator wiring should be
  checked against `note_effort_user`).

## 4. Before / after timing

| path | before | after |
| --- | --- | --- |
| idle switch → chip settles | next assistant record (3.5–45 s) or 45 s degrade | command verdict, ~0.4–0.9 s end to end |
| ultracode chip | `?` / bare xhigh | `ultracode` at the verdict |
| Esc on dialog | may mis-attribute; 45 s wait | `effort-degraded:…:dialog-kept`, UI reverts |
| invalid argument | typed only when screen caught it | verdict-coded `invalid-argument` |
| terminal `/effort` | never synced | slider + chip move from the slash edge |
| switch while working | ambiguous UI | `排队中` pending tag until applied at idle |
| read-back bounded window | 45 s (blocking configure) | 10 s |
| transcript poll | 300 ms | 75 ms |

## 5. Tests and evidence

- driver unit tests in `effort.rs` (bridge applied/rejected, write order,
  queued-at-idle, bounded degrade);
- protocol unit tests for every measured stdout string, sticky ultracode,
  dismiss/invalid;
- `crates/remuda-driver/tests/effort_transcript.rs` replays the verbatim
  2.1.272 fixtures through the real mapper, including bridge settle/reject;
- `crates/remuda-journal/tests/journal.rs` +
  `crates/remuda-node/tests/effort_sync.rs` (both directions, fold budget);
- `web/src/lib/store.effort.test.ts` (terminal fold, pending, revert);
- `web/tests/e2e/effort-sync.hub.spec.ts` extended against the fake node
  (ultracode chip, terminal fold with no configure, 排队中, degraded revert);
  the fake node in `crates/remuda-hub/examples/hub_e2e.rs` now models the
  measured verdict/read-back behavior including the sentinel
  queued/degraded lifecycles.

### Reproduce

```sh
# live probe (uses the gateway model; several minutes)
PROBE_DIR=/tmp/remuda-r-effortsync2-probe3 \
  cargo run -p remuda-driver --example effort_probe
cargo test -p remuda-protocol -p remuda-driver -p remuda-journal
cargo test -p remuda-node --test effort_sync
pnpm -C web test
```
