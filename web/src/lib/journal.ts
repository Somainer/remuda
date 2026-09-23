import type { EventsBatch, Observation, Snapshot } from "../types/observation";
import type { Id, U64 } from "../types/wire";

export type JournalListener = {
  onEvents?: (events: Observation[]) => void;
  /** Older history fetched by {@link JournalClient.loadEarlier}, ascending seq. */
  onPrepend?: (events: Observation[]) => void;
  onGap?: (missingFrom: U64, missingTo: U64) => void;
  onSnapshot?: (snapshot: Snapshot) => void;
  onDiverged?: (seq: U64) => void;
  onStatus?: (status: "live" | "reconnecting" | "gap-backfill" | "readonly-stale") => void;
};

export type JournalRead = (args: {
  journalId: Id;
  afterSeq?: U64;
  /** Inclusive upper bound for descending below a bounded tail window. */
  beforeSeq?: U64;
  limit: number;
}) => Promise<{
  events: Observation[];
  durableSeq: U64;
  /**
   * Window floor (`events[0].seq`) as reported by the Hub, or null on an empty
   * window. This is a WINDOW floor, not a retention floor: it only says where
   * this page stops, not where the journal begins.
   */
  windowFromSeq: U64 | null;
  /**
   * False when rows below the window floor were cut, so the caller must descend
   * with `beforeSeq = windowFromSeq - 1`. Absent on pre-window Hubs, which
   * always served the whole range.
   */
  reachedAfterSeq: boolean;
}>;

/**
 * Maximum descending pages one {@link JournalClient.fillGap} will issue before
 * it flushes the contiguous prefix it has and settles readonly-stale. At the
 * Hub's 2,000-row window this is far more backfill than any real disconnect
 * needs and a hard stop against buffering forever.
 */
const MAX_FILL_PAGES = 16;

/** Event-count companion to {@link MAX_FILL_PAGES} (about 10 windows deep). */
const MAX_FILL_EVENTS = 20_000;

/** One Hub window; load-earlier fetches a single page per click. */
const JOURNAL_WINDOW_READ = 2_000;

function n(seq: U64): number {
  const v = Number(seq);
  if (!Number.isFinite(v)) throw new Error(`invalid seq ${seq}`);
  return v;
}

function s(v: number): U64 {
  return String(v);
}

/** Client-side snapshot + seq follow + gap fill (protocol.md §7.3). */
export class JournalClient {
  readonly journalId: Id;
  private applied = 0;
  private durableSeq = 0;
  // +Infinity until the first snapshot seeds the window floor; min() then
  // tracks the lowest loaded window.
  private floorSeq = Number.POSITIVE_INFINITY;
  private buffer = new Map<number, Observation>();
  private seen = new Map<number, Id>();
  private listeners: JournalListener;
  private read: JournalRead;
  private filling = false;
  private loadingEarlier = false;
  status: "live" | "reconnecting" | "gap-backfill" | "readonly-stale" = "live";

  constructor(journalId: Id, read: JournalRead, listeners: JournalListener = {}) {
    this.journalId = journalId;
    this.read = read;
    this.listeners = listeners;
  }

  get appliedSeq(): U64 {
    return s(this.applied);
  }

  /**
   * Seq of the oldest loaded event. Rows below it exist server-side only while
   * it is above 1; a load-earlier read moves it down.
   */
  get retainedFloorSeq(): U64 {
    return Number.isFinite(this.floorSeq) ? s(this.floorSeq) : "1";
  }

  applySnapshot(snapshot: Snapshot): void {
    this.applied = n(snapshot.asOfSeq);
    this.durableSeq = n(snapshot.asOfSeq);
    const nextFloor = n(snapshot.history.earliestRetainedSeq);
    // A partial snapshot carries a WINDOW floor, not a retention floor: it can
    // move up as the journal grows, and load-earlier must remember the lowest
    // window already loaded. Only a complete (authoritative) snapshot resets
    // the floor.
    this.floorSeq = snapshot.history.complete ? nextFloor : Math.min(this.floorSeq, nextFloor);
    this.listeners.onSnapshot?.(snapshot);
  }

