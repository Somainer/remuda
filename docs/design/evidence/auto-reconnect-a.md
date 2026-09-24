# Evidence: auto-reconnect Task A (client connection machine + offline outbox)

Date: 2026-09-24. Branch: `wt/c-authrace/b-reconna-md`.
Design: coordinator brief `auto-reconnect.md` §3 / §6.1; ADR [D-055](../decisions.md);
spec change [hub-resilience.md §5.4/§7](../hub-resilience.md).

## What shipped (client half; Task B is separate)

- New `web/src/lib/connection.ts`: four-state machine (`live` / `stale` /
  `offline` / `recovering`), full-jitter backoff capped at 30 s, 20 s
  recovering watchdog, triggers on socket close/frame, `visibilitychange`,
  `pageshow(persisted)`, `online`/`offline`, and a throttled focus probe.
  It can never stay in `recovering`/`reconnecting`.
- New `web/src/lib/outbox.ts`: `cmd_`+canonical UUIDv7 ids, write-before-POST
  persistence (IndexedDB, localStorage fallback), per-instance serial flush
  under a Web Locks guard, ≤20 attempts / ≤24 h envelope, and bootstrap
  restoration of unresolved rows.
- `store.ts`: `send`/`flushHeld` route through the outbox; same-id retry on
  network/5xx/503; 409 → unknown; other 4xx → rejected; offline steer
  degrades to a queued turn; `cancel` is refused offline (`canInterrupt`);
  journal success clears a row even while its Hub command reads queued; a
  bounded live-retry timer re-attempts transient failures without waiting for
  a link toggle; a `pagehide`/`beforeunload` guard stops the unloading page
  from starting a competing POST that the reloaded page would duplicate
  (the durable row is the resumed page's only source of truth). A 2xx that
  was **forwarded to the Node** (`transport-written`) is marked done even when
  the server row stays queued/reconciling — the fake node's non-durable
  `{ok:true}` ack used to leave the row pending, and a later flush re-POSTed
  it (the hub-live steer ordering regression, found via the Playwright trace
  showing two same-id `hold-working` POSTs); only a queued **un-forwarded**
  row (host offline) is kept pending for same-id re-forwarding. Steer rows
  flush ahead of ordinary queued rows so a 插队 lands before the held queue.
- `api.ts`: `rest()` aborts reads at 10 s / writes at 15 s (caller signal
  wins, e.g. the 65 s doctor probe) and raises `RestNetworkError`; `command()`
  and `instanceSend` carry the client `commandId`; `eventsSubscribe` exposes
  `onClose`/`onFrame` hooks.
- `journal.ts`: each `resumeAfterReconnect` gets a generation — a slower
  overlapping read (success or failure) cannot overwrite the newest resume.
- Screen ordering (`screenCommitPatch` + the per-instance reconciliation
  chain): an unseen screen present only in the REST seed still orders a live
  RPC buffer, and catch-up is serialized before screen reconciliation.
- `commandStatus.ts`: new `pending-offline` (待发送（离线）) and
  `send-rejected` (未送达) rows; unconfirmed is narrowed; the explicit
  re-send chip only shows for the narrowed unknown set.
- UI: `JournalBanner` renders offline/recovering/pending-count copy and stays
  silent for stale; `ConnectionIndicator` gains the stale state; Shell /
  PhoneShell foreground resume drives the machine. `Composer.tsx`,
  `SessionPage.tsx`, `hub-live.spec.ts` were not modified (§6.3 boundary).

## G2 (command status GET)

Task B landed on main (`1c271528`) and is used directly (not feature-detected):
the store calls `GET /v1/instances/{id}/commands/{commandId}`
(`api.instanceCommandStatus`) after a network-shaped POST failure that may
have delivered — accepted/settled/forwarded → done, rejected → rejected,
inconclusive → same-id retry. Same-id re-POST also benefits from Task B's
Hub-side forwarding of a previously un-forwarded queued row (G1). `instance.configure`
replays remain out of the outbox by design (only sends are exactly-once).

## Verification

- Unit: `pnpm -C web typecheck`, lint, and `test -- --run` — 1680 tests
  pass. New pins cover the UUIDv7 shape/timestamp, the outbox order/lock/
  restore, the machine's immediate foreground reconnect / watchdog /
  backoff / stale-probe, overlapping resume generations both ways, the
  seeded-screen ordering, offline cancel refusal, offline steer
  degradation, and the narrowed command-status projection.
- E2e (`web/tests/e2e/offline-outbox.hub.spec.ts`, gated): two offline
  messages queue with zero Hub commands and deliver exactly once after
  `context.setOffline(false)`; an offline-queued message survives a reload
  during the outage; a POST delivered to the Hub with a dropped response is
  retried under the same commandId and executes once.
