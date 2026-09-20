# Truncated-JSONL journal append on reopen — journal-flake-1

In the landing gate on 2026-09-21,
`a_reopened_store_vouches_for_an_empty_inventory`
(`crates/remuda-node/tests/pty_lifecycle.rs`) failed once with
`reconcile: Driver("journal append failed: json: EOF while parsing a string at
line 1 column 5335")` and passed on retry.

## Cause in two sentences

A Node dropped the hard way (the test drops the composed `DevNode` to simulate a
process death) left its spawned instance workers, observation pump, and
interaction pump running: they held clones of the durable store, so the journal
writer thread stayed open with its SQLite connection and JSONL handles after a
second Node had reopened the same data directory, and the two writers raced seq
allocation and wrote the same offsets into the shared JSONL — one writer's stale
record landed mid-line where the other had already written, so the append path's
read-back parsed a truncated record (`json: EOF`) or hit the duplicated
`(instance_id, seq)` row (`UNIQUE constraint failed`). The journal had no
single-writer interlock and recovery only skipped an unparseable tail in memory,
so a half-written line that was *also* indexed in SQLite was read back forever.

## Mechanism, with citations

- Every append funnels through one OS thread per process
  (`crates/remuda-node/src/store.rs`, `DurableJournal::open`), which drives the
  single-writer `remuda-journal` store (`crates/remuda-journal/src/store.rs`,
  `Store::append`): `write_jsonl` wrote the JSONL line plus its newline,
  fdatasynced, and only then inserted the `(seq, jsonl_offset, jsonl_length)`
  row and bumped the watermark in SQLite. Within one process that ordering is
  safe; nothing stopped two *processes'* writers (or, in the test, two writers
  in the same process) from being open at once.
- `compose` returns a cloneable `DevNode` around an `Arc`; nothing stopped the
  journal when the last clone dropped. Explicit `shutdown()` drained workers
  and pumps (`crates/remuda-node/src/reclaim.rs`), but a bare drop aborted
  nothing: `materialize_instance` workers (`crates/remuda-node/src/runtime.rs`),
  the observation pump (`spawn_observation_pump`), the interaction broker pump
  and the 30 s expiry sweeper (`crates/remuda-node/src/interactions.rs`) all
  held `Arc<dyn LocalStore>` clones and kept appending.
- Under CPU contention the reopen (`compose` again) raced those live writers:
  both computed the same next seq from SQLite, and a writer whose `len` cache
  was stale issued an O_APPEND write at a physical offset different from the
  offset its SQLite row recorded, interleaving bytes on the JSONL. The next
  append's read-back (`DurableJournal::append` → `read_range(seq..=seq)`)
  deserialized a fragment: `json: EOF while parsing a string`. The same race
  surfaces as `UNIQUE constraint failed: events.instance_id, events.seq`.
- Recovery on open (`Store::recover`) tolerated an unparseable *last* chunk
  only while scanning, and rebuilt the watermark with `MAX(seq)`: when the
  torn append had already committed its SQLite row, the torn seq stayed the
  durable watermark and the bad span stayed indexed, so every later read of it
  failed.

## Fix

1. **Single-writer interlock, bounded.** `Journal::open_with` takes an
   exclusive `flock(2)` on `<data_dir>/journal.lock`, polled non-blocking for a
   bounded wait (`JournalOptions::writer_lock_wait`, default 5 s) and then
   failed with a clear `Error::Locked { path, waited }` — never an unbounded
   block, so a genuinely live predecessor cannot wedge a Node startup (or a
   test binary) forever. A hard-killed process releases the lock in the kernel
   immediately; a prompt in-process close releases it within one 25 ms poll,
   which covers a writer finishing its last in-flight append.
2. **Prompt, panic-safe close.** `Journal::close` and
   `MemoryStore::shutdown_journal` stop the writer threads; the node-side
   durable journal is an RAII guard (`impl Drop`) so the lock is released on
   every exit path including an unwind, and the graceful `DevNode::shutdown`
   closes the journal after the terminal lifecycle events have been journaled
   (so a `compose` on the same dir while the old binding is still alive — the
   daemon takeover test — reopens immediately). `DevNodeInner`'s `Drop` sets
   the stopping flag, aborts the instance workers, observation pumps,
   interaction pump and expiry sweeper (all held store clones), and closes the
   journal; all calls are idempotent.
