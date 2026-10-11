import { afterEach, expect, it, vi } from "vitest";
import { openOutboxStorage, Outbox, type OutboxRecord, type OutboxStorage } from "./outbox";

/**
 * Minimal IndexedDB stand-in that can abort a transaction AFTER the object
 * store request already fired onsuccess. The real IDB request succeeds before
 * the transaction commits: a write that resolves on onsuccess would report a
 * durable row even though onabort rolled it back.
 */
class FakeOpenRequest<T> {
  result!: T;
  error: Error | null = null;
  onupgradeneeded: ((ev: unknown) => void) | null = null;
  onsuccess: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
}

class FakeRequest {
  result: unknown = undefined;
  error: Error | null = null;
  onsuccess: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
}

class FakeTransaction {
  oncomplete: ((ev: unknown) => void) | null = null;
  onabort: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  /** Resolved when the db grants this tx its serialized start. */
  readonly started: Promise<void>;
  /** Resolved when this transaction ends (committed or aborted). */
  readonly done: Promise<void>;
  private startedResolve!: () => void;
  private doneResolve!: () => void;
  /** Monotonic creation order (conflict precedence). */
  readonly createdAt: number;
  error: Error | null = null;
  readonly db: FakeDb;
  readonly mode: IDBTransactionMode;
  aborted = false;
  private settled = false;
  private pending = 0;
  private readonly onEnd: (tx: FakeTransaction) => void;

  constructor(
    db: FakeDb,
    mode: IDBTransactionMode,
    onEnd: (tx: FakeTransaction) => void,
    createdAt: number,
  ) {
    this.db = db;
    this.mode = mode;
    this.onEnd = onEnd;
    this.createdAt = createdAt;
    this.started = new Promise<void>((r) => {
      this.startedResolve = r;
    });
    this.done = new Promise<void>((r) => {
      this.doneResolve = r;
    });
  }

  grantStart() {
    this.startedResolve();
  }

  objectStore(_name: string) {
    return new FakeObjectStore(this, this.db);
  }

  beginRequest() {
    this.pending += 1;
  }

  /** A request reached its terminal microtask; auto-commit when all settle. */
  requestSettled() {
    this.pending -= 1;
    if (this.pending !== 0) return;
    if (!this.settled && !this.aborted) {
      this.settled = true;
      queueMicrotask(() => this.oncomplete?.({ target: this }));
    }
    // All requests settled and the tx ended (committed or aborted) — release
    // the db serialization slot so a queued transaction can start.
    this.onEnd(this);
    this.doneResolve();
  }

  abort = () => {
    if (this.aborted || this.settled) return;
    this.aborted = true;
    this.db.abortCount += 1;
    queueMicrotask(() => this.onabort?.({ target: this }));
  };
}

class FakeObjectStore {
  private readonly tx: FakeTransaction;
  private readonly db: FakeDb;

  constructor(tx: FakeTransaction, db: FakeDb) {
    this.tx = tx;
    this.db = db;
  }

  private finish(req: FakeRequest, prepare: () => void, commit: () => void, opts?: { gateCommit?: boolean }) {
    this.tx.beginRequest();
    // Requests only run once the transaction is granted its serialized start.
    void this.tx.started.then(() => {
      // The request completes WITH its result on an earlier turn than the
      // transaction outcome — exactly the window the write must not trust.
      queueMicrotask(() => {
        prepare();
        try {
          req.onsuccess?.({ target: req });
        } catch (err) {
          // Real IndexedDB aborts the transaction when an event handler throws.
          queueMicrotask(() => {
            this.tx.error = err instanceof Error ? err : new Error(String(err));
            this.tx.abort();
            this.tx.requestSettled();
          });
          return;
        }
        queueMicrotask(async () => {
          try {
            if (this.tx.aborted) return;
            if (this.db.abortNextAfterRequest) {
              this.db.abortNextAfterRequest = false;
              this.db.requestSucceededBeforeAbort = true;
              this.tx.abort();
              return;
            }
            // WRITE commits wait on the disposal gate (reads never do) so a
            // transaction can be suspended post-request, pre-commit.
            if (opts?.gateCommit && this.db.commitGate) {
              this.db.gatedCommitEntered?.(req.result as string);
              await this.db.commitGate;
              if (this.tx.aborted) return;
            }
            commit();
          } finally {
            this.tx.requestSettled();
          }
        });
      });
    });
  }

