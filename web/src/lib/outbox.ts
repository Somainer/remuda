import type { AttachmentRef } from "./attachments";
import type { Id } from "../types/wire";
import { leaseWakeDecision, reconcileLeaseWakes } from "./leaseWakes";

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
 *   the journal join. Never re-POSTed; restored; the bubble projects 已受理
 *   (distinct from inflight's 已发送，等待确认 — a clear answer is in hand).
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
  /**
   * Atomically claim the per-instance delivery lease in ONE readwrite
   * transaction: read the `__lock__:<instanceId>` row and write `rec` only
   * when it is absent, carries an EXPIRED lease, or a lease this caller owns;
   * a LIVE lease owned by another owner makes no write and resolves false.
   * The check and the write MUST share one transaction — a separate
   * all()+put() pair lets two lock-less tabs both observe "no lease" and both
   * run the deliverer (a double POST). A storage failure rejects.
   */
  acquireLease(key: Id, rec: OutboxRecord, now: number): Promise<boolean>;
  /**
   * Release the per-instance delivery lease ONLY when the stored lease owner
   * still matches `owner` (c-reconnfu gate 15 item 1). One readwrite
   * transaction (or one synchronous read-check-write turn in the degraded
   * backend): if the row is missing, not a lease, or currently held by a
   * different owner (e.g. a steal after TTL expiry), NOTHING is deleted —
   * otherwise an exiting deliverer would erase a successor's live mutex.
   * Resolves true when this owner's row was removed.
   */
  releaseLease(key: Id, owner: Id): Promise<boolean>;
  /**
   * Merge a patch into one row atomically INSIDE one readwrite transaction:
   * the stored row is read and the merged record written in the SAME
   * transaction, so a terminal `done` committed by a journal callback whose
   * write is still in flight (or by another tab) can never be overwritten
   * with a full stale record built from an older cache read. A stored `done`
   * row is preserved verbatim and returned; a missing row resolves null.
   */
  mergeUnlessDone(commandId: Id, patch: Partial<OutboxRecord>): Promise<OutboxRecord | null>;
  /**
   * Atomically reclaim (load-time) a row THIS context observed inflight with a
   * lease snapshot `{ owner, until }`. In ONE readwrite transaction the stored
   * row is read and written to `state: "pending", lease: undefined` ONLY when
   * it is STILL inflight AND its lease owner AND deadline match the snapshot
   * exactly. A row another tab meanwhile moved to sent/reconciling/held/done —
   * or whose lease changed — is returned VERBATIM with no write (c-reconnfu
   * gate 11 item 4: reclaim via mergeUnlessDown guarded only `done`, so a
   * snapshot patch carrying `pending` downgraded a row already sent). Missing
   * resolves null. A storage failure rejects.
   *
   * Options:
   *  - `requireExpired` (c-reconnfu gate 13 item 2): the WAKE path additionally
   *    requires `lease.until <= Date.now()` INSIDE the transaction, so a timer
   *    that fired across a system-clock rollback (Date.now() moved back before
   *    `until`) cannot reclaim a still-fresh lease. The row is returned
   *    verbatim inflight in that case. Load-time reclaims do not pass it — load
   *    applies its own ownership rules (a holder-free fresh context reclaim is
   *    legitimate regardless of wall time).
   *  - `signal`: when aborted before the write commits, the transaction is
   *    aborted and the row stays inflight (c-reconnfu gate 13 item 3).
   */
  reclaimInflight(
    commandId: Id,
    expected: { owner: Id; until: number },
    opts?: { requireExpired?: boolean; signal?: AbortSignal },
  ): Promise<OutboxRecord | null>;
}

/** Lease lifetime for a delivery; a crashed owner's lease steals after this. */
export const LEASE_TTL_MS = 30_000;
/**
 * Bounded retry budget for a claim wakeup whose durable READ or conditional
 * reclaim keeps failing (gate 12 item 2): backoff 250 ms·2^n capped at 5 s,
 * this many failures (~≤ a few minutes) before the wake gives up — the next
 * flush/reload still picks the row up.
 */
export const CLAIM_WAKE_MAX_FAILURES = 12;
export const CLAIM_WAKE_BASE_RETRY_MS = 250;
export const CLAIM_WAKE_MAX_RETRY_MS = 5_000;

/**
 * The Web Lock name guarding delivery for ONE instance. This is the ONLY place
 * that computes it — acquireLock() requests exactly this name and Outbox.load
 * asks the LockManager whether exactly this name is held (c-reconnfu gate 11
 * item 3: the holder query used a different string than the request, so a live
 * same-owner cloned tab's lock was never recognised and the fresh claim was
 * requeued under the POSTing tab). Distinct from the durable lease row key
 * (`__lock__:<instance>`, see leaseKey), which exists only in the no-Web-Locks
 * fallback.
 */
