import type { AttachmentRef } from "./attachments";
import type { Id } from "../types/wire";

/**
 * D-055 client outbox: every `instance.send` is written here BEFORE its POST,
 * under a client-generated `cmd_` id, and retried with the SAME id until the
 * Hub gives a definite answer. Hub/Node commandId dedup makes the retry
 * exactly-once on the wire; this module is the durable, crash-surviving
 * client half.
 */

/**
 * Outbox row state.
 * - pending: not yet given an answer (a transient network failure returns here).
 * - inflight: a POST is running.
 * - sent: the POST was accepted/forwarded by the Hub; awaiting the journal
 *   join. Never re-POSTed; restored on reload; projected as 已送达.
 * - done: journal observation carrying the commandId confirmed execution.
 * - rejected: definite business rejection (settlement.outcome=rejected / 4xx).
 * - unknown: 409 conflict, retry window exhausted, or no durable store.
 * - held: Hub holds the row but the Node is offline / never forwarded; the
 *   SAME id is re-POSTed on host reconnect (does not burn the retry budget).
 */
export type OutboxState =
  | "pending"
  | "inflight"
  | "sent"
  | "done"
  | "rejected"
  | "unknown"
  | "held";

export type OutboxRecord = {
  /** Client-generated `cmd_<uuidv7>`; the ONLY id the POST ever uses. */
  commandId: Id;
  /** Render key shared with the optimistic bubble. */
  clientRequestId: Id;
  instanceId: Id;
  prompt: string;
  /** Manifest refs only; upload completes before enqueue. */
  attachments?: AttachmentRef[];
  /** Undefined/new-turn = ordinary send; "queue" = queued turn. A steer is
   * persisted as steer only when it was enqueued while still live; store.send
   * degrades an offline steer to a normal turn before enqueue. */
  mode?: "new-turn" | "queue" | "steer";
  /** Journal of the instance, captured at enqueue so an offline reload can
   * restore a minimal instance stub and still render the session/composer. */
  journalId?: string;
  createdAt: number;
  attempts: number;
  state: OutboxState;
  lastError?: string;
  /** Last command state the Hub reported on the row. */
  serverState?: string;
  /**
   * Whether any POST for this row has received an HTTP response (of any kind).
   * A 2xx that still reads "queued" means the Node acked but did not durably
   * accept — delivered, awaiting the journal; it is NOT retried. Only rows
   * that never got a response (network/5xx/503) are re-POSTed.
   */
  gotResponse?: boolean;
};

/** Automatic retry envelope (D-055): ≤20 attempts within 24 h. */
export const OUTBOX_MAX_ATTEMPTS = 20;
export const OUTBOX_MAX_AGE_MS = 24 * 60 * 60 * 1000;

const HEX = "0123456789abcdef";

/**
 * Generate a canonical `cmd_` + lowercase UUIDv7. The Node validates the
 * shape (`branded_id` scalar: 48-bit unix-ms timestamp, version nibble 7,
 * variant 10); `crypto.randomUUID()` is v4 and must not be used here.
 * Injectable clock/random for tests.
 */
