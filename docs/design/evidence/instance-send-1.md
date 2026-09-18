# instance.send lands on shell-pty native and its ledger row resolves honestly

Date: 2026-09-18 (UTC). Scope: how an `instance.send` built by the CLI reaches
a Claude agent on the native shell-pty carrier, and how the Hub command ledger
resolves — inside the protocol's three-state Command model. No demo data dir or
ports are touched; the live portion ran against a throwaway `remuda dev` server
with its own data dir and high loopback ports. Paths are redacted (`<HOME>`,
`<SCRATCH>`).

## Commit anchor

The code this document describes is complete at commit
`88c7b8f355e604a3d7e51e98ab6babce6e86d0bf`, the last code commit on branch
`wt/c-send3/b-send3-md` (the docs-only commit recording this note follows it);
every deterministic check below was re-run at that tree. The behavior lands
across these commits, all on the branch:

- `a995af1e` `fix(hub): settle rejected sends inside the three-state command
  model` — the Hub store/projection/OpenAPI change and the no-fourth-state
  tests;
- `bdcad14a` `fix(cli): resolve a send from its settlement and skip polling
  when offline` — the `remuda instance send` behavior described in the CLI
  section;
- `9ade8747` `fix(hub): document the full settlement-outcome set the
  projection persists` — the `expired` outcome and the enum-parity guard;
- `88c7b8f3` `fix(hub): migrate legacy failed commands and drop the orphaned
  reason column` — the one-time database migration.

Earlier drafts of this document pinned shas that were rebased away; this
revision anchors to the code tip, which contains both the Hub and the CLI
behavior, so the anchor and the described code no longer disagree.

## Protocol decision: no fourth state

Round 2 expressed a never-acked send as a fourth `Command.state` (`failed`)
and a fourth resolution. `protocol.md` forbids exactly that:

- §2.5 line 316 — `state` is `queued | accepted | settled`, "仅这三种业务进度状态";
- the §2.5 state diagram routes a pre-dispatch rejection or expiry to
  **settled with a settlement outcome**, not a new node;
- §12.2 line 1678 — "不增设 unknown / dispatch_unknown / decision_unknown 第四态".

The `remuda_protocol` enums `CommandState` / `ResolutionState`
(`crates/remuda-protocol/src/enums.rs`) cannot parse `failed`, so the Hub was
emitting a vocabulary the protocol crate rejects. Rather than carry a protocol
amendment through §2.5, §12.2, both wire enums, the schema and the generated
TS, this work uses only what §2.5 already allows — and the distinction the
protocol actually draws:

| Observation | Wire resolution | Why |
| --- | --- | --- |
| Node replies with an explicit error | `state=settled`, `resolution=clear`, `settlement.outcome=rejected`, reason in `settlement.reason` | Positive evidence the command did **not** run — the §2.5 "派发前拒绝" edge. |
| Node accepts the RPC | `state=accepted`, then the journal settles it | The normal path. |
| Node receives the call but the reply is lost / Node never answers | `state=queued`, `resolution=reconciling` (or `unknown`) | A forward intent exists, so §2.5 forbids expiring or rejecting without asking the Node, and without non-execution evidence it cannot be called rejected. |

The decisive point is that **the lost-reply case is not expressible as
`expired`/`rejected`**: §2.5 line 337 says "无法证明没执行时也不能标 rejected；
超时查询返回 unknown". So the round-2 bounded ack deadline — which settled a
possibly-delivered send on the Hub's own timer — is removed, along with its
`commandSettleTimeoutMs` config. A forward intent is durable and never
self-expired. The `reason` is not a top-level wire field: it rides the
settlement as `settlement.reason` (projected from the Node entity's
`settlement.value.error.message`), present only for `outcome=rejected`. Two
internal SQLite columns (`settlement_outcome`, `settlement_reason`) hold the
projection and are `#[serde(skip)]` so they never appear on the wire.

`mark_settled` — the path the Node's own settle frame takes (`ws.rs`) — records
`outcome=completed` and clears any reason, and a terminal settlement is never
regressed by a later accept/settle. This closes the round-2 hole where a
deadline-failed row could be flipped to `settled/clear` while keeping its
failure reason.

### Settlement outcomes: the whole protocol set is documented

