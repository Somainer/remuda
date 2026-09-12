import type { EventsBatch, Observation, Snapshot } from "../types/observation";
import type { Id, U64 } from "../types/wire";

export type JournalListener = {
  onEvents?: (events: Observation[]) => void;
  onGap?: (missingFrom: U64, missingTo: U64) => void;
  onSnapshot?: (snapshot: Snapshot) => void;
  onDiverged?: (seq: U64) => void;
  onStatus?: (status: "live" | "reconnecting" | "gap-backfill" | "readonly-stale") => void;
};

export type JournalRead = (args: {
  journalId: Id;
  afterSeq?: U64;
  beforeSeq?: U64;
  limit: number;
}) => Promise<{ events: Observation[]; durableSeq: U64; floorSeq: U64 }>;

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
  private floorSeq = 1;
  private buffer = new Map<number, Observation>();
  private seen = new Map<number, Id>();
  private listeners: JournalListener;
  private read: JournalRead;
  private filling = false;
  status: "live" | "reconnecting" | "gap-backfill" | "readonly-stale" = "live";

  constructor(journalId: Id, read: JournalRead, listeners: JournalListener = {}) {
    this.journalId = journalId;
    this.read = read;
    this.listeners = listeners;
  }

  get appliedSeq(): U64 {
    return s(this.applied);
  }

  applySnapshot(snapshot: Snapshot): void {
    this.applied = n(snapshot.asOfSeq);
    this.durableSeq = n(snapshot.asOfSeq);
    this.floorSeq = n(snapshot.history.earliestRetainedSeq);
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
    if (from < this.floorSeq) {
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

  async fillGap(from: U64, to: U64): Promise<U64 | null> {
    if (this.filling) return null;
    this.filling = true;
    this.setStatus("gap-backfill");
    try {
      const page = await this.read({
        journalId: this.journalId,
        afterSeq: s(n(from) - 1),
        limit: Math.max(1, n(to) - n(from) + 1),
      });
      this.floorSeq = n(page.floorSeq);
      this.durableSeq = Math.max(this.durableSeq, n(page.durableSeq));
      for (const ev of page.events) {
        const seq = n(ev.seq);
        if (seq < n(from) || seq > n(to)) continue;
        this.seen.set(seq, ev.eventId);
        this.buffer.set(seq, ev);
      }
      return this.flush();
    } catch {
      this.setStatus("readonly-stale");
      return null;
    } finally {
      this.filling = false;
    }
  }

  markReconnecting(): void {
    this.setStatus("reconnecting");
  }

  async resumeAfterReconnect(): Promise<U64 | null> {
    const page = await this.read({
      journalId: this.journalId,
      afterSeq: this.appliedSeq,
      limit: 128,
    });
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