export function instanceDeliveryLockName(instanceId: Id): string {
  return `remuda-outbox-${instanceId}`;
}

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

  /**
   * The single-deliverer lease claim in ONE readwrite transaction: the get
   * and the put share `t`, so IndexedDB serializes the check-and-write against
   * every other readwrite tx on this store. A live FOREIGN lease aborts the
   * transaction deliberately (contended lock → false), distinguished from a
   * real abort via `contended`.
   */
  async acquireLease(key: Id, rec: OutboxRecord, now: number): Promise<boolean> {
    return await new Promise<boolean>((resolve, reject) => {
      const t = this.db.transaction(IDB_STORE, "readwrite");
      const store = t.objectStore(IDB_STORE);
      let contended = false;
      const getReq = store.get(key);
      getReq.onsuccess = () => {
        const existing = getReq.result as OutboxRecord | undefined;
        const lease = existing?.lease;
        if (lease && rec.lease && lease.owner !== rec.lease.owner && lease.until > now) {
          contended = true;
          t.abort();
          return;
        }
        store.put(rec);
      };
      getReq.onerror = () => reject(getReq.error);
      t.oncomplete = () => resolve(true);
      t.onabort = () => {
        if (contended) resolve(false);
        else reject(t.error ?? new Error("IndexedDB lease transaction aborted"));
      };
      t.onerror = () => reject(t.error ?? getReq.error);
    });
  }

  /**
   * Conditional lease release in ONE readwrite transaction: delete the
   * `__lock__:` row only while its stored lease owner still matches
   * `owner`. A lock stolen after expiry (or otherwise held by a different
   * owner) is left exactly in place (gate 15 item 1).
   */
  async releaseLease(key: Id, owner: Id): Promise<boolean> {
    return await new Promise<boolean>((resolve, reject) => {
      const t = this.db.transaction(IDB_STORE, "readwrite");
      const store = t.objectStore(IDB_STORE);
      let released = false;
      const getReq = store.get(key);
      getReq.onsuccess = () => {
        const existing = getReq.result as OutboxRecord | undefined;
        if (existing?.lease?.owner === owner) {
          store.delete(key);
          released = true;
        }
      };
      getReq.onerror = () => reject(getReq.error);
      t.oncomplete = () => resolve(released);
      t.onabort = () => reject(t.error ?? new Error("IndexedDB lease release aborted"));
      t.onerror = () => reject(t.error ?? getReq.error);
    });
  }

  /**
   * Terminal preservation in ONE readwrite transaction (same serialization as
   * {@link acquireLease}): the get and the put share `t`, so IndexedDB runs
   * this read against every earlier readwrite tx on the store in creation
   * order — a `done` written by a journal callback whose transaction is still
   * open (or by another tab) is already visible here. The done row is
   * returned without a put; anything else is merged and written. Resolves
   * only on transaction commit, the same durability contract as {@link put}.
   */
  async mergeUnlessDone(commandId: Id, patch: Partial<OutboxRecord>): Promise<OutboxRecord | null> {
    return await new Promise<OutboxRecord | null>((resolve, reject) => {
      const t = this.db.transaction(IDB_STORE, "readwrite");
      const store = t.objectStore(IDB_STORE);
      let merged: OutboxRecord | null = null;
      const getReq = store.get(commandId);
      getReq.onsuccess = () => {
        const existing = getReq.result as OutboxRecord | undefined;
        if (!existing) return; // missing: resolve null on complete
        if (existing.state === "done") {
          merged = existing; // terminal: no put issued
          return;
        }
        merged = { ...existing, ...patch };
        store.put(merged);
      };
      getReq.onerror = () => reject(getReq.error);
      t.oncomplete = () => resolve(merged);
      t.onabort = () => reject(t.error ?? getReq.error ?? new Error("IndexedDB merge transaction aborted"));
      t.onerror = () => reject(t.error ?? getReq.error);
    });
  }

  /**
   * Conditional load-time reclaim in ONE readwrite transaction (same
   * serialization as {@link acquireLease}): the get, the lease comparison and
   * the put share `t`, so a sent/reconciling/done write from another tab whose
   * transaction is still open is already visible here. A row that no longer
   * matches the observed inflight lease is returned verbatim with no put.
   */
  async reclaimInflight(
    commandId: Id,
    expected: { owner: Id; until: number },
    opts?: { requireExpired?: boolean; signal?: AbortSignal },
  ): Promise<OutboxRecord | null> {
    return await new Promise<OutboxRecord | null>((resolve, reject) => {
      const t = this.db.transaction(IDB_STORE, "readwrite");
      const store = t.objectStore(IDB_STORE);
      let result: OutboxRecord | null = null;
      let finished = false;
      // Gate 13 item 3: dispose aborts the reclaim before its put commits.
      // Gate 14 item 3: a NAMED handler removed on EVERY terminal path so
      // listeners/closures do not accumulate over the Outbox's lifetime.
      const onSignalAbort = () => {
        if (finished) return;
        try {
          t.abort();
        } catch {
          /* already finishing */
        }
      };
      opts?.signal?.addEventListener("abort", onSignalAbort, { once: true });
      const detach = () => opts?.signal?.removeEventListener("abort", onSignalAbort);
      const getReq = store.get(commandId);
      getReq.onsuccess = () => {
        const existing = getReq.result as OutboxRecord | undefined;
        if (!existing) return; // missing: null on complete
        if (opts?.signal?.aborted) {
          t.abort();
          return;
        }
        const lease = existing.lease;
        if (
          existing.state === "inflight" &&
          lease &&
          lease.owner === expected.owner &&
          lease.until === expected.until
        ) {
          // Gate 13 item 2: the expiry test belongs INSIDE the matching
          // inflight branch — a concurrent completion (sent/done with lease
          // undefined) must return the authoritative terminal row, not throw
          // on a missing lease and abort/retry the transaction.
          const expired = !opts?.requireExpired || lease.until <= Date.now();
          if (!expired) {
            // Still fresh after a clock rollback: leave the row as stored.
            result = existing;
            return;
          }
          if (opts?.signal?.aborted) {
            t.abort();
            return;
          }
          const reclaimed: OutboxRecord = { ...existing, state: "pending", lease: undefined };
          result = reclaimed;
          store.put(reclaimed);
        } else {
          // Moved on (sent/reconciling/held/done with lease undefined), lease
          // changed or missing entirely: leave the authoritative row as
          // stored, even with requireExpired.
          result = existing;
        }
      };
      getReq.onerror = () => {
        detach();
        reject(getReq.error);
      };
      t.oncomplete = () => {
        finished = true;
        detach();
        resolve(result);
      };
      t.onabort = () => {
        finished = true;
        detach();
        reject(t.error ?? getReq.error ?? new Error("IndexedDB reclaim transaction aborted"));
      };
      t.onerror = () => {
        detach();
        reject(t.error ?? getReq.error);
      };
    });
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

  /**
   * localStorage has no cross-tab transactions, but read-check-write here is
   * one synchronous turn — atomic WITHIN the supported single-tab degraded
   * case (two same-tab callers cannot interleave). A crashed owner's lease is
   * still covered by the TTL steal in {@link Outbox}.
   */
  async acquireLease(key: Id, rec: OutboxRecord, now: number): Promise<boolean> {
    const recs = this.read();
    const existing = recs.find((r) => r.commandId === key);
    const lease = existing?.lease;
    if (lease && rec.lease && lease.owner !== rec.lease.owner && lease.until > now) {
      return false;
    }
    this.write([...recs.filter((r) => r.commandId !== key), rec]);
    return true;
  }

  async releaseLease(key: Id, owner: Id): Promise<boolean> {
    // One synchronous read-check-write turn: delete only while the stored
    // lease owner matches (gate 15 item 1).
    const recs = this.read();
    const existing = recs.find((r) => r.commandId === key);
    if (!existing || existing.lease?.owner !== owner) return false;
    this.write(recs.filter((r) => r.commandId !== key));
    return true;
  }

  /**
   * Synchronous read-check-write turn — atomic within the supported
   * single-tab degraded case (two same-tab callers cannot interleave); the
   * cross-tab guarantee is the IndexedDB transaction in the primary store.
   */
  async mergeUnlessDone(commandId: Id, patch: Partial<OutboxRecord>): Promise<OutboxRecord | null> {
    const recs = this.read();
    const existing = recs.find((r) => r.commandId === commandId);
    if (!existing) return null;
    if (existing.state === "done") return existing;
    const merged = { ...existing, ...patch };
    this.write([...recs.filter((r) => r.commandId !== commandId), merged]);
    return merged;
  }

  /**
   * Conditional reclaim as one synchronous read-check-write turn — atomic in
   * the supported single-tab degraded case the same way mergeUnlessDone is.
   */
  async reclaimInflight(
    commandId: Id,
    expected: { owner: Id; until: number },
    opts?: { requireExpired?: boolean; signal?: AbortSignal },
  ): Promise<OutboxRecord | null> {
    // Gate 13 item 3: a dispose must prevent the write even in the synchronous
    // degraded backend (the caller is suspended at an await boundary).
    if (opts?.signal?.aborted) throw new DOMException("reclaim aborted", "AbortError");
    const recs = this.read();
    const existing = recs.find((r) => r.commandId === commandId);
    if (!existing) return null;
    const lease = existing.lease;
    // Gate 13 item 2: the wake path proves expiry AGAIN here (wall clock may
    // have rolled back since the timer fired).
    const expired = !opts?.requireExpired || (!!lease && lease.until <= Date.now());
    if (
      existing.state !== "inflight" ||
      !lease ||
      lease.owner !== expected.owner ||
      lease.until !== expected.until ||
      !expired
    ) {
      return existing;
    }
    if (opts?.signal?.aborted) throw new DOMException("reclaim aborted", "AbortError");
    const reclaimed: OutboxRecord = { ...existing, state: "pending", lease: undefined };
    this.write([...recs.filter((r) => r.commandId !== commandId), reclaimed]);
    return reclaimed;
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
  /**
   * PER-DOCUMENT identity for the no-Web-Locks instance mutex (c-reconnfu
   * gate 15 item 1): fresh for every Outbox instance/document and NEVER
   * persisted. It is deliberately distinct from {@link owner}, the persisted
   * sessionStorage id reused across reloads for crash-recovery reclaim: two
   * documents that cloned sessionStorage share `owner`, but their
   * `leaseToken`s differ, so `acquireLease` refuses the second document
   * instead of granting both the same instance mutex (which let both POST).
   */
  private readonly leaseToken: Id = newCommandId();
  /**
   * The SINGLE lease-wake bookkeeping (c-reconnfu gate 20): one armed wake per
   * command this document is not itself POSTing but owes recovery for.
   *
   *  - kind "lease":   an inactive inflight row with a LIVE lease (a foreign
   *                    owner, or a cloned same-owner sibling — the policy is
   *                    owner-agnostic) waited out until `lease.until`;
   *  - kind "deliver": a deliverable row (pending / lease-less / stealable)
   *                    whose guarded delivery is not currently running. Fires
   *                    immediately the first time, then on the bounded backoff
   *                    the store records after a failed delivery attempt.
   *
   * Held rows never appear (the store's held-retry clock owns them) and
   * neither do rows in {@link activeDeliveries} (the live POST is the
   * in-flight delivery). Every snapshot reader — timer fire, in-lock
   * deliverer read, store flush, unload rearm — re-applies the SAME central
   * reconciliation instead of cancelling/arming wakes itself.
   */
  private leaseWakeups = new Map<
    Id,
    {
      instanceId: Id;
      kind: "lease" | "deliver";
      /** Exact live-lease snapshot for kind "lease" (adoption/rollback check). */
      lease?: { owner: Id; until: number };
      failures: number;
      /** Scheduled fire time, epoch ms (lease deadline or backoff instant). */
      at: number;
      handle: ReturnType<typeof setTimeout>;
    }
  >();
  /**
   * Bounded per-instance failure count for delivery attempts that could not
   * make progress (pre-lock/in-lock durable read failed, the claim transaction
   * aborted, the instance mutex was held): the next "deliver" wake for the
   * instance backs off 250 ms·2^n instead of firing immediately. Reset the
   * moment one of the instance's rows is actually POSTed.
   */
  private deliveryFailures = new Map<Id, number>();
  /**
   * Instances whose bounded DELIVERY budget is spent (gate 22 item 3): a
   * timer path whose lock request keeps rejecting (or the no-Web-Locks lease
   * wait keeps timing out) while reads succeed reaches the surfaced exhausted
   * state and arms no further wake until a real POST (or teardown) clears it.
   */
  private deliveryExhausted = new Set<Id>();
  /**
   * Per-FLUSH suppression of 0 ms delivery wakes (c-reconnfu gate 21 item 1):
   * each guarded flush receives a TOKEN scoped to the instances ITS pass read.
   * The count is REFERENCE-COUNTED because flushes overlap (a timer fires
   * while another flush is waiting on a contended instance lock): one flush
   * ending must never clear the suppression another flush still relies on
   * (the old shared Set let the first leaver drop the other's responsibility).
   * When the last owner releases an instance it disappears from
   * {@link deliverySuppressed}, and the ending flush's FRESH authoritative
   * reconciliation then re-arms every leftover row's one wake.
   */
  private deliverySuppressed = new Map<Id, number>();
  private deliveryPasses = new Map<number, Set<Id>>();
  private nextDeliveryPassToken = 1;
  /**
   * Instances whose next delivery is already owned by the STORE's bounded
   * `scheduleOutboxRetry` timer (an attempted POST failed at the network
   * boundary and the row was restored to lease-less pending). A flush
   * epilogue must NOT additionally arm an Outbox delivery wake for such an
   * instance — the store retry (and an online/resume flush) owns it; arming a
   * second 0 ms wake made the offline-restore path flap wake→flush and miss
   * the reconnect-driven delivery. The set is cleared when the store timer
   * fires or the store tears its retry down.
   */
  private storeRetryPending = new Set<Id>();
  /**
   * Single bounded READ-RETRY chain for a rejected authoritative read (timer
   * fire or store flush). The retry ends in the SAME central reconciliation;
   * teardown/dispose cancels it. `promise` lets concurrent callers coalesce.
   */
  private readRetry: {
    failures: number;
    handle: ReturnType<typeof setTimeout> | null;
    promise: Promise<OutboxRecord[] | null>;
  } | null = null;
  /**
   * Consecutive failed authoritative read attempts in the current recovery
   * chain (c-reconnfu gate 21 item 3): the backoff grows with this count
   * instead of re-entering at attempt 0 after every scheduled retry.
   */
  private readRecoveryFailures = 0;
  /** Latched once the bounded read-retry envelope is exhausted; cleared by a successful read. */
  private readRecoveryExhausted = false;
  /**
   * Set by {@link dispose}. A deadline callback already suspended inside
   * storage.all()/reclaimInflight when dispose() ran can resume afterwards;
   * it must then neither reclaim/cache nor re-arm nor fire the delivery
   * callback (gate 12 item 3). Checked at every async boundary.
   */
  private disposed = false;
  /**
   * Set by the store on pagehide/beforeunload, cleared when the page stays.
   * While true a reconciliation applies NO bookkeeping and a deadline fire
   * does no work: a read suspended across a genuine reload teardown must not
   * arm a timer (or trigger a flush) into the tearing-down page. BFCache
   * restore / the cancelled-prompt self-reset clears it and re-enters via a
   * fresh read.
   */
  private unloading = false;
  /**
   * Commands THIS document is currently POSTing (claimed via
   * {@link beginActiveDelivery}/{@link endActiveDelivery}). While present, a
   * durable inflight row for the command belongs to this live delivery and
   * must NOT get an expiry wake of its own (c-reconnfu gate 15 item 2).
   */
  private activeDeliveries = new Set<Id>();

  /**
   * Store hook fired when a "deliver" wake fires and an authoritative read
   * under the instance lock found the row deliverable: the store runs its
   * single-deliverer GUARDED flush (the Outbox itself never POSTs). The store
   * is expected to suppress currently-held rows there — a held row's only
   * wake is its held-retry timer (gate 20 item 3).
   */
  onClaimWake: (() => void) | null = null;
  /**
   * Store hook fired when the central reconciliation observes a HELD row that
   * currently has no other owner in this document: the store's bounded
   * held-retry timer (same-id re-POST without spending the attempt budget) is
   * that row's only clock, whether this tab delivered the held answer itself
   * or first saw the row held by a sibling. Fired from every reconciliation
   * pass; the store's arm is idempotent.
   */
  onHeldLease: ((instanceId: Id) => void) | null = null;
  /**
   * Fired ONCE per spent bounded envelope when automatic recovery has
   * stopped: authoritative durable READS failing for the whole read-retry
   * envelope (gate 21 item 3), or an instance's DELIVERY attempts (timer
   * lock requests that keep rejecting while reads succeed, gate 22 item 3)
   * spending their budget. The store tells the user and relies on a later
   * fresh trigger (manual/online flush, rearm, reload, or a later POST).
   */
  onReadRecoveryExhausted: (() => void) | null = null;
  /**
   * Fired when the condition behind a surfaced exhausted latch cleared (a
   * successful read after read exhaustion, or a real POST after delivery
   * exhaustion), so the store can clear its surface.
   */
  onReadRecoveryRecovered: (() => void) | null = null;

  private constructor(storage: OutboxStorage, owner?: Id) {
    this.storage = storage;
    this.owner = owner ?? `owner_${newCommandId()}`;
  }

  /**
   * Owner id stable across RELOAD/SW RESTORE but unique per TAB:
   *
   * `sessionStorage` is per-tab and survives same-document reload and
   * SW-controlled restore (the offline-outbox scenario), so a destroyed
   * context's own inflight claim is reclaimed immediately on load while a
   * LIVE sibling tab is a DIFFERENT owner — its claim is neither stolen nor
   * requeued (c-reconnfu gate 8 item 4: the per-profile localStorage owner
   * made every sibling tab identical, so load requeued a live tab's in-flight
   * POST and the no-Web-Locks fallback let two deliverers run). A genuinely
   * crashed tab's lease still passes via the 30 s foreign-lease TTL.
   */
  static tabOwner(): Id {
    const KEY = "remuda-outbox-owner";
    try {
      const existing = sessionStorage.getItem(KEY);
      if (existing) return existing;
      const created = `owner_${newCommandId()}`;
      sessionStorage.setItem(KEY, created);
      return created;
    } catch {
      // Storage locked down (private/cookies disabled): one per context, the
      // foreign-lease TTL is the only cross-context safety net.
      return `owner_${newCommandId()}`;
    }
  }

  static async load(storage: OutboxStorage, owner?: Id): Promise<Outbox> {
    const box = new Outbox(storage, owner ?? Outbox.tabOwner());
    for (const rec of (await storage.all()).filter((r) => commandIdFromKey(r.commandId))) {
      box.cache.set(rec.commandId, rec);
    }
    const now = Date.now();
    // c-reconnfu gate 11 item 3 (gate 10 item 6 corrected): a same-owner
    // FRESH lease may belong to a LIVE sibling tab/duplicated window that
    // cloned sessionStorage (same owner id). Before reclaiming it, ask the Web
    // Locks manager whether the instance delivery lock — the SAME name
    // acquireLock requests (instanceDeliveryLockName) — is held in THIS
    // browser context. Three outcomes:
    //  - held:    a live same-owner cloned tab is delivering; keep the fresh
    //             claim until its lease expires, never requeue under it;
    //  - free:    no holder → this profile's PREVIOUS context left the claim
    //             (a fresh context owns the fetch lifecycle) → reclaim now;
    //  - unknown: the Web Locks API is absent OR locks.query threw. We cannot
    //             tell a live cloned tab from a dead context → fail safe and
    //             keep the fresh lease until it expires (no immediate reclaim).
    let locksQueryOk = false;
    const locksHeld = new Set<string>();
    if (typeof navigator !== "undefined" && typeof navigator.locks?.query === "function") {
      try {
        const snapshot = await navigator.locks.query();
        for (const lock of snapshot.held ?? []) {
          if (typeof lock.name === "string") locksHeld.add(lock.name);
        }
        locksQueryOk = true;
      } catch {
        // Query failure is UNKNOWN ownership, not proof the context is dead.
        locksQueryOk = false;
      }
    }
    const requeue: { id: Id; instanceId: Id; expected: { owner: Id; until: number } }[] = [];
    for (const [id, rec] of box.cache) {
      if (rec.state !== "inflight" || !rec.lease) continue;
      if (rec.lease.owner === box.owner) {
        const ownership: "held" | "free" | "unknown" = !locksQueryOk
          ? "unknown"
          : locksHeld.has(instanceDeliveryLockName(rec.instanceId))
            ? "held"
            : "free";
        if (ownership !== "free" && rec.lease.until > now) {
          // Held by a live cloned tab, or ownership unknowable: the fresh
          // same-owner lease stands until its deadline. This load arms
          // NOTHING — the store's boot flush reads the authoritative set and
          // the central reconcileLeaseWakes arms the one deadline wake for it
          // (a cloned tab may be gone by the deadline; the in-lock read at
          // fire time re-judges). Until then this context neither requeues
          // nor delivers.
          continue;
        }
        requeue.push({ id, instanceId: rec.instanceId, expected: rec.lease });
      } else if (rec.lease.until <= now) {
        // A row left inflight by another (possibly crashed) process is
        // returned to a deliverable state once its lease expires; the durable
        // lease is re-judged inside the delivery lock.
        requeue.push({ id, instanceId: rec.instanceId, expected: rec.lease });
      }
    }
    // Persist each reclaim ATOMICALLY (c-reconnfu gate 11 item 4): the
    // transition to pending happens inside one transaction and ONLY while the
    // durable row is still inflight with EXACTLY the lease snapshot observed
    // here — a row another tab moved to sent/reconciling/held/done while this
    // load was in flight is returned verbatim and never downgraded. Where the
    // Web Lock exists the reclaim runs INSIDE it, so it is serialized against
    // the live tab's deliverer (the callback waits until A releases, by which
    // time the conditional sees the terminal row). Gate 10 item 8: retry a
    // one-shot transaction abort once; a second failure leaves the row
    // inflight and the expiry wakeup (item 5) owns the next attempt.
    await Promise.all(
      requeue.map(async ({ id, instanceId, expected }) => {
        const reclaimOnce = (): Promise<OutboxRecord | null> =>
          storage.reclaimInflight(id, expected);
        const reclaim = async (): Promise<OutboxRecord | null> => {
          try {
            return await reclaimOnce();
          } catch {
            // one retry on a one-shot abort; second failure falls through
            try {
              return await reclaimOnce();
            } catch {
              return null;
            }
          }
        };
        // Gate 12 item 5: serialize under the instance Web Lock WHENEVER
        // locks.request exists — independently of whether locks.query worked
        // (query failure is unknown OWNERSHIP, not absence of locking).
        const stored = await box.runUnderInstanceLock(instanceId, reclaim);
        if (stored) {
          // Authoritative current row — reclaimed to pending, terminal, or a
          // replacement inflight lease a live owner renewed while this load
          // reclaimed. Gate 13 item 1 (c): an inflight replacement is NEVER
          // terminal here — adopt its lease into the cache; the boot flush's
          // central reconciliation arms the deadline wake (future armed,
          // overdue delivered immediately).
          box.cache.set(id, stored);
        } else {
          // Persist failed twice: the row is still (durable) inflight with
          // its lease. The boot flush's authoritative read and the central
          // reconciliation own the deadline wake / delivery.
        }
      }),
    );
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
   *
   * c-reconnfu gate 22 item 1 (coordinator ruling): the durable commit IS the
   * success, full stop. The authoritative re-read that follows runs the
   * shared post-commit epilogue {@link reconcileAfterCommittedWrite}; if THAT
   * read rejects it only starts the bounded, lifetime-aware recovery — it
   * never rejects the enqueue (which used to make store.send report failure /
   * mint no visible intent for a row that had already committed with its one
   * command id) and it never triggers a second enqueue for the same intent.
   */
  async enqueue(
    rec: Omit<OutboxRecord, "attempts" | "state"> & { state?: OutboxState },
  ): Promise<OutboxRecord> {
    const full: OutboxRecord = { attempts: 0, state: "pending", ...rec };
    await this.storage.put(full);
    this.cache.set(full.commandId, full);
    this.emit();
    this.reconcileAfterCommittedWrite();
    return full;
  }

  /**
   * Storage-first state write with an ATOMIC terminal guarantee: the stored
   * row is re-read inside the same readwrite transaction that writes it, so a
   * `done` committed by a journal callback sharing the connection (its write
   * can still be in flight) or by another tab is never downgraded to
   * sent/held/rejected/unknown by a full record built from a stale cache
   * read. The cache is updated from the AUTHORITATIVE stored row (which may
   * be the preserved done) only after the transaction commits; the write
   * rejecting means the commit failed and the rejection propagates.
   *
   * Gate 22 item 1 ruling: once mergeUnlessDone COMMITTED, a failure of the
   * follow-up read does NOT turn the patch into a failure. The committed row
   * is returned (so a committed CLAIM proceeds to its POST instead of waiting
   * out the 30 s lease as "claim-aborted" and burning an attempt slot); the
   * failed read is owned by the bounded recovery via the shared epilogue.
   */
  async patch(commandId: Id, patch: Partial<OutboxRecord>): Promise<OutboxRecord | null> {
    const stored = await this.storage.mergeUnlessDone(commandId, patch);
    if (!stored) return null;
    this.cache.set(commandId, stored);
    this.emit();
    this.reconcileAfterCommittedWrite();
    return stored;
  }

  async remove(commandId: Id) {
    await this.storage.delete(commandId);
    this.cache.delete(commandId);
    this.emit();
    this.reconcileAfterCommittedWrite();
  }

  /**
   * The SHARED post-commit epilogue for enqueue/patch/remove (gate 22 item 1):
   * re-judge every row from an AUTHORITATIVE durable snapshot. The durable
   * mutation already committed, so a rejected read must never fail the
   * writer's promise — it enters the bounded, lifetime-aware recovery chain
   * (growing backoff, surfaced exhaustion), which ends in the SAME central
   * reconciliation as every other delivery entry point. Deliberately not
   * awaited by the writers: a failing chain can run for seconds, and the
   * committed intent must be visible/acted upon immediately.
   */
  private reconcileAfterCommittedWrite(): void {
    if (this.disposed || this.unloading) return;
    void this.refreshDurableRecovering(0).catch(() => undefined);
  }

  /**
   * Gate 22 item 1 (coordinator ruling): a writer's enqueue/claim call just
   * REJECTED — but a storage promise can reject AFTER the durable mutation
   * committed (a backend whose request errors post-commit, a wrapper failing
   * after the real write). Ask the durable backend with ONE authoritative
   * read whether the command's row exists, i.e. whether the commit in fact
   * landed. That read failing is not a "no": it starts the bounded,
   * lifetime-aware recovery in the BACKGROUND (never awaited — the caller has
   * a rejected promise to settle now) and this resolves null so an
   * unconfirmed mutation stays an honest failure rather than a phantom row.
   */
  async committedIfAny(commandId: Id): Promise<OutboxRecord | null> {
    if (this.disposed) return null;
    try {
      await this.readDurable();
    } catch {
      void this.refreshDurableRecovering(1).catch(() => undefined);
      return null;
    }
    return this.cache.get(commandId) ?? null;
  }

  /**
   * Delay for a "deliver" wake after `failures` unsuccessful delivery
   * attempts on its instance: 0 ms the first time, then capped exponential
   * backoff. The same bounded envelope the old per-claim deadline retry used.
   */
  private deliverWakeDelay(failures: number): number {
    return failures === 0
      ? 0
      : Math.min(CLAIM_WAKE_MAX_RETRY_MS, CLAIM_WAKE_BASE_RETRY_MS * 2 ** (failures - 1));
  }

  /** Arm (or replace) one lease/delivery wake. Fires at spec.at (epoch ms). */
  private armLeaseWake(
    commandId: Id,
    instanceId: Id,
    spec: { kind: "lease" | "deliver"; lease?: { owner: Id; until: number }; failures: number; at: number },
  ) {
    if (this.disposed) return;
    this.cancelLeaseWake(commandId);
    const handle = setTimeout(() => {
      this.leaseWakeups.delete(commandId);
      void this.fireLeaseWake(commandId, instanceId);
    }, Math.max(0, spec.at - Date.now()));
    this.leaseWakeups.set(commandId, {
      instanceId,
      kind: spec.kind,
      lease: spec.lease,
      failures: spec.failures,
      at: spec.at,
      handle,
    });
  }

  private cancelLeaseWake(commandId: Id) {
    const entry = this.leaseWakeups.get(commandId);
    if (entry) {
      clearTimeout(entry.handle);
      this.leaseWakeups.delete(commandId);
    }
  }

  /**
   * Cancel every armed wake and the bounded read retry WITHOUT disposing: a
   * genuine pagehide must leave a tearing-down page with zero timers. A
   * CANCELLED beforeunload re-enters via refreshDurable* (read-only rearm).
   * Also latches {@link unloading} so a read suspended across the teardown
   * applies no reconciliation when it resumes.
   */
  cancelAllWakesForUnload() {
    this.unloading = true;
    for (const entry of this.leaseWakeups.values()) clearTimeout(entry.handle);
    this.leaseWakeups.clear();
    if (this.readRetry) {
      if (this.readRetry.handle) clearTimeout(this.readRetry.handle);
      this.readRetry = null;
    }
    this.readRecoveryFailures = 0;
    this.readRecoveryExhausted = false;
    this.deliveryFailures.clear();
    this.deliveryExhausted.clear();
    this.deliveryPasses.clear();
    this.deliverySuppressed.clear();
    this.storeRetryPending.clear();
  }

  /** The page stayed / was restored (BFCache): allow reconciliation again. */
  clearUnloading() {
    this.unloading = false;
  }

  /** Test/teardown visibility for the set of armed lease/delivery wakes. */
  pendingLeaseWakeups(): Id[] {
    return [...this.leaseWakeups.keys()];
  }

  /** Test visibility: scheduled fire time (epoch ms) of one command's wake. */
  leaseWakeAt(commandId: Id): number | undefined {
    return this.leaseWakeups.get(commandId)?.at;
  }

  /** Test visibility: kind of an armed wake. */
  leaseWakeKind(commandId: Id): "lease" | "deliver" | undefined {
    return this.leaseWakeups.get(commandId)?.kind;
  }

  /**
   * Record a delivery attempt that could not make progress on the instance
   * (durable read failure, claim-transaction abort, contended mutex, or a
   * timer-path lock request that rejects): the next deliver wake for its rows
   * backs off instead of firing immediately. Bounded by
   * CLAIM_WAKE_MAX_FAILURES; spending the bound surfaces the explicit
   * exhausted state ONCE ({@link onReadRecoveryExhausted}) — with the lock
   * persistently rejecting while reads succeed, no wake is armed past it, so
   * without the surfaced latch the stop was silent (gate 22 item 3).
   */
  noteDeliveryFailure(instanceId: Id) {
    const failures = (this.deliveryFailures.get(instanceId) ?? 0) + 1;
    this.deliveryFailures.set(instanceId, failures);
    if (failures >= CLAIM_WAKE_MAX_FAILURES && !this.deliveryExhausted.has(instanceId)) {
      this.deliveryExhausted.add(instanceId);
      this.onReadRecoveryExhausted?.();
    }
  }

  /** A row of the instance was actually POSTed: clear its failure backoff. */
  noteDeliveryProgress(instanceId: Id) {
    const wasExhausted = this.deliveryExhausted.delete(instanceId);
    this.deliveryFailures.delete(instanceId);
    // Mirror the read-repair signal so the store clears the surfaced state
    // after the lock/storage condition that exhausted delivery clears.
    if (wasExhausted) this.onReadRecoveryRecovered?.();
  }

  /** Test visibility: delivery on this instance spent its bounded budget. */
  isDeliveryExhausted(instanceId: Id): boolean {
    return this.deliveryExhausted.has(instanceId);
  }

  /**
   * A guarded delivery attempt could not progress (c-reconnfu gate 20 item 4):
   * the flush's durable read rejected, the deliverer's in-lock read rejected,
   * or the claim storage transaction aborted. The failed attempt KEEPS
   * recovery responsibility: count the bounded per-instance failure and run
   * the lifetime-aware read retry, whose success ENDS IN THE CENTRAL
   * reconciliation — the row keeps exactly one (backed-off) delivery wake and
   * is delivered once the transient clears.
   *
   * With explicit instance ids the failure is attributed to them. Without,
   * the failure was the global flush read: the successful recovery read
   * itself identifies every deliverable instance and the policy is re-applied
   * once after accounting so the new backoff replaces the 0 ms wake the
   * recovery's own pass armed.
   */
  async deliveryAttemptFailed(instanceIds?: Iterable<Id>): Promise<void> {
    if (this.disposed || this.unloading) return;
    const ids = instanceIds ? [...new Set(instanceIds)] : null;
    if (ids) for (const id of ids) this.noteDeliveryFailure(id);
    const rows = await this.refreshDurableRecovering(1);
    if (!rows || this.disposed || this.unloading) return;
    if (!ids) {
      const now = Date.now();
      for (const rec of rows) {
        const d = leaseWakeDecision(rec, {
          now,
          isActiveDelivery: (id) => this.activeDeliveries.has(id),
        });
        if (d.kind === "deliver") this.noteDeliveryFailure(rec.instanceId);
      }
      this.applyLeaseWakeReconcile(rows);
    }
  }

  /**
   * Mark instances whose guarded delivery THIS flush pass is about to run
   * IMMEDIATELY after its snapshot read. Returns an opaque per-pass TOKEN the
   * flush later hands back; suppression is reference-counted so overlapping
   * flushes do not clear one another's cover (gate 21 item 1).
   */
  beginDeliveryPass(ids: Iterable<Id>): number {
    const token = this.nextDeliveryPassToken++;
    const owned = new Set<Id>();
    for (const id of ids) {
      if (owned.has(id)) continue;
      owned.add(id);
      this.deliverySuppressed.set(id, (this.deliverySuppressed.get(id) ?? 0) + 1);
    }
    this.deliveryPasses.set(token, owned);
    return token;
  }

  /** Release ONE pass's suppression without reconciling (teardown only). */
  releaseDeliveryPass(token: number) {
    const owned = this.deliveryPasses.get(token);
    if (!owned) return;
    this.deliveryPasses.delete(token);
    for (const id of owned) {
      const left = (this.deliverySuppressed.get(id) ?? 1) - 1;
      if (left <= 0) this.deliverySuppressed.delete(id);
      else this.deliverySuppressed.set(id, left);
    }
  }

  /**
   * Flush epilogue (gate 21 item 1): release THIS flush's passes and ALWAYS
   * re-judge every row from a FRESH authoritative snapshot. A flush that
   * posted nothing — its instance lock rejected, a claim aborted, or a sibling
   * held the lock past the 250 ms recovery read — used to leave deliverable
   * rows without ANY cover: its recovery read had been skipped as
   * "suppressed" and the old finally only CLEARED the suppression set. The
   * fresh read runs AFTER this flush's counts are gone, so each leftover row
   * gets exactly one (backed-off when an attempt failed) wake; a rejected
   * re-read joins the bounded, lifetime-aware recovery chain.
   */
  async endDeliveryPasses(tokens: Iterable<number>): Promise<void> {
    for (const token of tokens) this.releaseDeliveryPass(token);
    if (this.disposed || this.unloading) return;
    await this.refreshDurableRecovering(0);
  }

  /**
   * Mark a command as being actively delivered BY THIS document (store calls
   * this right after winning its claim, for the duration of the POST). The
   * active POST is that row's in-flight guarded delivery, so the central
   * reconciliation arms nothing for it — even when its lease owner equals
   * this persisted recovery owner (a cloned sibling shares that id). No
   * reconciliation runs from the cache here: the claim patch and the finish
   * patch each reconcile from an AUTHORITATIVE durable snapshot (gate 21
   * item 2), which is when the active flag is observed.
   */
  beginActiveDelivery(commandId: Id) {
    this.activeDeliveries.add(commandId);
  }

  /**
   * The live delivery finished (or was skipped after claim). This only
   * CLEARS the active marker — the usual finish patch already ran its
   * post-commit epilogue. When that finishing write ABORTED (gate 22 item 2)
   * no patch epilogue ran, so a delivery entry point that does not own a
   * flush pass (the live direct-steer route) must follow this with
   * {@link deliveryEpilogue}; the flush route gets the same reconcile from
   * its pass epilogue {@link endDeliveryPasses}.
   */
  endActiveDelivery(commandId: Id) {
    this.activeDeliveries.delete(commandId);
  }

  /**
   * The SHARED post-delivery epilogue for entry points WITHOUT a flush pass
   * (c-reconnfu gate 22 item 2: the live direct-steer route). Runs AFTER
   * {@link endActiveDelivery} cleared the POST marker: re-judge every row
   * from an AUTHORITATIVE durable snapshot. This is what covers a finishing
   * write that ABORTED after the verdict returned — the durable row stays
   * inflight under this owner's LIVE lease, and while the POST was active the
   * central policy armed nothing; the fresh read now arms the row's one
   * deadline wake (so lease expiry recovers it) along with every other row
   * whose cover ended. A rejected read enters the bounded, lifetime-aware
   * recovery, which ends in this same central reconciliation.
   */
  async deliveryEpilogue(): Promise<void> {
    if (this.disposed || this.unloading) return;
    await this.refreshDurableRecovering(0);
  }

  isDeliveryActive(commandId: Id): boolean {
    return this.activeDeliveries.has(commandId);
  }

  /**
   * Run `fn` under the per-instance Web Lock whenever the API exists —
   * independent of whether locks.query was usable at load (gate 12 item 5 for
   * the load path; the deadline path uses it regardless). With no Web Locks
   * the durable IDB/localStorage lease remains the serialization boundary.
   */
  private async runUnderInstanceLock<T>(instanceId: Id, fn: () => Promise<T>): Promise<T> {
    const locks = (globalThis as { navigator?: Navigator & { locks?: LockManager } }).navigator?.locks;
    if (locks?.request) {
      return await locks.request(instanceDeliveryLockName(instanceId), { mode: "exclusive" }, fn);
    }
    return await fn();
  }

  /**
   * A lease/delivery wake fired. Take the instance lock and read the
   * AUTHORITATIVE durable snapshot; the CENTRAL reconciliation settles every
   * row — including the firing row's live-lease deadline (the timer path
   * never arms a lease directly, c-reconnfu gate 21 item 2). The firing
   * command's own handoff is decided from the same snapshot:
   *  - deliver: hand the store one guarded delivery trigger;
   *  - held:    the store's held-retry clock is the only allowed timer;
   *  - deadline/quiet/missing: the table already armed/cancelled it.
   * A failed read or lock never ends recovery in silence: the bounded,
   * lifetime-aware read-retry ends in the same central reconciliation.
   */
  private async fireLeaseWake(commandId: Id, instanceId: Id) {
    if (this.disposed || this.unloading) return;
    let durable: OutboxRecord[] | undefined;
    let lockEntered = false;
    try {
      await this.runUnderInstanceLock(instanceId, async () => {
        lockEntered = true;
        durable = await this.readDurable();
        if (!this.disposed && !this.unloading) {
          this.applyLeaseWakeReconcile(durable, { firingCommandId: commandId });
        }
      });
    } catch {
      // Gate 22 item 3: distinguish a LOCK failure (navigator.locks.request
      // rejected before the callback ran, or the no-Web-Locks lease wait
      // timed out) from a failed READ under an acquired lock. The lock
      // failure is attributed to the instance AS A DELIVERY FAILURE — reads
      // still succeed, so charging the read envelope would never spend the
      // delivery budget or surface exhaustion. The authoritative read below
      // re-enters the central reconciliation, which arms the one backed-off
      // deliver wake; at the bounded cap nothing is armed and the exhausted
      // state is surfaced (no further wakes).
      if (lockEntered) {
        // Lock acquired: the read (or reconcile) rejected — bounded,
        // lifetime-aware read recovery ends in the same reconcile.
        await this.refreshDurableRecovering(1);
        return;
      }
      this.noteDeliveryFailure(instanceId);
      await this.refreshDurableRecovering(0);
      return;
    }
    if (this.disposed || this.unloading) return;
    const current = durable!.find((r) => r.commandId === commandId);
    if (!current) return; // missing: the reconciliation already dropped it
    const decision = leaseWakeDecision(current, {
      now: Date.now(),
      isActiveDelivery: (id) => this.activeDeliveries.has(id),
    });
    if (decision.kind === "deliver") {
      // Exactly one guarded delivery trigger for this wake; the store flush
      // re-reads authoritatively under the lock and makes the durable claim.
      this.emit();
      this.onClaimWake?.();
      return;
    }
    if (decision.kind === "held") this.onHeldLease?.(decision.instanceId);
    // deadline: the central read above adopted the authoritative lease wake;
    // quiet/missing: it cancelled the wake — nothing else to do.
  }

  /** Read durable rows into the storage-first cache; no other side effect. */
  async readDurable(): Promise<OutboxRecord[]> {
    const durable = (await this.storage.all()).filter((r) => commandIdFromKey(r.commandId));
    if (this.disposed) return [];
    // Gate 13 item 3: check lifetime RIGHT after the read and BEFORE touching
    // the cache — a dispose while all() was suspended imports no rows.
    for (const rec of durable) this.cache.set(rec.commandId, rec);
    return durable;
  }

  /**
   * Apply the CENTRAL wake reconciliation (c-reconnfu gate 20) to an
   * authoritative durable snapshot — the ONLY place lease/delivery wakes are
   * armed or cancelled. For every row the policy table decides deliver /
   * deadline / held / quiet, and the bookkeeping enforces exactly one cover:
   * an armed wake, or the in-flight/imminent guarded delivery.
   */
  applyLeaseWakeReconcile(durable: OutboxRecord[], opts: { firingCommandId?: Id } = {}) {
    if (this.disposed || this.unloading) return;
    const now = Date.now();
    const decisions = reconcileLeaseWakes(durable, {
      now,
      isActiveDelivery: (id) => this.activeDeliveries.has(id),
    });
    const byId = new Map(decisions.map((d) => [d.commandId, d]));
    // The FIRING row settles itself when its decision is deliver/held (its
    // fire hands off to the guarded flush / held-retry clock below), so the
    // table must not also arm a duplicate wake for it. A firing row with a
    // live/renewed lease IS armed here: the timer path never arms a lease
    // directly (c-reconnfu gate 21 item 2).
    const firingSettlesItself = (id: Id) => {
      if (id !== opts.firingCommandId) return false;
      const k = byId.get(id)?.kind;
      return k === "deliver" || k === "held";
    };
    // Reconcile EXISTING wakes against the fresh decisions.
    for (const [id, entry] of [...this.leaseWakeups]) {
      if (firingSettlesItself(id)) continue;
      const d = byId.get(id);
      if (!d || d.kind === "quiet") {
        this.cancelLeaseWake(id);
        continue;
      }
      if (d.kind === "held") {
        this.cancelLeaseWake(id);
        this.onHeldLease?.(d.instanceId);
        continue;
      }
      if (d.kind === "deadline" && d.lease) {
        if (
          entry.kind === "lease" &&
          entry.lease &&
          entry.lease.owner === d.lease.owner &&
          entry.lease.until === d.lease.until
        ) {
          continue; // identical live lease: keep the one armed wake
        }
        this.armLeaseWake(id, d.instanceId, {
          kind: "lease",
          lease: d.lease,
          failures: 0,
          at: d.lease.until,
        });
        continue;
      }
      // Deliverable NOW: arm the bounded 0 ms/backoff delivery wake unless the
      // guarded delivery for this instance is already running/imminent. An
      // already-armed deliver wake whose failure count changed moves to the
      // new backoff instant (a failed attempt's recovery re-applies here).
      if (entry.kind === "deliver") {
        const failures = this.deliveryFailures.get(d.instanceId) ?? 0;
        if (entry.failures !== failures) this.armDeliverWake(id, d.instanceId, now);
        continue;
      }
      if (this.deliveryCovered(d.instanceId)) {
        this.cancelLeaseWake(id);
        continue;
      }
      this.armDeliverWake(id, d.instanceId, now);
    }
    // Discover rows with no wake yet.
    for (const d of decisions) {
      if (firingSettlesItself(d.commandId)) continue;
      if (this.leaseWakeups.has(d.commandId)) continue;
      if (d.kind === "deadline" && d.lease) {
        this.armLeaseWake(d.commandId, d.instanceId, {
          kind: "lease",
          lease: d.lease,
          failures: 0,
          at: d.lease.until,
        });
      } else if (d.kind === "held") {
        this.onHeldLease?.(d.instanceId);
      } else if (d.kind === "deliver") {
        if (this.deliveryCovered(d.instanceId)) continue;
        this.armDeliverWake(d.commandId, d.instanceId, now);
      }
    }
  }

  /**
   * True when a guarded delivery for the instance is already running or
   * promised by the store's flush loop: its deliverable rows need no wake.
   */
  private deliveryCovered(instanceId: Id): boolean {
    return (
      this.flushChain.has(instanceId) ||
      this.queuedFlush.has(instanceId) ||
      this.deliverySuppressed.has(instanceId) ||
      this.storeRetryPending.has(instanceId)
    );
  }

  /** Store marks an instance whose next delivery its own retry timer owns. */
  noteStoreRetryScheduled(instanceId: Id) {
    this.storeRetryPending.add(instanceId);
  }

  /** Store clears the marker when its retry fires or it tears the timer down. */
  clearStoreRetry(instanceId: Id) {
    this.storeRetryPending.delete(instanceId);
  }

  /** Arm a deliver wake at the instance's current bounded backoff instant. */
  private armDeliverWake(commandId: Id, instanceId: Id, now: number) {
    const failures = this.deliveryFailures.get(instanceId) ?? 0;
    if (failures >= CLAIM_WAKE_MAX_FAILURES) return; // bounded; a later trigger re-enters
    this.armLeaseWake(commandId, instanceId, {
      kind: "deliver",
      failures,
      at: now + this.deliverWakeDelay(failures),
    });
  }

  /** Authoritative read + central reconcile (store flush, timer, writes). */
  async refreshDurable(): Promise<OutboxRecord[]> {
    const durable = await this.readDurable();
    if (!this.disposed && !this.unloading) this.applyLeaseWakeReconcile(durable);
    return durable;
  }

  /**
   * Authoritative read + central reconcile with a BOUNDED, lifetime-aware
   * retry when storage rejects: the failed read keeps recovery responsibility
   * and the retry ENDS IN THE SAME central reconciliation — a deliverable row
   * never loses its only wake to a one-shot IndexedDB error. Concurrent
   * callers coalesce onto the one retry chain.
   *
   * The consecutive-failure count is carried ACROSS retry attempts (gate 21
   * item 3): delays grow 250 ms·2^(n-1) capped at 5 s, and after
   * CLAIM_WAKE_MAX_FAILURES real read attempts the chain stops and surfaces
   * an explicit exhausted state ({@link onReadRecoveryExhausted}) instead of
   * re-entering at attempt 0 forever. A later fresh trigger (failures=0), a
   * user/online flush, an unload-cancel rearm or a reload tries again; the
   * first successful read clears the latch.
   *
   * `failures` is the count of consecutive failures the CALLER already
   * observed (a timer fire or the flush passes 1); 0 attempts a fresh read
   * immediately. Resolves null when disposed or the budget is exhausted.
   */
  async refreshDurableRecovering(failures = 0): Promise<OutboxRecord[] | null> {
    if (this.disposed || this.unloading) return null;
    if (this.readRetry) return await this.readRetry.promise;
    if (failures === 0) return await this.attemptReadRecovery(0);
    return await this.scheduleReadRetry(failures);
  }

  /** One authoritative read attempt carrying `failures` prior failures. */
  private attemptReadRecovery(failures: number): Promise<OutboxRecord[] | null> {
    if (this.disposed || this.unloading) return Promise.resolve(null);
    const entry: {
      failures: number;
      handle: ReturnType<typeof setTimeout> | null;
      promise: Promise<OutboxRecord[] | null>;
    } = { failures, handle: null, promise: undefined as never };
    entry.promise = (async () => {
      try {
        const rows = await this.readDurable();
        if (this.disposed) return null;
        if (this.readRecoveryFailures !== 0 || this.readRecoveryExhausted) {
          this.readRecoveryFailures = 0;
          this.readRecoveryExhausted = false;
          this.onReadRecoveryRecovered?.();
        }
        this.applyLeaseWakeReconcile(rows);
        return rows;
      } catch {
        if (this.disposed || this.unloading) return null;
        return await this.scheduleReadRetry(failures + 1);
      } finally {
        if (this.readRetry === entry) this.readRetry = null;
      }
    })();
    this.readRetry = entry; // coalesce concurrent callers on the live attempt
    return entry.promise;
  }

  /** Schedule the next bounded read attempt, or surface the exhausted state. */
  private scheduleReadRetry(failures: number): Promise<OutboxRecord[] | null> {
    if (this.disposed || this.unloading) return Promise.resolve(null);
    this.readRecoveryFailures = failures;
    if (failures >= CLAIM_WAKE_MAX_FAILURES) {
      // The bounded envelope is spent: no more silent 250 ms re-entries. The
      // state is explicit so the store surfaces it; a fresh trigger restarts.
      if (!this.readRecoveryExhausted) {
        this.readRecoveryExhausted = true;
        this.onReadRecoveryExhausted?.();
      }
      return Promise.resolve(null);
    }
    const delay = Math.min(CLAIM_WAKE_MAX_RETRY_MS, CLAIM_WAKE_BASE_RETRY_MS * 2 ** (failures - 1));
    const entry: {
      failures: number;
      handle: ReturnType<typeof setTimeout>;
      promise: Promise<OutboxRecord[] | null>;
    } = { failures, handle: undefined as never, promise: undefined as never };
    entry.promise = new Promise<OutboxRecord[] | null>((resolve) => {
      entry.handle = setTimeout(() => {
        if (this.readRetry === entry) this.readRetry = null;
        void this.attemptReadRecovery(failures).then(resolve, () => resolve(null));
      }, delay);
      // Published inside the executor so concurrent callers coalesce onto
      // this chain before the scheduled read.
      this.readRetry = entry;
    });
    return entry.promise;
  }

  /** Consecutive failed authoritative read attempts in the current chain. */
  readRecoveryFailureCount(): number {
    return this.readRecoveryFailures;
  }

  /** True after the bounded read-retry envelope was exhausted (until recovery). */
  isReadRecoveryExhausted(): boolean {
    return this.readRecoveryExhausted;
  }

  /** Delay (ms) the next read retry waits after `failures` consecutive failures. */
  readRetryDelay(failures: number): number {
    return Math.min(CLAIM_WAKE_MAX_RETRY_MS, CLAIM_WAKE_BASE_RETRY_MS * 2 ** Math.max(0, failures - 1));
  }

  /**
   * Cancel every tracked claim wakeup (page teardown / logout / test
   * disposal). Permanent for this instance: a deadline callback suspended in
   * a storage call when dispose runs is neutralized by the `disposed` flag
   * (no reclaim side effects, no re-arm, no delivery), and the store's
   * delivery callback is dropped so a late wake can never flush a logged-out
   * auth epoch (gate 12 item 3).
   */
  dispose() {
    this.disposed = true;
    this.cancelAllWakesForUnload();
    this.activeDeliveries.clear();
    this.onClaimWake = null;
    this.onHeldLease = null;
    this.onReadRecoveryExhausted = null;
    this.onReadRecoveryRecovered = null;
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
      return await locks.request(instanceDeliveryLockName(instanceId), { mode: "exclusive" }, () =>
        this.runDeliverer(instanceId, fn),
      );
    }
    return await this.runWithLease(instanceId, fn);
  }

  /**
   * Build the deliverable set from FRESH durable command rows inside the lock.
   * The in-lock read is itself an authoritative snapshot: run the CENTRAL
   * reconciliation from it (deadline/held wakes for rows this turn does not
   * touch), then hand the deliverable selection to the deliverer. Its own
   * deliverable rows need no delivery wakes — flushChain marks the guarded
   * delivery in flight; a read failure here rejects the lock turn and the
   * store's bounded failure path re-enters the reconciliation (gate 20
   * item 4).
   */
  private async runDeliverer<T>(
    instanceId: Id,
    fn: (instanceId: Id, deliverable: OutboxRecord[]) => Promise<T>,
  ): Promise<T> {
    const now = Date.now();
    let durable: OutboxRecord[];
    try {
      durable = await this.readDurable();
    } catch {
      // c-reconnfu gate 20 item 4: the in-lock durable read rejected. No POST
      // may run on a stale selection: keep recovery responsibility with the
      // bounded, lifetime-aware retry that ends in the central reconciliation,
      // and hand the deliverer an EMPTY selection for this turn.
      void this.deliveryAttemptFailed([instanceId]);
      return await fn(instanceId, []);
    }
    if (!this.disposed) this.applyLeaseWakeReconcile(durable);
    const deliverable = durable
      .filter((r) => r.instanceId === instanceId && isDeliverable(r, now))
      .sort(
        (a, b) =>
          this.rank(a) - this.rank(b) || a.createdAt - b.createdAt || a.commandId.localeCompare(b.commandId),
      );
    return await fn(instanceId, deliverable);
  }

  private leaseKey(instanceId: Id): Id {
    return `__lock__:${instanceId}`;
  }

  /**
   * Durable lease fallback (no Web Locks). The per-instance lease lives in the
   * same store; the claim is a SINGLE atomic readwrite transaction
   * ({@link OutboxStorage.acquireLease}) — read `__lock__:<instanceId>`, write
   * only when it is absent/expired/already ours — so two tabs without Web
   * Locks cannot both pass a check-then-act pair and run two deliverers
   * (which would double-POST). A live foreign lease parks the caller until the
   * owner releases or (if it crashed) the lease passes {@link LEASE_TTL_MS}.
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
      const now = Date.now();
      const leaseRec: OutboxRecord = {
        commandId: key,
        clientRequestId: key,
        instanceId,
        prompt: "",
        createdAt: now,
        attempts: 0,
        state: "inflight",
        // Per-DOCUMENT token, not the persisted (cloned) recovery owner:
        // another document must not be granted this mutex even if it cloned
        // this tab's sessionStorage owner id (gate 15 item 1).
        lease: { owner: this.leaseToken, until: now + LEASE_TTL_MS },
      };
      let acquired = false;
      try {
        acquired = await this.storage.acquireLease(key, leaseRec, now);
      } catch {
        // A storage failure during the claim must never look like a grant:
        // stay out of delivery and retry within the bounded deadline.
        acquired = false;
      }
      if (acquired) {
        try {
          return await this.runDeliverer(instanceId, fn);
        } finally {
          // Release ONLY our own mutex — a successor that stole the lease
          // after TTL expiry must keep its lock (gate 15 item 1).
          await this.storage.releaseLease(key, this.leaseToken).catch(() => undefined);
        }
      }
      if (Date.now() > deadline) throw new Error("outbox lease wait timed out");
      await new Promise((r) => setTimeout(r, 250));
    }
  }
}
