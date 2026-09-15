# native-pty-2c — fast durable accept, journal convergence, zombie reap, pipelined uplink

- **When:** 2026-09-14/15
- **Where:** task worktree `/tmp/remuda-agents/wt/r-p2-createlag` (branch `wt/r-p2-createlag/fast-accept-and-reap`)
- **Agent:** r-p2-createlag
- **Predecessors:** `native-pty-2.md` (the demo), D-028 §5.3, protocol §2.3/§2.5
- **Scope addition (mid-task):** pipelined Node→Hub journal uplink, from `realtime/remuda-pipeline.md` §2 hop 7 and `realtime/design.md` §3.4.

---

## 1. The two demo failures, and what fixed them

### 1.1 The create ack waited on materialization

`POST /v1/instances` returned only after the instance worker had pinned the
binary (`pin_binary`: a blocking `--version` subprocess plus a SHA-256 of the
file), written the settings overlay / launch shims, prepared the native home,
and spawned the PTY. The pin alone is ~1.3 s of hashing for the 207 MB claude
binary and was 21 s on the demo host (0.39 s unloaded). On a loaded host the
reply landed **21 s after** the pinned binary logged and 5 s after the Hub's
accept deadline — so the Hub logged `node rpc unknown; will not resend`, left
the instance `requested / unknown`, and every node RPC on the runtime link
serialized because the outbound WSS `dispatch_in_background` awaited a
journal `catch_up` (a Hub round trip) before sending each reply.

**Fix — durable accept, then materialize:**

- `DevNode::create_instance` now inserts the instance at
  `preparing`, writes the accepted command ledger entry, journals the
  `requested → preparing` instance lifecycle, and replies. Driver build
  (`pin_binary`, overlay/shim generation, native-home prep), PTY spawn, and
  prompt delivery all happen in the spawned worker afterwards, which journals
  `preparing → starting` just before `driver.start()` and
  `starting → ready` when materialization finishes — protocol §2.3
  (`requested → preparing → starting → ready/running`).
- Driver build and the blocking recipe/pin run on `spawn_blocking` (and the
  shell-pty recipe materialization is wrapped in `spawn_blocking`), so a
  multi-second `--version` or a 207 MB hash cannot stall a tokio core; the
  remaining PTY spawn is non-blocking.
- Attachment bytes (D-027) are pulled in the worker after the accept; a pull
  failure settles the accepted command as `rejected/not-dispatched` instead of
  failing the RPC — a slow object store no longer stands on the accept path.
- Tracing spans kept on the moved work: `pin_binary` / `hash_binary`,
  `build_driver`, `materialize_recipe`, `driver.start`.

### 1.2 Digest cache

`pin_binary` now memoizes on `(canonical path, len, mtime)` in a bounded
(64-entry, oldest-evicted) process-wide cache. The ~207 MB SHA-256 runs once
per binary version; a second create of the same binary uses the cached pin.
The hash read buffer went 8 KiB → 1 MiB. Symlink aliases hit the same entry
(canonicalized key); a persistent-ETXTBSY copy is deliberately not cached.

### 1.3 A late ack converges from the journal instead of hanging

- The Hub timeout arm (`forward_if_online`) now moves the command
  `queued + unknown → queued + reconciling` (`Store::mark_reconciling`); the
  command is never resent (§2.5). The Node's independently mirrored journal
  is what converges it: the existing `apply_command_projection` flips a
  journaled `accepted` to `accepted / clear` and a journaled `settled` to
  `settled / clear`. The Node journals the accepted command **before**
  spawning the worker, so that fact is durable even if the RPC reply is lost.
- The runtime WSS replies to create/send/cancel/respond/close RPCs **before**
  journal catch-up: it ensures the per-instance pump exists and lets the pump
  mirror asynchronously, removing the Hub round trip from every accept.
  (Replay/resume semantics are unchanged: reconnect still flushes durable
  rows from the confirmed watermark, and the Hub dedupes by
  `(instanceId, seq)` → "already durable".)

