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

/**
 * Outcome of one {@link JournalClient.loadEarlier} click.
 *  - `prepended`: at least one unseen row was delivered through `onPrepend`.
 *  - `end`: no older window can exist anymore (seq 1 is held, the read reached
 *    its afterSeq cursor, or a read added nothing after descending).
 *  - `floor`: the retained (lowest LOADED) floor after the click, or "1" at end.
 *
 * A click can return `{prepended:false, end:false}`: a window that re-served
 * only rows already held (a reconnect snapshot re-anchored above them). The
 * descending cursor advanced, so the next click reaches further history.
 */
export type EarlierWindow = {
  prepended: boolean;
  end: boolean;
  floor: U64 | null;
};

function n(seq: U64): number {
  const v = Number(seq);
  if (!Number.isFinite(v)) throw new Error(`invalid seq ${seq}`);
  return v;
}

function s(v: number): U64 {
  return String(v);
}

/**
 * Thrown by {@link JournalClient.resumeAfterReconnect} when the resume read
 * (or its gap fill) settles readonly-stale: the journal is NOT whole, so the
 * connection machine's resume action must fail and retry instead of
 * certifying a false live. The status stays "readonly-stale" for the UI.
 */
export class JournalResumeStaleError extends Error {
  constructor(message = "journal resume ended readonly-stale") {
    super(message);
    this.name = "JournalResumeStaleError";
  }
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
  /** The single shared in-flight gap-fill (concurrent onGap/resume join it). */
  private fillingPromise: Promise<U64 | null> | null = null;
  /** Range/generation of the in-flight fill, so a caller can tell whether
   * joining it can actually reach ITS target. */
  private fillingFrom = 0;
  private fillingTo = 0;
  private fillingGen = -1;
  private loadingEarlier = false;
  /**
   * The `beforeSeq` of the next descending load-earlier read. Every snapshot
   * (seed / bounded resync after a reconnect) re-anchors it to that snapshot's
   * window floor minus 1, so paging starts below the CURRENT server window even
   * when that window moved above rows this client already holds. Every returned
   * window moves it to `windowFromSeq - 1`, including a window of rows all
   * already held, so repeated clicks descend past a re-anchor instead of
   * retiring on the duplicate page. 0 means paging reached the end.
   */
  private pagingBelow = Number.POSITIVE_INFINITY;
  status: "live" | "reconnecting" | "gap-backfill" | "readonly-stale" = "live";
  /** Bumped per resumeAfterReconnect; stale attempts are no-ops. */
  private resumeGen = 0;

  constructor(journalId: Id, read: JournalRead, listeners: JournalListener = {}) {
    this.journalId = journalId;
    this.read = read;
    this.listeners = listeners;
  }

  get appliedSeq(): U64 {
    return s(this.applied);
  }

  /**
   * A contiguous live socket batch that advanced the applied cursor is proof
   * the stream is current. It supersedes any resume/fill read still in flight:
   * bump the resume generation so a slower read cannot downgrade a live
   * recovery to readonly-stale (item: socket recovery must supersede a pending
   * catch-up failure).
   */
  noteSocketCaughtUp(): void {
    this.resumeGen += 1;
  }

  /** Generation captured by a fillGap caller; exposed so the caller passes it. */
  currentResumeGen(): number {
    return this.resumeGen;
  }

  /**
   * Seq of the oldest loaded event. Rows below it exist server-side only while
   * it is above 1; a load-earlier read moves it down.
   */
  get retainedFloorSeq(): U64 {
    return Number.isFinite(this.floorSeq) ? s(this.floorSeq) : "1";
  }

  /**
   * Register the rows delivered by the initial bounded seed READ (as opposed
   * to the follow socket / fills, which arrive through applyBatch). The seed
   * is held by the UI even though it never crossed this client's seen map;
   * without it a load-earlier window overlapping the seed would count those
   * rows as fresh and skip the retained floor straight past them.
   */
  noteHistory(events: Observation[]): void {
    for (const ev of events) {
      const seq = n(ev.seq);
      this.seen.set(seq, ev.eventId);
      if (events.length) this.floorSeq = Math.min(this.floorSeq, seq);
    }
    this.durableSeq = Math.max(
      this.durableSeq,
      events.reduce((max, ev) => Math.max(max, n(ev.seq)), 0),
    );
  }

