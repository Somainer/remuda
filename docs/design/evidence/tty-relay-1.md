# TTY relay 1 — Hub relay hardening and zombie-instance reconciliation

Date: 2026-09-13 (Asia/Shanghai). Agent: x-ttyhub. Branch:
`wt/x-ttyhub/tty-relay-hardening`. Base: `68684d5` (main). Binaries built from
this branch (`remuda 0.1.0`, herdr **0.9.0**).

## Result

An isolated real Hub on **60480** and a real Node (own data dir, own
`shell-pty` instances) exercised the four Hub-side weaknesses the terminal
diagnosis flagged, plus the zombie-instance wedge seen on the demo. All five
behaved as designed:

| Behaviour | Before | Observed on this branch |
| --- | --- | --- |
| B2 tty output after a Node reconnect | frames dropped silently on already-open follow sockets | `AFTER-RESTART` echoed back on the **same** socket opened before the cut |
| B3 cached snapshot fallback | `cached` computed, never sent | `{"type":"tty.snapshot","source":"hub-cache"}`, 991 B |
| B4 input failures | silent `return` / `let _ =` | 4 `tty.diagnostic` frames + matching `WARN` lines |
| Zombie instances | `running` forever, held `maxInstances` slots | `exited` + `lastError: node-epoch-changed` + journal diagnostic |
| Operator cap override | reset to 8 by hello/restart | `maxInstances: 32` survived a heartbeat and a Hub restart |

Nothing outside `/tmp/x-ttyhub-evidence` was touched. No shared Herdr session,
no macOS setting, no browser. The environment was stopped afterwards and ports
60480 / 60487 / 60489 are free.

## Setup

`remuda dev` runs Hub and Node in one process, so killing the Node would take
the Hub with it. The run therefore used separate processes:

```text
remuda hub  --listen 127.0.0.1:60480 --data-dir /tmp/x-ttyhub-evidence/hub
remuda node --data-dir /tmp/x-ttyhub-evidence/node \
            --hub-url ws://127.0.0.1:60489/v1/node \
            --host-token-file … --workspace /tmp/x-ttyhub-evidence/ws
```

Port **60489** is a small cuttable TCP proxy in front of the Hub. Touching a
file drops every live connection (and optionally refuses new ones), which
forces the Node's WSS link to reconnect **on a fresh socket while the Node
process and its PTYs stay alive** — exactly the B2 condition: same host id, new
socket. A `kill -9` would have destroyed the PTY too and proved nothing about
the relay.

The follow socket is a stdlib-only WebSocket client (`follow_probe.py`) that
attaches once and is **never reconnected** for the rest of the run.

Host `hst_01a09a8c-d824-7656-8796-c28aac29e679`, instance
`ins_01a09a8c-da78-76f3-9c27-ab9c8efdad31` (`kind: terminal`,
`driver: shell-pty`).

## B2 — output resumes on a follow socket that outlived the reconnect

The socket attached, echoed a marker, survived the transport cut, and received
live frames again with no re-attach from the client:

```text
[19:35:23] attach: follow socket open for ins_01a09a8c-da78-76f3-9c27-ab9c8efdad31
[19:35:23] attach: tty output 301B stream=01a09a8cdae671729a118cfaefe486a5 '…<user>@<host>:…/ws'
[19:35:33] before: marker echoed back on the live socket: True
[19:35:35] node-down: node is down; sending input that cannot reach any PTY
…
[19:36:22] after:  node reconnected; probing the SAME pre-restart socket
[19:36:28] after:  tty output 23B stream=01a09a8cdae671729a118cfaefe486a5 '\x1b]2;echo AFTER-RESTART\x07'
[19:36:38] after:  frames resumed on the pre-restart socket: True
```

The stream UUID is unchanged (`01a09a8c…86a5`) across the cut: the Hub keeps the
binding against the host, so the Node's re-announcement on the new socket is
honoured. Before this change the new socket owned no streams and every binary
frame was dropped while the attach snapshot still painted — alive-looking, dead.

## B3 — the cached snapshot is actually sent

