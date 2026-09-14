# D-028 P6: codex / grok signal adapters — evidence

Date: 2026-09-14 local time
Scope: `crates/remuda-driver/src/adapters/{mod,codex_adapter,grok_adapter,supervisor}.rs`,
`crates/remuda-driver/src/launch/shadow.rs`,
`crates/remuda-node/src/{adapter_registry.rs,signal.rs,runtime.rs}`,
plus tests in `crates/remuda-driver/src/adapters/**/tests.rs`,
`crates/remuda-driver/src/launch/shadow/tests.rs`,
`crates/remuda-testing/tests/fake_harness.rs` (`p6_*`),
`crates/remuda/tests/p6_adapter_parity.rs`.

Design references: [native-pty-first.md](../native-pty-first.md) §3/§4.2/§6/§7/§13 P6;
[codex-signals-1](codex-signals-1.md); [grok-signals-1](grok-signals-1.md);
[testing-fake-harness.md](../testing-fake-harness.md).

---

## 0. What shipped

| Requirement (§13 P6) | Where | Status |
|---|---|---|
| codex adapter: turn boundaries, completed items → messages, tool calls/outputs, token usage, session discovery, input semantics | `adapters/codex_adapter.rs` | ✅ |
| PermissionRequest via shadow `CODEX_HOME` hooks.json with trust-hash writer | `launch/shadow.rs` | ✅ hook registered; **verdict transport is P5** (documented TODO) |
| grok adapter: events.jsonl turn_started/ended + phase, updates.jsonl ACP chunks, tool_call/update, active_sessions pid↔session, usage.json | `adapters/grok_adapter.rs` | ✅ |
| PreToolUse deny/ask via hook; `GROK_CLAUDE_HOOKS_ENABLED=0` in shadow env | `launch/shadow.rs` | ✅ deny/ask observed; screen answer stays tier-D |
| UsagePayload per turn for claude/codex/grok (estimated, price-table version) | adapters feed `usage::*` + `to_usage_payload` | ✅ codex/grok (claude path already wired in P1) |
| Adapter reports signalTier and per-capability provision at runtime | `runtime_ref` + node `adapter_registry.rs` | ✅ |
| Small adapter registry the Node signal bus consults | `remuda-node/src/adapter_registry.rs` | ✅ |
| fake-harness `--kind codex/grok` deterministic tests | `fake_harness.rs::p6_*` (3 PTY tests) | ✅ |
| `remuda journal diff` parity for codex and grok | `p6_adapter_parity.rs` (3 tests) | ✅ |

## 1. Adapter architecture

The adapters are **pure state machines** behind one trait:

```rust
trait FileSignalAdapter: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn session_id(&self) -> Option<&str>;
    fn confirm_session(&mut self, session_id: &str);
    fn poll(&mut self) -> DriverResult<Vec<AdapterObservation>>;
}
```

`AdapterObservation` carries a payload plus the native turn/item ids; the
`supervisor` stamps the instance envelope (`File` channel, instance/run/journal
ids, monotonic seq) onto the instance's **single** observation channel — the
same `mpsc` the hook bus and promotion poller write to. Sequence numbers
therefore interleave correctly across Hook / File / Screen, which is what the
§4.3 ranking requires at the observation level.

`spawn_file_adapters(kind, AdapterCtx)` runs a 250 ms poll task per kind; the
task exits when the journal channel closes, and its `AdapterHandle` is aborted
in `ShellPtyDriver::close`. Shadow files survive close as launch-audit evidence
and are removed by instance purge.

Both adapters read **only** the per-session shadow home the hook session
materialized; a launched agent never tails the user's real `~/.codex` /
`~/.grok` (§4.2 boundary).

## 2. codex — measured mapping

All facts are asserted against the captured 0.154.0 session
(`crates/remuda-driver/tests/fixtures/codex/interactive-0.154.0.jsonl`, 97
records):

- **Lifecycle**: `event_msg/task_started` → working; `task_complete` → idle;
  `turn_aborted{reason:"interrupted"}` → idle (severity Info, never a failure).
  The adapter test counts **6 started / 5 complete / 1 aborted** on the real
  fixture, matching evidence §A3. A foreign abort reason does not end a turn.
- **Messages/thoughts**: only `item_completed` of `UserMessage` /
  `AgentMessage` / `Reasoning` map (item-level, `Structured`, status Complete).
  The standalone `response_item` duplicates are explicitly ignored so one
  message never double-journals.
- **Tools**: `response_item/function_call` announces the call (`call_id`,
  JSON `arguments`); `item_completed/CommandExecution` closes it with the
  `Rejected(…)` status → `ToolOutcome::Denied`, exit 0 → Succeeded, non-zero →
  Failed. `function_call_output` is used only when no CommandExecution arrives.
  A late completion after `turn_aborted` keeps the **old** turn id (evidence
  ordinal 86) — verified by
  `completed_items_map_messages_and_a_late_tool_result_keeps_the_old_turn`.