  put(rec: OutboxRecord) {
    const req = new FakeRequest();
    req.result = rec.commandId;
    this.db.requestsIssued.push("put");
    this.finish(req, () => {}, () => this.db.rows.set(rec.commandId, rec), { gateCommit: true });
    return req as unknown as IDBRequest;
  }

  getAll() {
    const req = new FakeRequest();
    this.db.requestsIssued.push("getAll");
    this.finish(
      req,
      () => {
        req.result = [...this.db.rows.values()];
      },
      () => {},
    );
    return req as unknown as IDBRequest;
  }

  get(key: string) {
    const req = new FakeRequest();
    this.db.requestsIssued.push("get");
    this.finish(
      req,
      () => {
        req.result = this.db.rows.get(key) ?? null;
      },
      () => {},
    );
    return req as unknown as IDBRequest;
  }

  delete(key: string) {
    const req = new FakeRequest();
    this.db.requestsIssued.push("delete");
    this.finish(req, () => {}, () => this.db.rows.delete(key), { gateCommit: true });
    return req as unknown as IDBRequest;
  }
}

class FakeDb {
  readonly rows = new Map<string, OutboxRecord>();
  private readonly names = new Set<string>();
  /**
   * Creation-order conflict scheduling like real IndexedDB:
   *  - a READONLY transaction starts once the last earlier READWRITE has
   *    finished (it runs concurrently with other readers);
   *  - a READWRITE transaction starts only after ALL earlier transactions
   *    (readers and writers) have finished.
   * The old counter model started a queued writer when a READER ended even
   * though an earlier writer was still active (gate 16 item 3).
   */
  private outstanding = new Set<FakeTransaction>();
  private pendingStart: FakeTransaction[] = [];
  /** Monotonic transaction creation sequence. */
  private creationSeq = 0;
  /** Test switch: abort the NEXT transaction after its request succeeds. */
  abortNextAfterRequest = false;
  /** Recorded proof the request succeeded before the abort arrived. */
  requestSucceededBeforeAbort = false;
  /** Total abort transitions across all transactions (gate-14 item 1 proof). */
  abortCount = 0;
  /** When truthy (a promise), post-request WRITE commits wait for it (disposal window tests). */
  commitGate: Promise<void> | null = null;
  /** Resolves (with the command id) when a gated write reaches its suspended commit. */
  gatedCommitEntered: ((commandId: string) => void) | null = null;
  /** Every object-store request issued (get/put/getAll/delete), in order. */
  readonly requestsIssued: string[] = [];

  createObjectStore(name: string) {
    this.names.add(name);
    return {} as IDBObjectStore;
  }

  get objectStoreNames() {
    const names = this.names;
    return { contains: (n: string) => names.has(n) };
  }

  private enqueueTransactionStart(tx: FakeTransaction, mode: IDBTransactionMode) {
    // Compute the set of EARLIER transactions this one conflicts with.
    const prerequisites: FakeTransaction[] = [];
    if (mode === "readonly") {
      const lastWriter = [...this.outstanding].reverse().find((t) => t.mode === "readwrite");
      if (lastWriter) prerequisites.push(lastWriter);
    } else {
      prerequisites.push(...this.outstanding);
    }
    this.outstanding.add(tx);
    if (prerequisites.length === 0) {
      queueMicrotask(() => tx.grantStart());
      return;
    }
    this.pendingStart.push(tx);
    void Promise.all(prerequisites.map((t) => t.done)).then(() => {
      // Start only when still queued (it may have been pumped already — the
      // queue pump re-checks prerequisites each time any transaction ends).
      if (this.pendingStart.includes(tx)) {
        this.pumpPendingStarts();
      }
    });
  }

  private pumpPendingStarts() {
    // Walk the queue in creation order; start each tx whose conflicting
    // predecessors are done. A started writer itself blocks later tx, so stop
    // the walk after granting a writer.
    for (const tx of [...this.pendingStart]) {
      if (this.isBlocked(tx)) continue;
      this.pendingStart.splice(this.pendingStart.indexOf(tx), 1);
      tx.grantStart();
      if (tx.mode === "readwrite") break;
    }
  }

  /** True while an earlier outstanding transaction conflicts with `tx`. */
  private isBlocked(tx: FakeTransaction): boolean {
    const earlier = (t: FakeTransaction) => t.createdAt < tx.createdAt;
    if (tx.mode === "readonly") {
      return [...this.outstanding].some((t) => earlier(t) && t.mode === "readwrite");
    }
    return [...this.outstanding].some((t) => earlier(t));
  }