**Hub test:** `a_late_create_ack_converges_from_the_journal_without_a_resend`
(`crates/remuda-hub/tests/lifecycle.rs`) — a Node that never answers the
create RPC within a 50 ms accept deadline yields `queued/reconciling`; its
journaled `accepted`/`starting`/`ready` then converges the row and instance
to `accepted`/`running`, with no second create forwarded.
**Store test:** `a_lost_accept_reconciling_row_is_converged_by_the_journal_not_a_resend`.

### 1.4 The SIGKILLed child was counted as a survivor

The stop ladder liveness check is `killpg(pgid, 0)`, which still returns
success for a **zombie** — a child that has died but whose parent has not
`wait`ed. The demo's claude process was dead in `E`/`Z`; its parent (the
Node) had not reaped it; the ladder declared
`stop-incomplete: the process group outlived SIGKILL survivors=[98691]`.

**Fix (`shell_pty/lifecycle.rs`):**

- The ladder now takes a `reap` callback and calls it before every liveness
  check. `stop_tree` holds the portable-pty child and reaps it with
  `try_wait`; the exit waiter remains the authoritative source of exit
  evidence.
- Group membership is read with zombie awareness: `/proc/<pid>/stat` on Linux
  (state field after the *last* `)`, because `comm` can contain spaces),
  `ps -eo pgid,pid,stat` on other unixes. A zombie — ours, or a grandchild
  reparented to init which reaps it — is never a live member and never named
  in `stop-incomplete`; the diagnostic lists only genuinely-live pids.
- `group_has_live_member` replaces the zombie-blind `group_alive` on the
  ladder path; `group_alive` stays for the public conservative check.

**Test:** `an_unreaped_zombie_leader_is_reaped_by_the_ladder_not_named_a_survivor`
— kill the unreaped leader (`exec sleep`, so it is the only group member),
prove via `/proc` it is a zombie that still answers `killpg(0)`, run the
ladder with a real reaper, and assert `AlreadyGone`, zero survivors.

---

## 2. The pipelined uplink (scope addition)

`remuda-pipeline.md` §2 measured the Node→Hub uplink as the single largest
structured-view latency: `forward_event` awaited one ACK per journal event,
+0.39 s median **per event** (a six-event batch: 2.8 s total, 2.4 s pure
serialization). The Hub processes one socket's frames in arrival order on a
single task and answers in order, and TCP already allows a deeper queue.

`transport/wss/uplink.rs` adds `UplinkWindow` — a bounded (16-frame), in-order
window over `futures::stream::FuturesOrdered`. The per-instance pump submits
appends while there is capacity and drains resolved ACKs in submission order
(the watermark advances strictly in journal order even if responses complete
out of order). On any failure the window is dropped and replay restarts from
the last **acknowledged** watermark; unacked frames are simply sent again and
deduplicated by the Hub ("already durable") — the durable_seq/watermark
contract in protocol.md is unchanged. Replay (`flush_journal`) is pipelined
the same way.

**Test:** `journal_uplink_pipelines_a_batch_within_one_rtt`
(`crates/remuda-node/tests/wss.rs`) — a fake-Hub WebSocket that delays every
append ACK independently by 120 ms; a real runtime Node emits journal events
through `instance.send`. A batch of six lands within ~1 RTT. Verified
discriminating: with the window forced to `1` (the old serial behaviour) the
same batch measures **515 ms** and the test fails; with window 16 it measures
**6 ms** and passes. The assertion is relative (`batch < 2×single + slack`)
so it stays meaningful under host load.

---

## 3. Live end-to-end (task-owned `remuda dev`, claude 2.1.221, 288 MB binary)

Task-owned server: `REMUDA_PTY_CARRIER=native REMUDA_PTY_HOOKS=1
REMUDA_EMULATOR=1 remuda dev --listen 127.0.0.1:58970 --hub-listen
127.0.0.1:58971` (scratch data under `/tmp/remuda-r-p2-createlag/`), real
claude `claude.exe` 2.1.221 (288,705,544 bytes, sha256
`60db8e88…f087ada`).