  applySnapshot(snapshot: Snapshot): void {
    const target = n(snapshot.asOfSeq);
    this.durableSeq = Math.max(this.durableSeq, target);
    // Never advance the applied cursor past rows the client has not EMITTED.
    // Fresh client on the initial seed jumps to the snapshot cursor; an
    // existing client keeps its applied cursor and catches up via the
    // follow's live frames / resumeAfterReconnect (a snapshot that claims a
    // higher cursor while rows are missing must not skip them).
    if (this.applied === 0 && this.buffer.size === 0) {
      this.applied = target;
    }
    const nextFloor = n(snapshot.history.earliestRetainedSeq);
    // Never raise the loaded floor above rows the client actually holds: a
    // bounded resync snapshot (complete or partial) after a reconnect can
    // name a floor above manually paged history, and retiring to it would make
    // that history unreachable. A genuinely truncated journal is discovered by
    // the next descending read returning nothing (one wasted click at most).
    this.floorSeq = Math.min(this.floorSeq, nextFloor);
    // Re-anchor the descending pager below THIS snapshot's window. It may sit
    // above rows already loaded (the next clicks walk down through duplicate
    // windows); the retained floor above keeps the button offered meanwhile.
    this.pagingBelow = Math.max(0, nextFloor - 1);
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
  fillGap(from: U64, to: U64, gen?: number): Promise<U64 | null> {
    // Share one in-flight fill only when it can serve THIS caller: its owner
    // generation must match (a fill superseded by a newer resume/socket
    // recovery must not be accepted as the newer caller's outcome) and its
    // range must reach the caller's target. A queued+forwarded "unknown" hole
    // discovered from the tail (gap 2..3000) and a resume whose own read sees
    // the taller tail (gap 2..4872) differ in BOTH: the resume waits for the
    // superseded fill to end and then starts a replacement, instead of
    // publishing live over the part the old fill never reached.
    if (this.status === "readonly-stale") return Promise.resolve(null);
    if (this.fillingPromise) {
      const callerGen = gen ?? this.resumeGen;
      const covers = this.fillingFrom <= n(from) && this.fillingTo >= n(to);
      if (this.fillingGen === callerGen && covers) return this.fillingPromise;
      // In-flight fill is stale or too short: do not race its reads. Wait for
      // it to finish (its finally clears the slot first), then re-enter — the
      // first waiter starts the replacement; later waiters join that one when
      // it covers their range/generation.
      return this.fillingPromise.then(() => this.fillGap(from, to, callerGen));
    }
    // Capture the current resume generation by default (every fill is
    // generation-guarded, not just explicit callers).
    const ownerGen = gen ?? this.resumeGen;
    const stale = () => ownerGen !== this.resumeGen;
    this.filling = true;
    this.fillingFrom = n(from);
    this.fillingTo = n(to);
    this.fillingGen = ownerGen;
    const run = this.doFillGap(from, to, stale);
    this.fillingPromise = run.finally(() => {
      this.filling = false;
      this.fillingPromise = null;
    });
    return this.fillingPromise;
  }

  private async doFillGap(
    from: U64,
    to: U64,
    stale: () => boolean,
  ): Promise<U64 | null> {
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
        // A newer resume/socket recovery landed while this page was in flight:
        // stop descending without mutating status or buffering.
        if (stale()) return this.flush();
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
        if (stale()) return flushed; // a newer resume/socket recovery owns this
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
      if (stale()) return null;
      this.setStatus("readonly-stale");
      return null;
    }
  }

