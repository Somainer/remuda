# effort-sync-4 — ultracode is its own toggle (claude 2.1.289)

Continuation of [`effort-sync-2.md`](./effort-sync-2.md) and
[`effort-sync-3.md`](./effort-sync-3.md), which measured the **coupled** model
(claude ≤ 2.1.283), where `ultracode` was an effort word meaning xhigh plus
dynamic workflows. Since 2.1.284 ultracode is a separate session toggle. This
note re-measures every `/effort` form and the launch channels on a real
**claude 2.1.289** and is the reference later effort tasks test against. Every
string below is copied from the recorded transcript or screen. None is
paraphrased.

## 1. Setup

| item | value |
| --- | --- |
| CLI | `claude --version` → `2.1.289 (Claude Code)`; every assistant record carries `"version":"2.1.289"` (≥ 2.1.284) |
| model | `claude-opus-5-5`; launch case g uses `claude-sonnet-4-6` |
| date | 2026-10-05, one claude session at a time |
| probe | `crates/remuda-driver/examples/effort_probe.rs`, `SCENARIO=walk\|launch` (code in `examples/effort_probe/decoupled.rs`) |
| terminal | native PTY, 120×40, `TERM=xterm-256color`; screens rendered by the `remuda-screen` vt100 emulator (ANSI-free grid) |
| config | a throwaway `CLAUDE_CONFIG_DIR` per session, seeded with the onboarding flag, folder trust for the cwd and logging hooks, so "saved as your default" lands there and nothing from the operator's own user config loads |
| prompt between steps | `reply with exactly the two characters: ok` |
| latency | submit CR → the first 10 ms transcript poll that sees the `<local-command-stdout>` record |
| fixtures | `crates/remuda-driver/tests/fixtures/effort-21289/` (transcripts + `.txt` screen logs), transcripts mirrored in `crates/remuda-journal/tests/fixtures/effort-21289/`; sequences and scrub in `SOURCES.md` |

**Process ledger (`PROBE_DIR/pids.txt`).**

- **Recorded runs** (probe as of `43352377`/`650cbc61`): the ledger held the
  probe's own pid and each claude session's pid, at spawn and with its exit
  status. Every session ended through `/exit` with status 0, so the kill path
  never ran.
- **What that ledger missed:** the short helpers that probe started directly
  and ran to completion: one `claude --version` per run, and four
  `hostname` / `scutil` lookups per session while scrubbing.
- **The probe now:** every process it starts directly goes into the ledger.
  Helpers go through one spawn/log/wait function. Each claude session is
  logged at spawn and again at confirmed termination: it must be reaped and
  its process group drained before the transcript is copied. A forced
  shutdown logs the kill result, and if termination cannot be confirmed the
  matrix aborts. Processes claude starts itself, such as its hook scripts,
  are not in the ledger.
- The recordings were not redone for this change.

## 2. Version boundary

The official changelog (`CHANGELOG.md` of `anthropics/claude-code`, read from
the copy the CLI caches locally) lists under **2.1.284**:

> - Changed Ultracode into its own toggle in `/effort` (Tab, or `/effort ultracode [on|off]`): it no longer forces xhigh effort and stays on at any effort level
> - Added `effortSlider:decreaseEffort`, `increaseEffort` and `toggleUltracode` keybinding actions, so the `/effort` slider's arrow and Tab keys can be rebound in `keybindings.json`
> - [VSCode] Added an Ultracode on/off switch under the Effort slider, replacing the slider's Ultracode stop; the model pill shows "· Ultracode" at any effort level

The changelog has no per-version dates. The 2026-09-28 release date comes from
the task brief and was not re-checked here.

**Coupled baseline.** The 2.1.272 and 2.1.273 recordings
(`fixtures/effort-21272/`, `fixtures/effort-21273/`, effort-sync-2/3) remain
the coupled-model baseline for claude ≤ 2.1.283 and were not modified by this
task. The tests that replay them, such as
`ultracode_slash_reads_back_as_xhigh_with_the_flag`, describe ≤ 2.1.283
behaviour only. A 2.1.277 build, like the one on the remote dev host, is in
that range. It was not measured here.