The journal projection persists any outcome the protocol `SettlementOutcome`
enum parses — `completed`, `rejected`, `cancelled`, **`expired`**. Round 3
initially documented only the first three in OpenAPI while accepting all four
in the projection, a one-level-down copy of the same vocabulary drift. The fix
documents `expired` too (it is the legitimate §2.5 pre-dispatch-expiry
settlement, distinct from a Hub-side timer) and the OpenAPI test compares the
documented `settlement.outcome` enum against the full set serialized from the
protocol enum, not merely membership of the sample.

## Accepted open gap: a never-answered send is not proactively converged

Removing the ack deadline has a consequence that is accepted here and named so
it is not mistaken for a finished design: **if a Node receives
`instance.send` and never answers and never journals, the row stays
`queued` / `reconciling` with no Hub-side mechanism that converges it.** There
is intentionally no Hub self-fail (that was the protocol violation), but there
is also, in this branch, **no same-command-query and no hub→node
reconciliation probe** — the Hub never calls the Node back to ask "did command
X run?"; the hello handler only records inventory, and convergence of a lost
reply relies entirely on the Node pushing its journal (`journal.append`,
`ws.rs`) on its own cadence. If the Node is wedged (rather than merely slow),
nothing on the Hub turns the row over.

Follow-up (not in this branch): add a Hub-driven reconciliation for a
`reconciling` forwarded command — a same-command-query / state probe issued on
reconnect and on a slow poll, which records the Node's durable answer (the §2.5
"恢复先查原生历史/请求状态 … 超时查询返回 unknown" path) without ever resending the
operation. Until that exists the honest posture is the current one: keep
`reconciling`, expose it on the listing and to the CLI, and never promote
uncertainty to a terminal settlement. The CLI bounds its own wait (next
section) so a human is not blocked, but the ledger row itself remains open.

## Database migration: legacy `failed` rows and the orphaned `reason` column

A data directory written by the round-2 build still contains
`state='failed'` rows and a `commands.reason` column. On open, the Hub now:

1. folds each legacy `failed` row into the §2.5 form — `state='settled'`,
   `resolution='clear'`, `settlement_outcome='rejected'`,
   `settlement_reason=<old reason>` — preserving the message;
2. `ALTER TABLE commands DROP COLUMN reason`, which also discards a stale
   `reason` a pre-fix `mark_settled` could have left on a completed row.

The migration is guarded on the column existing, so it is a one-time no-op on
new and already-migrated databases. It is covered by
`store::tests::legacy_failed_rows_and_the_reason_column_are_migrated`, which
builds a real round-2-shaped file, reopens it, and asserts the folded row, the
discarded stale reason, and the physical absence of the column.

## Deterministic evidence (run at 88c7b8f3)

All commands below use the OpenSSL-3 devbox toolchain; output trimmed to the
relevant lines.

- Driver, fake harness (the send actually lands and the mid-turn hold works):

  ```
  $ cargo test -p remuda-driver --test shell_pty_fake_send
  test a_send_lands_when_idle_and_is_held_until_a_running_turn_ends ... ok
  test result: ok. 1 passed; 0 failed … finished in 15.44s
  ```

- Hub, explicit Node rejection settles `rejected` (not a fourth state), and a
  never-acked send rests `reconciling`:

  ```
  $ cargo test -p remuda-hub --test hub a_send
  test a_send_the_node_rejects_settles_rejected_and_is_listed_by_the_new_route ... ok
  test a_send_the_node_never_acks_rests_reconciling_and_is_never_self_failed ... ok
  ```

  The reject test asserts over real HTTP `state=settled`,
  `resolution=clear`, `settlement.outcome=rejected`, the Node's message in
  `settlement.reason`, and the absence of a top-level `reason`. The
  never-acked test leaves a fake Node that silently drops `instance.send`,
  then polls the listing for ~1.2 s (well past where round 2's 400 ms deadline
  fired) and asserts the row stays three-state `queued`/`reconciling`,
  `forwarded=true`, with no settlement and no `reason` — the Hub never
  self-fails a forwarded command it cannot prove.

