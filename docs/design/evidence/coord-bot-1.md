# Evidence — co-botfix: Feishu dispatcher blockers (design §5.1, batch 2)

- Date: 2026-09-15
- Branch: `wt/co-botfix/feishu-dispatcher-blockers`
- Baseline: `origin/main` @ `9df9099`
- Design: `docs/design/coordinator-hierarchy.md` §5.1 (blockers 1–3), §8.1 row 2
- Scope: bot token answers interactions on behalf of an allowlisted owner;
  journal → card mapping driven by the real `ObservationKind` vocabulary;
  card ↔ interaction tickets persisted in the Hub across dispatcher restarts.

All evidence is produced with the fake Node + DryRun `lark-cli` only. No real
Feishu network calls, no real credentials; open ids / chat ids / tokens are
synthetic fixtures.

## What changed

1. **Bot relay of owner answers (`crates/remuda-hub/src/interactions.rs`)**
   - `POST /v1/interactions/{id}/answer` now accepts `InputOrigin::Bot`
     exactly when all hold:
     1. the body carries `actingOpenId` (the human who clicked),
     2. that open id is registered on the **bot device's** allowlist in the
        Hub (`bot_owner_allowlist` table),
     3. an **open, unexpired** `card_tickets` row binds the interaction to
        that bot device.
   - Missing/unknown open id, unknown binding, or expired ticket → `403`.
     `InputOrigin::Agent` stays `403`; `InputOrigin::Human` is unchanged.
   - On the winning answer the Hub (not the client) flips the ticket to
     `answered` and appends an `interaction.bot-answer` audit row whose detail
     names `actingOpenId`, `ticketId`, `commandId`, `relayedBy: "feishu"`.
   - D-005/D-011 are unchanged: the bot never bypasses approvals — it is only
     the owner's courier, and an agent-relayed approval without the above stays
     untrusted.
2. **Real journal vocabulary (`crates/remuda-feishu/src/hub_api.rs`)**
   - `map_journal_event` now matches the real `ObservationKind` wire values:
     `tool_call` / `tool_result` → progress card; `lifecycle` entity events on
     `run`/`instance` in terminal states (`succeeded`/`failed`/`exited`/
     `cancelled`) → completion card (green/red); `interaction.requested` →
     interaction card. Native lifecycle events (model switch, prompts) never
     complete a run. The old `"tool"/"tool_use"/"tool-boundary"/"result"/
     "completion"` strings never existed on the wire and are removed.
   - `Knowledge`-shaped fields (`toolName`, `displayTitle`) accept both the
     bare-string and `{"state":"known","value":…}` encodings.
3. **Durable ticket bindings (`crates/remuda-hub/src/store_tickets.rs`,
   `crates/remuda-hub/src/bot/mod.rs`, `crates/remuda-feishu/src/tickets.rs`)**
   - One additive migration block in the Hub store (new
     `crates/remuda-hub/src/store_tickets.rs` module, wired from `store.rs`
     init) creates `card_tickets` (partial unique index: one open ticket per
     interaction per bot device; TTL columns) and `bot_owner_allowlist`.
   - Internal bot HTTP surface under `/v1/bot/*` (allowlist registration,
     ticket upsert/list/state/expire-session), Bot-token only, fail-closed.
     These routes live in `src/bot/mod.rs` and intentionally are not part of
     the web `openapi.json` surface (the web client never calls them).
   - `TicketStore` gains a write-through `TicketBackend`;
     `HubTicketBackend` is the Hub implementation, `NoopTicketBackend` keeps
     the existing unit tests sync. `Dispatcher::open_with_backend` +
     `hydrate_tickets()` rehydrate open bindings at boot. The 10–15 min TTL is
     enforced by the existing deadline logic; due rows are excluded from
     hydration.
   - Composition root (`crates/remuda/src/cmd/dispatcher.rs`) registers the
     owner allowlist before supervise and hydrates tickets on boot.

No role/delegation is stored on tickets or the allowlist (2026-09-15 design
amendment: tiers are looked up through the Hub, never denormalized here).

## Tests

