# Review: `claude-pty` / `claude-bg` drivers (`fc29ae5`)

Adversarial check of `crates/remuda-driver/src/claude_pty.rs` and
`claude_bg.rs` against `docs/research/herdr-herdrx.md` §3 / §5 / §6,
`docs/research/claude-control-plane.md` §1 / §7, and the `Driver` trait.
Line numbers refer to the pre-fix file (commit `fc29ae5` / workspace copy
at review time).

## Verified (no change)

| Check | Evidence |
| --- | --- |
| Isolated Herdr session | Pty refuses `"default"` unless `socket_dir` is explicit (`claude_pty.rs` 219–223). |
| `agent.start` gets full recipe argv | Pty passes `recipe.argv` as Herdr trailing args (263–272). Herdr does not persist those args (§6.2 / `AgentStartParams`). |
| `--bare` on exact tokens | `refuse_bare` after materialize and after `agent.start` returned argv (203–205, 273). Bg after strip/`--bg` (234). |
| `--session-id` stripped on bg | `strip_named_flags(..., ["--session-id", "--cwd", "--continue"])` (230). Control-plane §1.1 / §7: `--bg` ignores `--session-id`; `--cwd` is not a `--bg` flag. |
| Bypass mapping at materialize | Print `--allow-dangerously-skip-permissions`; pty/bg `--dangerously-skip-permissions` (`materializer.rs` 478–494). Pty/bg omit `--permission-mode bypassPermissions`. |
| Blocked → interaction `answerable: false` | `screen_block_interaction` (748–823): `InteractionCarrier::NativeTty`, `answerable: false`, description says do not send keys by coordinate. Completeness of the interaction payload is already `ScreenDerived` (737–744). `respond_interaction` is unsupported (490–497). |
| Pty attach is observe, not herdr resume | `Driver::attach` opens `TerminalObserver` (403–452). Does not call herdr agent-resume. Closed pane → `ControlUnavailable`. |
| Bg attach does not wake | `Driver::attach` returns `AttachWouldWake` if `stopped` or `!dispatched` or `job_is_stopped` (469–483). `open_terminal` is the only `claude attach` path (135–198) and is not called from `Driver::attach`. |
| Stop not rm | `stop_job` runs `claude stop <shortId>` (414–425). No `rm` invocation. Control-plane §1.6 / §7: stop keeps job+jsonl; rm deletes the job record. |
| `--setting-sources` default | Materializer default `user,project,local` (`materializer.rs` 431–441). Empty list is `NATIVE_FEATURE_DISABLED`. |
| Hook files not in user home | SessionStart script + `session-meta.json` written under `launch_dir` (1040–1056), mode 0700/0600. Does not edit `~/.claude/hooks/` or the registered native home. |

## P0

### P0-1 Pty `resume` rebuilds a fixture spec, not the live launch

`resume` (`claude_pty.rs` 505–527) called `stub_spec_from_ref` (1232–1241), which deserializes `tests/fixtures/instance-spec.json` and only overwrites `host` / `cwd=pwd`. That drops:

- `model_id` (fixture `null` → first profile model, not the session’s model)
- `permission_mode` (fixture `manual`, not the live bypass/dontAsk/host mode)
- `args` (`--max-budget-usd`, …)
- cwd / workspace of the original instance

Herdr §6.3: herdr restore argv is hardcoded `claude --resume <id>` and **drops `--settings`**. Remuda must persist the full recipe and rebuild with original flags + `--resume`. `last_recipe` was stored but unused on resume; rematerialize from the fixture therefore also dropped overlay identity even though `inject_session_start_hook` would recreate *a* `--settings` file.

**Fix:** persist `last_spec` at successful materialize; `resume` clones it and only swaps `SessionAction::Resume`. `--settings`, `--setting-sources`, and `--model` come back from the live spec + overlay, not herdr agent-resume.

### P0-2 Bg `resume` also uses the fixture spec

`claude_bg.rs` 533–544 deserialized the same fixture, copied only `cwd` from `last_recipe`, then `prepare` with `SessionAction::New`. Restore therefore lost model / permission / args / `--settings` from the original job. Control-plane §7 send path for a kept job is `claude --bg --resume <shortId>` with the same flags; herdr §6.3 forbids relying on a reconstructed default argv.

