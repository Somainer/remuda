# c-apiroute-launch: the worker overlay, the truthful route echo, and the
# gateway-token exclusion

Date: 2026-09-20. Scope: task `c-apiroute-launch` (plan §C.4) — the overlay W
writes for a `via` launch, the Node-reported `apiRoute` write-back, and the
redaction guarantees around both.
Design references: [D-047 / D-048](./decisions.md),
[api-routing design](./api-routing.md), Amendment A1, and the sibling evidence
[api-routing-node-1](./evidence/api-routing-node-1.md).

## What this task changed

* **No overlay API change.** `write_claude_provider_overlay`
  (`crates/remuda-driver/src/profile.rs`) keeps the same gateway/direct shape;
  for a `via` launch the Node passes the per-instance loopback listener URL as
  `base_url` and the minted per-instance relay bearer as `secret`. The new docs
  on `ClaudeProviderOverlay`, `claude_provider_settings_json`,
  `merge_provider_overlay_over_user`, `OVERRIDDEN_ENV` and `redact_settings`
  record that the relay rides the ordinary gateway path — which is why the
  host's `ANTHROPIC_*` eviction and the model pin work unmodified.
* **Overlay input selection** lives in
  `crates/remuda-node/src/native.rs::resolve_claude_overlay`: when a
  `RelayOverlay` is present it is the *only* provider source written — the
  delivered `providerAuthToken` and gateway origin are never read on that
  branch — and the file stays 0600. Direct mode is byte-for-byte unchanged.
* **Truthful echo.** The Node's create result carries the observed
  `ApiRoute` (`direct-net` / `hub-relay`, absent for direct). The Hub stores
  only that Node-reported value on the instance projection
  (`project_echoed_api_route` → `reconcile_instance_api_route`), validates it
  against the requested route, and attaches its own registry label; the
  requested route on the spec is never copied. A `direct-net` the Node cannot
  establish fails the create before the instance row exists, with the stable
  `api-via-unreachable` code — never a silent hub-relay/direct reroute.

Every credential in these tests is a synthetic, delimited `fake-` value.

## Evidence 1: W's overlay carries the listener and the bearer, never the
## gateway credential

Node overlay test (`crates/remuda-node/src/native.rs`) — the request *does*
carry a delivered gateway token; the assertion is that even present it never
reaches any file under the launch root, while the env values are exactly the
loopback URL and relay bearer, mode 0600:

```
$ cargo test -p remuda-node --lib native::tests::via_route_materializes_loopback_overlay_with_relay_bearer
test native::tests::via_route_materializes_loopback_overlay_with_relay_bearer ... ok
test result: ok. 1 passed; 0 failed
```

Driver-level shape, eviction and pin tests
(`crates/remuda-driver/tests/provider_overlay.rs`):

```
$ cargo test -p remuda-driver --test provider_overlay
test writes_gateway_overlay_0600_without_leaking_debug ... ok
test writes_direct_overlay_with_api_key ... ok
test via_mode_writes_listener_url_and_relay_bearer_without_gateway_credential ... ok
test via_overlay_evicts_host_anthropic_keys_and_the_pin_wins ... ok
test no_launch_artefact_or_log_carries_the_gateway_token ... ok
test result: ok. 5 passed; 0 failed
```

`via_overlay_evicts_host_anthropic_keys_and_the_pin_wins` seeds a host layer
with its own base URL, auth token, API key and four model-naming variables,
merges the relay overlay over it, and asserts: the merged env points at the
loopback listener with the relay bearer; no host endpoint/credential/model key
survives; after `apply_model_pin` the settings `model` is the dispatch pin and
the relay route (URL + bearer) is untouched.

## Evidence 2: no launch artefact or log carries the gateway token

The redaction sweep tests walk **every file** a launch produced under the
scratch root — the 0600 `settings.json`, the merged launch settings, the
redacted settings render, the debug log line the native carrier emits, and the
`Debug` render of the overlay inputs:

```
$ cargo test -p remuda-node --lib native::tests::via_launch_artefacts_never_carry_the_gateway_token
test native::tests::via_launch_artefacts_never_carry_the_gateway_token ... ok
test result: ok. 1 passed; 0 failed
```

Assertions in the sweep: the synthetic gateway token, the gateway origin, the
host's own credential and the host gateway origin appear in **no** artefact;
the relay bearer appears only in the two settings documents (0600), never in
the `.log`/`.redacted`/`Debug` artefacts (it rides `ANTHROPIC_AUTH_TOKEN`,
which `redact_settings` masks, and `RelayOverlay`/`ClaudeProviderOverlay`
`Debug` redact it). The equivalent cross-layer sweep in the driver crate is
`no_launch_artefact_or_log_carries_the_gateway_token` (output above).

## Evidence 3: the instance projection records the route the Node reported

Requested `auto` against a proxy host advertising a `relayBind`; the Node's
probe fails and it echoes its one-time decision `hub-relay`. The Hub e2e
(`crates/remuda-hub/tests/api_relay.rs`) reads the real
`GET /v1/instances/{id}` projection and asserts `apiRoute` is
`{mode:"via", route:"hub-relay", viaHostId:<H>, viaHostLabel:"relay-proxy"}` —
never the requested `auto`, which exists only on the stored spec. The raw
response body is additionally asserted free of the synthetic gateway token,
which rides only the out-of-band `api.egress` notification to the proxy:

```
$ cargo test -p remuda-hub --test api_relay instance_projection_carries_the_node_reported_hub_relay_route
test instance_projection_carries_the_node_reported_hub_relay_route ... ok
test result: ok. 1 passed; 0 failed
```

## Evidence 4: an unhonoured direct-net refuses before any instance exists

```
$ cargo test -p remuda-node --lib api_relay_launch_test::direct_net_probe_failure_refuses_without_creating_an_instance
test runtime::api_relay_launch_test::direct_net_probe_failure_refuses_without_creating_an_instance ... ok
test result: ok. 1 passed; 0 failed
```

The create error names `api-via-unreachable`, no listener is registered for
the instance id, and the store has no instance row: refusal precedes the
insert (D-035 — refuse, never reroute).
