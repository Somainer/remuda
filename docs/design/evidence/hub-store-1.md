# hub-store-1: long reads must not queue in the Hub's single SQLite writer

Worktree `wt/c-hubstore2`. All numbers below are produced by the committed,
ignored harness `crates/remuda-hub/tests/hub_store_evidence.rs`, which anyone
can re-run:

```sh
cargo test -p remuda-hub --test hub_store_evidence -- --ignored --nocapture
```

Each before/after pair is one process, one database, one instant: the "before"
side routes through the pre-pool code path reconstructed in
`store_test_support` (the single writer connection, or an unbounded read with
no LIMIT); the "after" side uses the shipped reader pool and windowed methods.
It is `#[ignore]` because it seeds a 200,000-event journal and takes ~30 s.
The fast regression guards live in `crates/remuda-hub/tests/hub_store.rs`.

## 0 Symptom and root cause

On the demo (`hub.sqlite` ≈ 120 MB, three shell-pty workers streaming hook and
transcript events) every SQLite job drained one `std::mpsc` onto one
`rusqlite::Connection` on the `remuda-hub-sqlite` thread through `Store::run`,
a strict FIFO with no priority, no timeout, and no second connection. One long
job stalled everything queued behind it:

| Symptom (demo) | Observed | Recovered |
| --- | --- | --- |
| `GET /v1/interactions` (plain SELECT, web boot gate) | ~40 s, repeatedly | 0.09 s when reads stopped |
| `remuda instance read --source screen` (journal fallback, no cursor) | 31 s | — |
| `POST /v1/workers/observe` | > 45 s | — |

Two unbounded readers were the long jobs:

- `read_journal` selected `seq > ? ORDER BY seq ASC` with **no LIMIT** and
  parsed the whole busy instance journal to JSON while holding the only
  connection. The CLI screen fallback asks with no cursor, so one screen read
  paid O(journal).
- `worker_watch` fanned out per worker, each observation re-entering the same
  writer queue for `get_instance` + `read_journal_tail` + a 5 s screen RPC.

WAL and a 5 s `busy_timeout` were already set, so concurrent readers were legal
at the SQLite level — the Hub simply never opened a second connection.

## 1 Fix (no API/CLI/protocol/Node change)

1. **Reader pool beside the writer** (`store.rs`). Four connections opened
   `SQLITE_OPEN_READ_ONLY` on the same path, same `busy_timeout`, handed out
   under a `tokio::sync::Semaphore`; `Store::read(name, f)` runs on
   `spawn_blocking`. The writer keeps its own connection, FIFO, and its
   `Stop`/`wal_checkpoint(TRUNCATE)` behaviour. Read-only is enforced by the
   open flags, so a read job cannot break single-writer discipline. After
   `Store::close` the pool refuses new work at both the permit gate and
   `ReaderPool::take`, and a connection returned by an in-flight read is dropped
   rather than parked.
2. **Long reads route onto the pool**: `read_journal`, `read_journal_tail`,
   `get_object`, `read_object_bytes`, `list_interactions`, `list_instances`,
   `list_hosts`, `list_devices`, `list_passkeys`. The writer keeps writes and
   short point reads.
3. **Bounded, self-describing journal window.** `read_journal` returns at most
   `JOURNAL_WINDOW_ROWS = 2000` rows / `JOURNAL_WINDOW_BYTES = 8 MiB`, the
   newest rows in `(afterSeq, min(beforeSeq, durableSeq)]`. The response keeps
   `instanceId` / `durableSeq` / `events` and adds two **additive** keys:
   - `fromSeq` — the floor, `events[0].seq` (null for an empty window);
   - `reachedAfterSeq` — false when rows below the floor were cut.

   Re-issuing the same `afterSeq` returns the same tail, so the previous
   "re-page with afterSeq" story was false. Older history is walked with the
   new optional **`beforeSeq`** (inclusive) high bound: descend with
   `beforeSeq = fromSeq - 1` until `reachedAfterSeq` is true. The committed test
   `window_flags_and_before_seq_descend_to_full_history` walks a 5,000-event
   journal this way and asserts the pages tile `1..=durable` with no gap or
   overlap. The existing web client (`web/src/lib/api.ts` hardcodes
   `floorSeq: "1"`; `JournalClient.fillGap` slices the tail) still needs to
   consume `fromSeq`/send `beforeSeq`; that is a `web/` change outside this
   ownership and is not claimed here — the Hub now exposes what it needs.
4. **Snapshot consistency.** `durableSeq` and the window are read inside one
   deferred read transaction on the pool connection, so they share one WAL
   snapshot. Previously they were two autocommit statements, and an append
   landing between them could return an event newer than the `durableSeq`
   beside it, making a cursoring caller re-deliver. Guarded by
   `journal_window_and_durable_seq_share_one_snapshot` (400 reads while a writer
   appends; every event `seq <= durableSeq`).
