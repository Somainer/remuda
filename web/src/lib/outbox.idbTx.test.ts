import { afterEach, expect, it } from "vitest";
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
  error: Error | null = null;
  readonly db: FakeDb;
  readonly mode: IDBTransactionMode;
  aborted = false;

  constructor(db: FakeDb, mode: IDBTransactionMode) {
    this.db = db;
    this.mode = mode;
  }

  objectStore(_name: string) {
    return new FakeObjectStore(this, this.db);
  }

  abort = () => {
    if (this.aborted) return;
    this.aborted = true;
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

  private finish(req: FakeRequest, prepare: () => void, commit: () => void) {
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
        });
        return;
      }
      queueMicrotask(() => {
        if (this.tx.aborted) return; // external signal abort won the race
        if (this.db.abortNextAfterRequest) {
          this.db.abortNextAfterRequest = false;
          this.db.requestSucceededBeforeAbort = true;
          this.tx.abort();
          return;
        }
        if (this.db.commitGate) return; // held open for disposal-window tests
        commit();
        this.tx.oncomplete?.({ target: this.tx });
      });
    });
  }

  put(rec: OutboxRecord) {
    const req = new FakeRequest();
    req.result = rec.commandId;
    this.db.requestsIssued.push("put");
    this.finish(req, () => {}, () => this.db.rows.set(rec.commandId, rec));
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
    this.finish(req, () => {}, () => {
      this.db.rows.delete(key);
    });
    return req as unknown as IDBRequest;
  }
}

class FakeDb {
  readonly rows = new Map<string, OutboxRecord>();
  private readonly names = new Set<string>();
  /** Test switch: abort the NEXT transaction after its request succeeds. */
  abortNextAfterRequest = false;
  /** Recorded proof the request succeeded before the abort arrived. */
  requestSucceededBeforeAbort = false;
  /** When truthy, requests hold their commit microtask open (disposal tests). */
  commitGate = false;
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

  transaction(name: string | string[], mode: IDBTransactionMode) {
    void name;
    return new FakeTransaction(this, mode) as unknown as IDBTransaction;
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
