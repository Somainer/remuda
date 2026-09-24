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
 * - inflight: this tab holds the delivery lease and a POST is running.
 * - sent: the POST was accepted/forwarded with a CLEAR resolution; awaiting
 *   the journal join. Never re-POSTed; restored; projected 已送达.
 * - reconciling: accepted/forwarded but resolution "reconciling"; a bounded
 *   GET (not a re-forward) decides accepted/rejected/unknown.
 * - held: Hub holds the row but the Node is offline / never forwarded; bounded
 *   same-id retry without spending the attempt budget until the host returns.
 * - done: journal observation carrying the commandId confirmed execution.
 * - rejected: definite business rejection (settlement.outcome=rejected / 4xx).
 * - unknown: 409 conflict, retry window exhausted, or no durable store.
 */
export type OutboxState =
  | "pending"
  | "inflight"
  | "held"
  | "reconciling"
  | "sent"
  | "done"
  | "rejected"
  | "unknown";

/** A durable delivery lease on one row (single-deliverer across tabs). */
export type Lease = {
  /** Owner token of the tab/process currently delivering. */
  owner: Id;
  /** Epoch ms after which a crashed owner's lease may be stolen. */
  until: number;
};

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
   * restore the last known instance projection and still render offline. */
  journalId?: string;
  /**
   * Last known complete instance projection at enqueue. On an offline reload
   * (the instance list is unreachable) this restores a real, fully-shaped
   * Instance for SessionPage instead of a cast stub; a successful refresh
   * replaces it with the authoritative row.
   */
  instanceSnapshot?: import("../types/instance").Instance;
  createdAt: number;
  attempts: number;
  state: OutboxState;
  lastError?: string;
  /** Last command state the Hub reported on the row. */
  serverState?: string;
  /** Whether any POST has received an HTTP response. */
  gotResponse?: boolean;
  /** Set when state === "inflight": the durable single-deliverer lease. */
  lease?: Lease;
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
/**
 * Monotonic UUIDv7 (RFC 9562 §6.2 method 1): ids minted within the same
 * millisecond get a strictly increasing 74-bit random tail, so lexical order
 * equals mint order. The outbox orders equal-createdAt rows by commandId
 * (FIFO for two rows enqueued in one burst); a random tail could deliver a
 * later queued turn ahead of an earlier one when Date.now() ties.
 */
let lastIdTs = -1;
let lastIdTail = 0n;
const TAIL_74_MASK = (1n << 74n) - 1n;
const RAND_B_62_MASK = (1n << 62n) - 1n;

