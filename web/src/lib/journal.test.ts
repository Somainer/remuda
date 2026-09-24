import { describe, expect, it, vi } from "vitest";
import type { Observation, Snapshot } from "../types/observation";
import { JournalClient, type JournalRead } from "./journal";
import { known, unknownKnowledge, type Id } from "../types/wire";

type ReadPage = Awaited<ReturnType<JournalRead>>;

function obs(seq: number, eventId = `evt_${seq}`): Observation {
  return {
    schemaVersion: 1,
    eventId: eventId as Id,
    journalId: "obj_journal" as Id,
    instanceId: "ins_1" as Id,
    runId: null,
    hostId: "hst_1" as Id,
    processGeneration: "1",
    runGeneration: null,
    seq: String(seq),
    observedAt: "2026-09-12T00:00:00.000Z",
    nativeAt: known("2026-09-12T00:00:00.000Z"),
    source: {
      driverKind: "claude-print",
      driverVersion: "2.1.268",
      adapterVersion: "0.1.0",
      channel: "stdout",
      delivery: "replay",
      nativeSessionId: unknownKnowledge("none"),
      nativeTurnId: unknownKnowledge("none"),
      nativeAgentId: unknownKnowledge("none"),
      nativeItemId: unknownKnowledge("none"),
      nativeEventId: unknownKnowledge("none"),
      nativeRequestId: { type: "none" },
      sourceCursor: { type: "runtime", ledgerRevision: "1" },
    },
    kind: "opaque",
    completeness: "opaque",
    rawRef: null,
    evidenceEventIds: [],
    payload: { nativeType: "x", reason: "unmapped-fields", rawRef: { objectId: "obj_x" as Id, offset: "0", length: "0", digest: "sha256:00", mediaType: "application/json", redaction: "none" }, affects: [], summary: `seq ${seq}` },
  } as Observation;
}

function snapshot(asOf: number): Snapshot {
  return {
    projectionVersion: "v1",
    projectionEpoch: "epoch_1" as Id,
    asOfSeq: String(asOf),
    instance: {},
    runs: [],
    commands: [],
    pendingInteractions: [],
    nodes: [],
    history: { earliestRetainedSeq: "1", complete: true },
  };
}

/** Build one read result with complete-page metadata by default. */
function page(
  events: Observation[],
  opts: { durableSeq?: string; windowFromSeq?: string | null; reachedAfterSeq?: boolean } = {},
): ReadPage {
  return {
    events,
    durableSeq: opts.durableSeq ?? events.at(-1)?.seq ?? "0",
    windowFromSeq: opts.windowFromSeq === undefined ? (events[0]?.seq ?? null) : opts.windowFromSeq,
    reachedAfterSeq: opts.reachedAfterSeq ?? true,
  };
}

/**
 * Emulate the Hub's bounded tail window over a contiguous 1..total journal:
 * newest `window` rows of `(afterSeq, beforeSeq]`, floor/flag exactly as the
 * server computes them.
 */
function windowedRead(total: number, windowSize: number) {
  const calls: { afterSeq?: string; beforeSeq?: string }[] = [];
  const read: JournalRead = vi.fn(async (args) => {
    calls.push({ afterSeq: args.afterSeq, beforeSeq: args.beforeSeq });
    const after = Number(args.afterSeq ?? 0);
    const before = args.beforeSeq === undefined ? total : Math.min(Number(args.beforeSeq), total);
    // Newest rows win: walk back from before, then reverse to ascending.
    const picked: number[] = [];
    for (let seq = before; seq > after && picked.length < windowSize; seq -= 1) picked.push(seq);
    picked.reverse();
    return page(
      picked.map((seq) => obs(seq)),
      {
        durableSeq: String(total),
        windowFromSeq: picked.length ? String(picked[0]) : null,
        reachedAfterSeq: picked.length === 0 || picked[0] === after + 1,
      },
    );
  });
  return { read, calls };
}