  applyBatch(batch: EventsBatch["params"]): { acked: U64 | null; gap: { from: U64; to: U64 } | null } {
    const from = n(batch.fromSeq);
    const to = n(batch.toSeq);
    if (batch.events.length === 0) return { acked: null, gap: null };
    const first = n(batch.events[0].seq);
    const last = n(batch.events[batch.events.length - 1].seq);
    if (first !== from || last !== to) {
      this.setStatus("readonly-stale");
      return { acked: null, gap: null };
    }

    this.durableSeq = Math.max(this.durableSeq, n(batch.durableSeq));
    if (Number.isFinite(this.floorSeq) && from < this.floorSeq) {
      this.setStatus("readonly-stale");
      return { acked: null, gap: null };
    }

    for (const ev of batch.events) {
      const seq = n(ev.seq);
      const prev = this.seen.get(seq);
      if (prev && prev !== ev.eventId) {
        this.listeners.onDiverged?.(ev.seq);
        this.setStatus("readonly-stale");
        return { acked: null, gap: null };
      }
      this.seen.set(seq, ev.eventId);
      this.buffer.set(seq, ev);
    }

    if (from > this.applied + 1) {
      const gapFrom = s(this.applied + 1);
      const gapTo = s(from - 1);
      this.listeners.onGap?.(gapFrom, gapTo);
      this.setStatus("gap-backfill");
      return { acked: null, gap: { from: gapFrom, to: gapTo } };
    }

    const flushed = this.flush();
    return { acked: flushed, gap: null };
  }

  /**
   * Backfill `(applied, …]` after a gapped batch.
   *
   * The Hub serves a bounded TAIL window: the first read can return rows whose
   * floor is still above `from` (a burst wider than the window pushed the gap
   * out of the window's bottom). Descend with the same `afterSeq` and
   * `beforeSeq = windowFromSeq - 1` until a page `reachedAfterSeq`, buffering
   * every row in `[from, to]`, then flush once so the listener sees the range
   * in order. The descent is page- and event-bounded: when the budget runs out
   * the contiguous prefix flushes, the residual gap is reported once, and the
   * client settles readonly-stale instead of buffering forever.
   */
  async fillGap(from: U64, to: U64): Promise<U64 | null> {
    if (this.filling) return null;
    if (this.status === "readonly-stale") return null;
    this.filling = true;
    this.setStatus("gap-backfill");
    const gapFrom = n(from);
    const gapTo = n(to);
    let reached = false;
    try {
      const afterSeq = s(gapFrom - 1);
      let beforeSeq: U64 | undefined;
      let previousFloor: number | null = null;
      let inGapRows = 0;
      for (let pageNo = 0; pageNo < MAX_FILL_PAGES; pageNo += 1) {
        const page = await this.read({
          journalId: this.journalId,
          afterSeq,
          beforeSeq,
          limit: Math.max(1, gapTo - gapFrom + 1),
        });
        this.durableSeq = Math.max(this.durableSeq, n(page.durableSeq));
        for (const ev of page.events) {
          const seq = n(ev.seq);
          if (seq < gapFrom || seq > gapTo) continue;
          const prev = this.seen.get(seq);
          if (prev && prev !== ev.eventId) {
            this.listeners.onDiverged?.(ev.seq);
            this.setStatus("readonly-stale");
            return null;
          }
          this.seen.set(seq, ev.eventId);
          this.buffer.set(seq, ev);
          inGapRows += 1;
        }
        const floor = page.windowFromSeq === null ? null : n(page.windowFromSeq);
        // Every descended window became part of the loaded range; keep the
        // load-earlier anchor at the lowest RETAINED window. Rows returned
        // below gapFrom are filtered out (they predate the gap), so a page
        // that reaches into them must not pull the floor past rows the client
        // never kept — that would retire load-earlier while those rows are
        // still missing.
        if (floor !== null) this.floorSeq = Math.min(this.floorSeq, Math.max(floor, gapFrom));
        if (page.reachedAfterSeq || floor === null || floor <= gapFrom) {
          // The window reaches the afterSeq cursor (or is empty): the whole
          // queried range is covered. Treating "floor at/below gap start" as
          // complete also tolerates an old Hub that omits the flag but did
          // return rows covering gapFrom.
          reached = true;
          break;
        }
        if (previousFloor !== null && floor >= previousFloor) {
          // The server stopped descending without declaring completeness: do
          // not spin on the same window; flush what is contiguous and go stale.
          break;
        }
        previousFloor = floor;
        if (inGapRows >= MAX_FILL_EVENTS) break;
        beforeSeq = s(floor - 1);
      }
      const flushed = this.flush();
      if (!reached) {
        // Rows are still missing below the flushed contiguous prefix. Report
        // the residual gap exactly once: the store routes onGap straight back
        // into fillGap, but the readonly-stale status makes that a no-op.
        const missingFrom = this.applied + 1;
        if (missingFrom <= gapTo && !this.buffer.has(missingFrom)) {
          this.listeners.onGap?.(s(missingFrom), to);
          this.setStatus("readonly-stale");
        }
        // Otherwise the whole requested range flushed even though the server
        // never declared the window complete; leave the status flush chose.
      }
      return flushed;
    } catch {
      this.setStatus("readonly-stale");
      return null;
    } finally {
      this.filling = false;
    }
  }