export function newCommandId(nowMs: number = Date.now(), randomBytes?: Uint8Array): Id {
  const bytes = randomBytes ?? crypto.getRandomValues(new Uint8Array(16));
  const b = Array.from(bytes, (x) => x & 0xff);
  // First 48 bits: unix millisecond timestamp.
  let ts = BigInt(nowMs);
  for (let i = 5; i >= 0; i--) {
    b[i] = Number(ts & 0xffn);
    ts >>= 8n;
  }
  b[6] = (b[6] & 0x0f) | 0x70; // version 7
  b[8] = (b[8] & 0x3f) | 0x80; // variant 10
  const hex = b.map((x) => HEX[x >> 4] + HEX[x & 0x0f]).join("");
  return `cmd_${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** True while the record is still inside the automatic retry envelope. */
export function withinRetryWindow(rec: Pick<OutboxRecord, "attempts" | "createdAt">, now = Date.now()): boolean {
  return rec.attempts < OUTBOX_MAX_ATTEMPTS && now - rec.createdAt <= OUTBOX_MAX_AGE_MS;
}

export interface OutboxStorage {
  all(): Promise<OutboxRecord[]>;
  put(rec: OutboxRecord): Promise<void>;
  delete(commandId: Id): Promise<void>;
}

const IDB_NAME = "remuda-outbox";
const IDB_STORE = "commands";
const LS_KEY = "remuda-outbox-v1";
/** Exported for test teardown (the fallback store when IndexedDB is absent). */
export const OUTBOX_LS_KEY = LS_KEY;

/**
 * IndexedDB storage with a localStorage fallback (private mode where IDB
 * writes throw). The caller decides how to present the degraded mode; this
 * only resolves with whichever backend writes succeed.
 */
class IdbOutboxStorage implements OutboxStorage {
  private db: IDBDatabase;

  private constructor(db: IDBDatabase) {
    this.db = db;
  }

  static async open(): Promise<IdbOutboxStorage> {
    const indexedDB = globalThis.indexedDB;
    if (!indexedDB) throw new Error("IndexedDB unavailable");
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const req = indexedDB.open(IDB_NAME, 1);
      req.onupgradeneeded = () => {
        const d = req.result;
        if (!d.objectStoreNames.contains(IDB_STORE)) d.createObjectStore(IDB_STORE, { keyPath: "commandId" });
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error ?? new Error("IndexedDB open failed"));
    });
    return new IdbOutboxStorage(db);
  }

  /**
   * Run one write/read transaction and resolve only when the TRANSACTION
   * commits. A request's `onsuccess` fires before commit; resolving there
   * would let the caller believe an enqueue/delete is durable even if the
   * transaction aborts afterwards. `oncomplete` is the durability point;
   * `onabort`/`onerror` reject so the caller can surface the failure.
   */
  private tx(mode: IDBTransactionMode, fn: (store: IDBObjectStore) => IDBRequest): Promise<unknown> {
    return new Promise((resolve, reject) => {
      const t = this.db.transaction(IDB_STORE, mode);
      const req = fn(t.objectStore(IDB_STORE));
      let result: unknown = undefined;
      req.onsuccess = () => {
        result = req.result;
      };
      req.onerror = () => reject(req.error);
      t.oncomplete = () => resolve(result);
      t.onabort = () => reject(t.error ?? req.error ?? new Error("IndexedDB transaction aborted"));
      t.onerror = () => reject(t.error ?? req.error);
    });
  }

  async all() {
    return (await this.tx("readonly", (s) => s.getAll())) as OutboxRecord[];
  }

  async put(rec: OutboxRecord) {
    await this.tx("readwrite", (s) => s.put(rec));
  }

  async delete(commandId: Id) {
    await this.tx("readwrite", (s) => s.delete(commandId));
  }
}

class LocalStorageOutboxStorage implements OutboxStorage {
  private read(): OutboxRecord[] {
    try {
      const raw = localStorage.getItem(LS_KEY);
      const parsed: unknown = raw ? JSON.parse(raw) : [];
      return Array.isArray(parsed) ? (parsed as OutboxRecord[]) : [];
    } catch {
      return [];
    }
  }

  private write(recs: OutboxRecord[]) {
    localStorage.setItem(LS_KEY, JSON.stringify(recs));
  }

  async all() {
    return this.read();
  }

  async put(rec: OutboxRecord) {
    const recs = this.read().filter((r) => r.commandId !== rec.commandId);
    recs.push(rec);
    this.write(recs);
  }

  async delete(commandId: Id) {
    this.write(this.read().filter((r) => r.commandId !== commandId));
  }
}

export async function openOutboxStorage(): Promise<{ storage: OutboxStorage; degraded: boolean }> {
  try {
    const storage = await IdbOutboxStorage.open();
    // Probe a write so "opened but quota-denied" private modes land in the
    // fallback rather than failing the first enqueue.
    await storage.put({
      commandId: "cmd_probe",
      clientRequestId: "local_probe",
      instanceId: "ins_probe",
      prompt: "",
      createdAt: 0,
      attempts: 0,
      state: "pending",
    });
    await storage.delete("cmd_probe");
    return { storage, degraded: false };
  } catch {
    return { storage: new LocalStorageOutboxStorage(), degraded: true };
  }
}

/**
 * The outbox: a write-through cache over {@link OutboxStorage} with an
 * in-process serial lock per instance (durable cross-tab dedup is the
 * server's job; the lock only stops two tabs doubling the POST traffic).
 */
export class Outbox {
  private cache = new Map<Id, OutboxRecord>();
  /**
   * Per-instance flush chain: appended runs run strictly after every earlier
   * run. It never has a "lock released" gap, so a caller arriving while a
   * flush is active cannot accidentally start a competing run.
   */
  private flushChain = new Map<Id, Promise<unknown>>();
  /**
   * The single coalesced follow-up for callers that arrived while a flush was
   * already running (all such callers join this one promise).
   */
  private queuedFlush = new Map<Id, Promise<unknown>>();
  /**
   * Rows whose first POST is already running. A second flush (e.g. the idle
   * edge racing a steer) must NOT re-POST them — server 409 is a safety net,
   * not the ordering mechanism. Cleared as each delivery settles.
   */
  private inflightCommands = new Set<Id>();
  private listeners = new Set<() => void>();
  private storage: OutboxStorage;

  private constructor(storage: OutboxStorage) {
    this.storage = storage;
  }

  static async load(storage: OutboxStorage): Promise<Outbox> {
    const box = new Outbox(storage);
    for (const rec of await storage.all()) box.cache.set(rec.commandId, rec);
    return box;
  }

  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private emit() {
    for (const fn of this.listeners) fn();
  }

  /**
   * Records not yet given a definite answer. Ordering: a steer jumps ahead of
   * the ordinary held queue (it interrupts the running turn and must land
   * first); among the same kind, oldest enqueue wins. An inflight row always
   * precedes not-yet-started rows.
   */
  pending(): OutboxRecord[] {
    const rank = (r: OutboxRecord) => (r.state === "inflight" ? 0 : r.mode === "steer" ? 1 : 2);
    return [...this.cache.values()]
      // Deliverable now: not-yet-answered (pending/inflight) or Hub-held but
      // never forwarded to the Node ("held"). "sent" already reached the Hub
      // and must never be re-POSTed.
      .filter((r) => r.state === "pending" || r.state === "inflight" || r.state === "held")
      .sort((a, b) => {
        const ra = rank(a);
        const rb = rank(b);
        if (ra !== rb) return ra - rb;
        return a.createdAt - b.createdAt || a.commandId.localeCompare(b.commandId);
      });
  }

  pendingFor(instanceId: Id): OutboxRecord[] {
    return this.pending().filter((r) => r.instanceId === instanceId);
  }

  pendingCount(): number {
    let n = 0;
    for (const r of this.cache.values()) {
      if (r.state === "pending" || r.state === "inflight" || r.state === "held") n++;
    }
    return n;
  }

  /**
   * Records restored as bubbles at bootstrap: everything not journal-confirmed
   * ("done"). Includes "sent" (delivered, awaiting journal) and "unknown"
   * (explicit resend chip) so neither is lost on reload.
   */
  unresolved(): OutboxRecord[] {
    return [...this.cache.values()].filter((r) => r.state !== "done");
  }

  get(commandId: Id): OutboxRecord | undefined {
    return this.cache.get(commandId);
  }

  /** Whether the first POST for this row is currently in flight. */
  isInflight(commandId: Id): boolean {
    return this.inflightCommands.has(commandId);
  }

  markInflight(commandId: Id) {
    this.inflightCommands.add(commandId);
  }

  clearInflight(commandId: Id) {
    this.inflightCommands.delete(commandId);
  }

  async enqueue(rec: Omit<OutboxRecord, "attempts" | "state"> & { state?: OutboxState }): Promise<OutboxRecord> {
    const full: OutboxRecord = { attempts: 0, state: "pending", ...rec };
    this.cache.set(full.commandId, full);
    this.emit();
    await this.storage.put(full);
    return full;
  }

  async patch(commandId: Id, patch: Partial<OutboxRecord>): Promise<OutboxRecord | null> {
    const cur = this.cache.get(commandId);
    if (!cur) return null;
    const next = { ...cur, ...patch };
    this.cache.set(commandId, next);
    this.emit();
    await this.storage.put(next);
    return next;
  }

  async remove(commandId: Id) {
    this.cache.delete(commandId);
    this.emit();
    await this.storage.delete(commandId);
  }

  isFlushing(instanceId: Id): boolean {
    return this.flushChain.has(instanceId);
  }

  /**
   * Run `fn` for one instance under the cross-tab Web Lock and serialized
   * strictly after every earlier run on the same instance. The server's
   * commandId dedup is the correctness guarantee; this both avoids redundant
   * concurrent POSTs and guarantees that a row enqueued mid-flush gets a
   * follow-up drain.
   *
   * Callers that arrive while a run is active coalesce onto ONE chained
   * follow-up (all join the same promise), so there is never a turn-away and
   * never one trigger per frame.
   */
  async withInstanceLock<T>(instanceId: Id, fn: () => Promise<T>): Promise<T | null> {
    if (this.queuedFlush.has(instanceId)) return (await this.queuedFlush.get(instanceId)!) as T | null;
    if (this.flushChain.has(instanceId)) {
      const queued = this.runAfterChain(instanceId, fn);
      this.queuedFlush.set(instanceId, queued);
      void queued.finally(() => this.queuedFlush.delete(instanceId));
      return (await queued) as T | null;
    }
    return (await this.runAfterChain(instanceId, fn)) as T | null;
  }

  private runAfterChain<T>(instanceId: Id, fn: () => Promise<T>): Promise<T | null> {
    const prev = this.flushChain.get(instanceId) ?? Promise.resolve();
    const run = prev.then(() => this.acquireWebLock(instanceId, fn), () =>
      this.acquireWebLock(instanceId, fn),
    );
    // A follow-up scheduled while this run was active has already replaced the
    // chain entry; only clear the marker when the chain is genuinely idle.
    const clear = () => {
      if (this.flushChain.get(instanceId) === run) this.flushChain.delete(instanceId);
    };
    this.flushChain.set(instanceId, run);
    void run.then(clear, clear);
    return run;
  }

  private async acquireWebLock<T>(instanceId: Id, fn: () => Promise<T>): Promise<T | null> {
    const locks = (globalThis as { navigator?: Navigator & { locks?: LockManager } }).navigator?.locks;
    if (locks?.request) {
      return await locks.request(`remuda-outbox-${instanceId}`, { mode: "exclusive" }, fn);
    }
    return await fn();
  }
}