  endTransaction(tx: FakeTransaction) {
    this.outstanding.delete(tx);
    this.pumpPendingStarts();
  }

  transaction(name: string | string[], mode: IDBTransactionMode) {
    void name;
    const tx = new FakeTransaction(
      this,
      mode,
      (ended) => this.endTransaction(ended),
      this.creationSeq++,
    ) as unknown as FakeTransaction;
    this.enqueueTransactionStart(tx, mode);
    return tx as unknown as IDBTransaction;
  }
}

function installFakeIndexedDB() {
  const db = new FakeDb();
  const factory = {
    open: () => {
      const req = new FakeOpenRequest<FakeDb>();
      queueMicrotask(() => {
        req.result = db;
        req.onupgradeneeded?.({ target: req });
        queueMicrotask(() => req.onsuccess?.({ target: req }));
      });
      return req as unknown as IDBOpenDBRequest;
    },
  };
  (globalThis as { indexedDB?: IDBFactory }).indexedDB = factory as unknown as IDBFactory;
  return db;
}

function probeRecord(commandId: string): OutboxRecord {
  return {
    commandId,
    clientRequestId: `local_${commandId}`,
    instanceId: "ins_idb_abort",
    prompt: "hi",
    createdAt: 1_000,
    attempts: 0,
    state: "pending",
  };
}

afterEach(() => {
  // Restore the "no IndexedDB" environment every other test runs in.
  delete (globalThis as { indexedDB?: IDBFactory }).indexedDB;
});

it("a write whose request succeeds but whose transaction aborts afterwards is not durable and can be retried", async () => {
  const db = installFakeIndexedDB();
  const opened = await openOutboxStorage();
  expect(opened.degraded).toBe(false);
  const storage: OutboxStorage = opened.storage;

  // The probe row committed; now abort the next write AFTER request success.
  db.abortNextAfterRequest = true;
  await expect(storage.put(probeRecord("cmd_aborted_after_success"))).rejects.toThrow(
    /transaction aborted/,
  );

  // The request really did succeed first, and the abort rolled the write back.
  expect(db.requestSucceededBeforeAbort).toBe(true);
  expect(db.rows.has("cmd_aborted_after_success")).toBe(false);
  expect((await storage.all()).map((r) => r.commandId)).not.toContain("cmd_aborted_after_success");

  // A follow-up transaction in a new attempt commits normally.
  await storage.put(probeRecord("cmd_retry_ok"));
  expect((await storage.all()).map((r) => r.commandId)).toContain("cmd_retry_ok");
});

it("GATE16 item 2: a tracked same-owner claim that becomes FOREIGN hands off — the Outbox wake cancels and never triggers a flush", async () => {
  // One expiry scheduler per lease: Outbox owns same-owner inactive claims,
  // the store scheduler owns foreign ones. If the Outbox wake adopted the
  // foreign lease it would fire onClaimWake (a second, independent
  // flushAllOutbox with its own attempted set) at the deadline — the exact
  // path that re-POSTed a 'held' command before its retry delay.
  const install = installFakeIndexedDB();
  void install;
  const opened = await openOutboxStorage();
  const storage = opened.storage;
  const originalLocks = (globalThis.navigator as { locks?: LockManager }).locks;
  Object.defineProperty(globalThis.navigator, "locks", { value: undefined, configurable: true });
  vi.useFakeTimers();
  try {
    vi.setSystemTime(1_300_000);
    const owner = "owner_same_then_foreign";
    const foreign = "owner_other_document";
    const T = Date.now() + 30_000;
    await storage.put({
      ...probeRecord("cmd_handoff"),
      instanceId: "ins_hf",
      state: "inflight",
      lease: { owner, until: T },
    });
    const box = await Outbox.load(storage, owner);
    expect(box.pendingClaimWakeups()).toEqual(["cmd_handoff"]);
    const wakes = vi.fn();
    box.onClaimWake = wakes;

    // Before the deadline another document claims the same row with its OWN
    // lease at the same deadline (a renewal by a different recovery owner).
    const replaceP = storage.put({
      ...probeRecord("cmd_handoff"),
      instanceId: "ins_hf",
      state: "inflight",
      lease: { owner: foreign, until: T },
    });
    await vi.advanceTimersByTimeAsync(5);
    await replaceP;
    // Any refresh (the store refreshes inside every flush) must hand the wake
    // off rather than adopt the foreign lease.
    await box.refreshDurable();
    expect(box.pendingClaimWakeups()).toEqual([]);
    expect(box.get("cmd_handoff")?.lease?.owner).toBe(foreign);

    // Well past the deadline: no Outbox-driven delivery trigger — the store's
    // foreign-lease scheduler is the only one allowed to recover this row.
    await vi.advanceTimersByTimeAsync(60_000);
    await vi.advanceTimersByTimeAsync(100);
    expect(wakes).not.toHaveBeenCalled();
    expect(box.pendingClaimWakeups()).toEqual([]);
    box.dispose();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis.navigator, "locks", { value: originalLocks, configurable: true });
  }
});

