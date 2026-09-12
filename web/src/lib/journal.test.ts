import { describe, expect, it } from "vitest";
import type { Observation, Snapshot } from "../types/observation";
import { JournalClient, type JournalRead } from "./journal";
import { known, unknownKnowledge, type Id } from "../types/wire";

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

describe("JournalClient", () => {
  it("applies contiguous batches and ACKs the prefix", () => {
    const emitted: number[] = [];
    const client = new JournalClient("obj_journal" as Id, async () => ({ events: [], durableSeq: "2", floorSeq: "1" }), {
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
    const read: JournalRead = async ({ afterSeq, limit }) => {
      const after = afterSeq ? Number(afterSeq) : 0;
      const events = all.filter((e) => Number(e.seq) > after).slice(0, limit);
      return { events, durableSeq: "3", floorSeq: "1" };
    };
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
    const read: JournalRead = async ({ afterSeq, limit }) => {
      const after = afterSeq ? Number(afterSeq) : 0;
      const events = all.filter((e) => Number(e.seq) > after).slice(0, limit);
      return { events, durableSeq: "4", floorSeq: "1" };
    };
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

  it("goes readonly-stale when the same seq has a different eventId", () => {
    const client = new JournalClient("obj_journal" as Id, async () => ({ events: [], durableSeq: "1", floorSeq: "1" }));
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
});