5. **Bounded ingest** (`ws.rs`): one frame is split by
   `journal_append_chunks` into jobs bounded by **both** `APPEND_CHUNK_MAX = 64`
   events **and** `APPEND_CHUNK_MAX_BYTES = 1 MiB` of serialized JSON, each a
   single writer transaction, awaited in order so the writer reaches queued
   jobs between batches. The byte cap stops 64 large transcript/screen frames
   becoming one long transaction; one oversized event still goes through alone.
   Per-event seq/gap/replay checks and the `replayed`/`watermark` reply are
   unchanged (shared `append_loaded_event`), and a gap rolls the chunk back.
6. **Job budget**: every writer job and pool read carries a static name
   (`run_named` / `read`); time covers queue wait + execution and a `warn`
   fires past 1 s. Evidence trail, not a cancellation.

## 2 Before / after timings (committed harness, same 200k-event DB)

Captured 2026-09-18 on this devbox (`cargo test … -- --ignored --nocapture`).

### 2.1 Screen journal envelope — the 31 s read

```
EVIDENCE envelope before=2273ms rows=200000 after=27ms rows=2000 durable=200000
```

84× faster; 100× fewer rows parsed; the last event is still the durable tail.
The unbounded figure is the exact old query plus JSON parse; on the demo's
120 MB three-ingester box the same read was the 31–40 s number. The window is
constant in journal length.

### 2.2 Interactions list vs a blocked writer — the 40 s web-boot stall

A 1.2 s job occupies the single writer (standing in for the long envelope read
that caused the demo stall). The same pending query runs where it used to live
(writer thread) and on the new pool:

```
EVIDENCE interactions vs blocked-writer before=1150ms rows=1000 after=17ms rows=1000
```

A pool read does not wait on the writer at all — this is the faithful
reproduction of the demo (queue wait behind one long job).

### 2.3 Observe fan-out — the >45 s request

20 sequential 256-event tail reads against the 200k-row instance during a fresh
three-host ingest burst, through the writer (before) vs the pool (after):

```
EVIDENCE observe fanout 20x tail256 under ingest before=593ms rows=5120 after=80ms rows=5120
```

7.4× faster for the store hops, each now independent of whatever the writer is
doing; the 5 s-per-worker Node screen RPC is unchanged.

### 2.4 Regression guards (`hub_store.rs`, part of the normal suite)

- `long_reader_does_not_block_writer_or_unrelated_read`: a 1.5 s read holds one
  pool connection; a write and an unrelated read each finish in < 500 ms.
- `journal_endpoint_windows_a_200k_event_journal`: HTTP GET over 200k events in
  < 2 s, exactly 2000 rows ending at the durable tail; a second instance of 200
  × 64 KB rows trips the byte cap before the row cap and stays contiguous.
- `window_flags_and_before_seq_descend_to_full_history`: `fromSeq` /
  `reachedAfterSeq` semantics and gap-free `beforeSeq` descent (§1.3).
- `journal_window_and_durable_seq_share_one_snapshot`: no event past its
  page's `durableSeq` under concurrent append (§1.4).
- `journal_http_response_exposes_window_metadata`: the additive JSON keys and
  `beforeSeq` over HTTP.
- `bounded_ingest_yields_to_a_concurrent_point_read`: a 256-event frame folded
  into bounded jobs; a concurrent `get_instance` lands in < 200 ms and all 256
  events stay durable seq 1..256.
- The store unit test `append_chunks_bound_count_and_bytes_and_cover_everything`
  pins both the count and the byte cut.

## 3 Budget warn lines

```
WARN remuda_hub::store: hub store job exceeded budget job="test.read_all_journal" kind="read"  elapsed_ms=2273
WARN remuda_hub::store: hub store job exceeded budget job="test.hold_writer"    kind="write" elapsed_ms=1200
WARN remuda_hub::store: hub store job exceeded budget job="test.pending_via_writer" kind="write" elapsed_ms=1150
WARN remuda_hub::store: hub store job exceeded budget job="test.hold_reader"    kind="read"  elapsed_ms=1500
```

The queued `pending_via_writer` is logged under its own name but at 1150 ms —
the wait behind the blocking job, which is the stall class this budget exists
to surface. Production names are the Store method / call site (e.g.
`read_journal`, `list_interactions`, `append_journal_batch`).

## 4 Invariants preserved

- Journal ordering, gap detection, `durableSeq`, and replay handling are
  unchanged; the batched transaction uses the same per-event logic as the
  single append (`append_loaded_event`, shared by both paths).
- Interaction / instance / command / native-session projections and the
  `replayed` + `watermark` reply are unchanged.
- The new JSON keys and the `beforeSeq` parameter are additive; existing callers
  that ignore them see the same tail as before (bounded, as in the first
  change). The CLI, `remuda-protocol`, `remuda-hub-client`, the Node, and `web/`
  are untouched.

## 5 Incidental fix

`tests/host_files.rs` dropped its `TempDir` when `fixture()` returned (the
handle was not stored), unlinking the data directory. Latent while every query
reused the one pre-opened writer file descriptor; the lazily opened reader pool
opens its own connections on demand and cannot open an unlinked path
(`SQLITE_CANTOPEN` → HTTP 500 on object download). `Fixture` now retains `_dir`.