  /**
   * Load one window of older history below the current descending cursor.
   * Unseen rows are delivered ascending through `onPrepend`. A window whose
   * rows are all already held (a reconnect snapshot re-anchored above loaded
   * history) fires no prepend but advances the cursor, so repeated clicks keep
   * descending until new rows — or the end at seq 1 / the afterSeq cursor.
   */
  async loadEarlier(): Promise<EarlierWindow> {
    const atEnd = (): EarlierWindow => ({
      prepended: false,
      end: true,
      floor: this.retainedFloorSeq,
    });
    if (this.loadingEarlier) return atEnd();
    if (!Number.isFinite(this.floorSeq) || this.floorSeq <= 1 || this.pagingBelow <= 0) {
      return atEnd();
    }
    this.loadingEarlier = true;
    try {
      const page = await this.read({
        journalId: this.journalId,
        afterSeq: "0",
        beforeSeq: s(this.pagingBelow),
        limit: JOURNAL_WINDOW_READ,
      });
      this.durableSeq = Math.max(this.durableSeq, n(page.durableSeq));
      if (page.events.length === 0) {
        // The paging cursor claimed older rows existed but none came back: end
        // of the line, stop offering the action instead of re-requesting.
        this.floorSeq = 1;
        this.pagingBelow = 0;
        return atEnd();
      }
      // Divergence check on every row; collect the ones not already held.
      const fresh: Observation[] = [];
      for (const ev of page.events) {
        const seq = n(ev.seq);
        const prev = this.seen.get(seq);
        if (prev && prev !== ev.eventId) {
          this.listeners.onDiverged?.(ev.seq);
          this.setStatus("readonly-stale");
          return atEnd();
        }
        if (!prev) fresh.push(ev);
      }
      const wFloor = page.windowFromSeq === null ? null : n(page.windowFromSeq);
      // Advance the DESCENDING cursor from the window this read actually
      // returned — even when every row was a duplicate — because the snapshot
      // re-anchor may point above the rows this client already holds.
      if (wFloor !== null) this.pagingBelow = wFloor - 1;
      // reachedAfterSeq names the afterSeq cursor ("0"); a null/<=1 floor says
      // the same for older Hubs.
      const reachedEnd = page.reachedAfterSeq || wFloor === null || wFloor <= 1;
      if (fresh.length === 0) {
        if (reachedEnd) {
          this.floorSeq = 1;
          this.pagingBelow = 0;
        }
        // Duplicate-only, history remains: no onPrepend, the retained floor is
        // preserved and the button stays offered; the cursor now points below
        // this window for the next click.
        return {
          prepended: false,
          end: reachedEnd,
          floor: this.retainedFloorSeq,
        };
      }
      // Rows ascend; the lowest unseen row is the new retained floor.
      this.floorSeq = Math.min(this.floorSeq, n(fresh[0]!.seq));
      for (const ev of fresh) this.seen.set(n(ev.seq), ev.eventId);
      this.listeners.onPrepend?.(fresh.slice());
      if (reachedEnd) {
        this.floorSeq = 1;
        this.pagingBelow = 0;
      }
      return {
        prepended: true,
        end: reachedEnd || this.floorSeq <= 1,
        floor: this.retainedFloorSeq,
      };
    } catch {
      this.setStatus("readonly-stale");
      return atEnd();
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
    // Each resume gets a generation: overlapping catch-ups (send resolves
    // while one is pending, then steer/visibility starts another) must never
    // let a SLOWER read downgrade the newer one's result. A stale attempt
    // neither changes status nor throws — the newest attempt owns the outcome.
    const gen = ++this.resumeGen;
    let page;
    try {
      page = await this.read({
        journalId: this.journalId,
        afterSeq: this.appliedSeq,
        limit: 128,
      });
    } catch (err) {
      if (gen !== this.resumeGen) return this.appliedSeq;
      // A rejected resync read must never leave the client latched at
      // "reconnecting": settle at the same truthful retryable state a gap
      // budget exhaustion uses. onStatus mirrors it to the UI (只读 banner
      // with its 重试 action), and a later contiguous socket batch or a
      // successful retry restores "live". Re-throw so the caller can report
      // why the resync did not happen.
      this.setStatus("readonly-stale");
      throw err;
    }
    if (gen !== this.resumeGen) return this.appliedSeq;
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
    if (gen !== this.resumeGen) return this.appliedSeq;
    if (result.gap) await this.fillGap(result.gap.from, result.gap.to, gen);
    if (gen !== this.resumeGen) return this.appliedSeq;
    // A fill that settled readonly-stale (descent budget exhausted,
    // divergence, or a failed fill read) means the resume did NOT make the
    // journal whole. Resolving here would let the machine publish live over
    // a hole; throw so the resume action fails, the machine stays offline and
    // retries (the read-rejection path above fails the same way). A
    // contiguous socket recovery flushes status to live first and skips this.
    if (this.status === "readonly-stale") throw new JournalResumeStaleError();
    // Contiguity is the success certificate, not the fill promise's mere
    // resolution: a joined/replacement fill can end while rows below its
    // target are still missing (e.g. a superseded tail fill the resume had to
    // replace). The applied cursor must actually have reached the gap's far
    // edge; otherwise fail the resume instead of going live over the hole.
    if (result.gap && this.applied < n(result.gap.to)) {
      this.setStatus("readonly-stale");
      throw new JournalResumeStaleError();
    }
    this.setStatus("live");
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
