# Review: Hub↔Node wire (`48083c3`)

Adversarial check of `remuda-protocol::hubnode`, Hub `ws.rs` / `registry.rs` /
`fleet.rs` / `interactions.rs`, and Node `transport/wss.rs` + `stdio.rs` +
`enroll.rs`. Line numbers refer to the pre-fix tree at `48083c3`.

## Verified (no change)

| Check | Evidence |
| --- | --- |
| WS upgrade requires Bearer | `presented_token` on `/v1/node` (`ws.rs` 89). Missing token fails handshake (`hub.rs` `node_ws_rejects_missing_token`). |
| Host tokens hashed at rest | `authenticate_host` stores Argon2id `token_hash`, never the secret (`store.rs` 326–327). Bootstrap compared with `secret_eq` (`314`). |
| Enrollment file 0600 | `enroll::save` (`enroll.rs` 59–75). |
| Node does not replay Commands | `reconnect` fails in-flight journal waiters and does not resend Hub `instance.*` (`wss.rs` 262, 745–758, 765). Integration: `wss.rs` test `…reconnect` asserts no second `instance.create`. |
| Journal queue is bounded | `journal_queue` default 32; `append` waits (`wss.rs` 30–31, 127–165). Unit: `journal_queue_applies_backpressure`. |
| Hub follow bus bounded | `broadcast::channel(256)` (`ws.rs` 47). |
| Outbound WS writer queue | `mpsc::channel(32)` (`ws.rs` 108). Hub→Node RPC times out and removes waiter (`transport.rs` 102–108). |
| First-answer-wins (single Hub) | HTTP `POST /v1/interactions/:id/answer` forwards to Node broker (`interactions.rs` 100–149). Broker uniqueness is in `remuda-driver` (`first_answer_wins_across_two_devices`). Hub is one process; no multi-Hub split-brain in this crate. |
| Instance journal isolation | Foreign host `journal.append` is Forbidden (`store.rs` 703–705; `hub.rs` stolen-append test). |

## P0

### P0-1 Stale WS disconnect marks a live host offline

`ConnectedNodes` is a flat `host_id → transport` map (`transport.rs` 149–177). Hello `insert`s the new session (`ws.rs` 258–264). When **any** session for that host ends, `node_session` always `remove`s the map entry and `mark_host_offline` (`ws.rs` 176–179).

Reconnect while the previous TCP/WS is not yet dropped (or a second dial with the host token) replaces the live transport, then the dying socket offlines the host. Placement then treats the Node as down (`placement.rs` 182–198) and `forward_if_online` skips RPC (`http.rs` 273–275).

**Fix:** stamp each insert with a generation; disconnect removes/offlines only if the generation still matches. Reject a second `node.hello` on the same socket.

### P0-2 Duplicate journal seq is a hard gap; `already_durable` is a substring match

Hub `append_journal` requires `seq == durable_seq+1` (`store.rs` 706–716). A reconnect resend of seq `N` after Hub already stored `N` returns `journal gap: expected {N+1}, got {N}`.

Node treats that as success only if the message contains both `"journal gap"` and `"got {seq}"` (`wss.rs` 810–817). `"got 1"` is a substring of `"got 10"`, so a gap for seq 10 is mis-acked as durable for seq 1 and the watermark jumps incorrectly.

**Fix:** if `seq < next` and the row exists, return that row (idempotent duplicate). Gap only when `seq > next` or the earlier seq is missing. Match `got {seq}` as the suffix after `"got "`, not a substring.

### P0-3 Batch `journal.append` reuses one seq for every event

`handle_node_method` walks `events_to_append` but passes the same `seq` into `append_journal` for each item (`ws.rs` 351–355). After the first row, `durable_seq` has advanced, so the second event is a gap. Node currently sends a one-element `events` array (`wss.rs` 690–704) so this is latent; a real batch (stdio/codec) would drop events.

**Fix:** after each successful append, continue from `record.seq + 1`.

## P1

### P1-1 `node.auth` on `/v1/node` always succeeds

`ws.rs` 194–197 returns `{ok:true}` without comparing `params.token` to the upgrade Bearer. Hello still uses the HTTP token (`228`), so this is not a full bypass, but a stdio-over-WS proxy that forwarded a forged `node.auth` would look authenticated at the method layer. Stdio itself sends `node.auth` then `node.hello` (`stdio.rs` 79–95).

**Fix:** constant-time compare `params.token` to the upgrade secret; mismatch → `Unauthenticated`.

### P1-2 Placement does not enforce `maxInstances`

`consider` ignores `running` (`placement.rs` 172–174). `max_instances` only affects sort order (`162–169`). `insert_instance` never checks the cap (`store.rs` 461–497). Two concurrent `POST /v1/instances` can oversubscribe a host.

**Fix:** reject hosts with `running >= max_instances` in `consider`; re-check the count inside the `insert_instance` SQLite transaction.

### P1-3 Hub WS does not apply `max_json_frame_bytes`

Hello advertises 1 MiB (`ws.rs` 494–499) but `node_session` parses any text frame (`ws.rs` 130–131). A connected Node can DoS the Hub process.

**Fix:** drop text frames larger than 1 MiB.

### P1-4 `NodeAuthParams` Debug prints the token

Derived `Debug` (`hubnode.rs` 130–134). Stdio encodes the secret in the first frame (`hubnode_codec.rs` 158–164). Accidental `tracing`/`:?` logs leak host tokens.

**Fix:** custom `Debug` that redacts `token`.

### P1-5 Hub→Node in-flight RPC map is unbounded until timeout

`WssTransport::call` inserts into `pending` with no cap (`transport.rs` 89–91). Timeouts remove the waiter (102–108), but a burst of `interaction.list` / commands can grow the map to one entry per in-flight call with no `max_in_flight_rpc` (hello `limits.max_in_flight_rpc` is 32).

**Fix:** refuse new calls when `pending.len() >= 32`.

## P2

| ID | Where | Note |
| --- | --- | --- |
| P2-1 | `ws.rs` 401–414 | Node-originated `instance.*` only `mark_settled`; Hub does not re-forward queued commands after hello. |
| P2-2 | `interactions.rs` 125–143 | Answer fans out to every connected host; extra RPCs, not a second Hub. |
| P2-3 | `NodeHelloParams` Debug | `enrollmentToken` still derived-Debug (stdio hello). |
| P2-4 | `stdio.rs` 79–95 | Token on stdout NDJSON is the carrier; operators must not log the pipe. |
| P2-5 | `ws.rs` 112 | `hello_done` is per-socket only; generation (P0-1) is the cross-socket fix. |
| P2-6 | `heartbeat` | Watermarks in hello/heartbeat are ignored by Hub; catch-up is Node-driven flush. |

## Tests

- `crates/remuda-hub/tests/hub.rs` — second hello, auth mismatch, duplicate seq, stale disconnect.
- `crates/remuda-hub/src/placement.rs` — at-capacity host rejected.
- `crates/remuda-node/src/transport/wss.rs` — `already_durable` exact match.
- `crates/remuda-protocol/src/hubnode.rs` — `NodeAuthParams` debug redacts token.
