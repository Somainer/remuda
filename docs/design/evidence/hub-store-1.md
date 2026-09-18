# hub-store-1: long reads must not queue in the Hub's single SQLite writer

Worktree `wt/c-hubstore2`. Evidence taken on 2026-09-18 against the synthetic
busy-instance harness in `crates/remuda-hub/tests/hub_store.rs` (the same
200,000-event journal is read the old way and the new way back-to-back, so each
before/after pair is one process, one database, one instant).

## 0 Symptom and root cause

On the demo (`hub.sqlite` ≈ 120 MB, three shell-pty workers streaming hook and
transcript events) every SQLite job drained one `std::mpsc` on one
`remuda-hub-sqlite` thread through one `rusqlite::Connection`. `Store::run`
was a strict FIFO — no priority, no timeout, no second connection — so one long
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
   `spawn_blocking`. The writer thread keeps its own connection, FIFO, and its
   `Stop`/`wal_checkpoint(TRUNCATE)` behaviour. Read-only is enforced by the
   open flags, not convention, so a read job cannot break single-writer
   discipline.
2. **Long reads route onto the pool**: `read_journal`, `read_journal_tail`,
   `get_object`, `read_object_bytes`, `list_interactions`, `list_instances`,
   `list_hosts`, `list_devices`, `list_passkeys`. The writer keeps writes and
   short point reads (`get_instance`, `get_command`, device/host lookup).
3. **Bounded journal window**: `read_journal` caps at `JOURNAL_WINDOW_ROWS =
   2000` rows and `JOURNAL_WINDOW_BYTES = 8 MiB`, returning the **tail nearest
   `durableSeq`**. Response keys are unchanged (`instanceId`, `durableSeq`,
   `events`); `events[0].seq > afterSeq + 1` is the caller's signal it holds a
   window and may re-page, which the client already supports. A single event
   larger than the byte budget is still returned (the newest row always fits).
4. **Bounded ingest** (`ws.rs`): one frame is folded into chunks of
   `APPEND_CHUNK_MAX = 64`, each a single writer job / transaction; the handler
   awaits a chunk before queuing the next, so the writer reaches queued jobs
   between batches instead of holding the connection for a whole 256-event
   `REPLAY_PAGE`. Per-event sequence/gap/replay checks and the
   `replayed`/`watermark` reply are unchanged; a gap rolls the chunk back.
5. **Job budget**: every writer job and pool read carries a static name
   (`run_named` / `read`); time covers **queue wait + execution** and a `warn`
   fires past 1 s. Evidence trail, not a cancellation.

## 2 Before / after timings (same harness, same 200k-event DB)

### 2.1 Screen journal envelope — the 31 s read

The no-cursor screen fallback over 200,000 events. "Before" is the exact old
query (`SELECT payload_json … WHERE seq > 0 ORDER BY seq ASC`) plus JSON parse;
"after" is the new windowed `read_journal`:

```
EVIDENCE envelope before=2209ms rows=200000 after=27ms rows=2000 durable=200000
```

82× faster; 100× fewer rows parsed; the last event is still the durable tail.
On the demo's busier box (120 MB, three live ingesters) the same unbounded read
was the 31–40 s number; the window is constant in journal length.

### 2.2 Interactions list vs a blocked writer — the 40 s web-boot stall

A 1.2 s job occupies the single writer (standing in for the long envelope read
that caused the demo stall). The identical pending-interactions query is then
run once where it used to live (writer thread) and once on the new pool:

```
EVIDENCE interactions vs blocked-writer before=1149ms rows=1000 after=18ms rows=1000
```

A read on the pool does not wait on the writer at all. Under three hosts
streaming 64-event ingest batches concurrently, the pool list also stays flat
(`before=26ms after=18ms` here only because a 64-event chunk is short; the
blocked-writer row is the faithful reproduction of the demo).

### 2.3 Observe fan-out — the >45 s request

20 sequential 256-event tail reads against the 200k-row instance while a fresh
three-host ingest burst lands, routed through the writer (before) vs the pool
(after):

```
EVIDENCE observe fanout 20x tail256 under ingest before=568ms rows=5120 after=85ms rows=5120
```

6.7× faster for the store hops, and every hop is now independent of whatever
the writer is doing; the 5 s-per-worker Node screen RPC is unchanged.

### 2.4 Regression-guarded timings in `hub_store.rs`

- `long_reader_does_not_block_writer_or_unrelated_read`: a 1.5 s read holds one
  pool connection; a write and an unrelated read each finish in < 500 ms.
- `journal_endpoint_windows_a_200k_event_journal`: HTTP GET over 200k events in
  < 2 s, exactly 2000 rows returned, tail = durable; a second instance of 200
  × 64 KB rows trips the **byte** cap before the row cap and stays contiguous
  and tail-anchored.
- `bounded_ingest_yields_to_a_concurrent_point_read`: a 256-event frame folded
  into 64-event jobs; a concurrent `get_instance` lands in < 200 ms and all 256
  events are durable with contiguous seq 1..256.

## 3 Budget warn lines

Captured with a `warn` subscriber during the harness. A read past 1 s and a
writer block past 1 s both name the job, the kind, and the elapsed time; queue
wait is included:

```
WARN remuda_hub::store: hub store job exceeded budget job="test.read_all_journal" kind="read" elapsed_ms=2209
WARN remuda_hub::store: hub store job exceeded budget job="test.hold_writer"    kind="write" elapsed_ms=1200
WARN remuda_hub::store: hub store job exceeded budget job="test.pending_via_writer" kind="write" elapsed_ms=1149
WARN remuda_hub::store: hub store job exceeded budget job="test.hold_reader"    kind="read" elapsed_ms=1500
```

The queued `pending_via_writer` is logged with its **own** name but 1149 ms —
the wait behind the blocking job, which is precisely the stall class this
budget exists to surface. Production names are the Store method / call site
(e.g. `read_journal`, `list_interactions`, `append_journal_batch`).

## 4 Invariants preserved

- Journal ordering, gap detection, `durableSeq`, and replay handling are
  unchanged; the batched transaction uses the same per-event logic as the
  single append (`append_loaded_event`, shared by both paths).
- Interaction / instance / command / native-session projections and the
  `replayed` + `watermark` reply are byte-for-byte the same.
- API shapes, the CLI, `remuda-protocol`, `remuda-hub-client`, the Node, and
  `web/` are untouched.

## 5 Incidental fix

`tests/host_files.rs` dropped its `TempDir` when `fixture()` returned (the
handle was not stored), unlinking the data directory. This was latent while
every query reused the one pre-opened writer file descriptor; the lazily opened
reader pool opens its own connections on demand and could not open the unlinked
path (`SQLITE_CANTOPEN`, surfaced as HTTP 500). `Fixture` now retains `_dir`.
