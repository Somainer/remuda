# Tail-only journal flush — journal-flush-1

The dev demo process burned 110–130% of one core with 25 threads while nothing
happened: 5 log lines in 10 minutes, 55 CPU-minutes over 50 elapsed minutes.
Two 5 s samples agreed on the hot frames — `runtime_wss::flush_journal`,
`submit_forward`, `encode_append_params`, `DevNode::read_journal`,
`DurableJournal::read_range`, with leaves in `serde_json` escape/parse, sip
hashing and `memmove`. The Node held 11 journal files open under its journal
directory, which held 108 files / 45 MB, the largest journal 20 MB across 7122
events (lifecycle `exited`, Hub durable at 7122).

This document records the mechanism, the cost arithmetic, and the measured
before/after CPU on the machine that reproduced it.

## Mechanism

The profile's shape is a bound that never reached the reader.

`crates/remuda-node/src/store.rs` `read_events` asked
`read_range(after + 1, None)` and then applied the page limit with
`.take(limit.clamp(1, 256))` *after* the read had already returned.
`crates/remuda-journal/src/store.rs` documents `to_seq = None` as "reads
through the durable watermark", so the reader selected every matching row and
sought and parsed one `Observation` per row. A 256-event page off a 20 MB
journal therefore parsed 20 MB and dropped 96% of it.

Three further multipliers compounded it:

- `runtime_wss.rs` paged from the Hub watermark with no cursor of its own, set
  a 250 ms retry, and re-entered the flush on every tick while an append was
  unacknowledged — a stuck instance re-parsed its tail four times a second.
- `resume_runtime_journals` walked every instance with no lifecycle or
  caught-up filter and called both `ensure_pump` (which itself replays) and
  `flush_journal`, so each instance replayed twice per (re)connect.
- Watermarks lived only in a process `HashMap` and `apply_resume_watermarks`
  cleared them on each hello, so a reconnect could restart every instance from
  seq 0.

The carriers repeated the shape: `daemon.rs` ticks every 50 ms and
`forward_journals` walks every instance; `stdio.rs` `forward_backlog` pages the
same way; `runtime_link.rs` flushes after every Hub request over all instances.

One suspected detail was refuted: a byte-offset index (`jsonl_offset`) does
exist, so the reader never scanned from byte 0. The cost was the unbounded seq
range plus the missing lifecycle/caught-up skip.

A hazard sat behind it: `store.rs` serves `Append` and `ReadRange` from one
journal thread, so an unbounded `ReadRange` blocked new appends for its whole
duration.

## Cost arithmetic

For the observed 7122-event / 20 MB journal:

- One 256-event page parsed the full ~10 MB average tail: 4× the bytes it kept.
- A full replay is `ceil(7122/256) = 28` pages × ~10 MB ≈ **280 MB parsed to
  move 20 MB**.
- Over the 11 open journals (33 MB total) one sweep was roughly **450 MB parsed
  per sweep**, repeating.
- A stuck instance re-entering at 250 ms paid that 4× per second.

## Measured before/after on this host

`replay_sweep_cost_is_linear_in_new_events` in
`crates/remuda-node/src/store.rs` is an ignored measurement over a seeded
7000-event, 30 082 676-byte journal. "Before" is the old shape verbatim
(`read_range(after + 1, None)` plus `take`); "after" is one bounded page per
call. CPU is process user+system ticks from `/proc/self/stat`, not wall time.
Three consecutive runs on this host:

```text
journal_bytes=30082676 events=7000
sweep  before: 28 unbounded pages, 196000 observations parsed, wall 22.6s, cpu 2263 ticks
sweep  after:  29 bounded pages, 7000 observations moved, 30082676 JSONL bytes, wall 838ms, cpu 84 ticks
stuck  before: wall 777ms, cpu 77 ticks to move 16 events (parsed 6999, dropped 6983)
stuck  after:  wall 2.3ms, cpu 0 ticks to move 16 events

sweep  before: wall 22.5s, cpu 2251 ticks
sweep  after:  wall 812ms, cpu 82 ticks
stuck  before: wall 785ms, cpu 78 ticks
stuck  after:  wall 2.3ms, cpu 0 ticks
```