it("GATE15 item 2: a same-owner inflight claim appearing AFTER load gets an expiry wake and delivers at the deadline; an active delivery is not woken", async () => {
  const db = installFakeIndexedDB();
  const opened = await openOutboxStorage();
  const storage = opened.storage;
  const originalLocks = (globalThis.navigator as { locks?: LockManager }).locks;
  Object.defineProperty(globalThis.navigator, "locks", { value: undefined, configurable: true });
  vi.useFakeTimers();
  try {
    vi.setSystemTime(1_200_000);
    const owner = "owner_late_claim";
    const box = await Outbox.load(storage, owner);
    const wakes = vi.fn();
    box.onClaimWake = wakes;

    // A cloned sibling document claims the row AFTER this Outbox loaded (same
    // persisted owner) and then crashes without finishing.
    const T = Date.now() + 30_000;
    const putP = storage.put({
      ...probeRecord("cmd_late_claim"),
      instanceId: "ins_lc",
      state: "inflight",
      lease: { owner, until: T },
    });
    await vi.advanceTimersByTimeAsync(5);
    await putP;
    expect(box.pendingClaimWakeups()).toEqual([]); // not at load time

    await box.refreshDurable(); // appears now
    expect(box.pendingClaimWakeups()).toEqual(["cmd_late_claim"]);

    // An active delivery of the same row suppresses its wake (this document
    // is POSTing it — it must not steal its own claim).
    box.beginActiveDelivery("cmd_late_claim");
    await box.refreshDurable();
    expect(box.pendingClaimWakeups()).toEqual([]);
    box.endActiveDelivery("cmd_late_claim");

    // Inactive again: the next refresh re-adopts the expiry recovery.
    await box.refreshDurable();
    expect(box.pendingClaimWakeups()).toEqual(["cmd_late_claim"]);

    // The sibling never finishes; at the deadline, with no other trigger, the
    // wake reclaims pending and fires delivery exactly once.
    await vi.advanceTimersByTimeAsync(30_000);
    await vi.advanceTimersByTimeAsync(10);
    expect(db.rows.get("cmd_late_claim")?.state).toBe("pending");
    expect(wakes).toHaveBeenCalledTimes(1);
    box.dispose();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis.navigator, "locks", { value: originalLocks, configurable: true });
  }
});

it("GATE16 item 3: a queued writer waits for ALL earlier transactions — reader and writer — and starts in creation order", async () => {
  // Real IndexedDB conflict order: writer1, reader (getAll), writer2. While
  // writer1's commit is held open the reader and writer2 BOTH queue (the
  // reader does NOT run, and writer2 must not run). When writer1 ends the
  // reader goes first; writer2 starts only after the reader also ends. The
  // old counter fake released a queued writer on a reader's end while an
  // earlier writer was still active.
  const db = installFakeIndexedDB();
  const opened = await openOutboxStorage();
  const storage = opened.storage;

  let releaseW1: (() => void) | null = null;
  db.commitGate = new Promise<void>((r) => void (releaseW1 = r));
  const w1 = storage.put(probeRecord("cmd_w1"));
  const order: string[] = [];
  // Reader queues behind writer1.
  const reader = storage.all().then((rows) => {
    order.push("reader");
    return rows;
  });
  // writer2 queues behind writer1 AND the reader.
  const w2 = storage.put(probeRecord("cmd_w2")).then(() => order.push("writer2"));
  await Promise.resolve();
  await Promise.resolve();
  // Neither queued tx ran while writer1 is open.
  expect(db.rows.get("cmd_w2")).toBeUndefined();
  expect(order).toEqual([]);

  releaseW1!();
  await w1;
  // Reader is ahead of writer2 in creation order and is a prerequisite of it:
  // the read settles before writer2's commit.
  await reader;
  expect(order).toEqual(["reader"]);
  await Promise.resolve();
  expect(order).toEqual(["reader"]); // writer2 not yet
  await w2;
  expect(order).toEqual(["reader", "writer2"]);
});