## 3. The walk (`effort-walk-21289.jsonl`, launched with `--effort high`)

Each command produces a `<local-command-caveat>` user record, then the slash
record (`<command-name>/effort</command-name>` …
`<command-args>…</command-args>`), then the verdict record. The slash record
and the verdict landed in the same poll tick every time. "Attachment" lists
the effort-relevant records that rode the next prompt. "Next assistant" is
`effort` / `perTurnEffort` of the assistant record that answered it (`version`
was `2.1.289` on all). The footer is the status-line indicator after that turn.

| # | command | verbatim verdict (`<local-command-stdout>` body) | dialog | CR→verdict | attachment on next prompt | next assistant | footer | status |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | *(launch, baseline prompt)* | — | — | — | — | `high` / `high` | `● high` | measured |
| 1 | `/effort ultracode` | Ultracode on (this session only): dynamic workflows on every task. Effort stays high. | no | 123 ms | `ultra_effort_enter` `{"reminderType":"full"}` + the bundled workflow-authoring skill expansion (§6) | `high` / `high` | `● high · ultracode` | measured |
| 2 | `/effort high` | Set effort level to high (saved as your default for new sessions): Comprehensive implementation with extensive testing and documentation | no | 146 ms | — | `high` / `high` | `● high · ultracode` | measured |
| 3 | `/effort max` | Set effort level to max (this session only): Maximum capability with deepest reasoning. May use excessive tokens resulting in long response times or overthinking. Use sparingly for the hardest tasks. | no | 112 ms | — | `max` / `max` | `◈ max · ultracode` | measured |
| 4 | `/effort ultracode off` | Ultracode off. Effort stays max. | no | 115 ms | `ultra_effort_exit` | `max` / `max` | `◈ max` | measured |
| 5 | `/effort ultracode on` | Ultracode on (this session only): dynamic workflows on every task. Effort stays max. | no | 124 ms | `ultra_effort_enter` `{"reminderType":"full"}` | `max` / `max` | `◈ max · ultracode` | measured |
| 6 | `/effort xhigh` | Set effort level to xhigh (saved as your default for new sessions): Deeper reasoning than high, just below maximum (on supported models) | no | 136 ms | — | `xhigh` / `xhigh` | `◉ xhigh · ultracode` | measured |
| 7 | `/effort auto` | Effort level set to auto | no | 139 ms | — | `medium` / `medium` | `◐ medium · ultracode` | measured |
| 8 | `/effort status` | Effort level: auto (currently medium) · Ultracode on | no | 112 ms | — | `medium` / `medium` | `◐ medium · ultracode` | measured |
| 9 | `/effort bogus` | Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, auto, ultracode [on\|off] | no | 113 ms | `ultra_effort_enter` `{"reminderType":"sparse"}` (a re-reminder while ultracode stays on; no state change) | `medium` / `medium` | `◐ medium · ultracode` | measured |
| 10 | `/effort ultracode bogus` | Invalid argument: ultracode bogus. Valid options are: low, medium, high, xhigh, max, auto, ultracode [on\|off] | no | 112 ms | — | `medium` / `medium` | `◐ medium · ultracode` | measured |
| 11 | bare `/effort` → Right → Tab → Enter | Set effort level to high (saved as your default for new sessions): Comprehensive implementation with extensive testing and documentation · Ultracode off | no | 134 ms (from the Enter) | `ultra_effort_exit` | `high` / `high` | `● high` | measured |
| 12 | `/model claude-opus-5-5` | ``Set model to `Opus 5.5` and saved as your default for new sessions`` | no | 1173 ms | — | `high` / `high` | `● high` | measured |

The slider (step 11) as rendered. It opened on the resolved level, medium
under auto. Right moved it to high, and Tab flipped the separate switch:

```text
  Effort

                             Faster                             Smarter
                             ──────────▲───────────────────────────────      Ultracode  on
                             low     medium     high     xhigh      max      Tab to toggle
                             Ultracode: dynamic workflows on every task

  ←/→ to adjust · Enter to confirm · s for this session only · Esc to cancel
```

after Right, then after Tab:

```text
                             ────────────────────▲─────────────────────      Ultracode  on
                             low     medium     high     xhigh      max      Tab to toggle
                             Ultracode: dynamic workflows on every task
…
                             ────────────────────▲─────────────────────      Ultracode  off
                             low     medium     high     xhigh      max      Tab to toggle
```

The slider has five stops (low…max) and no auto stop. Enter applied the level
and the toggle together, in one verdict.

## 4. Launch matrix

Every case runs `--model claude-opus-5-5` unless noted, then the listed
command(s), one prompt, and `/effort status`. The "first prompt" column
shows the effort-relevant attachment on the first prompt.

| case | launch flags | command → verbatim verdict | next assistant | first prompt | footer | status |
| --- | --- | --- | --- | --- | --- | --- |
| a | `--effort high --settings '{"ultracode":true}'` | `/effort status` → Current effort level: high (Comprehensive implementation with extensive testing and documentation) · Ultracode on | `high` / `high` | `ultra_effort_enter` full + skill expansion | `● high · ultracode` | measured |
| b | `--effort ultracode` | `/effort status` → Current effort level: xhigh (Deeper reasoning than high, just below maximum (on supported models)) · Ultracode on | `xhigh` / `xhigh` | `ultra_effort_enter` full + skill expansion | `◉ xhigh · ultracode` | measured |
| c1 | `--settings <file> --settings '{"ultracode":true}'` | `/effort status` → Effort level: auto (currently medium) · Ultracode on | `medium` / `medium` | `ultra_effort_enter` full + skill expansion | `◐ medium · ultracode` | measured |
| c2 | `--settings '{"ultracode":true}' --settings <file>` | `/effort status` → Current effort level: medium (Balanced approach with standard implementation and testing) | `medium` / `medium` | — | `◐ medium` | measured |
| d | `--settings '{"disableWorkflows":true}'` | `/effort ultracode` → Ultracode needs dynamic workflows enabled (see /config). Valid options are: low, medium, high, xhigh, max, auto; `/effort status` → Effort level: auto (currently medium) | `medium` / `medium` | — | `◐ medium` | measured |
| e | `--settings '{"maxEffortLevel":"high"}'` | `/effort max` → Effort 'max' exceeds the cap for claude-opus-5-5 set by your settings or organization; set to 'high' instead (this session only): Comprehensive implementation with extensive testing and documentation; `/effort status` → Current effort level: high (Comprehensive implementation with extensive testing and documentation) | `high` / `high` | — | `◐ medium` at boot, `● high` after | measured |
| f1 | `--effort high` | `/effort ultracode on` → Ultracode on (this session only): dynamic workflows on every task. Effort stays high.; `/effort status` → Current effort level: high (Comprehensive implementation with extensive testing and documentation) · Ultracode on | `high` / `high` | `ultra_effort_enter` full + skill expansion | `● high · ultracode` | measured |
| f2 | `--resume <f1 session id>` (new process, no `--effort`) | `/effort status` → Effort level: auto (currently medium) | `medium` / `medium` | `ultra_effort_exit` | `◐ medium` | measured |
| g | `--model claude-sonnet-4-6` | `/effort ultracode` → Ultracode isn't available on claude-sonnet-4-6. Valid options are: low, medium, high, xhigh, max, auto; `/effort xhigh` → Set effort level to xhigh (saved as your default for new sessions): Deeper reasoning than high, just below maximum (on supported models); `/effort status` → Current effort level: xhigh (Deeper reasoning than high, just below maximum (on supported models)) | `high` / `null` on every turn | — | `● high` throughout | measured |

`<file>` in c is a settings file holding `{"effortLevel":"medium"}` and a
SessionStart hook tagged `OverlayFileSessionStart`, so it shows on its own
whether it was applied. No case painted a confirmation dialog. CR→verdict was
112–171 ms for every launch-case command.

### (c) Do two `--settings` flags merge? **No: the last one wins whole.**

- c1 (file, then JSON): the file's `OverlayFileSessionStart` hook never fired
  (hooks seen: `SessionStart`, `UserPromptSubmit`, `Stop` only), and its
  `effortLevel` was not applied, because the status reads `auto (currently
  medium)`, the default, rather than `medium`. The JSON's ultracode was
  applied.
