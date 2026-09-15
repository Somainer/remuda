# Evidence: coordinator batch 3 — model supply scheduler (`co-supply`)

Date: 2026-09-15
Branch: `wt/co-supply/model-supply-scheduler`
Design: `docs/design/coordinator-hierarchy.md` §4.2–§4.6, §5.6, §8.1 row 3.

## What landed

| Surface | File |
| --- | --- |
| Supply + TaskSpec wire types (§4.2/§4.3) | `crates/remuda-protocol/src/supply.rs` |
| Built-in capability table (family/class/context/effort, revisioned) | `crates/remuda-hub/src/model_catalog.rs` |
| Admission, rank, cooldown, 429/529 feedback engine | `crates/remuda-hub/src/supply.rs` |
| Hub-side usage consumer + `usage_events` table (§4.5) | `crates/remuda-hub/src/usage_store.rs` |
| Additive per-model supply fields | `crates/remuda-hub/src/provider_models.rs` |
| `supply_json` column, ledger table, store methods | `crates/remuda-hub/src/store.rs` |
| Supply REST + secret-less native profiles | `crates/remuda-hub/src/providers.rs` |
| Dispatch admission on instance create; cpu/mem/disk dimensions | `crates/remuda-hub/src/http.rs`, `crates/remuda-hub/src/placement.rs` |
| `remuda profile` CLI (`show`/`declare`/`event`/`usage`/`probe --dry-run`) | `crates/remuda/src/cmd/profile.rs` |
| OpenAPI + generated web client | `crates/remuda-hub/openapi/openapi.json`, `web/src/lib/api.generated.ts` |
| Protocol schema + TS types | `crates/remuda-protocol/schema/protocol.schema.json`, `web/src/types/generated.ts` |

## Policy mapping (design §4.4)

1. **Capability filter** — model `class` ≥ `minClass` (catalog first, declared
   `role` fallback, `workhorse:true` shortcut), `contextWindow` vs
   `expectedInputTokens` / `needsLongContext`, effort level membership.
2. **Supply filter** — state (`cooling`/`exhausted`/`unknown`), per-family
   window cooldowns, profile- and model-level `concurrency.max` counted from
   live instances across **all** hosts, host-scope matching,
   `coordinator-only` reserve for non-coordinator callers.
3. **Host filter** — unchanged `placement` (hard labels, project members,
   maxInstances, cpu/mem; disk budget enforced when a snapshot reports
   `diskFreeGb`).
4. **Rank** — declared `priority` → remaining window share → cooldown
   freshness → warm-cache affinity (latency-sensitive only) → cost
   (cost-sensitive only, no inverse-price weighting) → host load.
5. **Reservation** — admission happens in `create_instance`; a
   `placement_ledger` row stores the whole decision (`chosen`, `ranked`,
   `reasons[]`, `rejected[]`) for the audit trail and the future bot card.
6. **Fallback chain** — per-model ordered `fallback` declared on the catalog
   (es1 → seed in the 09-14 shape); it is selection order, not an automatic
   retry: switching models is a new §4.6 solve (clean slate / switch-model).
7. **Cooldown with backoff** — unit is `(supplyId, windowId)`; known
   `resetsAt` parks exactly to it; otherwise 60s → 2m → 5m → 15m capped
   exponential backoff (`BACKOFF_LADDER_SECS`).
8. **Backpressure = explicit deferred** — no admissible candidate returns
   `SUPPLY_DEFERRED` (HTTP 429) on dispatch with the full decision JSON; a
   planning dry-run (`/v1/supply/resolve`, `profile probe`) returns 200 with
   `deferred:true`, `deferredUntil`, and every rejection reason. Nothing
   downgrades below `minClass` silently.
9. **Sticky session** — running instances keep their
   `(host, supply, model)`; this batch supplies the admission data batch 5
   (`worker.switch-model`) will consume.
10. **Fairness** — declared priority order is honored first; window share is
    the second key, which is the cross-project weight hook for batch 4.
11. **Hard limits in code** — `concurrency.max` on profiles/models plus the
    existing host `maxInstances`/project `maxConcurrentWorkers`.

## Feedback loop (§4.5 / §4.6)

- **observed-textual** — every fresh `journal.append` is scanned by
  `supply::observe_journal_text`. `Request rejected (429) {"error_code":-2001}`,
  `session/weekly/Opus limit`, `too many requests` classify as
  `RateLimited`; the running model's **family** gets a `model` window
  (`appliesTo:[family]`) parked, so sibling families stay admissible.
- **529 / overloaded** — `classify_rate_signal` checks this first and returns
  `FleetOverloaded`; the profile records `lastError` but **no window is
  cooled** ("冷错了会白白烧掉另一份供给").
