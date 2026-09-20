# Evidence: node-flakes-1

Date: 2026-09-21. Branch: `wt/c-nodeflakes/b-nodeflakes-md`. Base:
`db6b3c47`.

The landing gate's first `cargo test -p remuda-node` attempt failed two tests
under a host load average around 11 and both passed on retry. Each was made
deterministic by waiting on the state the test documents rather than on
elapsed wall time; neither test was widened, retried or skipped, and no sleep
was added. The relay change is a small product correction justified below.
All data below is synthetic (in-crate fakes, loopback fake gateway); no
hostnames, home paths or usernames appear. Every stress loop ran serialized
with the landing gate through its shared lock.

## Loop method

Per test: the exact single test name, `--exact --test-threads=1`, in a serial
loop of 20 while the host was loaded the way the gate loads it — one
`cargo build --workspace` loop and several continuously looping full-suite
test binaries as sibling load (the package has ~30 integration targets plus
the lib target, which `cargo test` runs as separate processes). Load and all
test processes were pinned to two CPU cores so scheduling latency fell on the
test's tasks, and the entire harness ran under the gate's shared lock.
Sibling-load density differed per test only because B reaches a result in
~1.3 s and A in ~0.5 s (B used three sibling binaries, A six); the before and
after loop for each test used the identical density, binary and pins. Before
loops ran on the base code; after loops ran on the fix.

## Test A — `runtime::tests::create_is_accepted_before_a_slow_launch_finishes`

**Before:** 5/20 green (15 failed). **After:** 20/20 green.

The test registered a fake driver whose `start()` slept 400 ms and asserted
the create RPC returned in under 200 ms; under load the durable-accept path's
ordinary scheduling latency (the accepted command write, the journal
observation, spawning the worker task) exceeded 200 ms even though none of it
awaited materialization — observed ack times of 206–241 ms — so the test
falsely reported that the ack had waited for launch. The fix replaces the
elapsed-time comparison with an event gate: the fake's `start()` now parks on
a `tokio::sync::Notify`, the test asserts the accepted/preparing reply
returns while that gate is held (and the instance cannot pass `start()`),
then releases the gate and waits for `ready`. A create that actually awaited
materialization can never return before the release, so the ordering is
structural instead of a 200 ms vs 400 ms race. Test and the node-internal
fake driver only; no production runtime change.

## Test B — `api_relay::relay_tests::credit_starved_egress_ends_at_the_hard_cap_after_four_chunks`

**Before:** 4/20 green (16 failed, signature `left: 0, right: 4`).
**After:** 20/20 green.

The test starves a stream of `api.credit` frames and expects exactly the
four-chunk initial window to leave before the tuned one-second hard cap ends
it, but the hard deadline was armed when the stream *opened*, before the
upstream response head; under load the loopback connect, gateway scheduling
and head delivery consumed the whole second before a single chunk was
produced — a diagnostic probe on failing runs recorded zero bytes downstream
and an end at 1.43–1.50 s — so the assertion sampled an unopened window
instead of a drained one. Two minimal product changes in the relay make the
window's size structural: (1) the egress hard deadline now starts when the
response head commits — the pre-head wait is already bounded by the
first-byte timeout, so connect latency can no longer spend the consumer's
delivery budget; (2) a gated send with a permit already granted (an open
window) no longer races the clock — only an *empty* window races
cancellation and the hard deadline, preserving the anti-park guarantee for a
consumer that stops acknowledging. The duplicate open-armed elapsed check at
the top of the body loop, which could fire before any chunk was ever eligible
to send, is removed. Upload-side gated sends on the worker listener keep
their existing deadline; nothing else in the relay or runtime modules
changed.

## Verification

- `cargo test -p remuda-node` — all targets green, zero failures (342
  passed, 3 ignored in the lib target; every integration target passed)
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo fmt --all --check` — clean (toolchain 1.94.1, style edition 2024)
- `bash scripts/ci/secret-scan.sh` — clean