- `crates/remuda-feishu/tests/hub_dispatcher.rs` (in-process Hub + fake WS
  Node, DryRun outbound):
  - `real_tool_call_observation_posts_golden_progress_card` — a real
    `tool_call` journal observation posts the golden progress card.
  - `tool_result_and_terminal_lifecycle_post_golden_cards` — `tool_result`
    refreshes progress; a native lifecycle posts nothing; terminal `run`
    (succeeded) and `instance` (failed) lifecycle events post the green/red
    golden completion cards.
  - `bot_token_relay_requires_allowlisted_open_id_and_open_ticket` — bot
    relay: 403 with no open id, 403 for a non-allowlisted open id, 403 with
    an allowlisted id but no open ticket, then 200 once the ticket exists; the
    audit row names the owner open id; a repeat answer does not double-write.
  - `dispatcher_restart_keeps_ticket_bindings` — dispatcher gen 1 issues an
    interaction card; the process dies; gen 2 (fresh in-memory state, same Hub
    and session db) hydrates exactly one open ticket and the owner's card
    click completes with an audit row naming the owner.
- All existing feishu/hub suites pass (72 feishu tests incl. 14 ticket mapping
  tests; hub 171; remuda dispatcher tests; remuda-node interaction tests).

## Golden card JSON (redacted, exact byte output from DryRun `--content`)

Progress card (from `tool_call` `Read` / `README.md`):

```json
{
  "schema": "2.0",
  "config": {
    "compact_width": false,
    "update_multi": true,
    "streaming_mode": false,
    "enable_forward": false
  },
  "header": {
    "template": "blue",
    "title": { "tag": "plain_text", "content": "Running", "text_align": "left" }
  },
  "body": {
    "elements": [
      {
        "tag": "markdown",
        "element_id": "progress_md",
        "content": "**Tool:** Read\n\nREADME.md\n\nElapsed: 0s",
        "text_align": "left"
      }
    ]
  }
}
```

Completion card, run `succeeded` / reason `run-complete`:

```json
{
  "schema": "2.0",
  "config": {
    "compact_width": false,
    "update_multi": true,
    "streaming_mode": false,
    "enable_forward": false
  },
  "header": {
    "template": "green",
    "title": { "tag": "plain_text", "content": "Done", "text_align": "left" }
  },
  "body": {
    "elements": [
      {
        "tag": "markdown",
        "content": "succeeded: run-complete",
        "text_align": "left"
      }
    ]
  }
}
```

Completion card, instance `failed` / reason `tool-exit-1` (red header):

```json
{
  "schema": "2.0",
  "config": {
    "compact_width": false,
    "update_multi": true,
    "streaming_mode": false,
    "enable_forward": false
  },
  "header": {
    "template": "red",
    "title": { "tag": "plain_text", "content": "Done", "text_align": "left" }
  },
  "body": {
    "elements": [
      {
        "tag": "markdown",
        "content": "failed: tool-exit-1",
        "text_align": "left"
      }
    ]
  }
}
```

Audit row written for the winning relay (`audit_log`, detail only — ids from
synthetic fixtures):

```json
{
  "action": "interaction.bot-answer",
  "subject": "int_01993ab0-0000-7000-8000-000000000002",
  "detail": {
    "actingOpenId": "ou_owner_aaaaaaaaaaaaaaaaaaaaaaaaaa",
    "ticketId": "tidgolden01",
    "relayedBy": "feishu"
  }
}
```

## Checks

- `cargo fmt --all`
- `cargo clippy -p remuda-hub -p remuda-feishu -p remuda --all-targets
  -- -D warnings` — clean
- `cargo test -p remuda-feishu`, `-p remuda-hub`, remuda dispatcher tests — pass
- `scripts/ci/secret-scan.sh` — pass
- No real network in tests (fake Node WS + DryRun lark-cli); no personal
  absolute paths/hostnames/tokens committed.

## Known follow-ups (later batches)

- `/project` routing and the three-card placement/decision/outcome split are
  batch 6 (`co-lanes`), per design §8.1; this batch keeps the single global
  `RouteDefaults`.
- The bot ticket routes could be folded into the main `openapi.json` if the
  web client ever needs to display card-broker state; today they are internal
  Hub machinery like Node RPC.
