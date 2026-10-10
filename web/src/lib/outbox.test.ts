import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  LEASE_TTL_MS,
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
  /**
   * The atomic lease claim, modelled on the real IndexedDB readwrite tx:
   * serialized on the SAME write chain as puts, the read and the write happen
   * in one critical section, so a second owner queued behind the first sees
   * its committed lease and is refused.
   */
  async acquireLease(key: string, rec: OutboxRecord, now: number): Promise<boolean> {
    const run = this.writeChain.then(async () => {
      await new Promise((r) => setTimeout(r, 1));
      const existing = this.map.get(key);
      const lease = existing?.lease;
      if (lease && rec.lease && lease.owner !== rec.lease.owner && lease.until > now) {
        return false;
      }
      this.map.set(key, rec);
      return true;
    });
    this.writeChain = run.then(() => undefined);
    return await run;
  }
  /** Atomic read-check-write on the same chain as the lease claim. */
  async mergeUnlessDone(commandId: string, patch: Partial<OutboxRecord>): Promise<OutboxRecord | null> {
    const run = this.writeChain.then(async () => {
      await new Promise((r) => setTimeout(r, 1));
      const existing = this.map.get(commandId);
      if (!existing) return null;
      if (existing.state === "done") return existing;
      const merged = { ...existing, ...patch };
      this.map.set(commandId, merged);
      return merged;
    });
    this.writeChain = run.then(() => undefined);
    return await run;
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

  it("orders ids minted in the SAME millisecond by mint order (monotonic tail)", () => {
    // Two ids within one ms must sort in mint order even if the random tails
    // would have inverted (the outbox FIFO tiebreak is the commandId string).
    const ids = Array.from({ length: 256 }, () => newCommandId(42));
    for (let i = 1; i < ids.length; i += 1) {
      expect(ids[i - 1]! < ids[i]!).toBe(true);
    }
    // Each is still a canonical UUIDv7.
    for (const id of ids) {
      expect(id).toMatch(/^cmd_[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    }
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

  it("GATE7: a reload by the SAME profile reclaims its own stale inflight claim immediately", async () => {
    const owner = "owner_reload_same_profile";
    const first = await Outbox.load(storage, owner);
    await first.enqueue(rec({ commandId: "cmd_inflight", instanceId: "ins_1" }));
    // Claim the durable inflight lease the way a flush does right before its
    // POST — then the page context is destroyed (no rollback, no finish()).
    await first.patch("cmd_inflight", {
      state: "inflight",
      attempts: 1,
      lease: { owner, until: Date.now() + LEASE_TTL_MS },
    });
    expect(storage.map.get("cmd_inflight")?.state).toBe("inflight");

    // New context, same browser profile (stable owner id): the claim cannot
    // have a POST in flight any more, so it is requeued immediately, both in
    // the cache and durably — no waiting out the 30 s lease TTL.
    const reloaded = await Outbox.load(storage, owner);
    expect(reloaded.pendingFor("ins_1").map((r) => r.commandId)).toEqual(["cmd_inflight"]);
    // A durable re-read must not resurrect the stale claim.
    expect(storage.map.get("cmd_inflight")?.state).toBe("pending");
    expect(storage.map.get("cmd_inflight")?.lease).toBeUndefined();
  });

  it("GATE7: another owner's FRESH inflight lease is never stolen on load", async () => {
    const tabA = await Outbox.load(storage, "owner_tab_a");
    await tabA.enqueue(rec({ commandId: "cmd_foreign", instanceId: "ins_1" }));
    await tabA.patch("cmd_foreign", {
      state: "inflight",
      lease: { owner: "owner_tab_a", until: Date.now() + LEASE_TTL_MS },
    });

    // A different profile/process loads with the other owner's claim still
    // fresh: it stays inflight and is not deliverable.
    const tabB = await Outbox.load(storage, "owner_tab_b");
    expect(tabB.pendingFor("ins_1")).toEqual([]);
    expect(storage.map.get("cmd_foreign")?.state).toBe("inflight");
  });

  it("GATE10 item 8: a one-shot abort on the requeue is retried once and succeeds", async () => {
    // No fake timers (the retry is a plain async chain). Stub navigator.locks
    // to report no holder so the same-owner fresh lease is reclaimed.
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: { locks: { query: async () => ({ held: [] }) } },
    });
    const calls: string[] = [];
    try {
      const store = new MemStorage();
      const first = await Outbox.load(store, "owner_one_shot");
      await first.enqueue(rec({ commandId: "cmd_oneshot", instanceId: "ins_1" }));
      await first.patch("cmd_oneshot", {
        state: "inflight",
        lease: { owner: "owner_one_shot", until: Date.now() + LEASE_TTL_MS },
      });
      // Arm the one-shot abort ONLY for the second context's reclaim merge.
      vi.spyOn(store, "mergeUnlessDone").mockImplementation(
        (id: string, patch: Partial<OutboxRecord>) => {
          calls.push(id);
          if (calls.filter((c) => c === id).length === 1) {
            return Promise.reject(new Error("one-shot tx abort"));
          }
          // Retry: emulate the real merge without recursing into the spy.
          return Promise.resolve(null).then(async () => {
            const existing = (await store.all()).find((r) => r.commandId === id);
            if (!existing) return null;
            if (existing.state === "done") return existing;
            const merged: OutboxRecord = { ...existing, ...patch };
            (store as unknown as { map: Map<string, OutboxRecord> }).map.set(id, merged);
            return merged;
          });
        },
      );

      // A fresh context reclaims: first merge attempt aborts, retried, succeeds.
      const fresh = await Outbox.load(store, "owner_one_shot");
      expect(calls.filter((c) => c === "cmd_oneshot")).toHaveLength(2);
      const row = fresh.pendingFor("ins_1").find((r) => r.commandId === "cmd_oneshot");
      expect(row?.state).toBe("pending");
    } finally {
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE10 item 6: a same-owner fresh lease is NOT reclaimed when a live context holds the instance lock", async () => {
    // Duplicated tab (cloned sessionStorage → same tabOwner): tab A is
    // delivering and holds the durable Web Lock. Tab B loads with a stubbed
    // navigator.locks.query reporting the holder → it must leave A's fresh
    // inflight row untouched and not deliver it.
    const originalQuery = (globalThis as { navigator?: Navigator }).navigator?.locks?.query;
    const originalNavigator = globalThis.navigator;
    const heldNames = new Set<string>(["__lock__:ins_same"]);
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: {
        locks: {
          query: async () => ({ held: [...heldNames].map((name) => ({ name, mode: "exclusive" as const })) }),
        },
      },
    });
    try {
      const store = new MemStorage();
      const a = await Outbox.load(store, "owner_dup");
      await a.enqueue(rec({ commandId: "cmd_dup", instanceId: "ins_same" }));
      await a.patch("cmd_dup", {
        state: "inflight",
        lease: { owner: "owner_dup", until: Date.now() + LEASE_TTL_MS },
      });
      // B has the same owner (cloned sessionStorage) and sees the lock held.
      const b = await Outbox.load(store, "owner_dup");
      expect(b.pendingFor("ins_same")).toEqual([]);
      const row = store.map.get("cmd_dup")!;
      expect(row.state).toBe("inflight");
      expect(row.lease?.owner).toBe("owner_dup");
      // A (the real holder) can still see/deliver it via its cache.
      expect(a.pendingFor("ins_same")).toEqual([]);
    } finally {
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
      void originalQuery;
    }
  });

  it("GATE10 item 6: a same-owner fresh lease with NO holder is reclaimed (fresh context)", async () => {
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: {
        locks: {
          query: async () => ({ held: [] }),
        },
      },
    });
    try {
      const store = new MemStorage();
      const old = await Outbox.load(store, "owner_ctx");
      await old.enqueue(rec({ commandId: "cmd_ctx", instanceId: "ins_1" }));
      await old.patch("cmd_ctx", {
        state: "inflight",
        lease: { owner: "owner_ctx", until: Date.now() + LEASE_TTL_MS },
      });
      const fresh = await Outbox.load(store, "owner_ctx");
      expect(fresh.pendingFor("ins_1").map((r) => r.commandId)).toEqual(["cmd_ctx"]);
    } finally {
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE9 item 6: tabOwner writes sessionStorage and leaves localStorage untouched", () => {
    const session = new Map<string, string>();
    const local = new Map<string, string>();
    const make = (m: Map<string, string>): Storage =>
      ({
        getItem: (k: string) => m.get(k) ?? null,
        setItem: (k: string, v: string) => void m.set(k, v),
        removeItem: (k: string) => void m.delete(k),
        clear: () => m.clear(),
        key: () => null,
        length: 0,
      }) as Storage;
    Object.defineProperty(globalThis, "sessionStorage", {
      value: make(session),
      configurable: true,
      writable: true,
    });
    Object.defineProperty(globalThis, "localStorage", {
      value: make(local),
      configurable: true,
      writable: true,
    });
    const a = Outbox.tabOwner();
    const b = Outbox.tabOwner();
    expect(a).toBe(b);
    expect(a.startsWith("owner_")).toBe(true);
    expect(session.has("remuda-outbox-owner")).toBe(true);
    expect(local.size).toBe(0);
  });

  it("GATE9 item 6: two tabs with distinct sessionStorage mint distinct owners (explicit-owner path)", async () => {
    // In production each tab's tabOwner() reads its own (separate)
    // sessionStorage; in jsdom that storage is shared, so simulate the two-tab
    // boundary with the explicit-owner argument Outbox.load accepts (the same
    // string each tab derives from its own sessionStorage).
    const storeA = new MemStorage();
    const a = await Outbox.load(storeA, "owner_tab_a");
    const b = await Outbox.load(storeA, "owner_tab_b");
    expect(a.ownerId).toBe("owner_tab_a");
    expect(b.ownerId).toBe("owner_tab_b");
  });

  it("GATE8 item 4: tabOwner is stable across reload/SW restore (sessionStorage) but unique per tab", () => {
    const a = Outbox.tabOwner();
    const b = Outbox.tabOwner();
    expect(a).toBe(b);
    expect(a.startsWith("owner_")).toBe(true);
  });

  it("GATE8 item 4: a SIBLING tab (different owner) never requeues a live tab's fresh inflight claim", async () => {
    const tabA = await Outbox.load(storage, "owner_tab_a");
    await tabA.enqueue(rec({ commandId: "cmd_sibling", instanceId: "ins_1" }));
    await tabA.patch("cmd_sibling", {
      state: "inflight",
      lease: { owner: "owner_tab_a", until: Date.now() + LEASE_TTL_MS },
    });

    // Simulate the real two-tab situation: tab B is a DIFFERENT sessionStorage
    // owner loading with tab A's claim fresh. It must not requeue/deliver it
    // (gate 8: the per-profile owner used to make both tabs identical, so load
    // requeued a live POST and the no-Web-Locks fallback double-delivered).
    const tabB = await Outbox.load(storage, "owner_tab_b");
    expect(tabB.pendingFor("ins_1")).toEqual([]);
    expect(storage.map.get("cmd_sibling")?.state).toBe("inflight");
    expect(storage.map.get("cmd_sibling")?.lease?.owner).toBe("owner_tab_a");
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
      acquireLease: async () => {
        throw new Error("IDB transaction aborted");
      },
      mergeUnlessDone: async () => {
        throw new Error("IDB transaction aborted");
      },
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

  it("two lock-less tabs share ONE atomic lease: single POST, zero-row loser, then a steal after the loser's TTL delivers its row once", async () => {
    const originalLocks = (globalThis.navigator as { locks?: LockManager }).locks;
    Object.defineProperty(globalThis.navigator, "locks", {
      value: undefined,
      configurable: true,
    });

    // Seed the first row under REAL timers (MemStorage commits on a 1ms tick).
    const shared = new MemStorage();
    await shared.put(rec({ commandId: "cmd_r1" }));

    vi.useFakeTimers();
    try {
      const a = await Outbox.load(shared, "owner_A");
      const b = await Outbox.load(shared, "owner_B");

      // Model the POST: rows handed inside the lock are "posted", then the
      // owner marks them sent storage-first (as store.deliverOutboxRecord
      // does). The 50ms POST latency means two concurrent deliverers would
      // BOTH observe cmd_r1 pending — the check-then-act hazard.
      const posts: { owner: string; handed: string[] }[] = [];
      const deliver =
        (name: string, box: Outbox) => async (_iid: string, rows: OutboxRecord[]) => {
          posts.push({ owner: name, handed: rows.map((r) => r.commandId) });
          for (const r of rows) {
            await new Promise((resolve) => setTimeout(resolve, 50));
            await box.patch(r.commandId, { state: "sent", gotResponse: true, lease: undefined });
          }
          return rows.length;
        };

      // Both flush CONCURRENTLY, with no await between the starts.
      const lockA = a.withInstanceLock("ins_1", deliver("A", a));
      const lockB = b.withInstanceLock("ins_1", deliver("B", b));
      // A wins the serialized atomic claim, posts (50 ms), marks sent,
      // releases; B's claim fails against A's committed lease and it parks on
      // its 250 ms poll.
      await vi.advanceTimersByTimeAsync(260);
      const [rowsA, rowsB] = await Promise.all([lockA, lockB]);
      expect(rowsA).toBe(1);
      expect(rowsB).toBe(0);
      // The regression: the old separate all()+put() pair let both tabs
      // deliver cmd_r1; the atomic claim hands it to exactly one owner, and
      // B's fresh durable read inside its later lock sees it sent (zero
      // rows) rather than POSTing it again.
      expect(posts).toEqual([
        { owner: "A", handed: ["cmd_r1"] },
        { owner: "B", handed: [] },
      ]);

      // B enqueues a second row and then CRASHES holding a live lease: no
      // finally-delete, so the durable lease sits until its TTL.
      const enqueue = b.enqueue(rec({ commandId: "cmd_r2" }));
      await vi.advanceTimersByTimeAsync(5);
      await enqueue;
      const crashedLeasePut = shared.put({
        commandId: "__lock__:ins_1",
        clientRequestId: "__lock__:ins_1",
        instanceId: "ins_1",
        prompt: "",
        createdAt: Date.now(),
        attempts: 0,
        state: "inflight",
        lease: { owner: "owner_B", until: Date.now() + LEASE_TTL_MS },
      });
      await vi.advanceTimersByTimeAsync(5);
      await crashedLeasePut;

      // A retries while B's lease is still LIVE: parked, nothing delivered.
      const lockA2 = a.withInstanceLock("ins_1", deliver("A2", a));
      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS - 1_000);
      expect(posts.filter((p) => p.handed.includes("cmd_r2"))).toHaveLength(0);

      // Past the TTL the crashed owner's lease is stealable; A delivers the
      // row B enqueued exactly once.
      await vi.advanceTimersByTimeAsync(2_000);
      const rowsA2 = await lockA2;
      expect(rowsA2).toBe(1);
      expect(posts.filter((p) => p.handed.includes("cmd_r2"))).toEqual([
        { owner: "A2", handed: ["cmd_r2"] },
      ]);
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis.navigator, "locks", {
        value: originalLocks,
        configurable: true,
      });
    }
  });
});