With the Node held off the Hub, a re-subscribe on the same socket produced the
Hub's own cache instead of silence:

```text
[19:35:43] node-down: journal snapshot asOfSeq=3
[19:35:43] node-down: HUB-CACHE snapshot 991B source=hub-cache
```

Frame shape (`remote-terminal.md` §Attach):
`{"type":"tty.snapshot","instanceId":"ins_…","source":"hub-cache","dataBase64":"…"}`.
It is a text frame because without a `streamId` from the Node there is no UUID
to address a binary frame to.

## B4 — every input failure is visible

Four distinct failure classes, each reported to the client and logged:

```text
[19:35:35] DIAGNOSTIC {"operation":"tty.write",  "reason":"node-offline",     "detail":"no live Node session for this host"}
[19:35:39] DIAGNOSTIC {"operation":"tty.resize", "reason":"node-offline",     "detail":"no live Node session for this host"}
[19:36:38] DIAGNOSTIC {"operation":"tty.write",  "reason":"malformed-frame",  "detail":"truncated binary header"}
[19:36:41] DIAGNOSTIC {"operation":"tty.write",  "reason":"wrong-channel",    "detail":"tty input must use the tty-input binary channel"}
```

Matching Hub log lines, each carrying instance, host, and method:

```text
WARN tty call dropped: node offline instance_id=ins_01a09a8c-… host_id=hst_01a09a8c-… method=tty.write
WARN tty call dropped: node offline instance_id=ins_01a09a8c-… host_id=hst_01a09a8c-… method=tty.resize
WARN follow tty input frame rejected instance_id=Some("ins_01a09a8c-…") frame_bytes=7 error=truncated binary header
WARN follow tty input frame on the wrong channel instance_id=Some("ins_01a09a8c-…") channel=1
```

A `node-error` diagnostic (`detail: "invalid request: instance has no TTY
bridge"`) was also produced in an earlier cycle where the PTY really had died,
confirming the Node's own rejection reaches the browser rather than being eaten
by `let _ =`.

## Zombie instances

### A Node that still knows its instances keeps them

The Node persists instances in `node.sqlite` and re-reported this one as
`ready` after a full process restart, so the Hub **kept** it — reconciliation
is not a blanket wipe. A stop then completed normally:

```text
POST /v1/instances/{id}/commands {"operation":"instance.close"}
  → command state=settled resolution=clear
  → instance exited
```

### A Node that lost its instances gets them reconciled

Second instance `ins_01a09a90-7b6f-70d1-aa82-42cf5486ffe7` (`running`). The Node
was killed, its instance table cleared (the demo's "node epochs are gone"
condition), then restarted on the same identity:

```text
WARN node epoch changed; instance lost
     host_id=hst_01a09a8c-d824-7656-8796-c28aac29e679
     instance_id=ins_01a09a90-7b6f-70d1-aa82-42cf5486ffe7

GET /v1/instances/ins_01a09a90-…  → exited, lastError=node-epoch-changed
```

The journal carries the Hub-authored reason, not a forged Node observation:

| seq | kind | payload.type | origin | nativeName |
| ---: | --- | --- | --- | --- |
| 1–3 | lifecycle | entity | — | — |
| 4 | lifecycle | native | **hub** | `node_epoch_changed` |

### The cap is released

`instanceCount` 2, both rows terminal, and
`POST /v1/placement/resolve {hostId, driver: "shell-pty"}` resolved the host
again instead of `PLACEMENT_UNSATISFIABLE`.

### A stop while the Node is offline never hangs

`POST …/commands {"operation":"instance.close"}` with the Node gone returned in
**0.14 s** as `state=queued` (parked for the reconnect, not blocked). When a
Node answers such a stop with a not-found error, the Hub settles the command and
projects the row to `exited` with `lastError: node-lost-instance`
(`crates/remuda-hub/tests/tty_relay.rs::stop_for_an_instance_the_node_forgot_settles_as_exited`).

## Operator `maxInstances` override