describe("JournalClient", () => {
  it("applies contiguous batches and ACKs the prefix", () => {
    const emitted: number[] = [];
    const client = new JournalClient("obj_journal" as Id, async () => page([]), {
      onEvents: (events) => emitted.push(...events.map((e) => Number(e.seq))),
    });
    const a = client.applyBatch({
      subscriptionId: "sub",
      journalId: "obj_journal" as Id,
      fromSeq: "1",
      toSeq: "2",
      events: [obs(1), obs(2)],
      durableSeq: "2",
    });
    expect(a.acked).toBe("2");
    expect(a.gap).toBeNull();
    expect(emitted).toEqual([1, 2]);
    expect(client.appliedSeq).toBe("2");
    expect(client.status).toBe("live");
  });

  it("buffers later seq, reports a gap, then fillGap flushes in order", async () => {
    const emitted: number[] = [];
    const all = [obs(1), obs(2), obs(3)];
    const read: JournalRead = vi.fn(async ({ afterSeq, limit }) => {
      const after = afterSeq ? Number(afterSeq) : 0;
      const events = all.filter((e) => Number(e.seq) > after).slice(0, limit);
      return page(events, { durableSeq: "3" });
    });
    const client = new JournalClient("obj_journal" as Id, read, {
      onEvents: (events) => emitted.push(...events.map((e) => Number(e.seq))),
    });
    client.applyBatch({
      subscriptionId: "sub",
      journalId: "obj_journal" as Id,
      fromSeq: "1",
      toSeq: "1",
      events: [obs(1)],
      durableSeq: "3",
    });
    const gapped = client.applyBatch({
      subscriptionId: "sub",
      journalId: "obj_journal" as Id,
      fromSeq: "3",
      toSeq: "3",
      events: [obs(3)],
      durableSeq: "3",
    });
    expect(gapped.gap).toEqual({ from: "2", to: "2" });
    expect(client.status).toBe("gap-backfill");
    expect(emitted).toEqual([1]);
    const acked = await client.fillGap("2", "2");
    expect(acked).toBe("3");
    expect(emitted).toEqual([1, 2, 3]);
    expect(client.status).toBe("live");
  });

  it("resumes after reconnect from the last applied seq", async () => {
    const all = [obs(1), obs(2), obs(3), obs(4)];
    const read: JournalRead = vi.fn(async ({ afterSeq, limit }) => {
      const after = afterSeq ? Number(afterSeq) : 0;
      const events = all.filter((e) => Number(e.seq) > after).slice(0, limit);
      return page(events, { durableSeq: "4" });
    });
    const emitted: number[] = [];
    const client = new JournalClient("obj_journal" as Id, read, {
      onEvents: (events) => emitted.push(...events.map((e) => Number(e.seq))),
    });
    client.applySnapshot(snapshot(2));
    expect(client.appliedSeq).toBe("2");
    client.markReconnecting();
    expect(client.status).toBe("reconnecting");
    const applied = await client.resumeAfterReconnect();
    expect(applied).toBe("4");
    expect(emitted).toEqual([3, 4]);
    expect(client.status).toBe("live");
  });

  it("a slower failed resume cannot downgrade a newer successful empty resume (generation)", async () => {
    // Overlapping catch-ups (send resolves while catch-up is pending, then a
    // steer/visibility starts another): A's read rejects AFTER B's empty read
    // (cursor does not advance) succeeds. A must be a no-op — status live.
    let rejectA: (err: Error) => void = () => {};
    const aRead = new Promise<never>((_resolve, reject) => {
      rejectA = reject;
    });
    const read: JournalRead = vi
      .fn()
      .mockReturnValueOnce(aRead)
      .mockResolvedValueOnce(page([], { durableSeq: "0" }));
    const client = new JournalClient("obj_journal" as Id, read);
    client.markReconnecting();

    const a = client.resumeAfterReconnect().catch((err: unknown) => err);
    const b = await client.resumeAfterReconnect();
    expect(b).toBe("0"); // empty read, cursor unchanged
    expect(client.status).toBe("live");

    rejectA(new Error("HTTP 502"));
    await a;
    expect(client.status).toBe("live");
  });

  it("a slower successful resume cannot overwrite a newer failed one either (generation both ways)", async () => {
    let resolveA: (value: ReadPage) => void = () => {};
    const aRead = new Promise<ReadPage>((resolve) => {
      resolveA = resolve;
    });
    const read: JournalRead = vi
      .fn()
      .mockReturnValueOnce(aRead)
      .mockRejectedValueOnce(new Error("HTTP 502"));
    const client = new JournalClient("obj_journal" as Id, read);
    client.markReconnecting();
    const a = client.resumeAfterReconnect().catch((err: unknown) => err);
    await expect(client.resumeAfterReconnect()).rejects.toThrow("502");
    expect(client.status).toBe("readonly-stale");

    resolveA(page([], { durableSeq: "0" }));
    await a;
    // The older successful resume cannot restore a false "live".
    expect(client.status).toBe("readonly-stale");
  });

  it("goes readonly-stale when the same seq has a different eventId", () => {
    const client = new JournalClient("obj_journal" as Id, async () => page([]));
    client.applyBatch({
      subscriptionId: "sub",
      journalId: "obj_journal" as Id,
      fromSeq: "1",
      toSeq: "1",
      events: [obs(1, "evt_a")],
      durableSeq: "1",
    });
    const again = client.applyBatch({
      subscriptionId: "sub",
      journalId: "obj_journal" as Id,
      fromSeq: "1",
      toSeq: "1",
      events: [obs(1, "evt_b")],
      durableSeq: "1",
    });
    expect(again.acked).toBeNull();
    expect(client.status).toBe("readonly-stale");
  });

  it("fillGap descends bounded tail windows with beforeSeq, then flushes 2..N in order and goes live", async () => {
    // 10 events, 4-row windows: the first read for gap 2..10 is the tail
    // 7..10 (floor 7, above the gap), so the old single-read fill would
    // emit nothing and stay in gap-backfill forever.
    const { read, calls } = windowedRead(10, 4);
    const emitted: number[] = [];
    const statuses: string[] = [];
    const client = new JournalClient("obj_journal" as Id, read, {
      onEvents: (events) => emitted.push(...events.map((e) => Number(e.seq))),
      onStatus: (status) => statuses.push(status),
    });
    client.applySnapshot(snapshot(1));

    const acked = await client.fillGap("2", "10");

    // Three descending pages: tail, beforeSeq=6, beforeSeq=2.
    expect(calls).toHaveLength(3);
    expect(calls.map((c) => c.afterSeq)).toEqual(["1", "1", "1"]);
    expect(calls.map((c) => c.beforeSeq)).toEqual([undefined, "6", "2"]);
    expect(acked).toBe("10");
    expect(emitted).toEqual([2, 3, 4, 5, 6, 7, 8, 9, 10]);
    expect(client.appliedSeq).toBe("10");
    expect(client.status).toBe("live");
  });

  it("fillGap honours the descent budget: one residual onGap, readonly-stale, and the call resolves", async () => {
    // A 50k-event journal with 500-row windows: 16 pages cover only 8k rows
    // (under the 20k event budget) and never descend to seq 2. The page
    // budget is the binding constraint and the gap is 2..50000.
    const { read, calls } = windowedRead(50_000, 500);
    const residual: [string, string][] = [];
    const client = new JournalClient("obj_journal" as Id, read, {
      // The contiguous prefix is empty by construction (seq 2 never loads),
      // so onEvents must not fire at all.
      onEvents: () => {
        throw new Error("no contiguous prefix should flush under the budget");
      },
      onGap: (from, to) => residual.push([from, to]),
    });
    client.applySnapshot(snapshot(1));

    const start = Date.now();
    const acked = await client.fillGap("2", "50000");

    // Exactly the bounded number of reads, then settlement — no spin.
    expect(calls).toHaveLength(16);
    expect(calls[0]?.beforeSeq).toBeUndefined();
    for (let i = 1; i < 16; i += 1) expect(calls[i]?.beforeSeq).toBeDefined();
    expect(acked).toBeNull();
    expect(client.status).toBe("readonly-stale");
    // Residual gap reported exactly once, unchanged from the request.
    expect(residual).toEqual([["2", "50000"]]);
    expect(Date.now() - start).toBeLessThan(5_000);

    // The settled client refuses to re-enter the same fill loop.
    expect(await client.fillGap("2", "50000")).toBeNull();
    expect(calls).toHaveLength(16);
  });

  it("fillResyncGap descends from the applied cursor to the resync snapshot floor", async () => {
    const { read, calls } = windowedRead(12_000, 2_000);
    let status = "";
    const client = new JournalClient("obj_journal" as Id, read, {
      onStatus: (s) => {
        status = s;
      },
    });
    client.applySnapshot(snapshot(5_005));
    // The backpressure resync snapshot starts at 8_007 (tail minus one window);
    // fillResyncGap backfills only the hole [5_006..8_006] — the snapshot
    // batch itself advances applied through 8_007..10_006 afterwards.
    await client.fillResyncGap("8007");
    expect(status).toBe("live");
    expect(client.appliedSeq).toBe("8006");
    // It descended with beforeSeq from the window floor downward.
    expect(calls.length).toBeGreaterThan(1);
    expect(calls[0]?.afterSeq).toBe("5005");
    expect(calls[1]?.beforeSeq).toBeDefined();
  });

  it("fillResyncGap is a no-op when the snapshot floor reaches the cursor", async () => {
    const { read, calls } = windowedRead(10, 2_000);
    const client = new JournalClient("obj_journal" as Id, read);
    client.applySnapshot(snapshot(8));
    await client.fillResyncGap("9");
    // Floor 9 is only one row above applied 8: the batch carries it, no fill.
    expect(calls).toHaveLength(0);
  });

  it("loadEarlier pages below the loaded floor and tracks the lowest window", async () => {
    const { read, calls } = windowedRead(5_000, 2_000);
    const prepended: number[][] = [];
    const client = new JournalClient("obj_journal" as Id, read, {
      onPrepend: (events) => prepended.push(events.map((e) => Number(e.seq))),
    });
    // Late attach: the snapshot is a partial tail window.
    client.applySnapshot({
      ...snapshot(5_000),
      history: { earliestRetainedSeq: "3001", complete: false },
    });
    expect(client.retainedFloorSeq).toBe("3001");

    const floor = await client.loadEarlier();
    expect(floor).toBe("1001");
    expect(calls).toHaveLength(1);
    expect(calls[0]).toMatchObject({ afterSeq: "0", beforeSeq: "3000" });
    expect(prepended).toHaveLength(1);
    expect(prepended[0]?.[0]).toBe(1001);
    expect(prepended[0]?.at(-1)).toBe(3000);

    // A later partial snapshot with a higher floor must not move the loaded
    // floor back up.
    client.applySnapshot({
      ...snapshot(5_000),
      history: { earliestRetainedSeq: "3001", complete: false },
    });
    expect(client.retainedFloorSeq).toBe("1001");
  });
});

