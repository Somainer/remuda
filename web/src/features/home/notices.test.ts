import { afterEach, describe, expect, it, vi } from "vitest";
import type { Observation } from "../../types/observation";

const api = vi.hoisted(() => ({ rest: vi.fn() }));
vi.mock("../../lib/api", () => ({ rest: api.rest }));

vi.mock("../../lib/store", () => ({
  hubStore: {
    subscribeRefreshHealth: () => () => undefined,
    getRefreshHealth: () => ({ lastOkAt: null, failingSince: null }),
  },
}));

import {
  NOTICE_LIST_MAX,
  NoticeStore,
  listUnackedNotices,
  noticeVoice,
  noticesByReply,
  type OwnerNotice,
} from "./notices";
import { T0, T1, T2, notice } from "./queueTestData";

/**
 * D-060 §11.5 「告诉你」 data: the shared unacknowledged list rides the hub
 * store's settled refreshes and emits only when the list changed; 知道了
 * drops a card only after the Hub answered; the transcript places each notice
 * under the reply of the turn that wrote it.
 */

function lifecycle(nativeName: string, seq: number): Observation {
  return {
    kind: "lifecycle",
    seq: String(seq),
    payload: { type: "native", topic: "turn", nativeName },
  } as unknown as Observation;
}

function message(seq: number, role: "assistant" | "user", nodeId: string): Observation {
  return {
    kind: "message",
    seq: String(seq),
    payload: { role, nodeId, messageId: nodeId, operation: "close", revision: "1" },
  } as unknown as Observation;
}

type Harness = {
  store: NoticeStore;
  reads: () => number;
  settle: (lastOkAt: number | null) => void;
  setList: (rows: OwnerNotice[]) => void;
};

function harness(initial: OwnerNotice[] = []): Harness {
  let rows = initial;
  let count = 0;
  let lastOkAt: number | null = 1;
  let listener: (() => void) | null = null;
  const store = new NoticeStore(
    async () => {
      count += 1;
      return rows;
    },
    (fn) => {
      listener = fn;
      return () => {
        listener = null;
      };
    },
    () => lastOkAt,
  );
  return {
    store,
    reads: () => count,
    settle: (at) => {
      lastOkAt = at;
      listener?.();
    },
    setList: (next) => {
      rows = next;
    },
  };
}