- **observed-structured** — `POST /v1/providers/{id}/supply/events
  {"type":"structured","windows":[…]}` feeds the same Codex-vocabulary merge
  (`usedPercent`, `resetsAt`, `windowDurationMins`) used by
  `apply_structured_windows`.
  - The Node does not relay Codex `account/rateLimits/updated` frames yet;
    `remuda-codex-wire` decodes the variant
    (`crates/remuda-codex-wire/src/notification.rs:106`) but zero hubnode
    frames carry it. The typed hook point when P6 wires that relay is the
    `report_supply_event` handler → `apply_structured_windows`; the merge
    semantics and tests are already in place. The vendored schema file cited
    in the design (`docs/research/cli-help/.../GetAccountRateLimitsResponse.json`)
    is not present in this tree; the `RateLimitWindow` shape follows the
    fields the design quotes (`usedPercent`, `resetsAt`,
    `windowDurationMins`) verbatim.
- **usage** — `kind:"usage"` journal events are projected once (dedupe on
  `(instance_id, seq)`) into `usage_events` with profile/model resolved from
  the instance row, and aggregated per `(profile, model)`; budget status uses
  the estimate × 1.15 band (`BUDGET_STOP_FACTOR`).

## Minimal declaration

A user can declare only "workhorse X, scarce Y":

```json
{ "models": [
  { "id": "gw/seed[1m]", "workhorse": true },
  { "id": "gw/es1[1m]",  "family": "es1", "priority": 20,
    "concurrencyMax": 4, "fallback": ["gw/seed[1m]"] }
] }
```

Everything else defaults: absent windows make a profile `unknown` state
(used only when explicitly prioritised/pinned or all windowed supply is
exhausted); a secret-less `kind:"native"` profile needs no auth token.

## 09-14 es1 incident replay

`crates/remuda-hub/tests/supply.rs::family_window_admits_sibling_account_window_parks_all`
and the CLI golden
`crates/remuda/tests/profile_cli.rs::profile_declare_event_and_probe_dry_run_against_hub`
replay the incident shape with synthetic fixtures (no real models):

1. `gw/es1[1m]` (family `es1`, priority 20, fallback `gw/seed[1m]`) 429s.
2. A family window `appliesTo:["es1"]` parks with cooldown; account state is
   `degraded`, not `cooling` — ordinary usage is not false-rejected.
3. Re-solving admits the `seed` sibling (priority 18) and rejects es1 with a
   `family es1 window 'model' cooling until …` reason.
4. A subsequent account-level `weekly` window (`appliesTo:["*"]`, 100%)
   parks **all** models and produces a deferred decision with
   `deferredUntil` = the reset.
5. A 529 between steps leaves every window untouched.

## Test evidence

Commands run from the worktree root:

```
cargo test -p remuda-protocol -p remuda-hub -p remuda        # all green
cargo clippy --all-targets -- -D warnings                    # clean
cargo run -p remuda-protocol --example gen_types             # generated files committed
cd web && pnpm gen:api                                        # api.generated.ts committed
```

Key suites (counts observed locally):

- remuda-hub lib: 104 unit tests, including the supply engine rank/admission/
  cooldown matrix (priority vs share, concurrency.max, minClass deferral,
  unknown-last-resort, pin, reserve, ladder 60→120→300→900, 529 no-cool).
- `crates/remuda-hub/tests/supply.rs` — 6 fake-node integration tests:
  catalog endpoint, synthetic 429 journal text → cooling + audit
  `supply.cooldown` + 529 no-op, family vs account window semantics,
  `concurrency.max` across **two** fake remote hosts while both are under
  their host maxInstances, dispatch `taskSpec` → `SUPPLY_DEFERRED` and
  successful sibling dispatch writing a `placement_ledger` row with
  `reasons[]`/`rejected[]`, and usage-event journal flow into the
  per-profile aggregation with the ×1.15 budget band.
- `crates/remuda/tests/profile_cli.rs` — help lists subcommands; the CLI
  golden exercises catalog → declare → textual 429 → 529 → probe dry-run
  (sibling wins) → frontier probe (explicit deferred JSON) → list.
- `cargo test -p remuda-hub --test openapi` — the hand-maintained spec covers
  every source route including the 5 new supply paths.

All tests use the fake node (`runtime.hello` + `journal.append` WS) or
in-process Hub + tempdir data dirs; no real model calls are made and no real
credentials appear anywhere.

## Follow-ups deliberately out of scope

- `worker.switch-model` confirmation dance (batch 5) consumes the chosen
  fallback; this batch only records the choice in the ledger.
- Task ledger / deferred queue persistence (batch 4's `placements` table);
  the supply-side `placement_ledger` already exists with the same JSON shape.
- Node-side relay of Codex structured rate-limit frames (P6); REST + merge
  semantics land now.
- `remuda profile probe` real 1-token upstream probe: this wave ships the
  admission dry-run; the network probe reuses `/v1/providers/{id}/test`.