it("a contiguous socket batch supersedes a pending resume read that later rejects (item 9)", async () => {
  const readRejects = [false];
  const read = vi.fn(async (args: { afterSeq?: string }) => {
    // Resume A's read hangs; it rejects after the socket catches up.
    if (args.afterSeq === "0" && readRejects[0]) throw new Error("read gone stale");
    return page([], { durableSeq: "0" });
  });
  const onStatus = vi.fn();
  const client = new JournalClient("obj_x", read, { onStatus });
  client.markReconnecting();

  // Resume A starts (read will reject when told to).
  const resumeA = client.resumeAfterReconnect();
  await new Promise((r) => setTimeout(r, 4));
  expect(read).toHaveBeenCalled();

  // Socket delivers a contiguous batch up to seq 1 — caught up, live.
  readRejects[0] = true;
  client.applyBatch({
    subscriptionId: "sub",
    journalId: "obj_x" as Id,
    fromSeq: "1",
    toSeq: "1",
    events: [obs(1)],
    durableSeq: "1",
  });
  client.noteSocketCaughtUp();

  // Resume A's read now rejects; it must NOT downgrade to readonly-stale.
  await resumeA;
  await new Promise((r) => setTimeout(r, 4));
  expect(client.status).toBe("live");
  expect(onStatus).not.toHaveBeenCalledWith("readonly-stale");
});
