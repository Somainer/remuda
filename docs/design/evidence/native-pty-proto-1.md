# D-028 P1 · Protocol increments: every additive change and its compatibility proof

2026-09-14 · `wt/x-p1-proto/d028-protocol-increments` · branched from `origin/main` (`f359e5c`)

D-028 §13 conflict rule ② requires all protocol changes to land **once**, purely additively, so the other workers rebase exactly once. This branch is that landing. It opens the wire for native-PTY-first; it deliberately changes **no driver behaviour**. Every capability it adds is reported as `unknown` until someone measures it.

## The changes, one per row

| # | Change | Section | Where |
| --- | --- | --- | --- |
| 1 | `Instance.launchedBy: "remuda" \| "user"` | §1.0 rule 4, §2.3 | `entities.rs`, `enums.rs` |
| 2 | `NativeRef.signalTier` + `NativeRef.capabilities[]`, and `capability_set_with_runtime` layering them over the static matrix | §4.3 | `native.rs`, `remuda-driver/capabilities.rs` |
| 3 | `PromptMode` gains `steer` / `queue`; `CapabilityName` gains `queue` / `interrupt` | §6 | `enums.rs`, `capabilities.rs` |
| 4 | `InstanceSpec.effort: {name, ultracode}`, legacy normalization, `--effort` emission, flag value allowlist | §9.1 | `launch.rs`, `materializer.rs`, `flags.rs`, `remuda-hub/store.rs` |
| 5 | kind/driver matrix admits `(claude\|codex\|grok\|agy, shell-pty)`; materializer `ShellPty` agent arm; presets moved out of the herdr driver | §5.1 | `runtime.rs`, `materializer.rs`, `presets.rs` |
| 6 | `SourceChannel` gains `file` / `osc` / `screen`; `exited` / `failed` / `interrupted` documented | §4.3, §5.3, §5.5 | `enums.rs`, `protocol.md` |
| 7 | `protocol.md`, OpenAPI, generated schema and clients | §12 | see *Generated artifacts* |

## Why each one is additive, and what proves it

Additive is a claim about **the other side of the wire**, so each field is paired with the test that loads a pre-D-028 payload and checks it still means what it meant.

### 1 · `launchedBy`

A new optional field. Rows written before D-028 do not have it, so reading derives it from the `mode` / `promotedAt` pair D-025 already stored: `promoted` means an agent CLI took over a login shell's foreground, and nobody but a person types that — so it derives to `user`; everything else to `remuda`. No migration, no column.

The old `mode` field stays. It answers a different question (*was this kind promoted*) than `launchedBy` (*who typed the command*), and dropping it would break every existing reader for no gain.

The field records provenance and nothing else. §1.0 rule 4 is explicit that it must not express a capability level, and rule 5 makes any "only when Remuda started it" behaviour a P-grade defect — so nothing in this branch reads `launchedBy` to decide what a session may do.

### 2 · `signalTier` and runtime capabilities

Both optional. The distinction that matters: **absent is not `none`**. Absent means nobody reported, and the static §3.3 matrix still applies. `none` is a session asserting it reached no signal tier at all. Collapsing the two would silently disable the matrix for every pre-D-028 peer.

Precedence has two rules, and the second is the one that is easy to get wrong:

- a runtime entry replaces its matrix cell **whatever its state**, so a session that measured a capability as unavailable can say so;
- a capability with no runtime entry keeps its matrix value, because "not reported" is not evidence of absence.

Without the first rule, "runtime override" could only ever add optimism. The test asserts a downgrade in both directions: an `interactive-approval` entry lowering a tier-granted `supported`, and a `resume` entry lowering the statically-supported `claude-print` cell to `unknown`.

The tier itself is evidence, not just a label. `hook` can block the agent and return a verdict — which is what `interactive-approval` means — so it grants that plus `resume` / `hooks` / `structured-workflow` / `completion-native-turn`. `file` proves identity and turn boundaries but has no blocking channel, so it grants the subset without `interactive-approval`. `osc` and `screen` prove neither and grant nothing; that is why they are the floor.

### 3 · `steer` / `queue` and the two new capability names

`PromptMode` was a single-variant enum, so adding variants cannot break a parse. `SteerInput` already existed and is **not** duplicated: the `type: "steer"` branch names an exact `expectedNativeTurnId`, which only a structured driver can supply, while `mode: "steer"` says "insert into whatever is running now", which is all a PTY can express. Defined-turn versus defined-now — documented in §3.1 rather than collapsed.

`queue` and `interrupt` are new `CapabilityName` values, so a pre-D-028 `CapabilitySnapshot` has no entry for them. They are `#[serde(default)]` to `unknown`, never `unsupported`: a peer that did not mention a capability has not told us it lacks one.

