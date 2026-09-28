import { afterEach, expect, it, vi } from "vitest";
import { openOutboxStorage, Outbox, type OutboxRecord, type OutboxStorage } from "./outbox";

/**
 * c-reconnfu round 2 item 2: a terminal `done` must survive a downgrade patch
 * that races it at the IndexedDB transaction level. This fake models the one
 * IDB guarantee the fix relies on: readwrite transactions on one object store
 * run strictly in CREATION order — a later transaction's requests do not run
 * until the earlier transaction COMMITS. A transaction can additionally be
 * held open (its requests finished, its commit delayed) to reproduce a
 * journal-callback write in flight while a GET verdict resolves.
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

class FakeObjectStore {
  private readonly tx: FakeTransaction;
  private readonly db: SerialFakeDb;

  constructor(tx: FakeTransaction, db: SerialFakeDb) {
    this.tx = tx;
    this.db = db;
  }

  get(key: string) {
    const req = new FakeRequest();
    this.tx.enqueue(() => {
      req.result = this.db.rows.get(key) ?? null;
      req.onsuccess?.({ target: req });
    });
    return req as unknown as IDBRequest;
  }

  getAll() {
    const req = new FakeRequest();
    this.tx.enqueue(() => {
      req.result = [...this.db.rows.values()];
      req.onsuccess?.({ target: req });
    });
    return req as unknown as IDBRequest;
  }

  put(rec: OutboxRecord) {
    const req = new FakeRequest();
    req.result = rec.commandId;
    this.tx.enqueue(() => {
      this.db.putCalls.push(rec);
      this.db.rows.set(rec.commandId, rec);
      req.onsuccess?.({ target: req });
    });
    return req as unknown as IDBRequest;
  }

  delete(key: string) {
    const req = new FakeRequest();
    this.tx.enqueue(() => {
      this.db.rows.delete(key);
      req.onsuccess?.({ target: req });
    });
    return req as unknown as IDBRequest;
  }
}

class FakeTransaction {
  oncomplete: ((ev: unknown) => void) | null = null;
  onabort: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  private tasks: (() => void)[] = [];
  private scheduled = false;
  private readonly db: SerialFakeDb;
  readonly mode: IDBTransactionMode;

  constructor(db: SerialFakeDb, mode: IDBTransactionMode) {
    this.db = db;
    this.mode = mode;
  }

  objectStore(_name: string) {
    return new FakeObjectStore(this, this.db);
  }

  enqueue(task: () => void) {
    this.tasks.push(task);
    if (this.mode !== "readwrite" || !this.scheduled) {
      if (this.mode === "readwrite") {
        this.scheduled = true;
        this.db.queueReadwrite(this.drain);
      } else {
        queueMicrotask(() => void this.drain());
      }
    }
  }

  private drain = async () => {
    while (this.tasks.length) {
      const task = this.tasks.shift()!;
      task();
      // Let the request's onsuccess queue follow-up requests (e.g. get→put).
      await Promise.resolve();
    }
    // A held commit waits here; later readwrite transactions queue behind it.
    for (const gate of this.db.takeCommitHolds()) await gate;
    this.oncomplete?.({ target: this });
  };
}

class SerialFakeDb {
  readonly rows = new Map<string, OutboxRecord>();
  /** Every put REQUEST issued (preserved-done issues none). */
  readonly putCalls: OutboxRecord[] = [];
  private chain: Promise<void> = Promise.resolve();
  private commitHolds: (() => Promise<void>)[] = [];

  createObjectStore() {
    return {} as IDBObjectStore;
  }

  get objectStoreNames() {
    return { contains: () => true };
  }

  transaction(_name: string | string[], mode: IDBTransactionMode) {
    return new FakeTransaction(this, mode) as unknown as IDBTransaction;
  }

  queueReadwrite(drain: () => Promise<void>) {
    this.chain = this.chain.then(drain, drain);
  }

  /** Hold every readwrite commit until release(): models an open tx. */
  holdCommit(): { release: () => void } {
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    this.commitHolds.push(() => gate);
    return { release };
  }

  takeCommitHolds() {
    const holds = this.commitHolds;
    this.commitHolds = [];
    return holds.map((fn) => fn());
  }
}

