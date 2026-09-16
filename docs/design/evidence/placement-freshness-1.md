# Placement freshness — no refusal on a stale CPU sample (1)

Date: 2026-09-16 · Branch: `wt/c-placement/fresh-resource-admission`

## Owner-visible symptom

Mac demo, 2026-09-16 ~08:0x. After a `cargo build` finished, **every** session
create returned `422 PLACEMENT_UNSATISFIABLE` with
`…: host CPU at 100% (limit 90%)` for the next ~20 minutes although the
1-minute load had already fallen to 6 on 14 cores. The grok smoke and the
native-carrier test both failed on it. The Hub host list kept showing
`resources.cpuPct = 100` until the Node was restarted and re-sent its hello.

## Root cause

- The Node's `cpuPct` is `loadavg()[0] / cpu_count`
  (`crates/remuda-node/src/inventory.rs`), collected with the full inventory
  probe at connect time.
- That snapshot is serialized into `WssConfig.host` **once**. The 15 s
  heartbeat already re-sent the whole host inventory — but it re-serialized
  the same frozen value, so the Hub kept overwriting `resources_json` with the
  connect-time reading.
- The Hub admission (`placement.rs` `RESOURCE_CPU_PCT_MAX = 90`) read the
  persisted value unconditionally: a fossilized 100 excluded the host on every
  create, and the only refresh was a reconnect.
- An explicitly requested `hostId` was refused by the same check — there was
  no distinction between auto-placement guessing and an operator pinning a
  host.

## Fix

1. **Node — periodic fresh samples.** `inventory::sample_resources()`
   re-reads loadavg and meminfo directly (the full probe cache and its CLI
   subprocesses are bypassed). The 15 s heartbeat piggybacks a fresh
   `resources` block (also refreshed on hello/reconnect). Documented on
   `ResourceReport`: the figure is the **1-minute** load average — index 0 of
   `/proc/loadavg` on Linux, the first numeric token of
   `sysctl -n vm.loadavg` on macOS (same `{1m 5m 15m}` order). 5/15-minute
   values are deliberately not used.
2. **Node — on-demand `host.resources` RPC** returning one uncached sample,
   wired through all four Hub→Node dispatch paths (wss runtime, stdio codec,
   node server, daemon runtime link).
3. **Hub — fresh-before-refuse.** The Hub stamps every persisted resources
   object with `sampledAt` (Hub clock — immune to Node clock skew).
   `pick_hosts` treats a sample older than `resourceSampleMaxAgeMs`
   (60 s default) as stale, asks the live Node for a fresh sample
   (`resourceRefreshTimeoutMs`, 1 s default; ≤8 concurrent refreshes), and
   refuses only on fresh data. If no fresh reading arrives (old Node,
   timeout, socket error) the stale sample is dropped for that one decision —
   nobody is 422'd on a number that could not be re-confirmed — while the
   persisted row is left untouched for the operator UI.
4. **Pin = warn, not refuse.** Auto/labels/project placement keeps the hard
   refusal; an explicit `hostId` admits over the limit with `warnings[]` on
   the create/fleet response and a journaled
   `placement_resource_warning` Hub diagnostic.
5. **Visibility.** `GET /v1/hosts` exposes `resources.sampledAt`; the web
   host cards show the sample time; OpenAPI gains a `HostResources` schema
   and `warnings` on `InstanceCreateResult`/`FleetCreateResult`; the Rust
   hub client and the generated web client are regenerated.

### Coordination with co-dispatch (`remuda hostcap`)

This branch was rebased after co-dispatch merged to `origin/main`
(`dac89826`, route `GET /v1/hosts/{id}/hostcap`, CLI `remuda hostcap`).
The integration is additive on both sides:

- hostcap already reads `host.resources` verbatim, so `cpuPct`/`memPct`
  now move on their own and the co-dispatch-added `loadAvg1`/
  `diskFreeGb` fields come out of the same fresh sampler;
- the hostcap response gains `sampledAt` and `sampleAgeSec` so the CLI
  shows whether the reading is live or a fossil;
- an explicit `dispatch --host` pin over the ceiling returns
  `warnings[]` (journaled like the plain create pin) instead of 422.

## Before/after on this host (2026-09-16)

Shared 64-core devbox; to make the spike local and visible to the sampler
without starving the other 15 logged-in users, every process in the pair
(Hub, Node, burners) is pinned to an 8-CPU cpuset (`taskset -c 0-7`), so
`available_parallelism()` reports 8 and loadavg is read against 8. The spike
is 12 `nice -n 19` busy loops round-robined over those cores — the ticket's
`cargo build` equivalent. It is started **before** the Node says hello, so
the baseline's connect-time sample is genuinely saturated, then killed to
model "build finished". Harness (scratch-only, kills only its own spawned
pids): `/tmp/remuda-agents/remuda-mq-placement1/placement-evidence.sh`.

