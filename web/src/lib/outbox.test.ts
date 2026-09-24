import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  newCommandId,
  Outbox,
  OUTBOX_MAX_AGE_MS,
  OUTBOX_MAX_ATTEMPTS,
  withinRetryWindow,
  type OutboxRecord,
  type OutboxStorage,
} from "./outbox";

/** Deterministic in-memory storage standing in for IndexedDB. Read/modify/
 * write inside `put` is serialized (an async turn is awaited before commit),
 * modelling IndexedDB transaction atomicity so the lease fallback can exclude
 * a truly concurrent second owner. */
class MemStorage implements OutboxStorage {
  map = new Map<string, OutboxRecord>();
  private writeChain: Promise<void> = Promise.resolve();
  async all() {
    await this.writeChain;
    return [...this.map.values()];
  }
  async put(rec: OutboxRecord) {
    // Serialize writes; one tick before committing gives a concurrent owner a
    // chance to observe the lease (mirrors IDB readwrite transaction order).
    const run = this.writeChain.then(async () => {
      await new Promise((r) => setTimeout(r, 1));
      this.map.set(rec.commandId, rec);
    });
    this.writeChain = run.catch(() => undefined);
    await run;
  }
  async delete(id: string) {
    const run = this.writeChain.then(async () => {
      await new Promise((r) => setTimeout(r, 1));
      this.map.delete(id);
    });
    this.writeChain = run.catch(() => undefined);
    await run;
  }
}

function rec(over: Partial<OutboxRecord> = {}): OutboxRecord {
  return {
    commandId: newCommandId(),
    clientRequestId: "local_1",
    instanceId: "ins_1",
    prompt: "hi",
    createdAt: 1_000,
    attempts: 0,
    state: "pending",
    ...over,
  };
}

