# D-047 / D-048 node half: per-instance API relay lifecycle

Date: 2026-09-19. Scope: the `remuda-node` half of the in-band model-API relay
(`crates/remuda-node/src/api_relay/`).
Design references: [D-047 / D-048](../decisions.md),
[api-routing design](../api-routing.md),
[Amendment A1 (route auto + direct-net)](../api-routing.md).

This document records what the node-side code does and, for each security
statement, the test that proves it. The protocol frames, timeout ladder and
credential rules are defined in the design; this note is about lifecycle,
flow control, and the credential-handoff consumer on the node.

## The two roles

A Node can play both relay roles at once, for different instances:

* **worker (W)** — for an instance whose route is `via`, the Node binds one
  loopback-only HTTP listener to `127.0.0.1:0`, mints a 32-byte bearer,
  writes both into the instance's 0600 settings overlay, and forwards accepted
  requests either as `api.*` frames on the existing Hub↔Node link
  (`hub-relay`) or as a direct streaming HTTP call to H's optional relay
  listener (`direct-net`). The worker listener is the only listener that
  exists by default, and it only ever binds loopback.
* **proxy (H)** — an optional listener bound solely to an operator-configured
  `Host.relayBind`, whose address must be loopback or private (the bind
  policy refuses `0.0.0.0`/`::` and public addresses), with per-peer
  allowFrom CIDR rules. It runs the pinned-origin egress with no frame layer.

## Listener lifecycle and idempotent create

The lifecycle is deliberately one-listener-per-instance, and it is decided on
the accept path **before** the create command is journaled:

1. `provision_for_request` validates the one shape the wire leaves to the
   projector (`via` requires a `viaHostId`; a nameless `via` is refused, never
   projected as direct), then for `route: auto` runs the 3 s direct-net
   probe. The observed route is chosen once and echoed back; a later
   direct-net failure never downgrades or reroutes the instance.
2. The listener and its observed route are registered in a **single**
   registry insert. A previous entry for the same instance id is shut down by
   the insert itself, so a bind can never evict a live listener while leaving
   its axum task holding the port and bearer. A proxy entry (route-less) is
   not reusable for a worker provision and yields a `NodeError`.
3. A `ProvisionGuard` is handed to `create_instance`; the worker task commits
   it once spawned, and any failure before that (including a driver-build
   error such as the generic-pty refusal below) revokes the bearer and shuts
   the listener. The worker terminal block and instance purge revoke
   idempotently at exit.
4. Retried `instance.create` is idempotent in both forms:
   * same `commandId` + payload: the recorded accepted command, instance and
     observed route are echoed;
   * same client-allocated `instanceId`, new `commandId`: the fresh command
     id is accepted, saved and journaled to `accepted` before the reply, and
     the first attempt's listener is reused.
   A racing exact duplicate that loses the `insert_command` race during the
   up-to-3 s probe receives the idempotent reply rather than a `Conflict`
   after the instance row exists.
   Evidence: `retried_create_keeps_one_listener_and_exit_revokes_it` and
   `concurrent_creates_with_one_command_id_both_return_the_accepted_one`.

## Credit window and hard deadline

`api.chunk` (response) and `api.body` (request) are data frames gated by a
per-stream semaphore with an initial window of four permits. `api.credit`,
`api.head`, `api.cancel` and `api.end` are control frames and bypass the
window entirely — a credit answers for bytes the consumer already drained, so
gating it would deadlock the producer it is trying to unblock.

Every gated `send_gated` races three futures:

1. permit acquisition (then the actual channel send),
2. the stream's cancellation `Notify` (client disconnect, remote cancel,
   error `api.end`, egress revoke),
3. `sleep_until(hard_deadline)` — the stream's hard cap from the negotiated
   egress timeouts, so a consumer that stops acknowledging cannot park the
   proxy task holding the gateway connection for 30 minutes.

On H the `EventRouter` is the only thing that turns inbound credits into
permits, so it stays alive through the tail flush and the zero-byte terminal
`api.chunk`; it is aborted only after every terminal send settles. The tail
flush increments `seq`, and the terminal chunk takes the next one — `seq` is
the ordering key the Hub validates, so a repeated seq looks like a duplicate
chunk. Evidence: `producer_waits_for_credits_then_moves`,
`credit_starved_egress_ends_at_the_hard_cap_after_four_chunks`,
`delayed_credits_keep_a_long_response_clean_to_its_last_chunk`, and the
strictly-increasing chunk-seq assertion in
`hub_relay_streams_sse_with_credential_swap_in_order`.

## `api.egress`: the credential only ever lives on H

Credentials never ride `api.open`. The Hub installs the proxy's per-instance
gateway context with a Hub→Node `api.egress` notification when the route is
decided at launch and again after every reconnect, and sends `revoke: true`
on instance exit or when the route goes down. The node consumer:

* parses the notification in the `api.*` demux (bypassing the RPC dispatch
  table), installs the context in an in-memory map keyed by instance id, and
  renders the auth token redacted in `Debug` — it is never persisted or
  logged;
* on revoke removes the context from memory and ends the instance's live
  proxy streams with a terminal `api.end`;
* serves an `api.open` for an instance with no installed context with
  `destination-refused`, before any socket is opened.

Until the sibling `c-apiroute-hub` branch lands, `METHOD_API_EGRESS` and
`ApiEgressParams` mirror the protocol types locally in
`crates/remuda-node/src/api_relay/mod.rs` (they mirror
`remuda_protocol::hubnode::{METHOD_API_EGRESS, ApiEgressParams}`); adopting
the protocol types after merge is a one-commit switch. Evidence:
`api_open_before_egress_is_refused_after_install_succeeds_and_revoke_ends`.

## Origin and credential confinement

The egress constructs the gateway URL solely from the pinned context; frame
paths are validated against traversal/backslash escapes, redirects are
disabled, request/response headers cross allowlists (`set-cookie`,
`cookie`, client `authorization` and `x-api-key` are dropped), and the
profile credential is installed only on H. Reqwest error text carries the
request URL, so it is logged on H only; W receives fixed messages and stable
codes from the typed ladder (`destination-refused` 502,
`via-host-offline` 503, `upstream-timeout` 504). Evidence:
`hub_relay_streams_sse_with_credential_swap_in_order` (value-free FakeGateway
credential verdicts), `egress_refuses_path_escape_and_missing_context`,
`gateway_failure_body_never_carries_the_pinned_origin_to_w`.

## `apiRoute: via` is refused on `generic-pty`, never downgraded to direct

A `generic-pty` carrier runs the CLI with the host operator's own
environment and owns no sealed per-instance settings, so routing a `via`
model-API session through it would hand the harness the host's gateway base
URL and credential while the Hub record still claimed the relay — the exact
D-035 "silent reroute" lie. An instance create whose resolved route is
`via` and whose driver resolves to `generic-pty` is therefore **refused at
launch with a `DriverError` naming the unsupported combination**; the
provisioned listener is released through the build-failure revoke path, no
CLI is started, and the launch is never downgraded to direct. Evidence:
`native::tests::via_route_is_refused_for_generic_pty_never_rerouted_to_direct`.

## Negotiated limits

The node adopts `hello.limits.maxApiStreams` and `hello.limits.apiChunkBytes`
at every hello site (wss, stdio, daemon, ssh carrier). The caps govern the
broker stream tables (per-link 8 default, per-instance 2) and both the W
inline/`api.body` split and the H coalescing threshold (clamped to the
negotiated chunk size), so a Hub that lowers either value never receives
oversized frames or more live streams than it advertised. Evidence:
`hello_limits_lower_chunk_size_and_stream_cap`.
