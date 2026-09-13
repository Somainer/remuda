# remuda-rules

Screen-signal rule engine for agent state detection — D-028 §10.

herdr's "state classifier" is not a trained model: it is a table of TOML rules
with regions, priorities, matchers and combinators. This crate holds that table
as a versioned asset under `rules/`, extracted verbatim from the herdr 0.9.0
binary, plus the engine that evaluates it against a rendered terminal grid.

```rust
use remuda_rules::{Screen, State, bundled};

let manifest = bundled("claude")?;
let screen = Screen::new(grid_rows)         // emulator rows, no ANSI
    .with_cols(80)                          // pin the width — see anchor ⑤
    .with_osc_title(title)                  // whatever the VT retained
    .with_osc_progress(progress);           // payload after `9;`, e.g. "4;1;-1"

let verdict = manifest.evaluate(&screen);
// verdict.state / .rule / .region / .priority / .evidence
// verdict.skip_state_update / .visible_idle / .visible_blocker / .visible_working
```

Screen is the **bottom** of the `Hook > File > OSC > Screen` ladder (§4.3). Use
it when nothing better spoke.

## Regenerating the rules

The manifests are third-party assets. Do not hand edit them — re-extract:

```sh
python3 crates/remuda-rules/scripts/extract-herdr-rules.py <herdr-binary> --list
python3 crates/remuda-rules/scripts/extract-herdr-rules.py <herdr-binary> \
    --out-dir crates/remuda-rules/rules --herdr-version 0.9.0
cargo test -p remuda-rules --locked
```

The script reads the binary as **data** (never executes it), finds each
`id = "<kind>"` / `version = "…"` header in `.rodata`, and walks a line grammar
to the end of each document. Every file gets a provenance banner; everything
below it is byte-for-byte upstream text.

After a regeneration:

- `manifest_versions_are_pinned_to_the_extracted_herdr_build` will fail if a
  version moved. That is the prompt to re-verify the fixtures against the new
  behaviour, then update the pin — not to delete the assertion.
- If `BUNDLED` gains or loses a kind, update the table in `src/lib.rs`; it is
  `include_str!`-based so a new file is not picked up automatically.
- `min_engine_version` above `ENGINE_VERSION` (3) is a hard error. A newer
  manifest needs engine work first, not a version bump.

Pair this with `binary.rs`'s `pin_binary`: a herdr upgrade should prompt a rule
re-verification.

## Feeding the engine from a carrier

The input is the **emulator grid**, not a de-ANSI'd byte tail (§4.1). Cursor
motion dropped by a byte tail silently shifts every signature.

| `Screen` field | Carrier source |
| --- | --- |
| `rows` | rendered grid text, one entry per row, blanks included |
| `cols` | the width pinned at startup |
| `cursor` | `(row, col)`, zero-based |
| `osc_title` | OSC 0/2 payload **retained in VT state** |
| `osc_progress` | OSC 9;4 payload *after* `9;` — `4;1;-1` busy, `4;0;0` idle |

The two OSC payloads must be kept, not just rendered and forgotten; several
top-priority rules read nothing else. `terminalProgressBarEnabled` and
`showStatusInTerminalTab` being off loses that whole layer (§9.2). An absent
payload yields an empty region, so those rules simply do not fire.

Re-evaluate at each frame boundary, debounce ~120 ms, and emit only on
transitions — that replaces herdr's `pane.agent_status_changed`.

## What the caller still owns

- **`done` is derived, not detected.** `done = idle ∧ unread since the last
  turn`, from per-device seen state. There is no `State::Done`.
- **`unknown` never collapses into `idle`.** It means no rule spoke, which is
  not evidence the agent finished.
- **`skip_state_update` means leave the state alone** — do not write `unknown`.
- **Latching is per-session state** the engine cannot hold (anchor ④).
- **Tiering**: a Hook or File observation outranks anything here.

## The five false-positive anchors (§10)

Simplifying any of these is a regression. Each has a test named for it.

1. **Activity lines are anchored at column zero, continuations indented.** Keeps
   text the user typed from impersonating a signal. Enforced in the rules'
   `^`-anchored patterns and in `screen.rs`'s wrap logic, which never folds a
   column-zero bullet onto the row above.
   → `anchor_one_user_text_cannot_impersonate_an_activity_line`
2. **grok anchors on the `[stop]` chip, not a braille glyph** — its startup
   splash draws its logo in braille.
   → `grok_braille_splash_without_a_stop_chip_is_not_working`
3. **Transcript viewers and model pickers get high-priority
   `skip_state_update`** — they are laid out exactly like approval dialogs.
   → `claude_transcript_viewer_skips_the_state_update`,
   `claude_model_picker_skips_the_state_update`,
   `codex_transcript_viewer_skips_the_state_update`
4. **A blinking `⚠ Action Required` drops frames while unfocused, so `blocked`
   latches** until a positive idle/working signal. The engine is per-frame and
   reports the flags; the *caller* holds the latch.
   → `grok_action_required_title_outranks_everything`
5. **Narrow widths soft-wrap and break every two-token `contains`**, so rows are
   rejoined before matching and `cols` is pinned at startup. The join is
   character-exact: trimming the continuation would weld `to` onto `proceed?`.
   → `anchor_five_soft_wrap_is_joined_before_matching_at_forty_cols`,
   `the_same_dialog_reads_the_same_at_eighty_and_forty_cols`

A sixth, learned while porting: **a bordered dialog is not the composer.**
`prompt_box_body` only accepts a box whose body *opens* with the prompt marker.
Without that test a permission dialog wins the priority-950 `live_prompt_box`
rule and reports **idle** while the agent waits on a human.
→ `claude_dialog_box_is_not_mistaken_for_the_composer`

## Engine semantics

- **Gate** — a conjunction over whichever fields are present: every `contains`
  (case-insensitive) must appear, every `regex` must match the region text,
  every `line_regex` must hit some line, every `all` must hold, at least one
  `any` must hold, no `not` may hold. Absent fields impose nothing.
- **Resolution** — highest priority wins; ties break on document order, so the
  outcome is total and stable regardless of sort implementation.
- **Regions** — the ten from §10. Only `prompt_box_body` rewrites its lines (it
  strips the box gutters its `^\s*❯` anchors could not otherwise pass); every
  other region is verbatim, which is what lets grok's `option_dialog_blocked`
  match `┃` and gemini's match `│ Apply this change`.

## Licensing

The files under `rules/` are copied from herdr 0.9.0 (Apache-2.0). See the repo
`NOTICE`. The engine in `src/` is an original implementation — no herdr source
was copied, only the data table.