```text
PATCH /v1/hosts/{id} {"maxInstances":32}   → maxInstances 32
… node heartbeat re-advertising 8 …        → maxInstances 32
… Hub SIGTERM + restart on the same data dir …
GET /v1/hosts/{id}                          → maxInstances 32
GET /v1/instances/ins_01a09a90-…            → exited, node-epoch-changed
```

The override lives in its own `hosts.max_instances_override` column; hello and
heartbeat still write the Node-advertised `max_instances`, and `load_host`
prefers the override. Both the ceiling and the reconciled row survived the
restart.

## A bug this run found

The first cycle logged:

```text
WARN node epoch changed but hello carried no instance inventory; leaving instance rows untouched
```

Only a *daemon* Node attached `instances` to `node.hello`, so an ordinary Node —
including the one behind `remuda dev` — could never be reconciled: precisely the
demo's situation. `crates/remuda-node/src/transport/wss.rs` now sends the
inventory on every runtime hello, daemon or not. The reconcile above is from the
rebuilt binary.

The Hub deliberately still does nothing when an epoch changes and the hello
carries **no** `instances` key at all: a stateless Node must not wipe rows it
simply never enumerates.

## Session deletion (`DELETE /v1/instances/{id}`)

Separate run on the same isolated Hub 60480 + Node, one `shell-pty` session
(`ins_01a09add-a94a-7329-b0bd-36ac2e1725cf`), still `running` at the start.

| Call | Result |
| --- | --- |
| `DELETE` with an agent credential (its own instance) | **403** |
| `DELETE` unauthenticated | **401** |
| `DELETE` while running, no force | **409** `instance is running; stop it first or retry with ?force=1` — and `GET` still **200** |
| `DELETE ?force=1` | **200** `{"deleted":true,"nodePurge":"purged"}` |
| `GET` after delete | **404** |
| `DELETE` again | **404** (idempotent) |

Store state afterwards — Hub:

```text
instances: 0 row(s)   journal: 0 row(s)   commands: 0 row(s)   interactions: 0 row(s)
audit: instance.delete device dev_01a09add…
       {"forced":true,"hostId":"hst_01a09add-…","lifecycle":"running","nodePurge":"purged"}
```

Node (`node.sqlite`), after `instance.purge`:

```text
node instances: 0 row(s)   node commands: 0 row(s)   node recipes: 0 row(s)
instances/<id> directory: removed
```

`~/.claude` and `~/.codex` are still present: the purge removes only the Node's
own `<data_dir>/instances/<id>`, never the agent's native transcripts.

### Two bugs this run found

1. **The deleted row came back.** The `instance.close` issued by `force=1` was
   still draining, and its next `journal.append` hit `ensure_instance`, which
   recreated the row — `GET` returned 200 after a successful delete. Deleting
   now writes a tombstone (`deleted_instances`) that `ensure_instance` refuses,
   so a late Node event cannot resurrect a deleted session. The repeated
   `DELETE` also returned 409 instead of 404 for the same reason.
2. **The Node reported a purge it never performed.** Both runtime dispatchers
   (`runtime_wss.rs`, `runtime_link.rs`) end in a `_ => Ok(json!({"ok": true}))`
   catch-all, so `instance.purge` was answered "fine" while nothing was removed
   — the Hub logged `nodePurge: purged` with the data directory still on disk.
   Both now handle the method explicitly. The Node also waits (up to 5 s) for a
   just-closed driver to finish exiting before removing the directory, instead
   of refusing a purge that is milliseconds early.

Neither was reachable from the unit tests, which stub the Node: only the live
run surfaced them.

## Automated coverage

`crates/remuda-hub/tests/tty_relay.rs` (11 tests, four of them deletion) and the new
`crates/remuda-hub/src/store.rs` unit tests cover the same ground without a
live Node. Both B2 and B3 tests were verified to fail when their fix is reverted
(host-scoped lookup forced to `None`; cached fallback returned early).

```text
cargo test -p remuda-hub --test tty_relay   → 11 passed
cargo test -p remuda-hub --lib              → 53 passed
cargo test --workspace --locked             → all suites pass
```