**No driver reports these as supported in this branch.** Not an oversight — §14 risks 6 and 7 record that claude's queue-vs-steer semantics and grok/agy's keys are unmeasured, and codex's measured `Tab`/`Esc` are not yet implemented in Remuda. `unsupported` would be an unbacked assertion; `unknown` returns `CAPABILITY_UNKNOWN` and shows "尚未验证". `shell-pty`'s `steer` moved from `unsupported` to `unknown` for the same reason.

### 4 · effort

`InstanceSpec.effort` is optional, and **absent stays absent** — it means "the harness decides", and the materializer emits no `--effort` at all. Silently substituting the default level would pin a tier the user never chose.

Legacy tiers normalize by **name**: `default→low`, `think→high`, `think-hard→xhigh`, `ultracode→xhigh + ultracode:true`, unrecognized→`high`. Normalizing by index would be wrong, and the test pins the specific collision: the legacy tables were per-harness and different lengths, so index 3 was `ultracode` for claude but `ultra` for codex. By index, a stored codex row would silently become ultracode. The deserializer accepts three spellings — the pre-D-028 `{index, name}` object, a bare string, and the D-028 object — and lands all of them on one value; `index` is read-ignored and not written back.

Unrecognized names fall to `high` rather than erroring. An effort tier is a preference; failing a launch over a stale UI string would be the worse failure.

`ultracode` is not a sixth level. It is stored as its level plus a boolean, and emitted as the single flag `--effort ultracode`, because the native flag takes one value.

`flags.rs` gains a value allowlist for `--effort`. The legacy names are rejected there deliberately: normalization happens before argv is built, so a legacy name reaching the allowlist means a caller hand-wrote a flag the binary would reject — better to fail naming the flag than to crash at startup.

`CLAUDE_CODE_EFFORT_LEVEL` outranks in-session `/effort` and would pin the tier, so §9.1 requires stripping it from the child environment. **That is not done in this branch** — see *Handoff* below.

### 5 · kind/driver matrix and the shell-pty agent arm

Four new legal `(kind, driver)` pairs. Purely a widening: no previously-valid pair was removed.

`materialize` previously refused `shell-pty` outright. It now branches on `spec.kind`, because one driver carries two different launches — a login shell under `terminal`/`generic`, an agent CLI under its own kind. Both arms stay reachable, and a test asserts the login shell still gets empty argv.

The agent arm produces a complete recipe: real env allowlist, real settings digest, real provider kind. The old `shell_pty.rs` stub emitted an empty allowlist and a hardcoded provider, which §5.1 step 4 lists as an audit requirement rather than a detail.

`flags.rs` validation now reaches this path. It previously ran only inside the materializer, which `shell-pty` bypassed entirely — so the native path had no flag allowlist at all.

The per-kind presets and yolo gating moved from `generic_pty.rs` into a carrier-independent `presets` module, re-exported under the old names so existing callers are untouched. **Values are unchanged** — §5.1 asks for each to be re-verified against its binary pin, which is a measurement task, not something to do while moving a table. D-011/D-017 gating moved with them and is re-asserted on the new path: yolo argv requires both an explicit bypass request and a non-agent origin, and an agent requesting bypass is refused before argv is built.

New agent-pty sessions do not pin `--session-id`: the id comes back from the harness, and inventing one creates a second identity to reconcile.

### 6 · SourceChannel and the lifecycle distinction

Three new variants on an enum that already had nine. Existing ones are untouched — `transcript` keeps the established Claude path, `pty` keeps raw bytes.

`exited` / `failed` / `interrupted` are documented as three different things because conflating them makes the UI lie. `interrupted` is a *message* status, not a lifecycle state. Exit detection needs two witnesses (`child.wait()` and PTY EOF), and an unconfirmed process-group teardown records `stop-incomplete` rather than reporting `exited`.

## Compatibility tests

`crates/remuda-protocol/tests/wire.rs::d028` — each loads a payload in its pre-D-028 shape and asserts the reading:

| Test | Proves |
| --- | --- |
| `instance_without_launched_by_parses_and_derives_it` | Old Instance parses; derivation yields `remuda` with no mode, `user` when promoted; an explicit value wins |
| `native_ref_without_runtime_capabilities_parses` | Old NativeRef parses; `signalTier` absent and capabilities empty; the populated form round-trips |
| `capability_set_without_queue_and_interrupt_reads_as_unknown` | A snapshot with both names removed parses, and both read `unknown` — not `unsupported` |
| `prompt_mode_gains_steer_and_queue_without_moving_new_turn` | `new-turn` unchanged; the two new values parse; an unknown value is still refused |
| `source_channel_gains_file_osc_screen` | New variants parse; existing ones unchanged |
| `effort_selection_normalizes_legacy_tier_names` | 11 legacy/current names × 3 payload shapes land on one value each; the `ultra`/`ultracode` index collision is pinned; `{index}` with no name is refused |
| `instance_spec_without_effort_parses_and_stays_absent` | Old spec parses, effort stays `None` rather than defaulting |