describe("newCommandId (UUIDv7, Node-acceptable)", () => {
  it("has the cmd_ prefix and canonical lowercase UUID shape", () => {
    const id = newCommandId(0x018fe000_0000);
    expect(id).toMatch(/^cmd_[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  });

  it("embeds the millisecond timestamp in the first 48 bits", () => {
    const ms = Date.parse("2026-09-24T00:00:00.000Z");
    const hex = newCommandId(ms).slice(4).replace(/-/g, "");
    expect(BigInt(`0x${hex.slice(0, 12)}`)).toBe(BigInt(ms));
  });

  it("orders by timestamp regardless of the random tail", () => {
    expect(newCommandId(1000) < newCommandId(1001)).toBe(true);
  });
});

describe("withinRetryWindow", () => {
  it("is open under the caps and closed at/over either", () => {
    expect(withinRetryWindow({ attempts: 0, createdAt: 1000 }, 1000)).toBe(true);
    expect(withinRetryWindow({ attempts: OUTBOX_MAX_ATTEMPTS, createdAt: 1000 }, 1000)).toBe(false);
    expect(withinRetryWindow({ attempts: 0, createdAt: 1000 }, 1000 + OUTBOX_MAX_AGE_MS + 1)).toBe(false);
  });
});

describe("Outbox", () => {
  let storage: MemStorage;
  let box: Outbox;

  beforeEach(async () => {
    storage = new MemStorage();
    box = await Outbox.load(storage);
  });

  it("persists before the send returns and restores from storage", async () => {
    await box.enqueue({
      commandId: "cmd_a",
      clientRequestId: "local_a",
      instanceId: "ins_1",
      prompt: "offline text",
      createdAt: 5,
    });
    expect(storage.map.get("cmd_a")?.state).toBe("pending");

    const reloaded = await Outbox.load(storage);
    expect(reloaded.pending().map((r) => r.commandId)).toEqual(["cmd_a"]);
  });

  it("lists pending oldest-first, scoped to one instance, and counts them", async () => {
    await box.enqueue(rec({ commandId: "cmd_2", instanceId: "ins_1", createdAt: 20 }));
    await box.enqueue(rec({ commandId: "cmd_1", instanceId: "ins_1", createdAt: 10 }));
    await box.enqueue(rec({ commandId: "cmd_3", instanceId: "ins_2", createdAt: 5 }));
    await box.patch("cmd_2", { state: "done" });

    expect(box.pendingFor("ins_1").map((r) => r.commandId)).toEqual(["cmd_1"]);
    expect(box.pendingCount()).toBe(2);
  });

  it("flushes a steer ahead of older ordinary queued rows", async () => {
    await box.enqueue(rec({ commandId: "cmd_first", instanceId: "ins_x", prompt: "first", createdAt: 1 }));
    await box.enqueue(rec({ commandId: "cmd_second", instanceId: "ins_x", prompt: "second", createdAt: 2 }));
    await box.enqueue(rec({ commandId: "cmd_third", instanceId: "ins_x", prompt: "third", createdAt: 3 }));
    await box.patch("cmd_third", { mode: "steer" });

    const order = box.pendingFor("ins_x").map((r) =>
      r.mode === "steer" ? `steer:${r.prompt}` : r.prompt,
    );
    expect(order).toEqual(["steer:third", "first", "second"]);
  });

  it("excludes done records from bootstrap restoration", async () => {
    await box.enqueue(rec({ commandId: "cmd_done", state: "done" }));
    await box.enqueue(rec({ commandId: "cmd_unknown", state: "unknown" }));
    expect(box.unresolved().map((r) => r.commandId)).toEqual(["cmd_unknown"]);
  });

  it("serializes per instance (one active run; concurrent callers coalesce onto a follow-up)", async () => {
    let active = 0;
    let maxActive = 0;
    const job = async () => {
      active++;
      maxActive = Math.max(maxActive, active);
      await Promise.resolve();
      active--;
    };
    // First call runs; the second call while active is coalesced onto one
    // chained follow-up rather than turned away or racing.
    const [a, b] = await Promise.all([
      box.withInstanceLock("ins_1", job),
      box.withInstanceLock("ins_1", job),
    ]);
    expect(maxActive).toBe(1);
    expect(a).not.toBeNull();
    expect(b).not.toBeNull();
    // Both runs happened (the follow-up drained too).
    expect(active).toBe(0);
  });

  it("notifies subscribers on every mutation", async () => {
    const fn = vi.fn();
    box.subscribe(fn);
    await box.enqueue(rec({ commandId: "cmd_n" }));
    await box.patch("cmd_n", { state: "inflight" });
    await box.remove("cmd_n");
    expect(fn).toHaveBeenCalledTimes(3);
  });

  it("enqueue leaves the in-memory cache unchanged and emits nothing when storage aborts (commit-before-publish)", async () => {
    const failing: OutboxStorage = {
      all: async () => [],
      put: async () => {
        throw new Error("IDB transaction aborted");
      },
      delete: async () => undefined,
    };
    const b = await Outbox.load(failing, "owner_abort");
    const fn = vi.fn();
    b.subscribe(fn);
    await expect(b.enqueue(rec({ commandId: "cmd_aborted" }))).rejects.toThrow(/aborted/);
    // No durable entry, no cache entry, no subscriber notification: an
    // uncommitted row must never be rendered or POSTed.
    expect(b.get("cmd_aborted")).toBeUndefined();
    expect(fn).not.toHaveBeenCalled();
  });

  it("a crashed owner's expired lease is stealable and a sent row is never re-POSTed (lease fallback)", async () => {
    // No navigator.locks in this harness → exercises the durable lease path.
    const originalLocks = (globalThis.navigator as { locks?: LockManager }).locks;
    Object.defineProperty(globalThis.navigator, "locks", {
      value: undefined,
      configurable: true,
    });
    const shared = new MemStorage();
    // Owner A loaded and crashed holding a row mid-inflight with an EXPIRED lease.
    const stale: OutboxRecord = rec({
      commandId: "cmd_crashed",
      state: "inflight",
      lease: { owner: "owner_DEAD", until: Date.now() - 1 },
    });
    await shared.put(stale);

    // Owner B (a fresh process) loads; the dead owner's expired inflight row
    // is returned to a deliverable state in B's cache.
    const b = await Outbox.load(shared, "owner_B");
    expect(b.get("cmd_crashed")?.state).toBe("pending");
    let postedRows: string[] = [];
    const result = await b.withInstanceLock("ins_1", (_id, rows) => {
      postedRows = rows.map((r) => r.commandId);
      return Promise.resolve(rows.length);
    });
    expect(result).toBe(1);
    expect(postedRows).toContain("cmd_crashed");

    // Once A (now B) marks the row sent, a subsequent lock re-read must NOT
    // hand it out for delivery again.
    await b.patch("cmd_crashed", { state: "sent", gotResponse: true, lease: undefined });
    const second = await b.withInstanceLock("ins_1", (_id, rows) => Promise.resolve(rows.length));
    expect(second).toBe(0);
    Object.defineProperty(globalThis.navigator, "locks", {
      value: originalLocks,
      configurable: true,
    });
  });
});
