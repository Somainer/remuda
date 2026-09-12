import { describe, expect, it } from "vitest";
import type { Id } from "../types/wire";
import { JournalClient } from "./journal";
import {
  GAP_HISTORY_SEQ,
  GAP_TAIL_SEQ,
  mockGappedTail,
  mockJournalIds,
  mockReadJournal,
} from "./mock";

describe("mock gap journal", () => {
  it("truncates history then exposes a gapped tail", () => {
    const page = mockReadJournal(mockJournalIds.journalGap);
    expect(page.events.map((e) => Number(e.seq))).toEqual([1, 2, 3, 4]);
    expect(Number(page.durableSeq)).toBeGreaterThan(GAP_HISTORY_SEQ);
    const tail = mockGappedTail(mockJournalIds.journalGap);
    expect(tail).not.toBeNull();
    expect(Number(tail!.fromSeq)).toBe(GAP_TAIL_SEQ);
    expect(Number(tail!.fromSeq)).toBeGreaterThan(GAP_HISTORY_SEQ + 1);
  });

  it("drives JournalClient into gap-backfill then live", async () => {
    const emitted: number[] = [];
    const statuses: string[] = [];
    const client = new JournalClient(mockJournalIds.journalGap, async ({ afterSeq, limit }) => mockReadJournal(mockJournalIds.journalGap, afterSeq, limit), {
      onEvents: (events) => emitted.push(...events.map((e) => Number(e.seq))),
      onStatus: (status) => statuses.push(status),
    });
    client.applySnapshot({
      projectionVersion: "v1",
      projectionEpoch: "epoch_1" as Id,
      asOfSeq: String(GAP_HISTORY_SEQ),
      instance: {},
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "1", complete: true },
    });
    const tail = mockGappedTail(mockJournalIds.journalGap)!;
    const gapped = client.applyBatch(tail);
    expect(gapped.gap).toEqual({ from: String(GAP_HISTORY_SEQ + 1), to: String(GAP_TAIL_SEQ - 1) });
    expect(client.status).toBe("gap-backfill");
    await client.fillGap(gapped.gap!.from, gapped.gap!.to);
    expect(client.status).toBe("live");
    expect(emitted[0]).toBe(GAP_HISTORY_SEQ + 1);
    expect(emitted.at(-1)).toBeGreaterThanOrEqual(GAP_TAIL_SEQ);
  });

  it("goes readonly-stale when gap fill throws", async () => {
    const client = new JournalClient(mockJournalIds.journalStale, async ({ afterSeq, limit }) => mockReadJournal(mockJournalIds.journalStale, afterSeq, limit));
    client.applySnapshot({
      projectionVersion: "v1",
      projectionEpoch: "epoch_1" as Id,
      asOfSeq: String(GAP_HISTORY_SEQ),
      instance: {},
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "1", complete: true },
    });
    const tail = mockGappedTail(mockJournalIds.journalStale)!;
    const gapped = client.applyBatch(tail);
    expect(client.status).toBe("gap-backfill");
    await client.fillGap(gapped.gap!.from, gapped.gap!.to);
    expect(client.status).toBe("readonly-stale");
  });
});