it("GATE15 item 1: releaseLease deletes only the matching owner's mutex row", async () => {
  installFakeIndexedDB();
  const opened = await openOutboxStorage();
  const storage = opened.storage;
  const now = Date.now();
  const key = "__lock__:ins_rel";
  const rec = (token: string): OutboxRecord => ({
    commandId: key,
    clientRequestId: key,
    instanceId: "ins_rel",
    prompt: "",
    createdAt: now,
    attempts: 0,
    state: "inflight",
    lease: { owner: token, until: now + 30_000 },
  });
  expect(await storage.acquireLease(key, rec("tok_A"), now)).toBe(true);
  // A different token cannot erase A's live lock.
  expect(await storage.releaseLease(key, "tok_B")).toBe(false);
  expect((await storage.all()).some((r) => r.commandId === key)).toBe(true);
  // The real owner releases.
  expect(await storage.releaseLease(key, "tok_A")).toBe(true);
  expect((await storage.all()).some((r) => r.commandId === key)).toBe(false);
  // Releasing an absent row is a no-op (false), not an error.
  expect(await storage.releaseLease(key, "tok_A")).toBe(false);
});

it("GATE14 item 1: Outbox.dispose during a production IDB reclaim (requests issued, commit pending) aborts the tx — durable row stays inflight", async () => {
  // Exercises the REAL IndexedDB backend, not the localStorage mock used by
  // the gate-12 test: the reclaim has issued its get/put requests but the
  // transaction commit is held open; dispose fires the lifetime signal; the
  // production abort listener must roll the transaction back BEFORE commit.
  const db = installFakeIndexedDB();
  const originalLocks = (globalThis.navigator as { locks?: LockManager }).locks;
  Object.defineProperty(globalThis.navigator, "locks", { value: undefined, configurable: true });
  vi.useFakeTimers();
  try {
    vi.setSystemTime(1_100_000);
    const opened = await openOutboxStorage();
    const storage = opened.storage;
    const owner = "owner_idb_dispose";
    const T = Date.now() + 30_000;
    // Live inactive claim armed for the deadline.
    await storage.put({
      ...probeRecord("cmd_idb_dispose_wake"),
      state: "inflight",
      lease: { owner, until: T },
    });
    const box = await Outbox.load(storage, owner);
    expect(box.pendingClaimWakeups()).toEqual(["cmd_idb_dispose_wake"]);
    const wakes = vi.fn();
    box.onClaimWake = wakes;

    // Deadline fires: the wake reads and enters the reclaim transaction. Hold
    // its COMMIT open (post get+put, pre oncomplete) and wait SPECIFICALLY for
    // the target row's reclaim write to enter the gate — not an unrelated
    // probe/seed write (gate 15 item 4 tightens the old '>= 2 puts' check).
    let releaseCommit: (() => void) | null = null;
    let gateEntered = false;
    db.commitGate = new Promise<void>((r) => void (releaseCommit = r));
    db.gatedCommitEntered = (commandId: string) => {
      if (commandId === "cmd_idb_dispose_wake") gateEntered = true;
    };
    await vi.advanceTimersByTimeAsync(30_000);
    for (let i = 0; i < 50 && !gateEntered; i += 1) {
      await Promise.resolve();
    }
    expect(gateEntered).toBe(true);

    // Exactly the target reclaim transaction is suspended.
    const abortsBeforeDispose = db.abortCount;
    // Teardown while THAT transaction is uncommitted.
    box.dispose();
    for (let i = 0; i < 20 && db.abortCount === abortsBeforeDispose; i += 1) {
      await Promise.resolve(); // signal abort -> t.abort -> onabort settled
    }
    // The reclaim transaction itself fired an abort event.
    expect(db.abortCount).toBe(abortsBeforeDispose + 1);
    // Release the held commit: the aborted transaction must NOT commit.
    releaseCommit!();
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(60_000);

    // The durable row is unchanged (no rolled-back pending write), no delivery
    // trigger, no wake left behind.
    expect(db.rows.get("cmd_idb_dispose_wake")?.state).toBe("inflight");
    expect(db.rows.get("cmd_idb_dispose_wake")?.lease?.until).toBe(T);
    expect(box.get("cmd_idb_dispose_wake")?.state).toBe("inflight");
    expect(wakes).not.toHaveBeenCalled();
    expect(box.pendingClaimWakeups()).toEqual([]);
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis.navigator, "locks", {
      value: originalLocks,
      configurable: true,
    });
  }
});

