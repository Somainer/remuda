# Grok native-session spike fixtures

Source: installed `grok 1.0.30 (04b7ffed98c6)`, SHA-256
`d53b6e543e482716236748914331db50145c696ac7af91f1ebdedcf5654cfecb`.
Captured 2026-09-14 local time (2026-09-13 UTC). See
[the evidence](../../../../../docs/design/evidence/grok-signals-1.md) for exact
commands, results, failure boundaries, and source-only corroboration.
The client is real; an independently written loopback Chat Completions server
supplied deterministic text, reasoning, and tools. There was no hosted-model
inference. Its only API-key value was an invalid local placeholder. No real
account credentials were copied into this apparatus or these fixtures.

## Captures

- `tui-updates.jsonl`: all 54 native frames of recorded session
  `01a09c24-46ef-7a03-9c89-88f1bc00bd0c`, including startup/shutdown hooks. Both
  `session/update` and `_x.ai/session/update` are retained in original order.
- `tui-events.jsonl`: all 74 native events from that same session. Seven turns,
  five completed and two cancelled. No missing event is invented.
- `tui-chat-history.jsonl`: all 27 native history records after that run, with
  injected instruction/context text redacted. User prompts, tool invocations,
  tool results, reasoning, and responses remain intact.
- `tui-summary.json`: final native summary, including model and turn counters.
- `active-sessions.json`: a live registry snapshot before that TUI exited. PID
  is the observed historical process ID, not a currently running process.
- `hook-payloads.jsonl`: one real stdin sample for each of SessionStart,
  UserPromptSubmit, PreToolUse, PostToolUse, Stop, Notification. First five are
  from separate headless probes: SessionStart/UserPromptSubmit/Stop are from
  the initial schema-failure run, while PreToolUse/PostToolUse are from the
  corrected successful tool run. Notification is from the
  earlier permission probe session `01a09c22-f3fd-7922-9412-ee8053e7804a`.
- `hook-deny-{updates,events}.jsonl`: complete real denial-session files, proving
  PreToolUse denial under `--always-approve`. The hook annotation and terminal
  outcome are intentionally retained as unknown parser variants.
- `*.ansi`: exact styled rendered snapshots from
  `herdr pane read w5:p2F --source visible --lines N --format ansi`.
  Names identify working, typed/queued input, Esc, first/second Ctrl+C, permission,
  question, idle, and send-now states. These are terminal snapshots, not GUI
  screenshots or original terminal output bytes. The permission snapshot is
  width-clipped after `Esc:`; it cannot establish that key's permission behavior.
- `osc-samples.jsonl`: the first occurrence of each distinct OSC 0 / OSC 9;4
  sequence from the raw nested PTY capture, preserving original byte offsets.
  JSON escapes round-trip the ESC/BEL bytes. Repeated identical sequences and
  non-OSC output are omitted. Offsets refer to the unmodified temporary raw
  recording, not the committed rendered snapshots.

## Scrubbing

Primary native cwd becomes `/workspace/grok-spike`, its encoded folder becomes
`%2Fworkspace%2Fgrok-spike`, and the shadow native home becomes `/home/dev/.grok`.
Any personal home prefix becomes `/home/dev`. Hook sub-probe files use
`/fixture/work` and `/fixture/grok-home` placeholders; native session IDs keep
these separate sessions distinguishable from the primary run.
System and synthetic reminder text in history is replaced with explicit
redaction markers, preserving record order and content container shape.
No timestamps, session/prompt/tool IDs, counters, outcomes, or native metadata
are fabricated. The local model's usage numbers are not billing evidence.
Fixtures contain no injected invalid JSON: synthetic malformed, sparse,
future-event, UTF-8, truncation, and discovery cases live in the Rust tests.

## Reproduction apparatus

`probe-server.py`, `probe-hook.py`, `probe-keys.py`, `probe-record.py`, and
`probe-child.py` were independently written for this spike; no Grok code was
lifted. They are manual research apparatus, not tests run by Cargo. They write
only their fixed `/tmp/remuda-grokspike/tui/` tree. `probe-server.py` additionally
allows a missing `port` file on first launch (the final live server reused the
initial server's port). It never logs HTTP headers.

Copy them to the temporary filenames used during capture:

```sh
mkdir -p /tmp/remuda-grokspike/tui/home/hooks /tmp/remuda-grokspike/tui/work
cp crates/remuda-driver/tests/fixtures/grok/probe-server.py /tmp/remuda-grokspike/tui/server.py
cp crates/remuda-driver/tests/fixtures/grok/probe-hook.py /tmp/remuda-grokspike/tui/capture-hook.py
cp crates/remuda-driver/tests/fixtures/grok/probe-keys.py /tmp/remuda-grokspike/tui/keys.py
cp crates/remuda-driver/tests/fixtures/grok/probe-record.py /tmp/remuda-grokspike/tui/record.py
cp crates/remuda-driver/tests/fixtures/grok/probe-child.py /tmp/remuda-grokspike/tui/child.py
python3 /tmp/remuda-grokspike/tui/server.py
```

Set up the shadow config/hooks and `launch.sh` as shown in the evidence document.
The recorder sets its child PTY to 54 rows by 105 columns before exec; its parent
PTY stays in the owned Herdr pane. After a native quit, this simple recorder may
remain waiting on its PTY even after Grok exits. Inspect owned process IDs and
interrupt that recorder explicitly, then verify Grok/recorder are gone before
closing the owned pane. This research helper is not a production carrier.

The separate headless hook probe used `probe-headless-{server,hook,launch}.py`.
The launcher is preserved with the personal executable path replaced by the
required `GROK_BIN` environment variable. It constructs an allowlisted child
environment, disables vendor compatibility activation, and uses the same local
model configuration with its independently allocated port. To reconstruct:

```sh
mkdir -p /tmp/remuda-grokspike/hooks/home/hooks /tmp/remuda-grokspike/hooks/work
cp crates/remuda-driver/tests/fixtures/grok/probe-headless-server.py /tmp/remuda-grokspike/hooks/server.py
cp crates/remuda-driver/tests/fixtures/grok/probe-headless-hook.py /tmp/remuda-grokspike/hooks/capture.py
cp crates/remuda-driver/tests/fixtures/grok/probe-headless-launch.py /tmp/remuda-grokspike/hooks/launch.py
python3 /tmp/remuda-grokspike/hooks/server.py
```

Point the shadow home's `config.toml` model endpoint at the port written by this
server. Register command hooks in `home/hooks/probe.json` as in the evidence,
using `python3 /tmp/remuda-grokspike/hooks/capture.py grok-<EventName>`.
Run `launch.py --always-approve --output-format streaming-json -p TOOL` for
the baseline, then create `/tmp/remuda-grokspike/hooks/deny` and repeat for the
denial. The initial missing-description tool failure is historical evidence;
the committed helper contains the corrected successful command schema.
