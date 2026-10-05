import { describe, expect, it, vi } from "vitest";
import type { Observation, Snapshot } from "../types/observation";
import { JournalClient, JournalResumeStaleError, type JournalRead } from "./journal";
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

  it("a resume whose gap fill settles readonly-stale REJECTS (ROUND4-3): the machine must not publish live", async () => {
    // The resume read comes back gapped (seq 2 missing: 3..5 arrive), and the
    // Hub's tail window never descends to seq 2 — the fill settles
    // readonly-stale. The resume must THROW instead of resolving, so the
    // connection machine's resume action fails and retries offline.
    const read: JournalRead = vi
      .fn()
      .mockResolvedValueOnce(
        page([obs(3), obs(4), obs(5)], { durableSeq: "5", windowFromSeq: "3", reachedAfterSeq: false }),
      )
      .mockResolvedValue(
        page([], { durableSeq: "5", windowFromSeq: "3", reachedAfterSeq: false }),
      );
    const statuses: string[] = [];
    const client = new JournalClient("obj_journal" as Id, read, {
      onStatus: (status) => statuses.push(status),
    });
    client.applySnapshot(snapshot(1));
    client.markReconnecting();

    await expect(client.resumeAfterReconnect()).rejects.toBeInstanceOf(JournalResumeStaleError);
    expect(client.status).toBe("readonly-stale");
    // The failure status was published for the banner; no false "live".
    expect(statuses.at(-1)).toBe("readonly-stale");
    expect(statuses).not.toContain("live");
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
    client.noteHistory(Array.from({ length: 2_000 }, (_, i) => obs(3001 + i)));
    client.applySnapshot({
      ...snapshot(5_000),
      history: { earliestRetainedSeq: "3001", complete: false },
    });
    expect(client.retainedFloorSeq).toBe("3001");

    const result = await client.loadEarlier();
    expect(result).toEqual({ prepended: true, end: false, floor: "1001" });
    expect(calls).toHaveLength(1);
    expect(calls[0]).toMatchObject({ afterSeq: "0", beforeSeq: "3000" });
    expect(prepended).toHaveLength(1);
    expect(prepended[0]?.[0]).toBe(1001);
    expect(prepended[0]?.at(-1)).toBe(3000);

    // A later partial snapshot with a higher floor must not move the loaded
    // floor back up (it only re-anchors the descending cursor).
    client.applySnapshot({
      ...snapshot(5_000),
      history: { earliestRetainedSeq: "3001", complete: false },
    });
    expect(client.retainedFloorSeq).toBe("1001");
  });

  it("duplicate pages after a reconnect re-anchor keep descending to held history", async () => {
    // Real bounded Hub: newest `window` rows of (afterSeq, beforeSeq]. Every
    // response is one such window, so a duplicate page (rows already held) is
    // only possible when a snapshot re-anchored the paging cursor ABOVE them.
    const { read, calls } = windowedRead(10_000, 2_000);
    const prepended: number[][] = [];
    const client = new JournalClient("obj_journal" as Id, read, {
      onPrepend: (events) => prepended.push(events.map((e) => Number(e.seq))),
    });
    // Late attach to a bounded tail window 3001..5000: the seed READ delivers
    // the rows (noteHistory), the snapshot carries the window floor.
    client.noteHistory(Array.from({ length: 2_000 }, (_, i) => obs(3001 + i)));
    client.applySnapshot({
      ...snapshot(5_000),
      history: { earliestRetainedSeq: "3001", complete: false },
    });
    // Live frames extend the applied cursor to 8000 while connected.
    const live = Array.from({ length: 3_000 }, (_, i) => obs(5001 + i));
    client.applyBatch({
      subscriptionId: "sub",
      journalId: "obj_journal" as Id,
      fromSeq: "5001",
      toSeq: "8000",
      events: live,
      durableSeq: "8000",
    });

    // Reconnect delivers a bounded resync snapshot whose window floor (8001)
    // is ABOVE manually-loadable history the client has not paged yet.
    client.applySnapshot({
      ...snapshot(10_000),
      history: { earliestRetainedSeq: "8001", complete: false },
    });
    expect(client.retainedFloorSeq).toBe("3001");

    // Click 1 pages below the snapshot window: 6001..8000, every row already
    // held. No prepend, NOT the end, the retained floor/button stay, and the
    // cursor descended.
    await expect(client.loadEarlier()).resolves.toEqual({
      prepended: false,
      end: false,
      floor: "3001",
    });
    expect(prepended).toHaveLength(0);
    expect(calls.at(-1)?.beforeSeq).toBe("8000");

    // Click 2 walks down through another fully-held window.
    await expect(client.loadEarlier()).resolves.toMatchObject({ prepended: false, end: false });
    expect(calls.at(-1)?.beforeSeq).toBe("6000");

    // Click 3 crosses into unseen rows: 2001..3000 arrive (3001..4000 were
    // already held); the retained floor moves, the end is not reached.
    const r3 = await client.loadEarlier();
    expect(calls.at(-1)?.beforeSeq).toBe("4000");
    expect(r3).toMatchObject({ prepended: true, end: false });
    expect(prepended).toHaveLength(1);
    expect(prepended[0]?.[0]).toBe(2001);
    expect(prepended[0]?.at(-1)).toBe(3000);
    expect(client.retainedFloorSeq).toBe("2001");

    // Click 4 reaches the authoritative end at seq 1.
    const r4 = await client.loadEarlier();
    expect(r4).toMatchObject({ prepended: true, end: true });
    expect(prepended[1]?.[0]).toBe(1);
    expect(prepended[1]?.at(-1)).toBe(2000);
    expect(client.retainedFloorSeq).toBe("1");

    // The end short-circuits further reads.
    expect((await client.loadEarlier()).end).toBe(true);
    expect(calls).toHaveLength(4);
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

it("an existing client's snapshot never advances applied past un-emitted rows (bounded tail 3001-5000, applied=1)", () => {
  const read = vi.fn(async () =>
    page([obs(3001), obs(3002)], {
      durableSeq: "5000",
      windowFromSeq: "3001",
      reachedAfterSeq: false,
    }),
  );
  const client = new JournalClient("obj_gap2", read);
  // Existing client already applied seq 1.
  client.applySnapshot(snapshot(1));
  expect(client.appliedSeq).toBe("1");
  // A bounded snapshot (3001..) must NOT jump applied to 5000; rows 2..3000
  // are un-emitted. applied stays at 1 and the cursor is recovered via
  // follow frames / resumeAfterReconnect, not the snapshot.
  client.applySnapshot({
    ...snapshot(5000),
    history: { earliestRetainedSeq: "3001", complete: false },
  });
  expect(client.appliedSeq).toBe("1");
});

it("concurrent fillGap calls share ONE in-flight fill and its definite outcome", async () => {
  let resolveRead: (value: ReadPage) => void = () => {};
  const pending = new Promise<ReadPage>((resolve) => {
    resolveRead = resolve;
  });
  const read = vi.fn(async () => pending);
  const client = new JournalClient("obj_journal" as Id, read);
  client.applySnapshot(snapshot(1));

  // Two onGap-style calls for the same gap while nothing is in store yet.
  const first = client.fillGap("2", "10");
  const second = client.fillGap("2", "10");

  // Both join the SAME promise and exactly ONE read is in flight.
  expect(second).toBe(first);
  expect(read).toHaveBeenCalledTimes(1);

  resolveRead(
    page(
      [obs(2), obs(3)],
      { durableSeq: "10", windowFromSeq: "2", reachedAfterSeq: true },
    ),
  );
  const [a, b] = await Promise.all([first, second]);
  expect(a).toBe("3");
  expect(b).toBe("3");
  expect(client.appliedSeq).toBe("3");
  expect(client.status).toBe("live");
});

it("ROUND5-2: a resume that joins the fill its generation just invalidated fails while the hole remains", async () => {
  // The owner's-phone sequence:
  //   1. the live socket delivers the tail 3001..5000 -> deferred fill 2..3000
  //   2. a resume bumps the generation mid-fill; its own bounded read returns
  //      the taller tail 4873..5000 -> gap 2..4872
  //   3. the deferred fill ends without reaching seq 2
  // The resume must NOT accept the superseded fill and publish live over the
  // hole: it waits for that fill, starts a replacement, and because the gap
  // is still present it settles readonly-stale and REJECTS (machine offline).
  const range = (from: number, to: number) => {
    const events: Observation[] = [];
    for (let seq = from; seq <= to; seq += 1) events.push(obs(seq));
    return events;
  };
  let resolveDeferredFill: (page: ReadPage) => void = () => {};
  const deferredFill = new Promise<ReadPage>((resolve) => {
    resolveDeferredFill = resolve;
  });
  // Call 1: the gen-0 fill (hung until we resolve it). Call 2: the resume's
  // own read, bounded tail 4873..5000. Calls 3+: the replacement fill, whose
  // window never descends toward seq 2.
  const read: JournalRead = vi
    .fn()
    .mockReturnValueOnce(deferredFill)
    .mockResolvedValueOnce(
      page(range(4873, 5000), { durableSeq: "5000", windowFromSeq: "4873", reachedAfterSeq: false }),
    )
    .mockResolvedValue(
      page([], { durableSeq: "5000", windowFromSeq: "4873", reachedAfterSeq: false }),
    );

  const statuses: string[] = [];
  const client = new JournalClient("obj_journal" as Id, read, {
    onStatus: (s) => statuses.push(s),
    // Emulate the store: every onGap joins/launches a fill at the current gen.
    onGap: (from, to) => void client.fillGap(from, to, client.currentResumeGen()),
  });
  client.applySnapshot(snapshot(1));

  // 1. Socket tail 3001..5000: gapped, the gen-0 fill for 2..3000 starts and
  // blocks on its deferred read.
  const tailed = client.applyBatch({
    subscriptionId: "sub",
    journalId: "obj_journal" as Id,
    fromSeq: "3001",
    toSeq: "5000",
    events: range(3001, 5000),
    durableSeq: "5000",
  });
  expect(tailed.gap).toEqual({ from: "2", to: "3000" });
  expect(read).toHaveBeenCalledTimes(1);

  // 2. Resume (bumps the generation); its read sees the taller tail.
  const resume = client.resumeAfterReconnect().catch((err: unknown) => err);
  await new Promise((r) => setTimeout(r, 0));
  expect(read).toHaveBeenCalledTimes(2);

  // 3. The deferred gen-0 fill ends with rows that still leave seq 2 missing.
  resolveDeferredFill(
    page(range(2001, 3000), { durableSeq: "5000", windowFromSeq: "2001", reachedAfterSeq: false }),
  );

  const err = await resume;
  // The resume failed: no false live over the hole.
  expect(err).toBeInstanceOf(JournalResumeStaleError);
  expect(client.status).toBe("readonly-stale");
  expect(statuses).not.toContain("live");
  // The cursor never crossed the missing rows.
  expect(client.appliedSeq).toBe("1");
  // The resume STARTED A REPLACEMENT fill after the superseded one ended (at
  // least one read beyond the resume read), rather than trusting the join.
  expect(vi.mocked(read).mock.calls.length).toBeGreaterThanOrEqual(3);
});

it("a newer resume with a taller gap reuses an in-flight same-generation fill only once it covers its target", async () => {
  // Companion to ROUND5-2: when the in-flight fill already runs at the
  // caller's generation AND covers the requested range, callers still join the
  // exact same promise (one read, one fill) — the sharing optimization is
  // preserved for the usable case.
  const read: JournalRead = vi
    .fn()
    .mockResolvedValueOnce(
      page([obs(2), obs(3)], { durableSeq: "3", windowFromSeq: "2", reachedAfterSeq: true }),
    );
  const client = new JournalClient("obj_journal" as Id, read);
  client.applySnapshot(snapshot(1));
  const a = client.fillGap("2", "3", 0);
  const b = client.fillGap("2", "3", 0);
  expect(b).toBe(a);
  expect(await Promise.all([a, b])).toEqual(["3", "3"]);
  expect(read).toHaveBeenCalledTimes(1);
});

it("a stale fill whose deferred read rejects after a newer resume went live never writes status", async () => {
  let rejectFill: (err: Error) => void = () => {};
  const fillRead = new Promise<ReadPage>((_resolve, reject) => {
    rejectFill = reject;
  });
  const read = vi
    .fn()
    .mockReturnValueOnce(fillRead)
    .mockResolvedValueOnce(
      page([obs(2), obs(3), obs(4), obs(5), obs(6)], { durableSeq: "6", windowFromSeq: "2" }),
    );
  const statuses: string[] = [];
  const client = new JournalClient("obj_journal" as Id, read, {
    onStatus: (s) => statuses.push(s),
  });
  client.applySnapshot(snapshot(1));

  // The fill for gap 2..6 is in flight (bounded tail would return
  // 3001-style rows; here it simply rejects late).
  const fill = client.fillGap("2", "6");
  expect(client.status).toBe("gap-backfill");

  // A NEWER resume succeeds and goes live (bumps the generation).
  await client.resumeAfterReconnect();
  expect(client.status).toBe("live");

  // Now the old fill's read rejects: its catch is generation-guarded, so it
  // must return the definite null outcome without downgrading to readonly-stale.
  rejectFill(new Error("HTTP 502 on the stale fill read"));
  expect(await fill).toBeNull();
  expect(client.status).toBe("live");
  expect(statuses.at(-1)).toBe("live");
});