- **Usage**: `CodexUsage` consumes every record in file order; the
  `UsageAggregator` accepts additive `token_usage_record` and refuses
  cumulative `token_count`. Per-turn and per-session snapshots
  (`accounting:"estimated"`) are emitted at each turn end with monotonic
  metric revisions (§5.5 snapshot replaces).
- **Discovery**: hook-confirmed id first; otherwise the newest
  `session_index.jsonl` name entry; `locate_rollout_in` then verifies the
  file's `session_meta.id` and refuses ambiguity. No queue/steer provenance is
  invented (the file has none).

## 3. grok — measured mapping

Against the captured 1.0.30 session (`tui-updates.jsonl` 54 frames,
`tui-events.jsonl` 74 events):

- **Lifecycle** from `events.jsonl`: `turn_started` → working, `turn_ended` →
  idle with `outcome` (`completed`/`cancelled`) and `cancellation_context.trigger`
  (`ctrl_c` / `send_now`) in related ids. The real-fixture test asserts **7
  starts / 7 ends / 2 cancelled**. `turn_completed` in updates.jsonl closes
  chunks but is never read as success — its `stop_reason` can be `cancelled`.
- **Chunk streaming**: `agent_message_chunk` / `agent_thought_chunk` append to
  one node per native prompt id (`Open`, then `Append` per chunk); the
  `turn_completed` frame emits a full-text `Close` with status `Complete` or
  `Interrupted`. The promise is chunk-level, never token-level.
- **Tools**: `tool_call` (`title` + `rawInput`) → proposal; the terminal
  `tool_call_update` carries status + `rawOutput.exit_code`. A hook-denied
  update (`error:"denied: …"` / `Hook denied`) maps to `Denied`.
- **Permissions**: `permission_requested`/`permission_resolved` are journaled
  as `Permission`-topic lifecycles only — they are post-hoc summaries, not an
  answer channel (`wait_ms:0` ≠ no visible dialog). `PermissionRequest` is not
  registered in the grok hook set; PreToolUse deny/ask is the hook channel.
- **Discovery**: `active_sessions.json` by pid, then cwd; the registry entry
  is removed at shutdown, so it is discovery-only. The live PTY test polls
  while the fake TUI is alive (the registry is empty after exit).
- **Usage**: `usage.json` snapshot + `usage_from_update_frame`; an empty
  `{}` file produces no snapshot.

## 4. Shadow homes and the codex trust hash

`launch/shadow.rs` materializes, per instance:

- **codex**: `<launch>/codex-home/config.toml` with `[features] hooks = true`
  **and** inline `[hooks.state."<abs hooks.json>:<snake_event>:0:0"]` trust
  entries; `hooks.json` registers `SessionStart` and `PermissionRequest`
  against `remuda hook emit --socket … --event …` with the 600 s default
  timeout.
- **grok**: `<launch>/grok-home/hooks/remuda.json` with the six events the
  binary accepts (no `PermissionRequest`), plus
  `GROK_CLAUDE_HOOKS_ENABLED=0` in the child env.

### Trust-hash ground truth

The hash is `sha256:` of compact, recursively-key-sorted
`{"event_name":<snake>,"hooks":[{"async":false,"command":<cmd>,"timeout":<secs>,"type":"command"}]}`.
This was verified against the **codex app-server installed on this host
(0.147.0)**, not just inferred: with
`hooks.json {"type":"command","command":"echo hello","timeout":60}`,
`hooks/list` reports
`currentHash = sha256:5f44c900f8dd4d3e93fadabc3e027676f42cd92c9228c171b8427aa9117ca510`,
which is exactly what `codex_trust_hash("permission_request", "echo hello", 60)`
produces (`the_canonical_hash_matches_a_live_codex_currenthash`). Writing that
hash into config.toml flips `trustStatus` from `untrusted` to **`trusted`**
on the next `hooks/list` (verified live). The 0.154 evidence hashes are
documented as a reference in §A1 but are not reproduced byte-for-byte by 0.147;
the algorithm equality is pinned against the binary actually present.

Trust placement (inside `config.toml` under `[hooks.state]`, key with two
`:0:0` ordinals) and the feature switch were both measured on 0.147 and are
asserted in `codex_shadow_writes_config_with_inline_trust_and_hooks`.

## 5. Node wiring and capability tier

- `remuda-node/src/adapter_registry.rs` is the single lookup: codex/grok have
  a file adapter + `SignalTier::Hook`; codex approvals are
  `TypedHookVerdict`, grok are `EmulatedScreen` (the §6 honesty split).
- `signal::file_activity` folds `task_started/turn_started` → Working and
  `task_complete/turn_aborted/turn_ended` → Idle for `File`-channel
  observations; `runtime.rs` applies hook activity first, then file activity
  only when the instance kind has a registered file adapter. The screen-derived
  `agent_status` is now outranked by both hook and file facts.