it("GATE14 item 3: every reclaim removes its lifetime abort listener on every terminal path (no listener accumulation)", async () => {
  const db = installFakeIndexedDB();
  const opened = await openOutboxStorage();
  const storage = opened.storage;
  const controller = new AbortController();
  const adds = vi.spyOn(controller.signal, "addEventListener");
  const removes = vi.spyOn(controller.signal, "removeEventListener");

  // Several SUCCESSFUL reclaim transactions (expired matching leases).
  const expiredLease = { owner: "owner_l", until: Date.now() - 60_000 };
  for (let i = 0; i < 5; i += 1) {
    const id = `cmd_listener_ok_${i}`;
    await storage.put({ ...probeRecord(id), state: "inflight", lease: expiredLease });
    const got = await storage.reclaimInflight(id, expiredLease, {
      requireExpired: true,
      signal: controller.signal,
    });
    expect(got?.state).toBe("pending");
  }
  // A non-matching terminal row: resolves on complete, listener must detach.
  await storage.put({ ...probeRecord("cmd_listener_sent"), state: "sent", gotResponse: true });
  const sent = await storage.reclaimInflight("cmd_listener_sent", expiredLease, {
    requireExpired: true,
    signal: controller.signal,
  });
  expect(sent?.state).toBe("sent");

  // An ABORTED reclaim (commit held open until the signal fires): listener
  // must detach on the abort terminal path too.
  await storage.put({
    ...probeRecord("cmd_listener_abort"),
    state: "inflight",
    lease: expiredLease,
  });
  let releaseGate: (() => void) | null = null;
  db.commitGate = new Promise<void>((r) => void (releaseGate = r));
  const abortP = storage.reclaimInflight("cmd_listener_abort", expiredLease, {
    requireExpired: true,
    signal: controller.signal,
  });
  await Promise.resolve(); // get onsuccess -> put issued, commit held open
  await Promise.resolve(); // put onsuccess fired
  expect(db.requestsIssued.filter((m) => m === "put").length).toBeGreaterThan(0);
  controller.abort();
  await expect(abortP).rejects.toThrow(/abort/);
  releaseGate!();
  db.commitGate = null;

  // Every registered abort handler reference was removed (detach is
  // idempotent — the fake fires oncomplete once per request, real IDB once
  // per tx — so compare handler identities, not raw call counts).
  const added = adds.mock.calls.filter((c) => c[0] === "abort").map((c) => c[1]);
  const removed = new Set(removes.mock.calls.filter((c) => c[0] === "abort").map((c) => c[1]));
  expect(added).toHaveLength(7);
  for (const fn of added) expect(removed.has(fn)).toBe(true);
});

it("GATE14 item 2: a requireExpired reclaim whose row completed to sent (lease undefined) returns the terminal row instead of throwing", async () => {
  const db = installFakeIndexedDB();
  const opened = await openOutboxStorage();
  const storage = opened.storage;
  const until = Date.now() - 1_000;
  // The wake read an inflight claim with this lease; between that read and the
  // conditional reclaim the delivery completed to sent with NO lease.
  await storage.put({ ...probeRecord("cmd_terminal"), state: "sent", gotResponse: true });
  // Must return the authoritative sent row, not reject on lease!.until.
  const got = await storage.reclaimInflight(
    "cmd_terminal",
    { owner: "owner_term", until },
    { requireExpired: true },
  );
  expect(got?.state).toBe("sent");
  expect(db.rows.get("cmd_terminal")?.state).toBe("sent");
});

it("Outbox.enqueue rejects and keeps the row out of its cache on a post-success abort, then succeeds on retry", async () => {
  const db = installFakeIndexedDB();
  const opened = await openOutboxStorage();
  const box = await Outbox.load(opened.storage, "owner_idb_abort");

  db.abortNextAfterRequest = true;
  await expect(box.enqueue(probeRecord("cmd_outbox_abort"))).rejects.toThrow(/transaction aborted/);
  // Storage-first: nothing is cached (no phantom bubble / no POST candidate).
  expect(box.get("cmd_outbox_abort")).toBeUndefined();
  expect(box.pendingCount()).toBe(0);

  await box.enqueue(probeRecord("cmd_outbox_abort"));
  expect(box.get("cmd_outbox_abort")?.state).toBe("pending");
  expect(box.pendingCount()).toBe(1);
});