**Fix:** persist `last_spec`; `resume` rematerializes from it (still strips `--session-id` / `--cwd`). `--resume <shortId>` is applied on the next `send` via `argv_for_resume`, which keeps `--settings`.

## P1

### P1-1 SessionStart overlay replaced the whole event array

`merge_session_start_hook` (1065–1082) did `hooks_object.insert("SessionStart", [remuda])`. Protocol §4.2: overlay **appends**, keeps original hook commands and order, does not replace the hooks table. A gateway `--settings` file (or a previous Remuda matcher) that already had SessionStart / PreToolUse would lose those commands. User global hooks loaded via `--setting-sources user` must not be clobbered by wiping the overlay event.

**Fix:** append a matcher if the command is not already present; leave other hook events untouched.

### P1-2 Bg never injected the SessionStart overlay

Module docs say observe jsonl when SessionStart records `transcript_path`. `prepare` (229–235) did not call `inject_session_start_hook`. Herdr §5 / §4.2: Remuda must install its own SessionStart beside herdr’s and record `transcript_path`.

**Fix:** inject the launch-dir overlay after `--bg` is ensured (writes `session-start.sh` + `--settings`, does not touch user global files).

### P1-3 `refuse_bare` missed `--bare=1` / banned tokens

Exact-token match only (924–935). `flags.rs` already treats `--bare=1` as banned. A future materializer slip or `agent.start` echo could pass `--bare=true`.

**Fix:** also reject `is_banned_flag_token` and `--bare=` / `--safe-mode=` / `--no-session-persistence=` / `--continue=`.

### P1-4 Bg `resume` of a stopped-not-rm job returned `AttachWouldWake`

`resume` (530–532) treated stop like attach. Control-plane §1.6 / §7: `claude stop` keeps the job; later `claude --bg --resume <shortId>` is the send path. `Driver::attach` must not wake; `Driver::resume` must restore the recipe. After resume, `Driver::attach` still consults `job_is_stopped` and refuses.

**Fix:** do not fail resume on stopped state; clear `live.stopped`; spawn the job observer. Attach remains `AttachWouldWake`.

### P1-5 Pty/bg did not re-assert TTY bypass on the final argv

Materializer maps bypass correctly, but print still re-applies its flag after materialize. Pty/bg could inherit `--allow-dangerously-skip-permissions` if extra_flags / argv mixed.

**Fix:** `apply_tty_bypass_flag` strips the print flag and keeps `--dangerously-skip-permissions`.

### P1-6 `--setting-sources` not re-checked after hook inject

Inject only `ensure_settings_flag`. If a recipe lost `--setting-sources`, Claude would inherit an unaudited default. Protocol §4.2 / herdr §5: production lists sources explicitly; empty is banned.

**Fix:** `ensure_setting_sources` after inject (default `user,project,local`; empty → `NATIVE_FEATURE_DISABLED`).

### P1-7 `parse_backgrounded` dropped UUID / compact middot forms

Control-plane §1.1 stdout is `backgrounded · b944e31c · name` (8 hex). Parser required `split_whitespace` + all-hex, so `b944e31c-bd52-…` (expanded UUID) and `backgrounded·b944e31c` failed.

**Fix:** split on middot; if the token is a UUID or ≥8 hex, take the first 8 hex chars (native shortId).

## P2

| ID | Where | Note |
| --- | --- | --- |
| P2-1 | status pump 734–744 | Every `blocked` event emits a new Interaction id; no debounce. |
| P2-2 | pty `close` 500–520 | Aborts tasks without join; late frames can be lost. |
| P2-3 | bg `send_followup` 387–395 | Does not forward `extra_env` (first dispatch does). |
| P2-4 | herdr §3.6 | Pane env does not clear inherited `CLAUDE_CODE_CHILD_SESSION`. |
| P2-5 | `job_is_stopped` 685–697 | Only reads `state`, not `status`; observer accepts both. |
| P2-6 | pty `attach` 403–452 | Always starts a tty pump (observe). Fine for this driver; Node must not call it as a wake. |

## Tests

`crates/remuda-driver/tests/claude_pty_review.rs` (does not touch `tests/claude_pty.rs`, `tests/claude_bg.rs`, or `tests/live_claude.rs`).