- c2 (JSON, then file): the marker hook fired and `effortLevel: "medium"`
  applied ("Current effort level: medium"), but the JSON's ultracode did not.
  There was no `ultra_effort_enter`, no "· Ultracode on" and no footer label.

So a launch that already passes a settings overlay must put `ultracode` inside
that same overlay. A second `--settings` flag silently drops the first.

### (f) Does ultracode come back on `--resume`? **No.**

After f1 turned ultracode on and exited, `--resume` of the same session (the
CLI appended to the same transcript file) came back with ultracode **off**:

- the status shows no "· Ultracode on";
- the first prompt after the resume carries `ultra_effort_exit`;
- the footer has no ultracode label.

The replayed history on screen still shows f1's "Ultracode on (this session
only)" line. The level also reverted to the default (`auto (currently
medium)`), since f1's `--effort high` was launch-scoped and f1 never saved a
level. "This session only" means this process, so a resumed session has to
assert ultracode again. Whether `--settings '{"ultracode":true}'` on a
`--resume` launch restores it was not measured.

### (g) Model without xhigh

`claude-sonnet-4-6` refuses ultracode, yet its refusal still lists `xhigh`. The
extra `/effort xhigh` step shows the CLI *accepts* xhigh there: the verdict and
`/effort status` both say xhigh. The turn still runs at `effort: "high"`
(`perTurnEffort: null`) with footer `● high`, so the model has no effective
xhigh. On such a model the verdict and the effective level disagree.

## 5. Bundle strings: confirmed or corrected

| # | string read from the 2.1.289 bundle | result |
| --- | --- | --- |
| 1 | `Ultracode on (this session only): dynamic workflows on every task. Effort stays <level>.` | **confirmed** (walk 1 and 5, f1) |
| 2 | `Ultracode off. Effort stays <level>.` | **confirmed** (walk 4) |
| 3 | `Set effort level to <level> (saved as your default for new sessions\|this session only): <desc>` | **confirmed**. high and xhigh say "saved as your default for new sessions", max says "this session only". A slider confirm that also flips the toggle appends ` · Ultracode off` (walk 11) |
| 4 | `Ultracode needs dynamic workflows enabled (see /config). Valid options are: …` | **confirmed** (d). The list is `low, medium, high, xhigh, max, auto`, without the ultracode form |
| 5 | `Ultracode isn't available on <model>. Valid options are: …` | **confirmed** (g). `<model>` is the raw id `claude-sonnet-4-6`, and the list still includes xhigh |
| 6 | `Invalid argument: <x>. Valid options are: low, medium, high, xhigh, max, auto, ultracode [on\|off]` | **confirmed** (walk 9 and 10). `<x>` is the whole argument string (`ultracode bogus`) |
| 7 | `Effort '<x>' exceeds the cap for <model> … set to '<y>' instead…` | **confirmed** (e), in full: `Effort 'max' exceeds the cap for claude-opus-5-5 set by your settings or organization; set to 'high' instead (this session only): Comprehensive implementation with extensive testing and documentation` |
| 8 | `Current effort level: <level> (<desc>) · Ultracode on` | **confirmed** (a, b, f1). The suffix is absent when ultracode is off (c2, e, g). Under auto the form differs: `Effort level: auto (currently <level>)`, plus ` · Ultracode on` when on (walk 8, c1, d, f2) |
| 9 | `/model` verdict ending ``with `<level>` effort`` | **not reproduced**. `/model claude-opus-5-5` at high effort gave ``Set model to `Opus 5.5` and saved as your default for new sessions``, with the marketing name and no effort suffix. A model change that also changes the effort may produce the suffix, but that was not measured |
| — | (not on the list) | `Effort level set to auto` (walk 7) |

## 6. What later effort tasks should take from this

- **No confirmation dialog.** None of the 26 commands across the 10
  recorded sessions painted "Change effort level?" or "Change model?". On
  2.1.272/2.1.273 a cached conversation always asked (effort-sync-2/3). The
  dialog handling must stay for older builds, but a switch must settle
  without one.