- The driver's `runtime_ref` reports `SignalTier::Hook` when the hook session
  exists; steer/queue/interrupt provisions come from the already-merged
  `keys.rs` table (codex: native steer + native Tab queue + native Esc; grok:
  emulated queue + emulated interrupt).

## 6. P5 seam (explicitly not closed)

The blocking verdict transport for codex `PermissionRequest` and grok
PreToolUse is **not** implemented here — it is P5, which is not merged. The
hook events are observed (waiting is visible), the shadow homes register the
right hooks, and `remuda_signal::SignalBus::handle` carries a
`TODO(x-p5)` describing the exact integration: broker-backed
`InteractionRequested`, the claude vs codex reply envelopes, and deny-on-
timeout (§4.4). Until P5 lands the bus replies `{}`, so the harness's own
screen dialog remains the single decision authority — the measured grok path
and the confined-session fallback both require this.

## 7. Deterministic tests and parity

- **Unit** (`cargo test -p remuda-driver adapters:: launch::shadow`): 23 tests
  — real-fixture boundary counts, late-tool-old-turn, denied-vs-failed,
  usage snapshot/revision, partial-line tail hold, grok chunk
  open/append/close, cancel trigger, pid→cwd discovery fallback, shadow-home
  contents/permissions/no-credential, and the live trust hash.
- **PTY end-to-end** (`cargo test -p remuda-testing --test fake_harness
  p6_`): 3 serial tests drive the real `fake-harness` binary through
  portable-pty and feed the production adapters — codex ok/approval lifecycle
  + tools + estimated usage; codex Esc interrupt producing `turn_aborted`;
  grok live registry discovery + chunks + tools.
- **Journal parity** (`cargo test -p remuda --test p6_adapter_parity`): both
  adapters run over the captured real fixtures, observations are stamped into
  Hub-shaped dumps, and `remuda journal diff` reports each dump
  self-identical (exit 0) under `--no-whitelist`; separate tests pin the
  measured codex lifecycle/usage facts and grok chunk/cancel facts.
- The whole workspace builds (`--workspace --locked`) and tests green;
  `cargo clippy --workspace` and `cargo fmt --all` are clean.

## 8. Open items / deferred

1. P5 blocking verdict transport (codex allow/deny, broker TTL/deny) — see
   §6. Live end-to-end approval allow with the UI is a P5 acceptance test.
2. The 0.154→0.147 trust-hash delta: identical *algorithm*, verified on the
   installed 0.147; a future codex upgrade should re-run the
   `hooks/list` ground-truth probe (the test is pinned to 0.147).
3. Promoted (hand-typed) codex/grok sessions get the file adapter only after
   promotion identifies the kind; a launched agent gets it immediately. The
   pass-through shims already export the shadow `CODEX_HOME`/`GROK_HOME` so a
   hand-typed command resolves the per-session home.
4. **Live grok evidence is unavailable on this host** — no grok binary is
   installed (checked `PATH`, `~/.local/bin`, `/usr/local/bin`, global npm).
   grok coverage is therefore the captured real 1.0.30 fixtures plus the
   fake-harness PTY tests, not a fresh live session. Codex was verified live
   (below).

## 9. Live codex verification (2026-09-14, codex-cli 0.147.0, this host)

Beyond the fixtures, the shadow-home + trust path was exercised against the
**real installed binary**:

1. Built `<shadow>/config.toml` (`[features] hooks=true` + inline
   `[hooks.state."…/hooks.json:session_start:0:0"]` trust hash) and
   `hooks.json` pointing SessionStart at a logger script, computing the hash
   with the production `codex_trust_hash` canonicalization.
2. `hooks/list` over the app-server reported **`trustStatus = trusted`** for
   the registered handler.
3. `CODEX_HOME=<shadow> codex exec --skip-git-repo-check "reply with exactly:
   P6OK"` exited 0 and **fired the trusted hook**: the logger captured the
   SessionStart stdin,
   ```json
   {"session_id":"01a0a088-a82f-7f23-bf40-acd33a2ea3f4",
    "transcript_path":"<shadow>/sessions/2026/09/14/rollout-…01a0a088….jsonl",
    "cwd":"/tmp/remuda-r6p6/work","hook_event_name":"SessionStart",
    "model":"gpt-5.6-sol","permission_mode":"bypassPermissions","source":"startup"}
   ```
   — exactly the `session_id` + `transcript_path` pair the adapter's discovery
   consumes.
4. The real rollout written by that session contains the typed
   `session_meta` / `turn_context` / `task_started` / `task_complete` records;
   the production `CodexAdapter` run over that file emitted the
   `task_started`/`task_complete` lifecycles (verified with a scratch harness
   run during development; the durable assertion is the 0.154 fixture test,
   since 0.147 `codex exec` persists only the request-side messages and streams
   its response to stdout rather than persisting an `AgentMessage` item).

Artifacts of this live run were left under `/tmp/remuda-r6p6/` for
inspection; nothing from that path is committed (no personal paths or tokens
in the tree, `secret-scan` passes).