### Before — baseline `c1c08929` (origin/main)

```
[09:04:42] SPIKE ON (pre-hello): 12 niced busy loops round-robin over cores 0-7
[09:05:02] system load now: 7.46 5.28 3.84          # 1m load crosses 7.2/8
[09:05:07] host hst_01a0a7be… online (spike already burning since before hello)
[09:05:10] persisted cpuPct=100 sampledAt=none; system load: 8.06 5.44 3.90
[09:05:10] AUTO create while fresh+saturated -> HTTP 422
           {"code":"PLACEMENT_UNSATISFIABLE",
            "reasons":["hst_01a0a7be…: host CPU at 100% (limit 90%)"]}
[09:05:10] PIN  create (explicit hostId) while saturated -> HTTP 422
[09:05:10] SPIKE OFF: burners killed
[09:05:26] t+15s  AUTO create -> HTTP 422 (persisted cpuPct=100, load 6.55)
[09:05:41] t+30s  AUTO create -> HTTP 422 (persisted cpuPct=100, load 4.91)
[09:05:57] t+45s  AUTO create -> HTTP 422 (persisted cpuPct=100, load 3.82)
[09:06:12] t+60s  AUTO create -> HTTP 422 (persisted cpuPct=100, load 3.26)
[09:06:27] t+75s  AUTO create -> HTTP 422 (persisted cpuPct=100, load 2.61)
[09:06:43] t+90s  AUTO create -> HTTP 422 (persisted cpuPct=100, load 2.19)
```

Same shape as the Mac incident: load is 2.19/8 = 27% and every create still
422s on the frozen 100; nothing but a Node reconnect clears it. Note the
baseline also refuses the **explicit pin** — the operator cannot override
the frozen reading either. `sampledAt=none` is the pre-fix row format.

### After — this branch

```
[09:03:15] SPIKE ON (pre-hello): 12 niced busy loops round-robin over cores 0-7
[09:03:57] system load now: 7.47 4.98 3.66
[09:04:02] host hst_01a0a7bd… online (spike already burning since before hello)
[09:04:05] persisted cpuPct=100 sampledAt=2026-09-16T01:04:02.204Z; load: 8.15 5.16 3.72
[09:04:05] AUTO create while fresh+saturated -> HTTP 422
           {"code":"PLACEMENT_UNSATISFIABLE",
            "reasons":["hst_01a0a7bd…: host CPU at 100% (limit 90%)"]}
[09:04:05] PIN  create (explicit hostId) while saturated -> HTTP 200
           {"hostId":"hst_01a0a7bd…",
            "warnings":["hst_01a0a7bd…: host CPU at 100% (limit 90%);
                        pinned host, admitting despite saturation"]}
[09:04:05] SPIKE OFF: burners killed
[09:04:11] t+5s  AUTO -> HTTP 422 (sample 9 s old, load 7.66 — still genuinely >=90%)
[09:04:16] t+10s AUTO -> HTTP 422 (sample 14 s old, within the 60 s window)
[09:04:22] t+15s AUTO -> HTTP 200 ADMIT (heartbeat sample cpuPct=85
           sampledAt=…01:04:17.218Z, load 6.25)
```

Saturation that is real and recent still refuses auto-placement; the pin is
a warning and the session starts; the first post-spike heartbeat
(~15 s) carries a fresh sample and admission recovers without any restart.
The stale-beyond-60 s path (which the 20-minute Mac incident would have
hit) additionally triggers the bounded `host.resources` round trip; that
path is covered deterministically in the integration tests below.

## Tests

- Node unit: `inventory::tests::sampler_reports_one_minute_load_as_cpu_percent_and_real_counts`
  (6/14 = 43, clamp, live sample), `wss::tests::heartbeat_resource_refresh_replaces_only_the_resources_block`.
- Hub unit: `saturated_cpu_or_mem_resources_exclude_a_host` (auto still
  refuses), `pinned_host_admits_over_limit_and_emits_warnings`,
  `resource_sample_age_distinguishes_fresh_stale_and_unknown`.
- Hub integration (`tests/placement_freshness.rs`, real `/v1/node` WS):
  stale saturated sample → bounded refresh → admit + persisted;
  unconfirmable stale sample → no refusal; fresh 100% → auto 422;
  pin → 200 with `warnings[]` + journaled diagnostic; `GET /v1/hosts`
  carries `sampledAt`.
- `cargo test` for touched crates green; `cargo clippy --workspace
  --all-targets -- -D warnings` clean; `cargo fmt` clean; web
  `typecheck`/`lint`/unit tests green; OpenAPI regenerated and the web
  typed client regenerated.