3. **Atomic record shape.** `write_jsonl` writes the whole record including
   its terminating newline in one `write_all` before fdatasync and before the
   offset is published to SQLite.
4. **Torn-tail recovery.** On open, each JSONL is cut at its final newline:
   bytes past it are an unterminated, therefore never-committed record, so they
   are physically truncated with one `WARN` carrying the dropped byte count,
   while every complete record is kept. The SQLite index and watermark are
   rebuilt from the surviving JSONL prefix (deleting index rows past it); a
   malformed *complete* line remains a hard error rather than silently
   discarding an acknowledged observation.

The wire format and envelope schema are unchanged: a journal file is still one
JSON object per newline-terminated line.

### Lock shape: bounded wait, not block-forever

A first version made the second opener block in `flock(2)` with no timeout.
That deadlocked the daemon suite
(`killed_bridge_keeps_fake_claude_alive_and_replays_completion_after_watermark`,
`crates/remuda-node/tests/daemon.rs`): it calls graceful `node.shutdown()` and
then `compose`s the same data dir while the old `DevNode` binding is still in
scope, and graceful shutdown did not close the journal — the reopen waited on a
writer that would never exit (36-minute stall observed, holding the gate lock).
The interlock is therefore deliberately *bounded*: the legitimate handoff (old
writer closing) completes within a poll, while a stale live writer fails the
open with a named error instead of hanging. The daemon test is the named
"reopen while a writer lives" case and stays green.

## Before / after

The loop was run with the test binary pinned to two cores alongside two CPU
burners on those cores (`taskset`, `--test-threads=1`), 30 iterations, the whole
loop wrapped in
`flock …/locks/gate-e2e.lock` so it serializes against the landing gate:

- **Before the fix:** PASS=26 FAIL=4 of 30. All four died in `reconcile` at
  the gate's call site: three with
  `UNIQUE constraint failed: events.instance_id, events.seq` and one with the
  gate's exact string,
  `reconcile: Driver("journal append failed: json: EOF while parsing a string
  at line 1 column 5335")` — same column 5335 the landing gate reported,
  confirming the same mechanism rather than a coincidental parse error. The
  captured writer trace showed the dropped Node's writer (`gen=0`) appending
  seq 6 at a stale offset after the reopened Node (`gen=1`) had already
  written its own seq 6; O_APPEND landed the bytes at EOF while the SQLite
  index claimed the old offset.
- **After the fix:** 30/30 pass under the same pinned-and-burned loop. After
  rebasing onto the main that landed meanwhile (one unrelated hunk in
  `runtime.rs`, applied cleanly), the journal suite and the node library,
  daemon, pty-lifecycle and restart binaries were re-run green (360 library
  tests; 8 + 11 + 4 integration tests, 0 failed); the real-PTY `live_pipeline`
  binary takes ~180 s but is deterministic and unrelated.
- **Known flake during development (not a residual test flake):** the first
  interlock version blocked in `flock(2)` with no timeout and wedged the
  daemon suite for 36 minutes while holding the gate lock; it was replaced by
  the bounded wait above before anything landed (see "Lock shape" below).

## Deterministic tests

- `crates/remuda-journal/tests/journal.rs`
  `reopen_truncates_a_torn_tail_and_keeps_complete_records`:
  appends a fixture journal, cuts the final line mid-record, reopens; asserts
  the watermark follows the surviving prefix, the next append takes the
  recycled seq and parses, exactly one `torn_bytes` warning is emitted, and a
  second reopen is a clean no-op.
  `a_corrupt_complete_record_fails_recovery` pins the boundary: a
  newline-terminated malformed line is corruption in the committed prefix and
  fails recovery rather than being trimmed.
  `a_live_writer_makes_a_second_open_fail_locked_then_succeeds_on_close` pins
  the bounded interlock: a live writer yields `Error::Locked` after the wait
  instead of hanging, and an opener after close acquires on the next poll.
- `crates/remuda-node/tests/restart.rs`
  `reopen_after_a_torn_journal_tail_reconciles_and_keeps_complete_records`
  drives the exact gate path (compose → drop → truncate → compose →
  `reconcile_herdr`) with synthetic data only; and
  `a_hard_dropped_node_cannot_write_after_its_successor_reopens` drops a Node
  mid-materialization five times in a row and asserts the successor's journal
  stays dense, indexed, and parseable.