async function flush() {
  for (let i = 0; i < 5; i += 1) await Promise.resolve();
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("NoticeStore", () => {
  it("reads on the first subscriber and on each successful refresh, never on a failed one", async () => {
    const h = harness([notice({ noticeId: "ntc_1" })]);
    const off = h.store.subscribe(() => undefined);
    await flush();
    expect(h.reads()).toBe(1);
    expect(h.store.getSnapshot()).toMatchObject({ loaded: true });
    expect(h.store.getSnapshot().notices.map((row) => row.noticeId)).toEqual(["ntc_1"]);
    // A settled refresh that did not succeed (lastOkAt unchanged) reads nothing.
    h.settle(1);
    await flush();
    expect(h.reads()).toBe(1);
    h.settle(2);
    await flush();
    expect(h.reads()).toBe(2);
    off();
    // No subscriber: refreshes are not followed any more.
    h.settle(3);
    await flush();
    expect(h.reads()).toBe(2);
  });

  it("keeps the snapshot identity when a read returns the same list", async () => {
    const h = harness([notice({ noticeId: "ntc_1" })]);
    let emits = 0;
    h.store.subscribe(() => {
      emits += 1;
    });
    await flush();
    const first = h.store.getSnapshot();
    h.setList([notice({ noticeId: "ntc_1" })]);
    h.settle(2);
    await flush();
    expect(h.store.getSnapshot()).toBe(first);
    expect(emits).toBe(1);
    h.setList([notice({ noticeId: "ntc_2" }), notice({ noticeId: "ntc_1" })]);
    h.settle(3);
    await flush();
    expect(h.store.getSnapshot().notices.map((row) => row.noticeId)).toEqual(["ntc_2", "ntc_1"]);
    expect(emits).toBe(2);
  });

  it("drops a card only after the Hub acknowledged it", async () => {
    const h = harness([notice({ noticeId: "ntc_1" }), notice({ noticeId: "ntc_2" })]);
    h.store.subscribe(() => undefined);
    await flush();
    const failing = vi.fn().mockRejectedValue(new Error("offline"));
    await expect(h.store.ack("ntc_1", failing)).rejects.toThrow("offline");
    expect(h.store.getSnapshot().notices).toHaveLength(2);
    const ok = vi.fn().mockResolvedValue(notice({ noticeId: "ntc_1", ackedAt: T2 }));
    await h.store.ack("ntc_1", ok);
    expect(ok).toHaveBeenCalledWith("ntc_1");
    expect(h.store.getSnapshot().notices.map((row) => row.noticeId)).toEqual(["ntc_2"]);
    // A read that started before the ack landed still lists it: not repainted.
    h.settle(5);
    await flush();
    expect(h.store.getSnapshot().notices.map((row) => row.noticeId)).toEqual(["ntc_2"]);
  });
});

describe("noticesByReply", () => {
  const events = [
    lifecycle("turn_started", 10),
    message(11, "user", "u1"),
    message(12, "assistant", "a1"),
    lifecycle("result", 13),
    lifecycle("turn_started", 20),
    message(21, "assistant", "a2-early"),
    message(25, "assistant", "a2"),
    lifecycle("result", 26),
  ];

  it("hangs each notice under its turn's reply, oldest first", () => {
    const placed = noticesByReply(events, [
      notice({ noticeId: "ntc_b", turnSeq: 20, createdAt: T2 }),
      notice({ noticeId: "ntc_a", turnSeq: 20, createdAt: T1 }),
      notice({ noticeId: "ntc_c", turnSeq: 10, createdAt: T0 }),
    ]);
    expect(placed.get("a2")?.map((row) => row.noticeId)).toEqual(["ntc_a", "ntc_b"]);
    expect(placed.get("a1")?.map((row) => row.noticeId)).toEqual(["ntc_c"]);
    expect(placed.has("a2-early")).toBe(false);
  });

  it("falls back to the newest assistant message when the carrier journals no turns", () => {
    const placed = noticesByReply(
      [message(1, "user", "u1"), message(2, "assistant", "echo-1"), message(3, "assistant", "echo-2")],
      [notice({ noticeId: "ntc_1", turnSeq: null })],
    );
    expect([...placed.keys()]).toEqual(["echo-2"]);
  });

  it("puts a notice whose turn has no reply under the newest reply, and places nothing without one", () => {
    const placed = noticesByReply(events, [
      notice({ noticeId: "ntc_unloaded", turnSeq: 3, createdAt: T0 }),
      notice({ noticeId: "ntc_none", turnSeq: null }),
    ]);
    expect(placed.get("a2")?.map((row) => row.noticeId)).toEqual(["ntc_unloaded", "ntc_none"]);
    expect(noticesByReply([lifecycle("turn_started", 1)], [notice({ noticeId: "ntc_x" })]).size).toBe(0);
  });

  it("names the voice: Main, the Hub, or the owner's own note", () => {
    expect(noticeVoice({ origin: "agent" })).toBe("Main");
    expect(noticeVoice({ origin: "hub" })).toBe("Hub");
    expect(noticeVoice({ origin: "human" })).toBe("备忘");
  });
});

describe("listUnackedNotices (r2 item 5)", () => {
  it("asks for every waiting notice, not the Hub's default page of 50", async () => {
    const rows = Array.from({ length: 60 }, (_, index) => notice({ noticeId: `ntc_${index}` }));
    api.rest.mockResolvedValueOnce({ items: rows });
    const listed = await listUnackedNotices();
    expect(api.rest).toHaveBeenCalledWith(`/v1/owner/notices?unacked=1&limit=${NOTICE_LIST_MAX}`, {});
    expect(NOTICE_LIST_MAX).toBe(200);
    expect(listed).toHaveLength(60);
  });
});
