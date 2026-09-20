# D-047 operator surface (CLI + web): refusal transcript and session strip

Date: 2026-09-20. Scope: task c-apiroute-cli-web — the operator-facing pieces of
D-047: `remuda dispatch --api-via/--api-route`, `remuda profile delivery`, the
`remuda watch` ROUTE column, `remuda instance show`, and the web Provider form
and Session strip. Design: [api-routing](../api-routing.md), decisions
[D-047](./decisions.md), [D-035](./decisions.md) (no silent substitution).

The relay itself (frames, egress credential, listener) was delivered in
c-apiroute-hub / c-apiroute-node and is out of scope here; see
[api-routing-node-1.md](./api-routing-node-1.md).

## CLI refusal transcript

A refusal prints the Hub's stable `api-via-*` code on the first line and exits
non-zero; the CLI never retries another route. Captured against the in-process
fake Hub (`cargo run -p remuda-hub --example hub_e2e`, one relay-capable fake
Node). Host ids are random UUIDv7s of an ephemeral Hub; no hostnames or home
paths appear.

Unknown proxy host → 400 `api-via-unknown-host`:

```text
$ remuda dispatch --project prj_… --brief brief.md --model passthrough/tr/auto \
    --api-via hst_00000000-0000-7000-8000-000000000000
Error: hub HTTP 400: api-via-unknown-host
  apiVia names unknown host hst_00000000-0000-7000-8000-000000000000
exit=1
```

`direct-net` to the Hub host (it binds no relay) → 409 `api-via-unreachable`:

```text
$ remuda dispatch … --api-via self --api-route direct-net
Error: hub HTTP 409: api-via-unreachable
  route direct-net to the Hub host is impossible: the Hub binds no relay; dispatch with route hub-relay
exit=1
```

Enrolled proxy host whose link is down → 409 `api-via-host-offline`, before any
name/port/worktree allocation (the dispatch below created no worker):

```text
$ remuda dispatch … --api-via hst_0188…-2b7…
Error: hub HTTP 409: api-via-host-offline
  apiVia host hst_0188…-2b7… is offline
exit=1
```

Local validation fails before anything is posted — a typo can never be read as
"no override":

```text
$ remuda dispatch … --api-via sideways
Error: --api-via "sideways": missing ID prefix
exit=1
$ remuda dispatch … --api-via self --api-route tunnel
Error: --api-route must be auto, hub-relay or direct-net: unknown variant `tunnel`, expected …
exit=1
$ remuda dispatch … --api-route hub-relay
Error: --api-route requires --api-via to name a proxy host
exit=1
```

A valid `--api-via none` (force direct) and `--api-via self --api-route
hub-relay` are accepted; the automated cases are
`crates/remuda/tests/dispatch_cli.rs::api_via_*` and
`crates/remuda/tests/profile_cli.rs::profile_delivery_mutation_and_display`.

## Web Session strip

The strip is fed only by the instance's Node-echoed `apiRoute`. Screenshots
from `web/tests/e2e/api-route.hub.spec.ts` (REMUDA_EVIDENCE=1, a dispatched
via session against the fake Hub):

* `api-route-strip.png` — `经 e2e-via-host Hub 中转` in the run-details
  diagnostics (`data-testid="session-api-route"`, `data-mode="via"`,
  `data-route="hub-relay"`).
* `api-route-down.png` — after the proxy Node's link drops, the error banner
  (`role="alert"`, `data-testid="session-api-route-down"`) shows
  `API 路由已断开（api-route-down）` while the route clause still names the via
  hub-relay route. It is a down route, not a reroute (D-035); reconnecting the
  proxy does not clear the block — the operator re-dispatches.

The clause wording: `直连`, `经 <host label> 直连网络`, `经 <host label> Hub
中转`; the host label comes from the echo, falling back to the id, and the
Hub host reads `Hub 主机`. Pure-function tests for the wording and the
down-diagnostic scan live in `web/src/lib/apiRoute.test.ts` and
`web/src/pages/SessionPage.api-route.test.tsx`.

## Harness knob

`crates/remuda-hub/examples/hub_e2e.rs` gains the smallest possible fixture
for this surface, documented at the top of the spec:

* `HUB_E2E_API_ROUTE=1` — the fake worker advertises `capabilities.apiRelay`
  (absent by default, so other specs still hit `api-via-unsupported` like a
  pre-D-048 Node), its `instance.create` result echoes the requested
  `apiRoute` (auto resolves to hub-relay, matching the Hub's no-relayBind
  decision), it announces one branded-id workspace and answers
  `worker.provision`/`worker.remove`; a second fake Node labelled
  `e2e-via-host` enrolls as the proxy host `H`.
* The port-scoped gate file `$TMPDIR/remuda-e2e-route-down-<listen-port>`
  (same convention as the existing rpc gate) holds the proxy WebSocket closed
  while it exists, so the Hub's link-lost hook blocks the via worker with
  `api-route-down`; deleting it reconnects the proxy.