- **The verdict is fast and hook-less.** The slash record and verdict arrive
  together 112–171 ms after the CR for every `/effort` form, and 1173 ms for
  `/model`. No hook fires for a slash command: the walk's hook log has 13
  `UserPromptSubmit` for 13 prompts and none for its 13 commands.
- **Ultracode is not on the assistant record.** `effort` / `perTurnEffort`
  carry the level only. Ultracode shows in:
  - the verdict text;
  - `ultra_effort_enter` (`reminderType` `full` on entry, `sparse` on later
    turns) and `ultra_effort_exit` attachments on the next prompt. These are
    the same names as 2.1.272, but now independent of the level: walk 4 exits
    at max;
  - on the first ultracode turn, two `isMeta` + `turnCompanion` user records
    expanding the bundled workflow-authoring skill. Their content starts
    `<command-message>workflow-authoring</command-message>\n<command-name>workflow-authoring</command-name>`,
    a command-name record the user never typed;
  - the footer `· ultracode ·` and an `ultracode` label on the composer's
    top border.
- **Coupled assumptions that are now wrong.** `/effort ultracode` does not
  set xhigh (walk 1 stays high). A level change does not clear ultracode
  (walk 2, 3, 6, 7). `ultra_effort_exit` can arrive with the level unchanged
  (walk 4). `--effort ultracode` is still xhigh plus ultracode (b).
- **auto reads back as its resolved level.** The verdict is "Effort level set
  to auto", but the next assistant `effort` is `medium` and the footer shows
  `◐ medium`. Only `/effort status` says `auto (currently medium)`, so the
  assistant record cannot tell auto from an explicit medium.
- **perTurnEffort** equals `effort` on Opus 5.5 and is `null` on Sonnet 4.6.
- **The Stop hook can run before the assistant record is on disk.** In 6 of
  the walk's 13 prompts the record was not yet in the transcript when the Stop
  hook line appeared; it landed within the next 800 ms. A read-back must not
  treat Stop as "the record is written".
- **Footer glyphs:** `◐ medium`, `● high`, `◉ xhigh`, `◈ max`, with low not
  observed. A fresh `--effort high` launch's banner reads "Opus 5.5 with high
  effort". The resumed session's banner has no effort.
- **Saved defaults:** `/effort high|xhigh`, the slider's Enter and `/model`
  save a default for new sessions, while `/effort max` and the ultracode
  toggle are this-session-only.

## 7. Scrub proof

The probe scrubs its own output (rules in `SOURCES.md`) and panics if any
forbidden needle survives; every probe output passed. The repo's secret scan
then ran on the **reconstructed** content: every record decoded, every key and
string value on its own line, and streamed `partial_json` / text deltas joined
per content block before writing. Transcripts store complete records, so there
were no fragments to join, but the join is part of the step regardless.

The reconstruction script, verbatim, run from the repo root:

```python
"""Reconstruct fixture content for scanning (effort-sync-4 scrub proof).

Every JSONL record is decoded and every key and string value is written out on
its own line, so escapes are undone and nothing hides inside a JSON encoding.
Streamed fragments (`partial_json` / `text` deltas, consecutive per content
block) are joined before writing, so a value split across deltas is scanned
whole. Screen logs are copied as they are.
"""
import json
import os
import sys

src_dirs, out = sys.argv[1:-1], sys.argv[-1]
os.makedirs(out, exist_ok=True)


def strings(value, sink):
    if isinstance(value, str):
        sink.append(value)
    elif isinstance(value, list):
        for item in value:
            strings(item, sink)
    elif isinstance(value, dict):
        for key, item in value.items():
            sink.append(key)
            strings(item, sink)


for src in src_dirs:
    tag = os.path.basename(os.path.dirname(os.path.dirname(os.path.dirname(src.rstrip("/")))))
    for name in sorted(os.listdir(src)):
        path = os.path.join(src, name)
        target = os.path.join(out, f"{tag}-{name}.recon")
        if name.endswith(".txt"):
            with open(path, encoding="utf-8") as f, open(target, "w", encoding="utf-8") as w:
                w.write(f.read())
            continue
        if not name.endswith(".jsonl"):
            continue
        lines, partial = [], {}
        with open(path, encoding="utf-8") as f:
            for raw in f:
                record = json.loads(raw)
                event = record.get("event") or {}
                delta = event.get("delta") or record.get("delta") or {}
                index = event.get("index", record.get("index"))
                piece = delta.get("partial_json", delta.get("text"))
                if isinstance(piece, str):
                    partial[index] = partial.get(index, "") + piece
                    continue
                lines.extend(partial.values())
                partial.clear()
                strings(record, lines)
        lines.extend(partial.values())
        with open(target, "w", encoding="utf-8") as w:
            w.write("\n".join(lines) + "\n")
print("reconstructed", len(os.listdir(out)), "files into", out)
```