A full sweep costs **27× less CPU** (2263 → 84 ticks) and **27× less wall
time**. The row that explains the idle demo is "stuck": a flush tick whose ACK
window is stalled early — exactly the state the 250 ms re-entry kept returning
to — fell from 78 CPU ticks and 6983 parsed-then-dropped observations per tick
to 0 ticks, because it now reads 16 events instead of 6999.

Run it with:

```text
cargo test -p remuda-node --lib replay_sweep_cost -- --ignored --nocapture
```

No home paths, usernames, hostnames or identifying pids appear above.

## Fix

1. **The bound reaches the reader.** `remuda-journal` gains `read_page`, which
   puts `LIMIT` in the SQL and deserializes only the selected rows' JSONL
   spans. It returns `Page { observations, bytes_read, last_seq, durable_seq }`
   where `bytes_read` is the reader's own accounting, so the cost is
   assertable rather than assumed. `read_range` is unchanged for every existing
   caller. `read_events` now passes an explicit `to_seq` (`after + limit`) and
   a page bound; `durable_seq` and `floor_seq` in the reply are unchanged.
2. **A per-instance cursor.** `journal_flush::FlushCursor` holds the last
   forwarded seq per instance beside the watermark, so a flush resumes at the
   tail and never re-parses confirmed bytes. It advances only on a resolved
   append, so a frame that never got an ACK is replayed rather than skipped.
3. **Work that cannot exist is skipped.** `flush_plan` returns `CaughtUp`
   without a read when the cursor already covers the journal's durable seq.
   Every carrier asks `journal_durable_seq` first, so an idle fleet costs one
   watermark query per instance per tick. The test is deliberately
   lifecycle-blind: `reclaim::reconcile_native_pty` marks an Instance failed
   and only *then* journals the diagnostic explaining why, so treating
   "terminal" as "finished" would strand that event. A caught-up pump costs
   nothing anyway — its replay returns without a read and its loop then idles
   on the broadcast. This is the case that makes the fix work for the observed
   demo, whose largest journal was a `exited` Instance at 7122 events.
4. **Resume is de-duplicated.** `resume_runtime_journals` calls `ensure_pump`
   only (which replays once before pumping); `catch_up` likewise. A hello that
   carries watermarks clears the cursors and re-derives them from the hello.
5. **Retries back off.** `FlushBackoff` replaces the flat 250 ms re-entry with
   a ladder from 250 ms to 5 s, reset on success, per instance.
6. **Bounded batches.** One tick reads at most one page per instance and yields
   between retries, so a failing instance cannot starve the others.
7. **Every carrier.** `runtime_wss`, `stdio`, `daemon::forward_journals` and
   `runtime_link::flush` all use the shared policy.

### Invariant held (D-019 / D-020)

The Node journal stays the authority and Hub indexes stay rebuildable. Cursors
are an optimisation only: an absent, lost or stale cursor falls back to a full
replay from the floor, never to a skipped event, and a watermark ahead of the
local journal is caught by `flush_plan`, which sees the gap and reads.

## Tests

- `one_flush_tick_costs_new_bytes_not_the_whole_journal` — a generated ~20 MB
  (7000-event) fixture plus one new event: one flush tick reads under 1% of the
  journal, and a caught-up tick reads **zero** bytes.
- `a_page_read_is_bounded_by_its_limit` — a 16-event page off a 600-event
  journal reads under a tenth of it, and the next page resumes exactly where
  the first ended.
- `a_failed_instance_at_the_tail_plans_no_read` and
  `caught_up_and_terminal_instances_plan_nothing` — a caught-up instance plans
  no read, while one this session has not caught up with still replays; an
  unknown cursor never skips.
- `a_terminal_lifecycle_that_is_journaled_after_the_fact_still_flushes` — pins
  the write-then-mark ordering above, so a future "skip terminal instances"
  shortcut cannot silently strand the reconcile diagnostic.
- `a_cursor_behind_the_tail_always_reads`, `backoff_grows_to_the_ceiling_then_resets`,
  `cursor_never_moves_backwards_and_forgets_on_clear`.
- `read_page_touches_only_its_limit` in `remuda-journal` covers the reader
  contract directly, including an empty page past the tail.
- The wss, stdio, daemon and restart suites are green, as are the full
  `remuda-node` and `remuda-journal` suites (239 + 13 lib tests and every
  integration target).