Hub-side, `legacy_effort_tier_names_normalize_by_name_not_index` covers the same collision through SQLite storage, `launched_by_is_derived_from_mode_for_legacy_rows` covers the derivation, and the end-to-end `instance_configure_is_journaled_and_persisted` now posts a **pre-D-028 client payload** (`{index: 1, name: "think"}`) over HTTP and asserts it still works, reading back the normalized level.

Driver-side, `capability_set_with_runtime` precedence has four tests and `shell_pty_agent` has six covering per-kind argv.

## Verification

| Check | Result |
| --- | --- |
| `cargo fmt --all` | clean |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | clean |
| `cargo test --workspace --locked` | PASS except two pre-existing environmental failures (below) |
| `pnpm test` (web) | PASS: 304 tests in 61 files |
| `pnpm exec tsc --noEmit` (web) | clean |
| `./scripts/ci/secret-scan.sh` | pass |

**Two failures are pre-existing, not from this branch.** `remuda-node`'s `native::tests::registry_constructs_all_three_native_claude_drivers` and `stdio::tests::composed_stdio_dispatches_create_and_streams_journal` fail because macOS denies this target directory's access probe on `~/.claude` (the error text names the missing Full Disk Access grant). Verified by checking out `origin/main` (then `b902d38`) into a scratch worktree with a separate target dir and running the same two tests: both fail identically there, with none of this branch's changes present.

### Mutation checks

Each load-bearing assertion was re-run against a deliberately broken implementation and failed:

| Mutation | Caught by |
| --- | --- |
| `think-hard` normalized to `high` instead of `xhigh` | `effort_selection_normalizes_legacy_tier_names` (`left: High, right: Xhigh`) |
| Missing `queue`/`interrupt` defaulting to `unsupported` | `capability_set_without_queue_and_interrupt_reads_as_unknown` (`left: Unsupported, right: Unknown`) |
| Runtime entries unable to lower a cell (only `supported` applied) | `explicit_entries_win_over_the_tier_and_may_lower_a_capability` (`left: Supported, right: Unsupported`) |
| `--effort` emitting the level name when `ultracode` is set | `effort_becomes_one_flag_or_none` (`left: Some("xhigh"), right: Some("ultracode")`) |

## Generated artifacts

Regenerated and committed, never hand-edited:

- `crates/remuda-protocol/schema/protocol.schema.json` and `web/src/types/generated.ts` — `cargo run -p remuda-protocol --example gen_types` (`just gen-types`). `generated_files_are_current_without_writing_to_the_workspace` fails if either drifts.
- `web/src/lib/api.generated.ts` — `pnpm --dir web run gen:api`, after adding `effortName` (now an enum), `effortUltracode`, `launchedBy` and the `EffortSelection` schema to `crates/remuda-hub/openapi/openapi.json`.

The two protocol fixtures carrying a full `CapabilitySet` (`instance.json`, `lifecycle-entities.json`) gained the two new names, since they are goldens for the current shape. The pre-D-028 shape they used to hold is now constructed inside the compatibility test, where it belongs.

## Handoff

**`child_env.rs` is owned by x-p1-signal and is not touched here.** D-028 §9.1 asks for `CLAUDE_CODE_EFFORT_LEVEL` to be stripped from the child environment, because it outranks in-session `/effort` and an inherited value would pin the tier — making every runtime effort change silently ineffective.

Checked before writing this: the **inheritance** half is already closed. `child_env::INHERIT` is a closed allowlist (`PATH`, `HOME`, `LANG`, `TERM`, `TMPDIR`, `SHELL`, `USER`, `LOGNAME`, `XDG_*`, plus the `LC_` prefix) applied after `env_clear()`, and `CLAUDE_CODE_EFFORT_LEVEL` is not on it, so it cannot reach a child from the Node's own environment through `base_env()`.

What remains open is **explicit injection**: `ShellPtyOptions::extra_env` and spec `env` bindings are filtered by `child_env::is_denied`, which does not list the variable (`DENY_PREFIXES` covers `LD_` / `DYLD_` / `REMUDA_`, not `CLAUDE_`). So a caller can still set it deliberately and silently pin the tier. **The one-line requirement: add `CLAUDE_CODE_EFFORT_LEVEL` to `child_env::DENY`.** That closes the injection path at both sites at once, since the materializer's `reject_banned_env` already routes through the same predicate.

**The settings-overlay slot is empty.** `materialize_shell_pty_agent` honours a caller-supplied `settings_overlay_path` and records its digest, but writes no overlay of its own. x-p1-signal's hook/settings overlay fills that slot. Until it does, an absent overlay emits no `--settings` flag rather than an empty file that would shadow the user's own settings.

**Preset values need re-verification.** §5.1 asks for each yolo argv to be re-checked against its binary pin. They moved unchanged; the measurement is still owed.

**The web effort table, normalizer and slider are x-effort-ui's.** This branch touches only `web/src/types/*` (generated plus the hand-written unions) and `web/src/lib/capabilities.ts`.
