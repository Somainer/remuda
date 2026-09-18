# instance.send lands on shell-pty native and its ledger row resolves honestly

Date: 2026-09-18 (UTC). Scope: how an `instance.send` built by the CLI reaches
a Claude agent on the native shell-pty carrier, and how the Hub command ledger
resolves — inside the protocol's three-state Command model. No demo data dir or
ports are touched; the live portion ran against a throwaway `remuda dev` server
with its own data dir and high loopback ports. Paths are redacted (`<HOME>`,
`<SCRATCH>`).

## Commit anchor

The behavior described here is reproducible at commit
`ed03b645e655a619e7cc48ee3ce31af2503f58a4` (`fix(hub): settle rejected sends
inside the three-state command model`), which is on branch
`wt/c-send3/b-send3-md`; the CLI resolution change follows it in
`d1479bbf…`. An earlier draft of this document pinned a sha that was rebased
away; this revision re-anchors to a sha on the branch and re-runs every
deterministic check it cites at that tree.

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
TS, this round uses only what §2.5 already allows — and the distinction the
protocol actually draws:

| Observation | Wire resolution | Why |
| --- | --- | --- |
| Node replies with an explicit error | `state=settled`, `resolution=clear`, `settlement.outcome=rejected`, reason in `settlement.reason` | Positive evidence the command did **not** run — the §2.5 "派发前拒绝" edge. |
| Node accepts the RPC | `state=accepted`, then the journal settles it | The normal path. |
| Node receives the call but the reply is lost / Node never answers | `state=queued`, `resolution=reconciling` (or `unknown`) | A forward intent exists, so §2.5 forbids expiring or rejecting without asking the Node, and without non-execution evidence it cannot be called rejected. The mirrored journal converges it. |

The decisive point is that **the lost-reply case is not expressible as
`expired`/`rejected`**: §2.5 line 337 says "无法证明没执行时也不能标 rejected；
超时查询返回 unknown". So the round-2 bounded ack deadline — which settled a
possibly-delivered send on the Hub's own timer — is removed, along with its
`commandSettleTimeoutMs` config. A forward intent is durable and never
self-expired; the Node's journaled `accepted`/`settled` (which carries the
outcome) is the authority. There is therefore no Hub-side fail timer any more:
the `completed`/`rejected` outcome arrives either on the synchronous RPC error
reply or on the mirrored journal.

The `reason` is not a top-level wire field. It rides the settlement as
`settlement.reason` (projected from the Node entity's
`settlement.value.error.message`), present only for `outcome=rejected`. Two
internal SQLite columns (`settlement_outcome`, `settlement_reason`) hold the
projection and are `#[serde(skip)]` so they never appear on the wire.

`mark_settled` — the path the Node's own settle frame takes (`ws.rs`) — now
records `outcome=completed` and clears any reason, and a terminal settlement is
never regressed by a later accept/settle. This closes the round-2 hole where a
deadline-failed row could be flipped to `settled/clear` while keeping its
failure reason.

## Deterministic evidence (run at ed03b645)

All commands below use the OpenSSL-3 devbox toolchain; output trimmed to the
relevant lines.

- Driver, fake harness (the send actually lands and the mid-turn hold works):

  ```
  $ cargo test -p remuda-driver --test shell_pty_fake_send
  test a_send_lands_when_idle_and_is_held_until_a_running_turn_ends ... ok
  test result: ok. 1 passed; 0 failed … finished in 15.44s
  ```

- Hub, explicit Node rejection settles `rejected` (not a fourth state), and is
  listed by the new route:

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

- Store projection and terminality:

  ```
  $ cargo test -p remuda-hub --lib \
      a_never_acked_send_rests_unknown_until_the_journal_settles_it \
      a_rejected_send_settles_rejected_and_a_later_settle_frame_keeps_it
  test store::tests::a_never_acked_send_rests_unknown_until_the_journal_settles_it ... ok
  test store::tests::a_rejected_send_settles_rejected_and_a_later_settle_frame_keeps_it ... ok
  ```

  The first drives a queued/reconciling row through a journaled `accepted`
  then `settled` entity carrying
  `{state:"known",value:{outcome:"completed",…}}` and asserts the projected
  `settlement.outcome=completed` and that the internal columns
  (`settlementOutcome`/`settlementReason`/`reason`) are absent from the
  serialized row. The second asserts a rejected settlement is terminal: a later
  positive settle frame and a late accept leave it `rejected` with its reason,
  while a clean completion on a different command records `completed` with no
  reason.

- OpenAPI / wire drift guard (`tests/openapi.rs`):

  ```
  $ cargo test -p remuda-hub --test openapi
  test command_record_schema_matches_the_serialized_struct ... ok
  test openapi_is_31_and_covers_source_routes_and_methods ... ok
  test overlapping_web_client_operations_exist ... ok
  ```

  The route scan now carries the HTTP **method** (it parses each `.route(`
  call's method-router services, with paren/string/comment-aware splitting) and
  asserts both directions against the spec, so a method change is caught. A
  fully-populated `CommandRecord` is serialized through
  `store_test_support::sample_command_record_json` and its property set diffed
  against the documented schema; the test also pins the `state` enum to exactly
  `accepted|queued|settled`, `resolution` to `clear|reconciling|unknown`, and
  asserts the internal columns are not documented wire fields. `cargo test -p
  remuda-hub` (all targets) and `cargo test -p remuda` are green at this tree,
  as is `pnpm run typecheck` for the regenerated `api.generated.ts`.

## Live happy path (shell-pty native), re-anchored

The live observations from the first revision concern paths this round does
not change — the block-fold that makes the stdio Node accept the send, the
mid-turn hold, and the journaled run of `queued → accepted → settled`. They
remain reproducible at `ed03b645` and are summarized here rather than re-run
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

`remuda instance send` resolves a *forwarded* row by polling the ledger for the
Node's journal to converge a lost reply, but it short-circuits immediately when
the POST returned `forwarded=false`: on an offline host no forward intent
exists, no timer is armed, and polling cannot change the outcome, so it prints
the honest `queued` row at once instead of waiting out the full window. A
rejection's message is surfaced from `settlement.reason`.

## Web

The generated client (`web/src/lib/api.generated.ts`) is regenerated from the
updated OpenAPI: `CommandRecord.state` is the `accepted|queued|settled` union,
`resolution` is `clear|reconciling|unknown`, and `settlement` is optional with
`outcome` and `reason`. Because the Hub no longer emits a fourth state, the
hand-written coercion in `web/src/lib/api.ts` (allowlists of those same three
states / three resolutions) is now exactly consistent with the wire — the
round-2 "unknown state renders as accepted" follow-up no longer has a triggering
input. Surfacing the rejection outcome/reason in the UI is left to the web
owner; this worker's ownership is the generated client.