Create (`kind=claude, driver=shell-pty`):

```
[1] POST /v1/instances -> 200 in 335–939 ms across runs (first pin ~6.7 s worker-side)
    lifecycle=requested command=accepted/clear
[2] instance converged to lifecycle=running
[3] journaled instance lifecycle transitions (UTC):
    17:14:43.125 requested -> preparing (create-accepted)
    17:14:43.129 preparing -> starting (driver-spawn)
    17:14:49.344 starting -> ready (driver-started)
```

The ack is sub-second while the first-ever pin hashes the 288 MB binary in
the worker; a second create is faster still (cache hit). The log shows the
pin nested entirely inside `driver.start:materialize_recipe:pin_binary`,
never on the RPC path.

- **Hook SessionStart binding:** `native hook/SessionStart status=idle`,
  with `transcriptPath` under
  `~/.claude/projects/-tmp-remuda-r-p2-createlag-workspace/…jsonl`,
  `source=startup`, cwd and ppid present.
  (A pre-existing direct-launch gap was found and fixed here: the agent path
  spawned the pinned binary without `--settings`, bypassing the generated
  hook overlay — it only worked through the PATH shim when a human typed
  `claude`. `materialize_shell_pty_agent` now picks up
  `<launch_dir>/settings.json`.)
- **First prompt landing:** `instance.send` of “Reply with exactly the single
  word: PONG”; the turn journaled `UserPromptSubmit working`,
  `transcript_bound`, and the CLI's own
  `MessageDisplay status=streaming delta="PONG" final=true`, then `Stop idle`.
  The prompt landed and was answered. (The folded structured assistant
  *message* is empty on this binary because `transcript_unbound`/no
  deterministic transcript binding — documented behaviour, unchanged.)
- **Delete:** `DELETE ?force=1` → 200 in 628 ms; instance row purged;
  `INFO pty process group stopped rung="sigint" pgid=…`; **zero**
  `stop-incomplete`; a `/proc` scan of the group pgid found **zero** live
  members after the stop.

The reproducible probe script and per-run journal dumps are under
`/tmp/remuda-r-p2-createlag/` (`e2e.sh`, `e2e_body.py`,
`e2e-results.txt`).

---

## 4. What changed

| Area | Files |
| --- | --- |
| fast accept + preparing/starting/ready, worker-side build (`spawn_blocking`), attachment pull off accept path, lifecycle journaling | `crates/remuda-node/src/runtime.rs`, `runtime/pty_queue.rs`, `driver.rs` |
| pipelined journal uplink | `crates/remuda-node/src/transport/wss/runtime_wss.rs`, `transport/wss/uplink.rs` (new), `transport/wss.rs` |
| pin digest cache `(path,len,mtime)`, 1 MiB hash buffer | `crates/remuda-driver/src/binary.rs` |
| zombie-aware stop ladder + reaper | `crates/remuda-driver/src/shell_pty/lifecycle.rs`, `shell_pty.rs` |
| shell-pty agent loads generated hook overlay | `crates/remuda-driver/src/materializer.rs` |
| Hub unknown → reconciling, journal converges | `crates/remuda-hub/src/http.rs`, `store.rs` |
| tests | node: `runtime.rs`, `tests/{attachments,pty_lifecycle,restart,wss}.rs`; driver: `binary.rs`, `shell_pty/lifecycle.rs`; hub: `tests/lifecycle.rs`, `store.rs` |

All checks green: `cargo fmt`; `cargo clippy -D warnings` on
remuda-driver/node/hub/remuda; full test suites for driver, node, hub,
protocol, remuda pass.

## 5. Not done / follow-ups

- The folded structured assistant **message** for this pinned 2.1.221 binary
  still depends on a deterministic transcript binding (`transcript_unbound`);
  the hook `MessageDisplay` stream is authoritative on this host. That is a
  P3/P6 adapter concern, not this task.
- The daemon NDJSON uplink (`daemon.rs`) already had a 16-frame in-flight cap
  and was not changed; the new window is the runtime-WSS analogue.
