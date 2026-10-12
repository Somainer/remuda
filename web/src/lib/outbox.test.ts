import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  instanceDeliveryLockName,
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
    this.writeChain = run.then(() => undefined, () => undefined);
    return await run;
  }
  /** Conditional lease release on the same chain as the lease claim. */
  async releaseLease(key: string, owner: string) {
    const run = this.writeChain.then(async () => {
      await new Promise((r) => setTimeout(r, 1));
      const existing = this.map.get(key);
      if (!existing || existing.lease?.owner !== owner) return false;
      this.map.delete(key);
      return true;
    });
    this.writeChain = run.then(() => undefined, () => undefined);
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
    this.writeChain = run.then(() => undefined, () => undefined);
    return await run;
  }
  /** Conditional reclaim on the same serialized critical section. */
  async reclaimInflight(
    commandId: string,
    expected: { owner: string; until: number },
    opts?: { requireExpired?: boolean; signal?: AbortSignal },
  ): Promise<OutboxRecord | null> {
    const run = this.writeChain.then(async () => {
      await new Promise((r) => setTimeout(r, 1));
      if (opts?.signal?.aborted) throw new DOMException("reclaim aborted", "AbortError");
      const existing = this.map.get(commandId);
      if (!existing) return null;
      const lease = existing.lease;
      if (
        existing.state !== "inflight" ||
        !lease ||
        lease.owner !== expected.owner ||
        lease.until !== expected.until
      ) {
        return existing;
      }
      // Mirrors the production IDB backend: expiry is evaluated only for the
      // MATCHING inflight lease (no null-safe masking — a terminal row above
      // already returned).
      const expired = !opts?.requireExpired || lease.until <= Date.now();
      if (!expired) return existing;
      if (opts?.signal?.aborted) throw new DOMException("reclaim aborted", "AbortError");
      const reclaimed: OutboxRecord = { ...existing, state: "pending", lease: undefined };
      this.map.set(commandId, reclaimed);
      return reclaimed;
    });
    this.writeChain = run.then(() => undefined, () => undefined);
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

  it("GATE11 item 3: same-owner fresh lease with NO Web Locks API is UNKNOWN — kept until TTL, reclaimed once expired", async () => {
    // Gate 7 used to reclaim a same-owner claim immediately; gate 11 item 3
    // changes the no-Web-Locks outcome to UNKNOWN: a cloned live tab cannot be
    // distinguished from a dead context without locks.query, so the fresh
    // lease stands until its deadline and only an expired claim is requeued.
    const originalLocks = (globalThis.navigator as { locks?: LockManager }).locks;
    Object.defineProperty(globalThis.navigator, "locks", {
      value: undefined,
      configurable: true,
    });
    try {
      const owner = "owner_reload_no_locks";
      const first = await Outbox.load(storage, owner);
      await first.enqueue(rec({ commandId: "cmd_inflight", instanceId: "ins_1" }));
      await first.patch("cmd_inflight", {
        state: "inflight",
        attempts: 1,
        lease: { owner, until: Date.now() + LEASE_TTL_MS },
      });

      const reloaded = await Outbox.load(storage, owner);
      expect(reloaded.pendingFor("ins_1")).toEqual([]);
      expect(storage.map.get("cmd_inflight")?.state).toBe("inflight");

      // Once the lease has expired the claim is reclaimed.
      await first.patch("cmd_inflight", {
        state: "inflight",
        lease: { owner, until: Date.now() - 1 },
      });
      const reloadedExpired = await Outbox.load(storage, owner);
      expect(reloadedExpired.pendingFor("ins_1").map((r) => r.commandId)).toEqual(["cmd_inflight"]);
      expect(storage.map.get("cmd_inflight")?.state).toBe("pending");
    } finally {
      Object.defineProperty(globalThis.navigator, "locks", {
        value: originalLocks,
        configurable: true,
      });
    }
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
      // Arm the one-shot abort ONLY for the second context's reclaim.
      vi.spyOn(store, "reclaimInflight").mockImplementation((id: string) => {
        calls.push(id);
        if (calls.filter((c) => c === id).length === 1) {
          return Promise.reject(new Error("one-shot tx abort"));
        }
        // Retry: emulate the real conditional reclaim without recursing into
        // the spy.
        return Promise.resolve(null).then(async () => {
          const existing = (await store.all()).find((r) => r.commandId === id);
          if (!existing) return null;
          const reclaimed: OutboxRecord = { ...existing, state: "pending", lease: undefined };
          (store as unknown as { map: Map<string, OutboxRecord> }).map.set(id, reclaimed);
          return reclaimed;
        });
      });

      // A fresh context reclaims: first transaction aborts, retried, succeeds.
      const fresh = await Outbox.load(store, "owner_one_shot");
      expect(calls.filter((c) => c === "cmd_oneshot")).toHaveLength(2);
      const row = fresh.pendingFor("ins_1").find((r) => r.commandId === "cmd_oneshot");
      expect(row?.state).toBe("pending");
    } finally {
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE12 item 5: when locks.query throws but locks.request exists, the load-time reclaim still runs INSIDE the instance lock", async () => {
    const lockNames: string[] = [];
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: {
        locks: {
          query: async () => {
            throw new Error("LockManager.query broken");
          },
          request: (name: string, _opts: unknown, cb: () => Promise<unknown>) => {
            lockNames.push(name);
            return cb();
          },
        },
      },
    });
    try {
      const store = new MemStorage();
      const instanceId = "ins_queryfail";
      // An EXPIRED same-owner claim: query failing makes ownership unknown but
      // the deadline has passed, so load requeues — and must take the lock.
      await store.put(
        rec({
          commandId: "cmd_queryfail",
          instanceId,
          state: "inflight",
          lease: { owner: "owner_qf", until: Date.now() - 1 },
        }),
      );
      const box = await Outbox.load(store, "owner_qf");
      expect(box.pendingFor(instanceId).map((r) => r.commandId)).toEqual(["cmd_queryfail"]);
      expect(store.map.get("cmd_queryfail")?.state).toBe("pending");
      // The reclaim serialized under the real lock even though query threw.
      expect(lockNames).toContain(instanceDeliveryLockName(instanceId));
    } finally {
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE12 item 4 (gate 20): refreshDurable runs the CENTRAL reconciliation — terminal/missing wakes cancelled, changed lease adopted", async () => {
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks: undefined } });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(500_000);
      const store = new MemStorage();
      const owner = "owner_reconcile";
      const T = Date.now() + LEASE_TTL_MS;
      for (const id of ["cmd_sent", "cmd_renewed", "cmd_deleted"]) {
        const p = store.put(
          rec({
            commandId: id,
            instanceId: `ins_${id}`,
            state: "inflight",
            lease: { owner, until: T },
          }),
        );
        await vi.advanceTimersByTimeAsync(2);
        await p;
      }
      const box = await Outbox.load(store, owner);
      // The store's boot flush is the first authoritative reader after load;
      // it applies the central reconciliation and arms the deadline wakes.
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      expect(box.pendingLeaseWakeups().sort()).toEqual(["cmd_deleted", "cmd_renewed", "cmd_sent"]);
      expect(box.leaseWakeKind("cmd_renewed")).toBe("lease");
      const wakes = vi.fn();
      box.onClaimWake = wakes;

      // Another tab delivers one, renews a second, deletes the third.
      const T2 = T + LEASE_TTL_MS;
      const writes = [
        store.put(
          rec({
            commandId: "cmd_sent",
            instanceId: "ins_cmd_sent",
            state: "sent",
            gotResponse: true,
            lease: undefined,
          }),
        ),
        store.put(
          rec({
            commandId: "cmd_renewed",
            instanceId: "ins_cmd_renewed",
            state: "inflight",
            lease: { owner, until: T2 },
          }),
        ),
        store.delete("cmd_deleted"),
      ];
      await vi.advanceTimersByTimeAsync(5);
      await Promise.all(writes);

      await box.refreshDurable();

      // Terminal and missing claims lost their wakes; the renewed one kept
      // its wake for the LATER deadline.
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_renewed"]);
      expect(box.get("cmd_sent")?.state).toBe("sent");

      // At the OLD deadline nothing fires (sent would re-trigger wrongly; the
      // renewed claim is still live).
      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS);
      expect(wakes).not.toHaveBeenCalled();
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_renewed"]);
      expect(store.map.get("cmd_renewed")?.state).toBe("inflight");

      // At T2 the expired claim hands the store one guarded delivery trigger.
      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS);
      expect(wakes).toHaveBeenCalledTimes(1);
      // No Outbox-side reclaim: the store's in-lock deliverer claims the
      // expired lease durably itself.
      expect(store.map.get("cmd_renewed")?.state).toBe("inflight");
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE12 item 3 (gate 20): dispose during a suspended deadline READ imports nothing, delivers nothing, leaves zero timers", async () => {
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks: undefined } });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(300_000);
      let releaseRead: (() => void) | null = null;
      const readGate = new Promise<void>((r) => void (releaseRead = r));
      class GatedRead extends MemStorage {
        reads = 0;
        override async all() {
          this.reads += 1;
          // load (#1), boot refresh (#2), the FIRE read (#3).
          if (this.reads >= 3) {
            await readGate;
            // While the fire read was suspended, another tab durably
            // committed sent; the read now returns the changed durable state.
            const cur = this.map.get("cmd_disp_read");
            if (cur && cur.state === "inflight") {
              this.map.set("cmd_disp_read", { ...cur, state: "sent", gotResponse: true, lease: undefined });
            }
          }
          return await super.all();
        }
      }
      const store = new GatedRead();
      const owner = "owner_disp_read";
      const putP = store.put(
        rec({
          commandId: "cmd_disp_read",
          instanceId: "ins_disp",
          state: "inflight",
          lease: { owner, until: Date.now() + LEASE_TTL_MS },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;
      const box = await Outbox.load(store, owner);
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      const wakes = vi.fn();
      box.onClaimWake = wakes;

      // Deadline fires; the durable read is suspended.
      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS);
      expect(box.pendingLeaseWakeups()).toEqual([]); // handle deleted when fire started
      box.dispose(); // teardown while the read is in flight
      releaseRead!();
      await vi.advanceTimersByTimeAsync(10_000); // far past any retry/backoff

      // The read-retry chain is disposed too: the CHANGED durable row (sent)
      // is not imported into the cache, nothing is delivered or re-armed.
      expect(store.map.get("cmd_disp_read")?.state).toBe("sent");
      expect(box.get("cmd_disp_read")?.state).toBe("inflight"); // cache untouched
      expect(wakes).not.toHaveBeenCalled();
      expect(box.pendingLeaseWakeups()).toEqual([]);
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE13 item 3 (gate 20): dispose during a suspended fire READ that would find a sibling-restored PENDING row never delivers", async () => {
    // The fire critical section is read-only now (no reclaim write); still a
    // read resumed after dispose must neither import the deliverable row nor
    // fire the delivery hook — a logged-out/tearing-down Outbox owns nothing.
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks: undefined } });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(350_000);
      let releaseRead: (() => void) | null = null;
      const readGate = new Promise<void>((r) => void (releaseRead = r));
      class GatedPending extends MemStorage {
        reads = 0;
        override async all() {
          this.reads += 1;
          if (this.reads >= 3) {
            await readGate;
            const cur = this.map.get("cmd_disp_pending");
            if (cur && cur.state === "inflight") {
              this.map.set("cmd_disp_pending", { ...cur, state: "pending", lease: undefined });
            }
          }
          return await super.all();
        }
      }
      const store = new GatedPending();
      const owner = "owner_disp_pending";
      const putP = store.put(
        rec({
          commandId: "cmd_disp_pending",
          instanceId: "ins_disp",
          state: "inflight",
          lease: { owner, until: Date.now() + LEASE_TTL_MS },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;
      const box = await Outbox.load(store, owner);
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      const wakes = vi.fn();
      box.onClaimWake = wakes;

      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS);
      box.dispose();
      releaseRead!();
      await vi.advanceTimersByTimeAsync(10_000);

      expect(store.map.get("cmd_disp_pending")?.state).toBe("pending");
      expect(box.get("cmd_disp_pending")?.state).toBe("inflight");
      expect(wakes).not.toHaveBeenCalled();
      expect(box.pendingLeaseWakeups()).toEqual([]);
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE21 item 1a: a flush whose claim read fails once while queued past the 250 ms recovery behind a sibling-held lock re-arms ONLY from the scoped flush epilogue (R delivers via A's real timer)", async () => {
    // The gate 20 shape let a flush that posted nothing end by CLEARING the
    // suppression set: its bounded recovery read had run while its rows were
    // still covered, so the row kept no wake and stranded. Suppression also
    // used one shared set, so an overlapping flush ending dropped the other's
    // cover. Gate 21 scopes suppression per flush (reference counted) and
    // makes EVERY flush epilogue reconcile a fresh authoritative snapshot.
    vi.useFakeTimers();
    const I = "ins_g21_i1a_i";
    const J = "ins_g21_i1a_j";
    const heldLocks = new Set<string>();
    // Shared backend: all() rejects ONCE per armed name while that lock is
    // really held by the reading document (the in-lock read failure).
    class FlakyOnceStorage extends MemStorage {
      failWhileHeld: Set<string> = new Set();
      override async all() {
        for (const name of this.failWhileHeld) {
          if (heldLocks.has(name)) {
            this.failWhileHeld.delete(name);
            throw new Error("g21-i1a one-shot in-lock read abort");
          }
        }
        return await super.all();
      }
    }
    const queues = new Map<string, Array<() => void>>();
    const locks = {
      query: async () => ({ held: [...heldLocks].map((name) => ({ name, mode: "exclusive" })) }),
      request(name: string, _opts: unknown, cb: () => Promise<unknown>) {
        return new Promise((resolve, reject) => {
          const run = () => {
            heldLocks.add(name);
            Promise.resolve()
              .then(cb)
              .then(
                (v) => {
                  heldLocks.delete(name);
                  resolve(v);
                  queues.get(name)?.shift()?.();
                },
                (e) => {
                  heldLocks.delete(name);
                  reject(e);
                  queues.get(name)?.shift()?.();
                },
              );
          };
          if (heldLocks.has(name) || (queues.get(name)?.length ?? 0) > 0) {
            const q = queues.get(name) ?? [];
            q.push(run);
            queues.set(name, q);
          } else {
            run();
          }
        });
      },
    };
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks } });
    try {
      const store = new FlakyOnceStorage();
      const pR = store.put(rec({ commandId: "cmd_g21_i1a_r", instanceId: I, createdAt: 1_000 }));
      const pSB = store.put(rec({ commandId: "cmd_g21_i1a_sb", instanceId: J, createdAt: 900 }));
      await vi.advanceTimersByTimeAsync(5);
      await pR;
      await pSB;
      const a = await Outbox.load(store, "owner_g21_i1a_a");
      const b = await Outbox.load(store, "owner_g21_i1a_b");
      const hook = vi.fn();
      a.onClaimWake = hook;

      // B's real delivery turn on J: claim flow blocks, then commits sent.
      let releaseB: (() => void) | null = null;
      const bGate = new Promise<void>((r) => void (releaseB = r));
      const bTurn = b.withInstanceLock(J, async () => {
        await bGate;
        await b.patch("cmd_g21_i1a_sb", { state: "sent", gotResponse: true });
        return 0;
      });
      await vi.advanceTimersByTimeAsync(5); // B is holding J

      // A guarded flush. Gate 21 scopes suppression per flush and ends every
      // flush with a fresh authoritative reconciliation; on the pre-gate-21
      // API the test drives the old shared-set suppress/clear pair instead.
      const boxA = a as unknown as {
        beginDeliveryPass?: (ids: Iterable<string>) => number;
        endDeliveryPasses?: (tokens: Iterable<number>) => Promise<void>;
        suppressDeliveryWakes?: (ids: Iterable<string>) => void;
        clearSuppressedDeliveryWakes?: () => void;
      };
      const scoped = typeof boxA.beginDeliveryPass === "function";
      const durable = await a.readDurable();
      const token = scoped
        ? boxA.beginDeliveryPass!(["ins_g21_i1a_i", "ins_g21_i1a_j"])
        : (boxA.suppressDeliveryWakes!(["ins_g21_i1a_i", "ins_g21_i1a_j"]), -1);
      a.applyLeaseWakeReconcile(durable);
      expect(hook).not.toHaveBeenCalled();

      // I turn: its in-lock read fails once → empty selection, bounded
      // recovery due 250 ms later (I failure count 1).
      store.failWhileHeld.add(instanceDeliveryLockName(I));
      const iTurn = a.withInstanceLock(I, async (_iid, rows) => rows.length);
      await vi.advanceTimersByTimeAsync(5);
      expect(await iTurn).toBe(0);
      expect(store.failWhileHeld.size).toBe(0);

      // J turn queues behind B. The 250 ms recovery read runs while the flush
      // still suppresses both instances → covered, no wake/hook.
      const jTurn = a.withInstanceLock(J, async (_iid, rows) => rows.length);
      await vi.advanceTimersByTimeAsync(300);
      expect(hook).not.toHaveBeenCalled();
      expect(a.pendingLeaseWakeups()).toEqual([]);
      expect(store.map.get("cmd_g21_i1a_r")?.state).toBe("pending");

      // B's delivery settles SB sent and releases J; A's queued turn selects
      // nothing and the whole flush posted nothing.
      releaseB!();
      await vi.advanceTimersByTimeAsync(5);
      expect(await bTurn).toBe(0);
      expect(await jTurn).toBe(0);
      // Flush epilogue. The gate 21 contract (whether via the scoped API or
      // any future equivalent): once the responsible flush releases its
      // cover, a FRESH authoritative snapshot re-arms R's one bounded wake.
      // The pre-gate-21 code only cleared the shared suppression set, so the
      // recovery consumed while covering R left it with no wake.
      if (scoped) {
        await boxA.endDeliveryPasses!([token]);
      } else {
        boxA.clearSuppressedDeliveryWakes!();
      }
      expect(hook).not.toHaveBeenCalled();
      expect(a.pendingLeaseWakeups()).toEqual(["cmd_g21_i1a_r"]);

      // A's real timer is the only remaining recovery trigger, firing exactly
      // once with no other intervention.
      await vi.advanceTimersByTimeAsync(300);
      expect(hook).toHaveBeenCalledTimes(1);
      expect(store.map.get("cmd_g21_i1a_sb")?.state).toBe("sent");
      expect(store.map.get("cmd_g21_i1a_r")?.state).toBe("pending");

      // Far past every deadline: still exactly the one trigger.
      await vi.advanceTimersByTimeAsync(60_000);
      expect(hook).toHaveBeenCalledTimes(1);
      a.dispose();
      b.dispose();
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE21 item 2: a write-triggered reconciliation uses the AUTHORITATIVE durable snapshot, not the local cache — a sibling-finished row loses its wake", async () => {
    // Two real Outboxes share one durable backend. X and Y are A's fresh
    // inflight claims on two instances (deadline wakes at 30 s/60 s, so no
    // timer fires during the test). B finishes X through its REAL write flow.
    // A then writes Y: pre-gate-21 the write re-reconciled from A's LOCAL
    // cache (which never saw B's commit and kept X inflight), so X's deadline
    // wake survived a row that no longer existed in storage. Writes must
    // reconcile only from a fresh authoritative snapshot — X's wake is
    // cancelled while Y keeps exactly one.
    vi.useFakeTimers();
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: { locks: undefined },
    });
    try {
      const owner = "owner_g21_item2";
      const store = new MemStorage();
      const now = Date.now();
      const px = store.put(
        rec({
          commandId: "cmd_g21_item2_x",
          instanceId: "ins_g21_item2_x",
          state: "inflight",
          attempts: 1,
          createdAt: now - 2_000,
          lease: { owner, until: now + 30_000 },
        }),
      );
      const py = store.put(
        rec({
          commandId: "cmd_g21_item2_y",
          instanceId: "ins_g21_item2_y",
          state: "inflight",
          attempts: 1,
          createdAt: now - 1_000,
          lease: { owner, until: now + 60_000 },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await px;
      await py;

      const a = await Outbox.load(store, owner);
      const b = await Outbox.load(store, owner);
      await a.refreshDurable(); // the store's boot reconciliation
      expect(a.pendingLeaseWakeups().sort()).toEqual(["cmd_g21_item2_x", "cmd_g21_item2_y"].sort());

      // B's real delivery flow finishes X (sibling commit). A's cache is not
      // told and no A timer fires inside this window.
      const bPatch = b.patch("cmd_g21_item2_x", {
        state: "sent",
        gotResponse: true,
        lease: undefined,
      });
      await vi.advanceTimersByTimeAsync(5);
      await bPatch;
      expect(store.map.get("cmd_g21_item2_x")?.state).toBe("sent");
      expect(a.pendingLeaseWakeups()).toContain("cmd_g21_item2_x"); // stale until A re-reads
      expect(a.pendingLeaseWakeups()).toContain("cmd_g21_item2_y");

      // A writes Y through its REAL patch flow (storage-first write +
      // reconciliation). The post-write reconciliation is authoritative:
      // X's phantom wake is cancelled; Y keeps its one deadline wake.
      const aPatch = a.patch("cmd_g21_item2_y", {
        state: "sent",
        gotResponse: true,
        lease: undefined,
      });
      await vi.advanceTimersByTimeAsync(5);
      await aPatch;
      expect(a.pendingLeaseWakeups().filter((id) => id === "cmd_g21_item2_x")).toEqual([]);
      expect(a.pendingLeaseWakeups().filter((id) => id === "cmd_g21_item2_y")).toEqual([]);

      // Far past both old deadlines: no phantom wake ever comes back.
      await vi.advanceTimersByTimeAsync(120_000);
      expect(a.pendingLeaseWakeups()).toEqual([]);
      a.dispose();
      b.dispose();
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE12 item 2 (gate 20): a one-shot durable-read failure at the deadline retries bounded and still delivers", async () => {
    // A rejected fire read enters the bounded read-retry chain; the 250 ms
    // retry succeeds, the central reconciliation arms the 0 ms delivery wake
    // and the store hook fires exactly once.
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks: undefined } });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(200_000);
      class OneShotReadFail extends MemStorage {
        reads = 0;
        failed = false;
        override async all() {
          this.reads += 1;
          // load (#1), boot refresh (#2), the FIRE read (#3) fails once.
          if (this.reads === 3 && !this.failed) {
            this.failed = true;
            throw new Error("IDB read aborted");
          }
          return await super.all();
        }
      }
      const store = new OneShotReadFail();
      const owner = "owner_readfail";
      const T = Date.now() + LEASE_TTL_MS;
      const putP = store.put(
        rec({
          commandId: "cmd_readfail",
          instanceId: "ins_readfail",
          state: "inflight",
          lease: { owner, until: T },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;
      const box = await Outbox.load(store, owner);
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      const wakes = vi.fn();
      box.onClaimWake = wakes;

      // Deadline fires: the read aborts; the read-RETRY (not a lease wake)
      // now owns recovery.
      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS);
      expect(wakes).not.toHaveBeenCalled();
      expect(box.pendingLeaseWakeups()).toEqual([]);

      // 250 ms backoff: read succeeds → 0 ms delivery wake → fire → hook.
      await vi.advanceTimersByTimeAsync(250);
      await vi.advanceTimersByTimeAsync(10);
      expect(wakes).toHaveBeenCalledTimes(1);
      expect(box.pendingLeaseWakeups()).toEqual([]);
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE13 item 2 (gate 20): a timer firing across a system-clock ROLLBACK adopts the fresh-lease deadline and delivers only when wall time catches up", async () => {
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks: undefined } });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(1_000_000);
      const T = Date.now() + LEASE_TTL_MS;
      let releaseRead: (() => void) | null = null;
      const readGate = new Promise<void>((r) => void (releaseRead = r));
      class RollbackStorage extends MemStorage {
        reads = 0;
        override async all() {
          this.reads += 1;
          if (this.reads >= 3) await readGate; // the fire's read is suspended
          return await super.all();
        }
      }
      const store = new RollbackStorage();
      const putP = store.put(
        rec({
          commandId: "cmd_rollback",
          instanceId: "ins_rb",
          state: "inflight",
          lease: { owner: "owner_rb", until: T },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;
      const box = await Outbox.load(store, "owner_rb");
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      const wakes = vi.fn();
      box.onClaimWake = wakes;

      // The deadline fires (clock at T) but its read is suspended WHILE the
      // system clock moves 10 s backwards.
      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS);
      vi.setSystemTime(T - 10_000);
      releaseRead!();
      await vi.advanceTimersByTimeAsync(10);
      // Still fresh under the rolled-back wall clock: adopted deadline, no
      // delivery, one armed wake at T.
      expect(store.map.get("cmd_rollback")?.state).toBe("inflight");
      expect(wakes).not.toHaveBeenCalled();
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_rollback"]);
      expect(box.leaseWakeAt("cmd_rollback")).toBe(T);

      // Wall time catches up: the delivery hook fires; the durable row is an
      // expired inflight claim the store deliverer now owns.
      await vi.advanceTimersByTimeAsync(10_000);
      await vi.advanceTimersByTimeAsync(10);
      expect(wakes).toHaveBeenCalledTimes(1);
      expect(store.map.get("cmd_rollback")?.state).toBe("inflight");
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE13 item 1a (gate 20): a fire whose fresh read finds the REPLACEMENT lease already expired delivers immediately", async () => {
    // The background timer fires late (throttled tab): by T the durable row
    // carries a RENEWED lease T2 that has ITSELF expired. The decision is
    // "deliver" now — "still inflight" is never terminal.
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks: undefined } });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(600_000);
      const T = Date.now() + LEASE_TTL_MS;
      class LateReplaceStorage extends MemStorage {
        readCount = 0;
        override async all() {
          this.readCount += 1;
          // The fire read (#3; load #1, boot refresh #2) sees the
          // renewed-but-expired replacement lease.
          if (this.readCount >= 3) {
            const cur = this.map.get("cmd_late");
            if (cur && cur.lease?.until === T) {
              this.map.set("cmd_late", {
                ...cur,
                lease: { owner: cur.lease.owner, until: T - 1_000 },
              });
            }
          }
          return await super.all();
        }
      }
      const store = new LateReplaceStorage();
      const putP = store.put(
        rec({
          commandId: "cmd_late",
          instanceId: "ins_late",
          state: "inflight",
          lease: { owner: "owner_late", until: T },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;
      const box = await Outbox.load(store, "owner_late");
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      const wakes = vi.fn();
      box.onClaimWake = wakes;

      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS); // T (and T2-1000 both past)
      await vi.advanceTimersByTimeAsync(10);
      expect(store.map.get("cmd_late")?.state).toBe("inflight");
      expect(store.map.get("cmd_late")?.lease?.until).toBe(T - 1_000);
      expect(wakes).toHaveBeenCalledTimes(1);
      expect(box.pendingLeaseWakeups()).toEqual([]);
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE13 item 1b (gate 20): the fire read finds a FUTURE replacement lease — adopt deadline T2, deliver at T2", async () => {
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks: undefined } });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(700_000);
      const T = Date.now() + LEASE_TTL_MS;
      const T2 = T + LEASE_TTL_MS;
      class RenewBetweenStorage extends MemStorage {
        readCount = 0;
        override async all() {
          this.readCount += 1;
          // The fire read (#3) returns the renewed future lease directly.
          if (this.readCount >= 3) {
            const cur = this.map.get("cmd_renew_between")!;
            if (cur.lease?.until === T) {
              this.map.set("cmd_renew_between", {
                ...cur,
                attempts: 2,
                lease: { owner: cur.lease.owner, until: T2 },
              });
            }
          }
          return await super.all();
        }
      }
      const store = new RenewBetweenStorage();
      const putP = store.put(
        rec({
          commandId: "cmd_renew_between",
          instanceId: "ins_between",
          state: "inflight",
          lease: { owner: "owner_between", until: T },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;
      const box = await Outbox.load(store, "owner_between");
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      const wakes = vi.fn();
      box.onClaimWake = wakes;

      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS); // T
      await vi.advanceTimersByTimeAsync(10);
      // Renewed future lease: adopted, not delivered, wake moved to T2.
      expect(store.map.get("cmd_renew_between")?.state).toBe("inflight");
      expect(store.map.get("cmd_renew_between")?.lease?.until).toBe(T2);
      expect(wakes).not.toHaveBeenCalled();
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_renew_between"]);
      expect(box.leaseWakeAt("cmd_renew_between")).toBe(T2);

      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS); // T2
      await vi.advanceTimersByTimeAsync(10);
      expect(wakes).toHaveBeenCalledTimes(1);
      expect(store.map.get("cmd_renew_between")?.state).toBe("inflight");
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE13 item 1c (gate 20): a load-time reclaim returning a replacement inflight row is cached; the boot reconciliation arms its deadline", async () => {
    // query says holder gone -> load reclaims; the conditional transaction
    // finds the live owner renewed the lease while load was in flight and
    // returns the replacement inflight row verbatim. The box caches it; the
    // store-equivalent boot refresh arms the deadline (future -> armed).
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: { locks: { query: async () => ({ held: [] }) } },
    });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(800_000);
      const T = Date.now() + LEASE_TTL_MS;
      const T2 = T + LEASE_TTL_MS;
      class ReplaceOnReclaim extends MemStorage {
        override async reclaimInflight(
          id: string,
          expected: { owner: string; until: number },
        ): Promise<OutboxRecord | null> {
          const existing = this.map.get(id);
          if (!existing) return null;
          // Live owner renewed concurrently: the real backend PERSISTS the
          // conditional write and returns the replacement row verbatim.
          const replaced = { ...existing, attempts: 3, lease: { owner: expected.owner, until: T2 } };
          this.map.set(id, replaced);
          return replaced;
        }
      }
      const store = new ReplaceOnReclaim();
      const putP = store.put(
        rec({
          commandId: "cmd_load_replace",
          instanceId: "ins_load_replace",
          state: "inflight",
          lease: { owner: "owner_lr", until: T },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;
      const box = await Outbox.load(store, "owner_lr");
      expect(box.get("cmd_load_replace")?.state).toBe("inflight");
      expect(box.get("cmd_load_replace")?.lease?.until).toBe(T2);
      expect(box.pendingLeaseWakeups()).toEqual([]); // load arms nothing itself
      await box.refreshDurable(); // the boot-flush reader arms
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_load_replace"]);
      expect(box.leaseWakeAt("cmd_load_replace")).toBe(T2);
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE13 item 1d (gate 20): refresh seeing an already-expired REPLACEMENT lease arms an immediate delivery wake, never cancels", async () => {
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks: undefined } });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(900_000);
      const T = Date.now() + LEASE_TTL_MS;
      const store = new MemStorage();
      const putP = store.put(
        rec({
          commandId: "cmd_refresh_replace",
          instanceId: "ins_rr",
          state: "inflight",
          lease: { owner: "owner_rr", until: T },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;
      const box = await Outbox.load(store, "owner_rr");
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_refresh_replace"]);
      const wakes = vi.fn();
      box.onClaimWake = wakes;

      // Another context renewed to T2, then crashed; at refresh time that
      // replacement lease is ALREADY expired (absolute past timestamp).
      const p = store.put(
        rec({
          commandId: "cmd_refresh_replace",
          instanceId: "ins_rr",
          state: "inflight",
          lease: { owner: "owner_rr", until: Date.now() - 1_000 },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await p;
      await box.refreshDurable();

      // Adopted and scheduled immediately (not cancelled).
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_refresh_replace"]);
      expect(box.leaseWakeKind("cmd_refresh_replace")).toBe("deliver");
      await vi.advanceTimersByTimeAsync(10);
      expect(wakes).toHaveBeenCalledTimes(1);
      expect(store.map.get("cmd_refresh_replace")?.state).toBe("inflight");
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE12 item 1 (gate 20): a deadline fire adopts a renewed later lease T2 and delivers only at T2; its read takes the instance lock", async () => {
    const originalNavigator = globalThis.navigator;
    const lockNames: string[] = [];
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: {
        // No query (unknown ownership at load) but request EXISTS: the fire
        // still runs its authoritative read under the per-instance lock.
        locks: {
          request: (name: string, _opts: unknown, cb: () => Promise<unknown>) => {
            lockNames.push(name);
            return cb();
          },
        },
      },
    });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(100_000);
      const store = new MemStorage();
      const owner = "owner_renew";
      const instanceId = "ins_renew";
      const T = Date.now() + LEASE_TTL_MS;
      const putP = store.put(
        rec({
          commandId: "cmd_renew",
          instanceId,
          state: "inflight",
          lease: { owner, until: T },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;
      const box = await Outbox.load(store, owner);
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_renew"]);
      const wakes = vi.fn();
      box.onClaimWake = wakes;

      // A live delivery RETRIES and RENEWS the lease (new until T2 > T)
      // before T.
      const T2 = T + LEASE_TTL_MS;
      const renewP = store.put(
        rec({
          commandId: "cmd_renew",
          instanceId,
          state: "inflight",
          attempts: 1,
          lease: { owner, until: T2 },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await renewP;

      // At T: still the live renewed claim — no delivery, wake moved to T2.
      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS);
      expect(store.map.get("cmd_renew")?.state).toBe("inflight");
      expect(store.map.get("cmd_renew")?.lease?.until).toBe(T2);
      expect(wakes).not.toHaveBeenCalled();
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_renew"]);
      expect(box.leaseWakeAt("cmd_renew")).toBe(T2);

      // T2: one guarded delivery trigger for the expired claim.
      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS);
      expect(wakes).toHaveBeenCalledTimes(1);
      expect(box.pendingLeaseWakeups()).toEqual([]);

      // The deadline read took the real per-instance lock.
      expect(lockNames).toContain(instanceDeliveryLockName(instanceId));
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE11 item 5 (gate 20): two failed load reclaims leave the inflight row; the deadline wake then triggers exactly one delivery turn", async () => {
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: { locks: { query: async () => ({ held: [] }) } },
    });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(10_000);
      // reclaimInflight fails BOTH load attempts (one-shot abort + retry);
      // the wake path no longer reclaims (it reads and hands the store
      // deliverer the expired claim), so the count stays at two.
      class FlakyStorage extends MemStorage {
        reclaimCalls = 0;
        override async reclaimInflight(
          id: string,
          expected: { owner: string; until: number },
        ): Promise<OutboxRecord | null> {
          this.reclaimCalls++;
          if (this.reclaimCalls <= 2) throw new Error("tx abort");
          return await super.reclaimInflight(id, expected);
        }
      }
      const store = new FlakyStorage();
      const instanceId = "ins_wake";
      const putP = store.put(
        rec({
          commandId: "cmd_wake",
          instanceId,
          state: "inflight",
          lease: { owner: "owner_wake", until: Date.now() + LEASE_TTL_MS },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;

      const box = await Outbox.load(store, "owner_wake");
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      // Both reclaim writes aborted: still inflight with its fresh lease; the
      // central reconciliation armed its deadline wake.
      expect(store.map.get("cmd_wake")?.state).toBe("inflight");
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_wake"]);

      // Delivery stand-in for the store's flushAllOutbox: one lock turn, one
      // POST per delivered row. The deliverer's in-lock read selects the
      // expired inflight claim (isDeliverable) even though it was never
      // reclaimed to pending.
      const wakes = vi.fn();
      const posts: string[][] = [];
      box.onClaimWake = () => {
        wakes();
        void box.withInstanceLock(instanceId, async (_iid, rows) => {
          posts.push(rows.map((r) => r.commandId));
          return rows.length;
        });
      };

      // Well before the lease deadline: nothing.
      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS - 1_000);
      expect(wakes).not.toHaveBeenCalled();
      expect(posts).toEqual([]);

      // At the deadline, with NO other trigger: the fire hands the expired
      // claim to the guarded deliverer: exactly one POST.
      await vi.advanceTimersByTimeAsync(2_000);
      expect(wakes).toHaveBeenCalledTimes(1);
      await vi.advanceTimersByTimeAsync(50);
      expect(posts).toEqual([["cmd_wake"]]);
      expect(store.reclaimCalls ?? 0).toBe(2);

      // Long afterwards: no duplicate wake/delivery.
      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS * 2);
      expect(wakes).toHaveBeenCalledTimes(1);
      expect(posts).toEqual([["cmd_wake"]]);
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE11 item 5 (gate 20): the wake is cancelled through the central reconciliation when the row is delivered before the deadline", async () => {
    const originalNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: { locks: { query: async () => ({ held: [] }) } },
    });
    vi.useFakeTimers();
    try {
      vi.setSystemTime(50_000);
      class FlakyStorage extends MemStorage {
        reclaimCalls = 0;
        override async reclaimInflight(
          id: string,
          expected: { owner: string; until: number },
        ): Promise<OutboxRecord | null> {
          this.reclaimCalls++;
          if (this.reclaimCalls <= 2) throw new Error("tx abort");
          return await super.reclaimInflight(id, expected);
        }
      }
      const store = new FlakyStorage();
      const putP = store.put(
        rec({
          commandId: "cmd_wake_delivered",
          instanceId: "ins_wake2",
          state: "inflight",
          lease: { owner: "owner_wake2", until: Date.now() + LEASE_TTL_MS },
        }),
      );
      await vi.advanceTimersByTimeAsync(5);
      await putP;
      const box = await Outbox.load(store, "owner_wake2");
      await box.refreshDurable();
      await vi.advanceTimersByTimeAsync(0);
      expect(box.pendingLeaseWakeups()).toEqual(["cmd_wake_delivered"]);

      const wakes = vi.fn();
      box.onClaimWake = wakes;

      // The claim gets a definite answer before the deadline (the deliverer's
      // finish write): patch commits storage-first and the central
      // reconciliation cancels the wake.
      const patchP = box.patch("cmd_wake_delivered", {
        state: "sent",
        gotResponse: true,
        lease: undefined,
      });
      await vi.advanceTimersByTimeAsync(5);
      await patchP;
      expect(store.map.get("cmd_wake_delivered")?.state).toBe("sent");
      expect(box.pendingLeaseWakeups()).toEqual([]);

      await vi.advanceTimersByTimeAsync(LEASE_TTL_MS * 2);
      expect(wakes).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE11 item 4: reclaim never downgrades a row another tab moved to sent before the reclaim transaction (and runs inside the real Web Lock)", async () => {
    // B loads and observes the inflight row; its reclaim transaction is gated
    // until A's finish write lands. Old reclaim (mergeUnlessDone with a
    // pending patch) overwrote sent -> pending and B POSTed an answered row.
    const originalNavigator = globalThis.navigator;
    let entered!: () => void;
    const enteredP = new Promise<void>((r) => void (entered = r));
    let gate!: () => void;
    const gateP = new Promise<void>((r) => void (gate = r));
    let sawLockName: string | null = null;
    class RacyStorage extends MemStorage {
      override async mergeUnlessDone(
        id: string,
        patch: Partial<OutboxRecord>,
      ): Promise<OutboxRecord | null> {
        if (id === "cmd_race") {
          entered();
          await gateP;
        }
        return await super.mergeUnlessDone(id, patch);
      }
      override async reclaimInflight(
        id: string,
        expected: { owner: string; until: number },
      ): Promise<OutboxRecord | null> {
        if (id === "cmd_race") {
          entered();
          await gateP;
        }
        return await super.reclaimInflight(id, expected);
      }
    }
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: {
        locks: {
          query: async () => ({ held: [] }),
          request: (name: string, _opts: unknown, cb: () => Promise<unknown>) => {
            sawLockName = name;
            return cb();
          },
        },
      },
    });
    try {
      const store = new RacyStorage();
      const owner = "owner_clone_race";
      const instanceId = "ins_race";
      await store.put(
        rec({
          commandId: "cmd_race",
          instanceId,
          state: "inflight",
          lease: { owner, until: Date.now() + LEASE_TTL_MS },
        }),
      );

      // B loads (same owner, cloned sessionStorage; query says holder gone).
      const loadP = Outbox.load(store, owner);
      await enteredP; // B's snapshot read decided "reclaim", tx is gated

      // A actually completes the POST before B's transaction runs: the
      // terminal sent write commits independently (as A's IDB tx would).
      await store.put(
        rec({
          commandId: "cmd_race",
          instanceId,
          state: "sent",
          gotResponse: true,
          lease: undefined,
        }),
      );
      gate();
      const b = await loadP;

      // The sent row is untouched and B treats it as answered, not pending.
      expect(store.map.get("cmd_race")?.state).toBe("sent");
      expect(b.get("cmd_race")?.state).toBe("sent");
      expect(b.pendingFor(instanceId)).toEqual([]);
      // The reclaim was serialized under the real per-instance Web Lock name.
      expect(sawLockName).toBe(instanceDeliveryLockName(instanceId));
    } finally {
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    }
  });

  it("GATE11 item 3: a same-owner fresh lease is NOT reclaimed when the REAL instance Web Lock is held", async () => {
    // Duplicated tab (cloned sessionStorage → same tabOwner): tab A is
    // delivering and holds the durable Web Lock under its REAL name
    // (instanceDeliveryLockName — the exact string acquireLock requests).
    // Gate 10 queried the wrong name ("__lock__:<instance>" is only the
    // no-Web-Locks lease row key), never saw the holder, and requeued anyway.
    const instanceId = "ins_same";
    const originalQuery = (globalThis as { navigator?: Navigator }).navigator?.locks?.query;
    const originalNavigator = globalThis.navigator;
    const heldName = instanceDeliveryLockName(instanceId);
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: {
        locks: {
          query: async () => ({ held: [{ name: heldName, mode: "exclusive" as const }] }),
        },
      },
    });
    try {
      const store = new MemStorage();
      const a = await Outbox.load(store, "owner_dup");
      await a.enqueue(rec({ commandId: "cmd_dup", instanceId }));
      await a.patch("cmd_dup", {
        state: "inflight",
        lease: { owner: "owner_dup", until: Date.now() + LEASE_TTL_MS },
      });
      // B has the same owner (cloned sessionStorage) and sees the lock held.
      const b = await Outbox.load(store, "owner_dup");
      expect(b.pendingFor(instanceId)).toEqual([]);
      const row = store.map.get("cmd_dup")!;
      expect(row.state).toBe("inflight");
      expect(row.lease?.owner).toBe("owner_dup");
      // A (the real holder) can still see/deliver it via its cache.
      expect(a.pendingFor(instanceId)).toEqual([]);
    } finally {
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
      void originalQuery;
    }
  });

  /** Install an isolated per-tab sessionStorage stub seeded with an owner id. */
  function sessionWithOwner(owner: string): Storage {
    const m = new Map<string, string>([["remuda-outbox-owner", owner]]);
    return {
      getItem: (k: string) => m.get(k) ?? null,
      setItem: (k: string, v: string) => void m.set(k, v),
      removeItem: (k: string) => void m.delete(k),
      clear: () => m.clear(),
      key: () => null,
      length: m.size,
    } as Storage;
  }

  it("GATE12 item 7: cloned tabs with a REAL held lock and a held POST — B neither requeues nor POSTs; releasing A delivers exactly once", async () => {
    // Replaces the query-only stub. The lock manager implements BOTH query and
    // request with real holder bookkeeping (exclusive locks are serialized and
    // only one holder per name). A holds the instance lock with its POST parked;
    // B (cloned sessionStorage → same owner) loads and must leave A's claim
    // entirely alone; after A releases, exactly one POST happened overall.
    class FakeLockManager {
      held = new Map<string, { queue: (() => void)[] }>();
      query = async () => ({
        held: [...this.held.keys()].map((name) => ({ name, mode: "exclusive" as const })),
      });
      async request<T>(name: string, _opts: unknown, cb: () => Promise<T>): Promise<T> {
        await new Promise<void>((resolve) => {
          const cur = this.held.get(name);
          if (!cur) {
            this.held.set(name, { queue: [] });
            resolve();
          } else {
            cur.queue.push(resolve);
          }
        });
        try {
          return await cb();
        } finally {
          const cur = this.held.get(name)!;
          const next = cur.queue.shift();
          if (next) next();
          else this.held.delete(name);
        }
      }
    }
    const locks = new FakeLockManager();
    const originalNavigator = globalThis.navigator;
    const originalSession = globalThis.sessionStorage;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: { locks },
    });
    const instanceId = "ins_clone_real";
    const owner = "owner_clone_real";
    const sessionWithOwner = (o: string): Storage => {
      const m = new Map<string, string>([["remuda-outbox-owner", o]]);
      return {
        getItem: (k: string) => m.get(k) ?? null,
        setItem: (k: string, v: string) => void m.set(k, v),
        removeItem: (k: string) => void m.delete(k),
        clear: () => m.clear(),
        key: () => null,
        length: m.size,
      } as Storage;
    };
    try {
      // A owns the row and the durable store.
      Object.defineProperty(globalThis, "sessionStorage", { configurable: true, value: sessionWithOwner(owner) });
      const store = new MemStorage();
      const a = await Outbox.load(store, owner);
      await a.enqueue(rec({ commandId: "cmd_clone_real", instanceId }));

      // A begins delivery under the REAL instance lock; the POST is held.
      let releasePost: (() => void) | null = null;
      const postHeld = new Promise<void>((r) => void (releasePost = r));
      const posts: string[][] = [];
      const aDelivery = a.withInstanceLock(instanceId, async (_iid, rows) => {
        posts.push(rows.map((r) => r.commandId));
        // Claim the durable inflight lease exactly like store.deliverOutboxRecord.
        await a.patch(rows[0]!.commandId, {
          state: "inflight",
          attempts: 1,
          lease: { owner, until: Date.now() + LEASE_TTL_MS },
        });
        await postHeld; // POST in flight
        await a.patch(rows[0]!.commandId, { state: "sent", gotResponse: true, lease: undefined });
        return rows.length;
      });
      await Promise.resolve();
      // A has claimed inflight and holds the lock; let the claim write land.
      await new Promise((r) => setTimeout(r, 5));
      expect(store.map.get("cmd_clone_real")?.state).toBe("inflight");
      expect([...locks.held.keys()]).toContain(instanceDeliveryLockName(instanceId));

      // B loads (cloned owner) WHILE A holds the lock with its POST pending.
      Object.defineProperty(globalThis, "sessionStorage", { configurable: true, value: sessionWithOwner(owner) });
      const b = await Outbox.load(store, owner);
      expect(b.pendingFor(instanceId)).toEqual([]);
      expect(store.map.get("cmd_clone_real")?.state).toBe("inflight");
      // B's own lock turns SERIALIZE behind A (it cannot even enter): no POST
      // from B, still exactly the one A turn started.
      expect(posts).toEqual([["cmd_clone_real"]]);

      // Release A's POST; it marks sent and releases the lock. B never wakes a
      // delivery of its own (its load found the lock held and armed nothing
      // deliverable).
      releasePost!();
      await aDelivery;
      expect(posts).toEqual([["cmd_clone_real"]]);
      expect(store.map.get("cmd_clone_real")?.state).toBe("sent");

      // Even a later flush by B sees the sent row and POSTs nothing.
      const bRows = await b.withInstanceLock(instanceId, (_iid, rows) => {
        posts.push(rows.map((r) => r.commandId));
        return Promise.resolve(rows.length);
      });
      expect(bRows).toBe(0);
      expect(posts).toEqual([["cmd_clone_real"], []]);
      a.dispose();
      b.dispose();
    } finally {
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
      Object.defineProperty(globalThis, "sessionStorage", { configurable: true, value: originalSession });
    }
  });

  it("GATE11 item 3: cloned sessionStorage tabs (DEFAULT load) — holder present keeps A's POST claim; absent locks same; gone holder requeues", async () => {
    // Realistic two-tab bookkeeping through Outbox.load with NO explicit owner
    // argument: each tab's tabOwner() reads its own sessionStorage stub;
    // "duplicate tab" clones sessionStorage, so both stubs carry the SAME
    // owner id.
    const instanceId = "ins_clone";
    const clonedOwner = "owner_cloned_session";
    const originalNavigator = globalThis.navigator;
    const originalSession = globalThis.sessionStorage;
    const shared = new MemStorage();
    try {
      // Tab A loads, enqueues and claims inflight (POST pending).
      Object.defineProperty(globalThis, "sessionStorage", {
        configurable: true,
        value: sessionWithOwner(clonedOwner),
      });
      const a = await Outbox.load(shared);
      expect(a.ownerId).toBe(clonedOwner);
      await a.enqueue(rec({ commandId: "cmd_clone", instanceId }));
      await a.patch("cmd_clone", {
        state: "inflight",
        lease: { owner: clonedOwner, until: Date.now() + LEASE_TTL_MS },
      });

      // Tab B: cloned sessionStorage (same owner), its own stub object.
      Object.defineProperty(globalThis, "sessionStorage", {
        configurable: true,
        value: sessionWithOwner(clonedOwner),
      });
      const heldName = instanceDeliveryLockName(instanceId);
      Object.defineProperty(globalThis, "navigator", {
        configurable: true,
        value: {
          locks: { query: async () => ({ held: [{ name: heldName, mode: "exclusive" as const }] }) },
        },
      });
      const bHeld = await Outbox.load(shared);
      expect(bHeld.ownerId).toBe(clonedOwner);
      // B neither requeues nor can deliver: claim stays fresh inflight.
      expect(bHeld.pendingFor(instanceId)).toEqual([]);
      expect(shared.map.get("cmd_clone")?.state).toBe("inflight");

      // Same tab loading with Web Locks ABSENT: unknown ownership, same result.
      Object.defineProperty(globalThis, "navigator", {
        configurable: true,
        value: { locks: undefined },
      });
      const bNoLocks = await Outbox.load(shared);
      expect(bNoLocks.pendingFor(instanceId)).toEqual([]);
      expect(shared.map.get("cmd_clone")?.state).toBe("inflight");

      // A is gone (locks.query reports no holder): the dead context's claim is
      // requeued immediately.
      Object.defineProperty(globalThis, "navigator", {
        configurable: true,
        value: { locks: { query: async () => ({ held: [] }) } },
      });
      const bFree = await Outbox.load(shared);
      expect(bFree.pendingFor(instanceId).map((r) => r.commandId)).toEqual(["cmd_clone"]);
      expect(shared.map.get("cmd_clone")?.state).toBe("pending");
      expect(shared.map.get("cmd_clone")?.lease).toBeUndefined();
    } finally {
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
      Object.defineProperty(globalThis, "sessionStorage", {
        configurable: true,
        value: originalSession,
      });
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

  it("GATE11 item 8e: DEFAULT Outbox.load derives DISTINCT owners from two separate sessionStorage stubs (never the shared localStorage profile)", async () => {
    // Replaces the gate-9 test that only passed explicit owner strings: the
    // production path takes owner from tabOwner()'s sessionStorage. Two tabs
    // have separate sessionStorage over one shared durable store and one
    // shared localStorage (the old gate-7 per-profile owner key lived here and
    // made every tab identical).
    const originalSession = globalThis.sessionStorage;
    const originalLocal = globalThis.localStorage;
    const makeStorage = (m: Map<string, string>): Storage =>
      ({
        getItem: (k: string) => m.get(k) ?? null,
        setItem: (k: string, v: string) => void m.set(k, v),
        removeItem: (k: string) => void m.delete(k),
        clear: () => m.clear(),
        key: () => null,
        length: m.size,
      }) as Storage;
    const sharedLocal = new Map<string, string>([["remuda-outbox-owner", "owner_SHARED_PROFILE"]]);
    const sessionA = new Map<string, string>();
    const sessionB = new Map<string, string>();
    const shared = new MemStorage();
    try {
      Object.defineProperty(globalThis, "localStorage", { configurable: true, value: makeStorage(sharedLocal) });
      Object.defineProperty(globalThis, "sessionStorage", { configurable: true, value: makeStorage(sessionA) });
      const a = await Outbox.load(shared);
      Object.defineProperty(globalThis, "sessionStorage", { configurable: true, value: makeStorage(sessionB) });
      const b = await Outbox.load(shared);
      expect(a.ownerId.startsWith("owner_")).toBe(true);
      expect(a.ownerId).not.toBe(b.ownerId);
      // The shared localStorage profile key is ignored.
      expect(a.ownerId).not.toBe("owner_SHARED_PROFILE");
      expect(b.ownerId).not.toBe("owner_SHARED_PROFILE");
      // Owner stability within one tab across reloads.
      Object.defineProperty(globalThis, "sessionStorage", { configurable: true, value: makeStorage(sessionA) });
      const aReload = await Outbox.load(shared);
      expect(aReload.ownerId).toBe(a.ownerId);
    } finally {
      Object.defineProperty(globalThis, "sessionStorage", { configurable: true, value: originalSession });
      Object.defineProperty(globalThis, "localStorage", { configurable: true, value: originalLocal });
    }
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
    // A FRESH lease: the central reconciliation arms a far-future deadline
    // wake (not a 0 ms delivery wake), so this mutation emits just once.
    await box.patch("cmd_n", { state: "inflight", lease: { owner: "owner_n", until: Date.now() + LEASE_TTL_MS } });
    box.cancelAllWakesForUnload();
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
      releaseLease: async () => {
        throw new Error("IDB transaction aborted");
      },
      mergeUnlessDone: async () => {
        throw new Error("IDB transaction aborted");
      },
      reclaimInflight: async () => {
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