export function newCommandId(nowMs: number = Date.now(), randomBytes?: Uint8Array): Id {
  const bytes = randomBytes ?? crypto.getRandomValues(new Uint8Array(16));
  const b = Array.from(bytes, (x) => x & 0xff);
  // First 48 bits: unix millisecond timestamp.
  let ts = BigInt(nowMs);
  for (let i = 5; i >= 0; i--) {
    b[i] = Number(ts & 0xffn);
    ts >>= 8n;
  }
  // 74-bit tail: rand_a 12 bits (b[6] low nibble + b[7]) over rand_b 62 bits
  // (b[8] low 6 bits + b[9..15]).
  const randA = ((BigInt(b[6] & 0x0f) << 8n) | BigInt(b[7])) & 0xfffn;
  const randB =
    ((BigInt(b[8] & 0x3f) << 56n) |
      BigInt(
        `0x${b
          .slice(9)
          .map((x) => x.toString(16).padStart(2, "0"))
          .join("")}`,
      )) &
    RAND_B_62_MASK;
  let tail = (randA << 62n) | randB;
  if (nowMs === lastIdTs) {
    tail = (lastIdTail + 1n) & TAIL_74_MASK;
  }
  lastIdTs = nowMs;
  lastIdTail = tail;

  // Re-encode: version 7 nibble, then the monotonic tail; variant 10 on b[8].
  const tailA = tail >> 62n; // 12 bits
  const tailB = tail & RAND_B_62_MASK; // 62 bits
  b[6] = 0x70 | Number((tailA >> 8n) & 0x0fn);
  b[7] = Number(tailA & 0xffn);
  b[8] = 0x80 | Number((tailB >> 56n) & 0x3fn);
  for (let i = 0; i < 7; i += 1) {
    b[9 + i] = Number((tailB >> BigInt(48 - i * 8)) & 0xffn);
  }
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

/** Lease lifetime for a delivery; a crashed owner's lease steals after this. */
export const LEASE_TTL_MS = 30_000;

const IDB_NAME = "remuda-outbox";
const IDB_STORE = "commands";
const LS_KEY = "remuda-outbox-v1";
/** Exported for test teardown (the fallback store when IndexedDB is absent). */
export const OUTBOX_LS_KEY = LS_KEY;

/** Is a row in a deliverable state for the single deliverer? */
export function isDeliverable(r: OutboxRecord, now: number): boolean {
  if (r.state === "pending" || r.state === "held") return true;
  // An inflight row is deliverable only by its own live owner; a stale lease
  // (crashed owner) becomes re-deliverable to whoever holds the lock.
  if (r.state === "inflight") return !r.lease || r.lease.until <= now;
  return false;
}

function commandIdFromKey(key: string): string | null {
  return key.startsWith("cmd_") ? key : null;
}

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
      const rows = Array.isArray(parsed) ? (parsed as OutboxRecord[]) : [];
      return rows;
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
 * The outbox: write-through cache over {@link OutboxStorage} with storage-first
 * durability (the cache and subscribers change ONLY after the storage
 * transaction commits) and a single deliverer per instance — Web Lock, or a
 * durable IDB lease with TTL when Web Locks are unavailable.
 */
export class Outbox {
  private cache = new Map<Id, OutboxRecord>();
  private flushChain = new Map<Id, Promise<unknown>>();
  private queuedFlush = new Map<Id, Promise<unknown>>();
  private listeners = new Set<() => void>();
  private storage: OutboxStorage;
  private readonly owner: Id;

  private constructor(storage: OutboxStorage, owner?: Id) {
    this.storage = storage;
    this.owner = owner ?? `owner_${newCommandId()}`;
  }

  static async load(storage: OutboxStorage, owner?: Id): Promise<Outbox> {
    const box = new Outbox(storage, owner);
    for (const rec of (await storage.all()).filter((r) => commandIdFromKey(r.commandId))) {
      box.cache.set(rec.commandId, rec);
    }
    // A row left inflight by another (possibly crashed) process is returned to
    // a deliverable state in THIS cache; the durable lease is re-judged inside
    // the delivery lock (an expired lease is stealable).
    const now = Date.now();
    for (const [id, rec] of box.cache) {
      if (rec.state === "inflight" && rec.lease?.owner !== box.owner && (!rec.lease || rec.lease.until <= now)) {
        box.cache.set(id, { ...rec, state: "pending", lease: undefined });
      }
    }
    return box;
  }

  get ownerId(): Id {
    return this.owner;
  }

  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  }

  private emit() {
    for (const fn of this.listeners) fn();
  }

  private rank(r: OutboxRecord): number {
    return r.state === "inflight" ? 0 : r.mode === "steer" ? 1 : 2;
  }

  private deliverableFromCache(now = Date.now()): OutboxRecord[] {
    return [...this.cache.values()].filter((r) => isDeliverable(r, now));
  }

  pending(): OutboxRecord[] {
    return this.deliverableFromCache().sort((a, b) => {
      const ra = this.rank(a);
      const rb = this.rank(b);
      return ra - rb || a.createdAt - b.createdAt || a.commandId.localeCompare(b.commandId);
    });
  }

  pendingFor(instanceId: Id): OutboxRecord[] {
    return this.pending().filter((r) => r.instanceId === instanceId);
  }

  pendingCount(): number {
    return this.deliverableFromCache().length;
  }

  unresolved(): OutboxRecord[] {
    return [...this.cache.values()].filter((r) => r.state !== "done");
  }

  get(commandId: Id): OutboxRecord | undefined {
    return this.cache.get(commandId);
  }

  /**
   * Storage-first enqueue: cache + subscribers update only after the durable
   * write commits. A transaction abort leaves the cache without the row, so no
   * uncommitted message is rendered or POSTed.
   */
  async enqueue(
    rec: Omit<OutboxRecord, "attempts" | "state"> & { state?: OutboxState },
  ): Promise<OutboxRecord> {
    const full: OutboxRecord = { attempts: 0, state: "pending", ...rec };
    await this.storage.put(full);
    this.cache.set(full.commandId, full);
    this.emit();
    return full;
  }

  /**
   * Storage-first state write: cache + emit after commit; on abort the cache is
   * untouched and the rejection propagates (the caller must not act as if the
   * write applied — e.g. it must keep the inflight marker).
   */
  async patch(commandId: Id, patch: Partial<OutboxRecord>): Promise<OutboxRecord | null> {
    const cur =
      this.cache.get(commandId) ??
      (await this.storage.all()).find((r) => r.commandId === commandId);
    if (!cur) return null;
    const next = { ...cur, ...patch };
    await this.storage.put(next);
    this.cache.set(commandId, next);
    this.emit();
    return next;
  }

  async remove(commandId: Id) {
    await this.storage.delete(commandId);
    this.cache.delete(commandId);
    this.emit();
  }

  isFlushing(instanceId: Id): boolean {
    return this.flushChain.has(instanceId);
  }

  /**
   * Run the deliverer for one instance as the SINGLE deliverer. The function
   * receives the rows re-read from the authoritative durable store inside the
   * lock, so a row another tab already marked sent/done is not POSTed again.
   */
  async withInstanceLock<T>(
    instanceId: Id,
    fn: (instanceId: Id, deliverable: OutboxRecord[]) => Promise<T>,
  ): Promise<T | null> {
    if (this.queuedFlush.has(instanceId)) return (await this.queuedFlush.get(instanceId)!) as T | null;
    if (this.flushChain.has(instanceId)) {
      const queued = this.runAfterChain(instanceId, fn);
      this.queuedFlush.set(instanceId, queued);
      void queued.finally(() => this.queuedFlush.delete(instanceId));
      return (await queued) as T | null;
    }
    return (await this.runAfterChain(instanceId, fn)) as T | null;
  }

  private runAfterChain<T>(
    instanceId: Id,
    fn: (instanceId: Id, deliverable: OutboxRecord[]) => Promise<T>,
  ): Promise<T | null> {
    const prev = this.flushChain.get(instanceId) ?? Promise.resolve();
    // A rejected predecessor must not poison the chain (its entry is cleared by
    // `clear`), but a rejected run must NEVER silently re-invoke fn: the
    // deliverer may already have POSTed once, and exactly-once is the property
    // this lock exists for. Callers retry from the outside under the same id.
    const run = prev.then(
      () => this.acquireLock(instanceId, fn),
      () => this.acquireLock(instanceId, fn),
    );
    const clear = () => {
      if (this.flushChain.get(instanceId) === run) this.flushChain.delete(instanceId);
    };
    this.flushChain.set(instanceId, run);
    void run.then(clear, clear);
    return run;
  }

  private async acquireLock<T>(
    instanceId: Id,
    fn: (instanceId: Id, deliverable: OutboxRecord[]) => Promise<T>,
  ): Promise<T | null> {
    const locks = (globalThis as { navigator?: Navigator & { locks?: LockManager } }).navigator?.locks;
    if (locks?.request) {
      return await locks.request(`remuda-outbox-${instanceId}`, { mode: "exclusive" }, () =>
        this.runDeliverer(instanceId, fn),
      );
    }
    return await this.runWithLease(instanceId, fn);
  }

  /** Build the deliverable set from FRESH durable command rows inside the lock. */
  private async runDeliverer<T>(
    instanceId: Id,
    fn: (instanceId: Id, deliverable: OutboxRecord[]) => Promise<T>,
  ): Promise<T> {
    const now = Date.now();
    const durable = (await this.refreshFromStorage()).filter((r) => commandIdFromKey(r.commandId));
    const deliverable = durable
      .filter((r) => r.instanceId === instanceId && isDeliverable(r, now))
      .sort(
        (a, b) =>
          this.rank(a) - this.rank(b) || a.createdAt - b.createdAt || a.commandId.localeCompare(b.commandId),
      );
    return await fn(instanceId, deliverable);
  }

  private async refreshFromStorage(): Promise<OutboxRecord[]> {
    const durable = (await this.storage.all()).filter((r) => commandIdFromKey(r.commandId));
    for (const rec of durable) this.cache.set(rec.commandId, rec);
    return durable;
  }

  private leaseKey(instanceId: Id): Id {
    return `__lock__:${instanceId}`;
  }

  /**
   * Durable lease fallback (no Web Locks). The per-instance lease lives in the
   * same store; a crashed owner's lease is stealable after {@link LEASE_TTL_MS}.
   * The owner releases in finally. localStorage has no cross-tab transactions;
   * the single-tab degraded case is the supported one.
   */
  private async runWithLease<T>(
    instanceId: Id,
    fn: (instanceId: Id, deliverable: OutboxRecord[]) => Promise<T>,
  ): Promise<T> {
    const key = this.leaseKey(instanceId);
    const deadline = Date.now() + LEASE_TTL_MS + 5_000;
    for (;;) {
      const all = await this.storage.all();
      const existing = all.find((r) => r.commandId === key);
      const now = Date.now();
      if (!existing?.lease || existing.lease.owner === this.owner || existing.lease.until <= now) {
        await this.storage.put({
          commandId: key,
          clientRequestId: key,
          instanceId,
          prompt: "",
          createdAt: now,
          attempts: 0,
          state: "inflight",
          lease: { owner: this.owner, until: now + LEASE_TTL_MS },
        });
        try {
          return await this.runDeliverer(instanceId, fn);
        } finally {
          await this.storage.delete(key).catch(() => undefined);
        }
      }
      if (Date.now() > deadline) throw new Error("outbox lease wait timed out");
      await new Promise((r) => setTimeout(r, 250));
    }
  }
}