  /**
   * Load one window of older history below the current loaded floor. Prepends
   * are delivered ascending through `onPrepend`; the loaded floor moves down
   * and returns null once seq 1 is held or an older read yields nothing.
   */
  async loadEarlier(): Promise<U64 | null> {
    if (this.loadingEarlier) return null;
    if (!Number.isFinite(this.floorSeq) || this.floorSeq <= 1) return null;
    this.loadingEarlier = true;
    try {
      const page = await this.read({
        journalId: this.journalId,
        afterSeq: "0",
        beforeSeq: s(this.floorSeq - 1),
        limit: JOURNAL_WINDOW_READ,
      });
      this.durableSeq = Math.max(this.durableSeq, n(page.durableSeq));
      if (page.events.length === 0) {
        // The floor claimed older rows existed but none came back; stop
        // offering the action rather than re-requesting forever.
        this.floorSeq = 1;
        return null;
      }
      for (const ev of page.events) {
        const seq = n(ev.seq);
        const prev = this.seen.get(seq);
        if (prev && prev !== ev.eventId) {
          this.listeners.onDiverged?.(ev.seq);
          this.setStatus("readonly-stale");
          return null;
        }
        this.seen.set(seq, ev.eventId);
      }
      this.floorSeq = Math.min(this.floorSeq, n(page.events[0].seq));
      this.listeners.onPrepend?.(page.events.slice());
      return this.retainedFloorSeq;
    } catch {
      this.setStatus("readonly-stale");
      return null;
    } finally {
      this.loadingEarlier = false;
    }
  }

  markReconnecting(): void {
    this.setStatus("reconnecting");
  }

  /**
   * The follow socket signalled hub-side backpressure, and the following
   * bounded resync snapshot starts at `windowFloor`, above the applied cursor.
   * Backfill [cursor+1, windowFloor-1] by descending with beforeSeq; the
   * snapshot batch itself advances applied through windowFloor onward.
   */
  async fillResyncGap(windowFloor: U64): Promise<void> {
    if (this.status === "readonly-stale" || this.filling) return;
    const from = this.applied + 1;
    const floor = n(windowFloor);
    // durableSeq is stale here (the burst landed while gated); the hole is
    // defined by the cursor and the snapshot floor, not by durableSeq.
    if (floor <= from) {
      // Snapshot starts at/below applied: its batch carries forward directly.
      return;
    }
    await this.fillGap(s(from), s(floor - 1));
  }

  async resumeAfterReconnect(): Promise<U64 | null> {
    let page;
    try {
      page = await this.read({
        journalId: this.journalId,
        afterSeq: this.appliedSeq,
        limit: 128,
      });
    } catch (err) {
      // A rejected resync read must never leave the client latched at
      // "reconnecting": settle at the same truthful retryable state a gap
      // budget exhaustion uses. onStatus mirrors it to the UI (只读 banner
      // with its 重试 action), and a later contiguous socket batch or a
      // successful retry restores "live". Re-throw so the caller can report
      // why the resync did not happen.
      this.setStatus("readonly-stale");
      throw err;
    }
    if (page.events.length === 0) {
      this.setStatus("live");
      return this.appliedSeq;
    }
    const from = page.events[0].seq;
    const to = page.events[page.events.length - 1].seq;
    const result = this.applyBatch({
      subscriptionId: "resume",
      journalId: this.journalId,
      fromSeq: from,
      toSeq: to,
      events: page.events,
      durableSeq: page.durableSeq,
    });
    if (result.gap) await this.fillGap(result.gap.from, result.gap.to);
    // fillGap settles readonly-stale when its descent budget runs out; do not
    // overwrite that verdict with a blanket "live".
    if (this.status !== "readonly-stale") this.setStatus("live");
    return this.appliedSeq;
  }

  private flush(): U64 | null {
    const emitted: Observation[] = [];
    let next = this.applied + 1;
    while (this.buffer.has(next)) {
      const ev = this.buffer.get(next)!;
      this.buffer.delete(next);
      emitted.push(ev);
      this.applied = next;
      next += 1;
    }
    if (emitted.length) this.listeners.onEvents?.(emitted);
    if (this.buffer.size === 0) this.setStatus("live");
    return emitted.length ? s(this.applied) : null;
  }

  private setStatus(status: JournalClient["status"]): void {
    if (this.status === status) return;
    this.status = status;
    this.listeners.onStatus?.(status);
  }
}