```console
$ python3 /tmp/remuda-c-effortev-work/reconstruct.py \
    crates/remuda-driver/tests/fixtures/effort-21289 \
    crates/remuda-journal/tests/fixtures/effort-21289 \
    /tmp/remuda-c-effortev-work/recon
reconstructed 28 files into /tmp/remuda-c-effortev-work/recon
$ python3 scripts/ci/secret-scan.py /tmp/remuda-c-effortev-work/recon \
    crates/remuda-driver/tests/fixtures/effort-21289 \
    crates/remuda-journal/tests/fixtures/effort-21289
secret-scan: pass
```

`scripts/ci/secret-scan.py` is what the gate's secret-scan step runs:
`gate.sh` calls `./scripts/ci/secret-scan.sh`, which `cd`s to the repo root
and `exec`s this script with the same arguments. It covers the secret patterns
and the hashed private-token denylist (`private-tokens.sha256`), which matches
host names, e-mail parts and home-path names. The commit hook's run of the
same scan also passed on every commit of this branch.

- **Positive control.** A scratch file holding a generated fake `sk-ant-api03-…`
  token makes the same command fail:
  `secret-scan: undeclared secrets` / `…/x.recon:1: sk- sk-…`.
- **Independent needle check** (counts only, no value ever printed). It ran
  over the fixture files and the reconstruction, 56 files, with needles taken
  from this machine:
  - home dir, user name, temp dir;
  - four host-name forms;
  - the gateway URL, its host and two of its labels;
  - the gateway token;
  - the 11 real session ids of the probe runs;
  - 52 gateway-side model ids from the operator's model cache.

  Result: `identity hits 0`; `/users/`, `/var/folders` and `/home/` appear 0
  times. All 144 `@` occurrences are `noreply@anthropic.com` attribution lines
  and the bundled skill listing's `` `@anthropic-ai` `` text.

## 8. Reproduce

```sh
REMUDA_PROBE_MODEL=claude-opus-5-5 SCENARIO=walk PROBE_DIR=/tmp/remuda-c-effortev-walk \
  cargo run -p remuda-driver --example effort_probe
REMUDA_PROBE_MODEL=claude-opus-5-5 SCENARIO=launch CASES=a,b,c1,c2,d \
  PROBE_DIR=/tmp/remuda-c-effortev-launch1 cargo run -p remuda-driver --example effort_probe
REMUDA_PROBE_MODEL=claude-opus-5-5 REMUDA_PROBE_MODEL_NO_XHIGH=claude-sonnet-4-6 \
  SCENARIO=launch CASES=e,f1,f2,g PROBE_DIR=/tmp/remuda-c-effortev-launch2 \
  cargo run -p remuda-driver --example effort_probe

cargo test -p remuda-driver --test effort_transcript   # shape test for effort-21289
```

The gateway credentials pass through by name (`ANTHROPIC_BASE_URL`,
`ANTHROPIC_AUTH_TOKEN`). Each `PROBE_DIR/out/` holds the scrubbed
`effort-<case>-21289.{jsonl,txt,actions.json}`. The `actions.json` (per-step
verdicts, dialog text, latencies, attachments) is not committed. Case g in
the fixtures comes from a later `CASES=g` re-run that added the
`/effort xhigh` step.