function installFakeIndexedDB() {
  const db = new SerialFakeDb();
  (globalThis as { indexedDB?: IDBFactory }).indexedDB = {
    open: () => {
      const req = new FakeOpenRequest<SerialFakeDb>();
      queueMicrotask(() => {
        req.result = db;
        req.onupgradeneeded?.({ target: req });
        queueMicrotask(() => req.onsuccess?.({ target: req }));
      });
      return req as unknown as IDBOpenDBRequest;
    },
  } as unknown as IDBFactory;
  return db;
}

function rec(commandId: string, state: OutboxRecord["state"] = "pending"): OutboxRecord {
  return {
    commandId,
    clientRequestId: `local_${commandId}`,
    instanceId: "ins_done_guard",
    prompt: "hi",
    createdAt: 1_000,
    attempts: 0,
    state,
  };
}

let storage: OutboxStorage;

afterEach(() => {
  delete (globalThis as { indexedDB?: IDBFactory }).indexedDB;
});

it("a downgrade patch racing an in-flight (uncommitted) done transaction preserves done", async () => {
  const db = installFakeIndexedDB();
  storage = (await openOutboxStorage()).storage;
  const box = await Outbox.load(storage, "owner_done_race");
  await box.enqueue(rec("cmd_done_race"));
  await box.patch("cmd_done_race", { state: "inflight" });

  // Journal callback writes done; its transaction is held OPEN (commit not
  // fired) — exactly when the lost-response GET resolves underneath it.
  const hold = db.holdCommit();
  const doneP = box.patch("cmd_done_race", { state: "done", serverState: "journal-settled" });
  // The done put REQUEST already executed inside its (uncommitted) tx.
  await vi.waitFor(() => expect(db.putCalls.some((r) => r.state === "done")).toBe(true));

  // The GET verdict (sent) lands concurrently: its tx queues behind the done
  // tx and MUST read the committed done instead of the stale inflight cache.
  const downgradeP = box.patch("cmd_done_race", { state: "sent", gotResponse: true });
  hold.release();

  expect((await doneP)?.state).toBe("done");
  expect((await downgradeP)?.state).toBe("done");
  const stored = await storage.all();
  expect(stored.find((r) => r.commandId === "cmd_done_race")?.state).toBe("done");
  expect(box.get("cmd_done_race")?.state).toBe("done");
  // enqueue/inflight/done puts only — the downgrade never issued one.
  expect(
    db.putCalls.filter((r) => r.commandId === "cmd_done_race").map((r) => r.state),
  ).toEqual(["pending", "inflight", "done"]);
});

it("a second tab's stale cache cannot downgrade a done committed by another tab", async () => {
  const db = installFakeIndexedDB();
  storage = (await openOutboxStorage()).storage;
  const tabA = await Outbox.load(storage, "owner_tab_a");
  await tabA.enqueue(rec("cmd_cross_tab"));
  await tabA.patch("cmd_cross_tab", {
    state: "inflight",
    lease: { owner: "owner_tab_a", until: Date.now() + 30_000 },
  });

  // Tab B loads while the row is inflight under A's live lease (cached
  // inflight), then A journals the command done.
  const tabB = await Outbox.load(storage, "owner_tab_b");
  expect(tabB.get("cmd_cross_tab")?.state).toBe("inflight");
  await tabA.patch("cmd_cross_tab", { state: "done", serverState: "journal-settled" });

  // B's held verdict (or any downgrade) is built from B's stale cache; the
  // storage transaction reads the committed done and preserves it.
  const row = await tabB.patch("cmd_cross_tab", { state: "held", gotResponse: true });
  expect(row?.state).toBe("done");
  expect(tabB.get("cmd_cross_tab")?.state).toBe("done");
  const stored = await storage.all();
  expect(stored.find((r) => r.commandId === "cmd_cross_tab")?.state).toBe("done");
  expect(db.putCalls.some((r) => r.commandId === "cmd_cross_tab" && r.state === "held")).toBe(false);
});