- Store projection, terminality, and the legacy migration:

  ```
  $ cargo test -p remuda-hub --lib \
      a_never_acked_send_rests_unknown_until_the_journal_settles_it \
      a_rejected_send_settles_rejected_and_a_later_settle_frame_keeps_it \
      legacy_failed_rows_and_the_reason_column_are_migrated
  test store::tests::a_never_acked_send_rests_unknown_until_the_journal_settles_it ... ok
  test store::tests::a_rejected_send_settles_rejected_and_a_later_settle_frame_keeps_it ... ok
  test store::tests::legacy_failed_rows_and_the_reason_column_are_migrated ... ok
  ```

- OpenAPI / wire drift guard (`tests/openapi.rs`):

  ```
  $ cargo test -p remuda-hub --test openapi
  test command_record_schema_matches_the_serialized_struct ... ok
  test openapi_is_31_and_covers_source_routes_and_methods ... ok
  test overlapping_web_client_operations_exist ... ok
  ```

  The route scan carries the HTTP **method** (it parses each `.route(` call's
  method-router services, with paren/string/comment-aware splitting) and
  asserts both directions against the spec, so a method change is caught. A
  fully-populated `CommandRecord` is serialized through
  `store_test_support::sample_command_record_json` and its property set diffed
  against the documented schema; the test pins `state` to exactly
  `accepted|queued|settled`, `resolution` to `clear|reconciling|unknown`, and
  `settlement.outcome` to the full protocol set (`completed|rejected|
  cancelled|expired`) via `settlement_outcome_wire_values`, and asserts the
  internal columns are not documented wire fields.

  `cargo test -p remuda-hub -p remuda` and `cargo clippy -p remuda-hub -p
  remuda --all-targets -- -D warnings` are green at `88c7b8f3`, as is
  `pnpm run typecheck` for the regenerated `api.generated.ts`.

## Live happy path (shell-pty native), summarized

The live observations from the first revision concern paths this work does not
change — the block-fold that makes the stdio Node accept the send, the
mid-turn hold, and the journaled run of `queued → accepted → settled`. They
remain reproducible at the code tip and are summarized rather than re-run
end-to-end (the deterministic driver/hub tests above re-verify them without a
live Claude process):

- an `instance.send` with `input.blocks:[{type:"text",text}]` and mode nested
  under `input` is folded to a prompt, **reaches the driver**, and the CLI
  reports `state: accepted`; the agent starts a turn;
- a second send issued mid-turn is **held and delivered at the turn boundary**;
- every accepted send later reads `state=settled` in
  `GET /v1/instances/{id}/commands`, which lists operation, state, resolution,
  the settlement projection and timestamps newest-first.

The original run was on Claude Code `2.1.274`, native carrier with the
emulator on; on that host the pinned hook relay could not load its
`libssl.so.3`, so submission used the emulator glyph rung throughout (Enter
still submitted every prompt). The send-lands and hold mechanics are covered
with hooks on by `shell_pty_fake_send::a_send_lands_when_idle_and_is_held_until_a_running_turn_ends`.

## CLI: offline send no longer burns the resolve window

`remuda instance send` (commit `bdcad14a`) resolves a *forwarded* row by
polling the ledger for the Node's journal to converge a lost reply, but it
short-circuits immediately when the POST returned `forwarded=false`: on an
offline host no forward intent exists, no timer is armed, and polling cannot
change the outcome, so it prints the honest `queued` row at once instead of
waiting out the full window. A rejection's message is surfaced from
`settlement.reason`. For the accepted-open-gap case above, this poll window
(and not any Hub timer) is the only bound on how long the CLI waits; the
ledger row may still read `reconciling` afterwards.

## Web

The generated client (`web/src/lib/api.generated.ts`) is regenerated from the
updated OpenAPI: `CommandRecord.state` is the `accepted|queued|settled` union,
`resolution` is `clear|reconciling|unknown`, and `settlement.outcome` is
`cancelled|completed|expired|rejected`. Because the Hub no longer emits a
fourth state, the hand-written coercion in `web/src/lib/api.ts` (allowlists of
those same three states / three resolutions) is consistent with the wire — the
round-2 "unknown state renders as accepted" follow-up no longer has a
triggering input. Surfacing a rejection (or a stuck `reconciling` row) in the
UI is left to the web owner; this worker's ownership is the generated client.
