import { afterEach, expect, it, vi } from "vitest";
import { type OutboxRecord, type OutboxStorage } from "./outbox";

const INSTANCE = "ins_done_guard_integration";
const JOURNAL = "obj_done_guard_integration_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

/**
 * c-reconnfu round 3 item 6: the terminal-done guarantee exercised through the
 * REAL delivery + follow + reconciliation stack (the earlier test called
 * box.patch directly and never raced the integration). A command-confirming
 * journal event arrives through the captured follow onBatch callback; its
 * durable done transaction is held BEFORE commit. While held, the OUTSTANDING
 * bounded reconciliation GET (a forwarded-but-unresolved POST is parked in it)
 * resolves with a "held" verdict. After the done commits: durable done, no
 * held re-POST scheduled, exactly one POST for the commandId.
 */

// ---- serial fake IndexedDB (readwrite txs run strictly in creation order; a
// tx can hold its commit after its requests ran) ----------------------------
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

  objectStore() {
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
      await Promise.resolve();
    }
    for (const gate of this.db.takeCommitHolds()) await gate;
    this.oncomplete?.({ target: this });
  };
}

class SerialFakeDb {
  readonly rows = new Map<string, OutboxRecord>();
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

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

afterEach(() => {
  vi.restoreAllMocks();
  delete (globalThis as { indexedDB?: IDBFactory }).indexedDB;
});

it("a held done txn makes the reconciling GET's held verdict preserve done: no held retry, one POST", async () => {
  const db = installFakeIndexedDB();
  const { api, hubStore } = await fresh();

  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      { id: INSTANCE, journalId: JOURNAL, revision: "1", durableSeq: "0", lifecycle: "running" } as never,
    ],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_done_guard_integration",
    journalId: JOURNAL,
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
    getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_done_guard_integration",
      asOfSeq: "0",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "0", complete: true },
    },
  });

  await hubStore.refresh();
  await hubStore.follow(INSTANCE);
  const subscribe = vi.mocked(api.eventsSubscribe);
  await vi.waitFor(() => expect(subscribe).toHaveBeenCalledTimes(1));
  const onBatch = subscribe.mock.calls[0]?.[2] as (batch: Record<string, unknown>) => void;

  // The POST is accepted/forwarded but UNRESOLVED (resolution "reconciling"):
  // the store hands the row to the bounded GET loop. The FIRST GET parks.
  let releaseGet: (record: Awaited<ReturnType<Api["instanceCommandStatus"]>>) => void = () => undefined;
  const status = vi.spyOn(api, "instanceCommandStatus").mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        releaseGet = resolve;
      }),
  );
  vi.spyOn(api, "instanceSend").mockImplementation(
    (async (
      _iid: string,
      _prompt: string,
      _attachments: unknown[],
      _mode: string,
      commandId: string,
    ) => ({
      relatedCommandIds: [],
      command: {
        commandId,
        id: commandId,
        revision: "1",
        createdAt: "2026-09-15T00:00:00.000Z",
        updatedAt: "2026-09-15T00:00:00.000Z",
        // queued + forwarded + resolution reconciling → classify "reconciling":
        // the bounded GET owns the row (never a re-POST).
        state: "queued",
        dispatch: "transport-written",
        resolution: "reconciling",
      },
    })) as unknown as Api["instanceSend"],
  );

  await hubStore.send(INSTANCE, "lost response race");
  await vi.waitFor(() => expect(status).toHaveBeenCalledTimes(1));
  const commandId = vi.mocked(api.instanceSend).mock.calls[0]?.[4]!;
  expect(commandId.startsWith("cmd_")).toBe(true);

  // The command-confirming journal event arrives through the REAL follow
  // callback; hold its durable done transaction BEFORE commit.
  const hold = db.holdCommit();
  onBatch({
    subscriptionId: "sub_done_guard_integration",
    journalId: JOURNAL,
    fromSeq: "1",
    toSeq: "1",
    durableSeq: "1",
    events: [
      {
        kind: "message",
        eventId: "evt_done_guard_1",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "1",
        payload: { role: "user", commandId },
      },
    ],
  });
  // The done put REQUEST ran inside the still-uncommitted transaction.
  await vi.waitFor(() => expect(db.putCalls.some((r) => r.state === "done")).toBe(true));

  // Resolve the outstanding GET with a HELD verdict while the done txn is
  // open. Its held patch queues BEHIND the held done transaction.
  releaseGet({
    state: "queued",
    resolution: "clear",
    forwarded: false,
  } as Awaited<ReturnType<Api["instanceCommandStatus"]>>);
  // Let the held verdict's merge txn queue behind the uncommitted done txn.
  await new Promise((r) => setTimeout(r, 20));

  // Commit the done txn: the queued held merge must READ the committed done
  // and preserve it — the row stays durably done.
  hold.release();
  await vi.waitFor(async () => {
    const stored = await (
      hubStore as unknown as { outbox: { storage: OutboxStorage } }
    ).outbox.storage.all();
    expect(stored.find((r) => r.commandId === commandId)?.state).toBe("done");
  });

  // A preserved done never arms the bounded same-id held re-POST: give the
  // held branch's (queued-behind-done) merge transaction time to settle, but
  // stay well inside the 2 s held-retry delay so a fired timer cannot hide.
  await new Promise((r) => setTimeout(r, 100));
  const heldTimers = (
    hubStore as unknown as { heldRetryTimer: Map<string, unknown> }
  ).heldRetryTimer;
  expect(heldTimers.has(INSTANCE)).toBe(false);
  // … and past the held-retry delay the command was POSTed exactly once.
  await new Promise((r) => setTimeout(r, 2_100));
  expect(vi.mocked(api.instanceSend)).toHaveBeenCalledTimes(1);
  expect(db.putCalls.filter((r) => r.commandId === commandId && r.state === "held")).toHaveLength(0);

  hubStore.logout();
});
