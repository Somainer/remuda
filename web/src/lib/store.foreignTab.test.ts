import { afterEach, expect, it, vi } from "vitest";
import type { ConnectionMachine } from "./connection";
import { REST_PROBE_MS } from "./connection";
import { CLAIM_WAKE_MAX_FAILURES, OUTBOX_LS_KEY, type OutboxRecord } from "./outbox";

const INSTANCE = "ins_foreign_tab";
const JOURNAL = "obj_foreign_tab_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

/**
 * c-reconnfu item 6: a surviving tab loaded before another tab persisted rows
 * must still deliver them. Delivery chose instance locks from the load-time
 * cache, so foreign instances were never attempted, and a foreign in-flight
 * lease (whose owner crashed) had no wakeup at expiry.
 */
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

async function bootLive(api: Api, hubStore: Store) {
  vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null } as Awaited<
    ReturnType<Api["hostList"]>
  >);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["deviceList"]>>);
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as Awaited<
    ReturnType<Api["passkeyList"]>
  >);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "0", durableSeq: "0", lifecycle: "running" } as never],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  await hubStore.bootstrap();
}

function row(over: Partial<OutboxRecord>): OutboxRecord {
  return {
    commandId: over.commandId ?? "cmd_foreign",
    clientRequestId: "local_foreign",
    instanceId: INSTANCE,
    journalId: JOURNAL,
    prompt: "from another tab",
    createdAt: Date.now() - 1_000,
    attempts: 0,
    state: "pending",
    ...over,
  };
}

it("GATE16 item 4a: two documents with a cloned owner deliver a shared pending row exactly once (real store flushes, no Web Locks/IDB)", async () => {
  // Production no-Web-Locks path through TWO HubStores sharing the
  // localStorage backend and a cloned sessionStorage owner id: their flushes
  // race on the per-document leaseToken mutex; api.instanceSend must fire
  // exactly once (the gate-15 idbTx test only counted its own posts.push).
  const owner = "owner_g16_4a";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: { query: async () => ({ held: [] }) } }, // query present, NO request
  });
  // Two documents share the persisted owner (simulate cloned sessionStorage).
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  localStorage.setItem("remuda-outbox-owner", owner);
  try {
    const { api, hubStore: a } = await fresh();
    // B must read the same persisted owner: share its sessionStorage by setting
    // the key before the second module loads (fresh() resets modules but keeps
    // localStorage/sessionStorage globals).
    sessionStorage.setItem("remuda-outbox-owner", owner);
    const { api: apiB, hubStore: b } = await fresh();

    await bootLive(api, a);
    await bootLive(apiB, b);
    // Settle the fire-and-forget bootstrap flushes against the EMPTY outbox
    // so they cannot race the row seeded below through the unmocked API.
    await (a as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await (b as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();

    // Shared pending row in the localStorage backend (both stores read it).
    const row: OutboxRecord = {
      commandId: "cmd_4a_once",
      clientRequestId: "local_4a",
      instanceId: INSTANCE,
      journalId: JOURNAL,
      prompt: "one",
      createdAt: Date.now() - 1_000,
      attempts: 0,
      state: "pending",
    };
    localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([row]));

    const sendA = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, _p: string, _a: unknown, _m: unknown, commandId: string) => {
        // Hold A's POST open long enough for B to attempt delivery too.
        await new Promise<void>((r) => setTimeout(r, 500));
        return {
          relatedCommandIds: [],
          command: {
            commandId,
            id: commandId,
            state: "accepted",
            revision: "1",
            dispatch: "native-acknowledged",
            resolution: "clear",
          },
        } as unknown as Awaited<ReturnType<Api["instanceSend"]>>;
      }) as Api["instanceSend"],
    );
    const sendB = vi.spyOn(apiB, "instanceSend");
    sendB.mockImplementation(sendA.getMockImplementation() ?? (() => Promise.reject(new Error("no impl"))));

    const flushA = (a as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    const flushB = (b as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await flushA;
    await flushB;

    expect(sendA.mock.calls.length + sendB.mock.calls.length).toBe(1);
    // A either won the mutex; B never POSTed (it parked behind the lease and
    // only re-read the authoritative sent row once A released).
    expect(sendB).not.toHaveBeenCalled();
    const stored = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored.find((r) => r.commandId === "cmd_4a_once")?.state).toBe("sent");
    a.logout();
    b.logout();
  } finally {
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
}, 15_000);

it("GATE16 item 4b: a same-owner inflight claim first seen AFTER load is delivered at its lease deadline with exactly one POST and no other trigger", async () => {
  // The gate-15 idbTx test replaced delivery with a bare spy; this asserts the
  // real store: a cloned sibling's inflight row appearing after this store's
  // bootstrap is reclaimed by the Outbox expiry wake at the deadline, the
  // registered onClaimWake -> flushAllOutbox POSTs exactly once.
  vi.useFakeTimers();
  const owner = "owner_g16_4b";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: { query: async () => ({ held: [] }) } }, // query, no request
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  sessionStorage.setItem("remuda-outbox-owner", owner);
  try {
    const { api, hubStore } = await fresh();
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    // Empty boot flush settled (no POST).
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    expect(send).not.toHaveBeenCalled();

    // A sibling (same persisted owner) claims the row AFTER load, then crashes.
    const LEASE_MS = 30_000;
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: "cmd_4b_late",
          clientRequestId: "local_4b",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "late",
          createdAt: Date.now() - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner, until: Date.now() + LEASE_MS },
        } satisfies OutboxRecord,
      ]),
    );
    // The store's periodic durable refresh discovers the late claim; the
    // Outbox reconciles and arms its expiry wake.
    const box = (hubStore as unknown as { outbox: { refreshDurable: () => Promise<unknown> } }).outbox;
    await box.refreshDurable();
    expect(send).not.toHaveBeenCalled();

    // Advance EXACTLY to the deadline; zero extra wall time. Drain only
    // zero-duration microtasks until the single POST lands.
    await vi.advanceTimersByTimeAsync(LEASE_MS);
    for (let i = 0; i < 30 && send.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[4]).toBe("cmd_4b_late");
    const stored = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored.find((r) => r.commandId === "cmd_4b_late")?.state).toBe("sent");

    // No duplicate long afterwards.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(send).toHaveBeenCalledTimes(1);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE19 item 1: a cloned sibling's sole claim timer reads the lease-less PENDING row its dead POST left and delivers (never abandons it)", async () => {
  // A loaded while a CLONED document B (same persisted owner) holds an
  // inflight claim, so A's Outbox tracks the claim's expiry. B's
  // POST/status reconciliation then fails: B restores the row to a
  // lease-less PENDING state and closes BEFORE its in-memory retry runs (a
  // real pagehide clears it). A performs no flush — at T its claim timer is
  // the row's ONLY trigger. The timer's critical section used to classify
  // the non-inflight row as "stop" and returned in silence, so the command
  // was never POSTed on the idle focused tab. It must end in the guarded
  // delivery path: one real POST, exactly once.
  vi.useFakeTimers();
  const owner = "owner_g19_i1";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: { query: async () => ({ held: [] }) } }, // query, no request
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  sessionStorage.setItem("remuda-outbox-owner", owner);
  try {
    const { api, hubStore } = await fresh();
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    const internal = hubStore as unknown as {
      flushAllOutbox: () => Promise<void>;
      outbox: { refreshDurable: () => Promise<unknown>; pendingLeaseWakeups: () => string[] };
    };
    // Settle the empty boot flush (no row yet, no POST).
    await internal.flushAllOutbox();
    expect(send).not.toHaveBeenCalled();

    const cmd = "cmd_g19_i1_pending_after_failed_post";
    const LEASE_MS = 30_000;
    // B (cloned same owner) holds the live inflight claim; A discovers it via
    // a durable refresh and tracks its expiry.
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmd,
          clientRequestId: "local_g19_i1",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "B failed and closed",
          createdAt: Date.now() - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner, until: Date.now() + LEASE_MS },
        } satisfies OutboxRecord,
      ]),
    );
    await internal.outbox.refreshDurable();
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([cmd]);
    expect(send).not.toHaveBeenCalled();

    // B's POST/status reconciliation fails: lease-less pending, then B closes
    // before its in-memory retry (which pagehide would clear anyway).
    await vi.advanceTimersByTimeAsync(2_000);
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmd,
          clientRequestId: "local_g19_i1",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "B failed and closed",
          createdAt: Date.now() - 3_000,
          attempts: 1,
          state: "pending",
          lastError: "network down",
        } satisfies OutboxRecord,
      ]),
    );

    // The tracked expiry timer is the only trigger on an idle tab: it must
    // deliver the pending row exactly once at the deadline.
    await vi.advanceTimersByTimeAsync(LEASE_MS - 2_000);
    for (let i = 0; i < 30 && send.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[4]).toBe(cmd);
    const stored = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored.find((r) => r.commandId === cmd)?.state).toBe("sent");

    // No duplicate long afterwards.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(send).toHaveBeenCalledTimes(1);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

/**
 * FIFO Web Locks shim for gate 21: query reports really held names; the first
 * requester runs, queued requesters run in order on release.
 */
function fifoWebLocks() {
  const heldNow = new Set<string>();
  const queues = new Map<string, Array<() => void>>();
  const pop = (name: string) => queues.get(name)?.shift()?.();
  const locks = {
    query: async () => ({ held: [...heldNow].map((name) => ({ name, mode: "exclusive" })) }),
    request(name: string, _opts: unknown, cb: () => Promise<unknown>) {
      return new Promise((resolve, reject) => {
        const run = () => {
          heldNow.add(name);
          Promise.resolve()
            .then(cb)
            .then(
              (v) => {
                heldNow.delete(name);
                resolve(v);
                pop(name);
              },
              (e) => {
                heldNow.delete(name);
                reject(e);
                pop(name);
              },
            );
        };
        if (heldNow.has(name) || (queues.get(name)?.length ?? 0) > 0) {
          const q = queues.get(name) ?? [];
          q.push(run);
          queues.set(name, q);
        } else {
          run();
        }
      });
    },
  };
  return { locks };
}

it("GATE21 item 5 (was GATE20 item 2): sibling B really finishes X and fails Y at the API boundary then closes — surviving store A delivers Y with only its real timer", async () => {
  // Rebuilt from the hand-written localStorage version: TWO real
  // HubStores/Outboxes on one backend (distinct tab owners), serialized by a
  // FIFO Web Locks shim. Initial inflight claims are the only seeded state;
  // every later transition is a real delivery flow. X (instance IX) is B's
  // claim until 10 s; Y (instance IY) is B's claim until 30 s.
  //
  // At 10 s B's real flow sends X (clear → sent); A's X fire reads the
  // terminal row. At 30 s B claims Y and the POST fails at the API boundary
  // (network error AND status GET rejected) → Y returns lease-less pending;
  // B closes, its jittered retry cancelled. A's real deadline wake is queued
  // on the same FIFO lock: its guarded flush reads pending Y and delivers
  // exactly once. No storage hand-editing, no manual flush.
  vi.useFakeTimers();
  const ownerA = "owner_g21_i5_2_a";
  const ownerB = "owner_g21_i5_2_b";
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  const realQuery = shim.locks.query;
  shim.locks.query = async () => {
    throw new Error("locks.query unavailable during load");
  };
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: shim.locks },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  try {
    const X = "cmd_g21_i5_2_x";
    const Y = "cmd_g21_i5_2_y";
    const IX = "ins_g21_i5_2_x";
    const IY = "ins_g21_i5_2_y";
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: X,
          clientRequestId: `local_${X}`,
          instanceId: IX,
          journalId: JOURNAL,
          prompt: "sibling finishes this one",
          createdAt: Date.now() - 2_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: ownerB, until: Date.now() + 10_000 },
        } satisfies OutboxRecord,
        {
          commandId: Y,
          clientRequestId: `local_${Y}`,
          instanceId: IY,
          journalId: JOURNAL,
          prompt: "sibling fails this one",
          createdAt: Date.now() - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: ownerB, until: Date.now() + 30_000 },
        } satisfies OutboxRecord,
      ]),
    );

    sessionStorage.setItem("remuda-outbox-owner", ownerB);
    const { api: apiB, hubStore: b } = await fresh();
    vi.spyOn(apiB, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiB, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(apiB, "instanceCommandStatus").mockRejectedValue(new Error("status network down"));
    const sendB = vi.spyOn(apiB, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => {
        if (commandId === X) {
          return {
            relatedCommandIds: [],
            command: {
              commandId: X,
              id: X,
              state: "accepted",
              revision: "1",
              dispatch: "native-acknowledged",
              resolution: "clear",
            },
          };
        }
        throw new Error("network POST failed");
      }) as unknown as Api["instanceSend"],
    );
    await bootLive(apiB, b);
    shim.locks.query = realQuery;
    sessionStorage.setItem("remuda-outbox-owner", ownerA);
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    const sendA = vi.spyOn(apiA, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(apiA, a);

    // Pin B's network-retry jitter high so it cannot retry Y inside the
    // windows below; the logout right after the failure cancels it anyway.
    vi.spyOn(Math, "random").mockReturnValue(0.999);

    const ai = a as unknown as {
      outbox: { refreshDurable: () => Promise<unknown>; pendingLeaseWakeups: () => string[] };
    };
    await ai.outbox.refreshDurable();
    expect(ai.outbox.pendingLeaseWakeups().sort()).toEqual([X, Y].sort());

    // 10 s: B sends X for real; A's X fire settles on the sent row.
    await vi.advanceTimersByTimeAsync(10_000);
    for (let i = 0; i < 40 && sendB.mock.calls.filter((c) => c[4] === X).length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(1);
      await Promise.resolve();
    }
    expect(sendB.mock.calls.map((c) => c[4])).toEqual([X]);
    expect(sendA).not.toHaveBeenCalled();

    // 30 s: B's real flow claims Y, POST + status fail at the boundary and Y
    // is restored lease-less pending. Pump microtasks only, then close B so
    // its bounded jittered retry never POSTs Y again.
    await vi.advanceTimersByTimeAsync(20_000);
    for (let i = 0; i < 400 && sendB.mock.calls.filter((c) => c[4] === Y).length === 0; i += 1) {
      await Promise.resolve();
    }
    expect(sendB.mock.calls.map((c) => c[4])).toEqual([X, Y]);
    const stored = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    // B closes before its pinned ~500 ms jittered retry can fire; A's queued
    // real deadline wake is what carries Y the rest of the way.
    b.logout();

    // A's guarded flush delivers Y exactly once (microtask drain of the
    // FIFO-queued timer turn, otherwise a few ms of fake time).
    for (let i = 0; i < 200 && sendA.mock.calls.length === 0; i += 1) {
      await Promise.resolve();
      await vi.advanceTimersByTimeAsync(1);
    }
    expect(sendA).toHaveBeenCalledTimes(1);
    expect(sendA.mock.calls[0]?.[4]).toBe(Y);
    expect(stored().find((r) => r.commandId === X)?.state).toBe("sent");
    expect(stored().find((r) => r.commandId === Y)?.state).toBe("sent");

    // Well past every deadline: no second POST, X never posted by A.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(sendA).toHaveBeenCalledTimes(1);
    a.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE21 item 5 (was GATE20 item 3a): sibling B's real flow answers H HELD and fails P at the boundary then closes — A's timer-driven guarded flush sends only P; H retries from A's real held-retry clock", async () => {
  // Rebuilt from the hand-written localStorage version. TWO real
  // HubStores/Outboxes on one backend (distinct owners), serialized by a FIFO
  // Web Locks shim. Initial state: H and P are B's fresh inflight claims on
  // the SAME instance, both due at 10 s (H created first so B claims it
  // first). Everything afterwards is a real delivery flow:
  //   - B claims H and the Hub answers HELD (resolution "held");
  //   - B's same flush then claims P; the POST fails at the API boundary and
  //     the status GET is also rejected → P returns to lease-less pending;
  //   - B closes immediately (its held/outbox retry timers are cancelled).
  // A tracked both leases; its queued deadline wake then runs the GUARDED
  // flush (skipCurrentlyHeld): P is sent once and H is never posted early.
  // H's NEXT POST comes from A's real held-retry clock (~2 s), exactly once.
  vi.useFakeTimers();
  const ownerA = "owner_g21_i5_3a_a";
  const ownerB = "owner_g21_i5_3a_b";
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  const realQuery = shim.locks.query;
  shim.locks.query = async () => {
    throw new Error("locks.query unavailable during load");
  };
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: shim.locks },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  try {
    const H = "cmd_g21_i5_3a_h";
    const P = "cmd_g21_i5_3a_p";
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: H,
          clientRequestId: `local_${H}`,
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "answered held by a sibling",
          createdAt: Date.now() - 2_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: ownerB, until: Date.now() + 10_000 },
        } satisfies OutboxRecord,
        {
          commandId: P,
          clientRequestId: `local_${P}`,
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "restored pending after sibling failure",
          createdAt: Date.now() - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: ownerB, until: Date.now() + 10_000 },
        } satisfies OutboxRecord,
      ]),
    );

    sessionStorage.setItem("remuda-outbox-owner", ownerB);
    const { api: apiB, hubStore: b } = await fresh();
    vi.spyOn(apiB, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiB, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(apiB, "instanceCommandStatus").mockRejectedValue(new Error("status network down"));
    let bCalls = 0;
    const sendB = vi.spyOn(apiB, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => {
        bCalls += 1;
        if (commandId === H && bCalls === 1) {
          return {
            relatedCommandIds: [],
            command: {
              commandId: H,
              id: H,
              state: "queued",
              revision: "1",
              dispatch: "not-dispatched",
              resolution: "held",
            },
          };
        }
        throw new Error("network POST failed");
      }) as unknown as Api["instanceSend"],
    );
    await bootLive(apiB, b);
    shim.locks.query = realQuery;

    sessionStorage.setItem("remuda-outbox-owner", ownerA);
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    const sendA = vi.spyOn(apiA, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(apiA, a);
    vi.spyOn(Math, "random").mockReturnValue(0.999);

    // 10 s: B claims H (held answer) then P (network+status fail → pending).
    // Its fires were registered first, so it holds the FIFO lock; A's guarded
    // flush queues behind it.
    await vi.advanceTimersByTimeAsync(10_000);
    for (let i = 0; i < 400; i += 1) {
      await Promise.resolve();
      const rows = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
      if (
        rows.find((r) => r.commandId === H)?.state === "held" &&
        rows.find((r) => r.commandId === P)?.state === "pending"
      ) {
        break;
      }
    }
    const stored = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored().find((r) => r.commandId === H)?.state).toBe("held");
    expect(stored().find((r) => r.commandId === P)?.state).toBe("pending");
    expect(sendB.mock.calls.map((c) => c[4])).toEqual([H, P]);

    // B closes at once: its held-retry and network-retry timers are cancelled,
    // so it never POSTs either row again.
    const T0 = Date.now();
    b.logout();

    // A's queued guarded flush runs: only P is posted; H stays held.
    for (let i = 0; i < 200 && sendA.mock.calls.length === 0; i += 1) {
      await Promise.resolve();
      await vi.advanceTimersByTimeAsync(1);
    }
    const allPosts = () => [...sendA.mock.calls, ...sendB.mock.calls];
    const postsFor = (id: string) => allPosts().filter((c) => c[4] === id);
    expect(sendA.mock.calls.map((c) => c[4])).toEqual([P]);
    expect(stored().find((r) => r.commandId === P)?.state).toBe("sent");
    expect(stored().find((r) => r.commandId === H)?.state).toBe("held");

    // H stays quiet until A's real held-retry deadline (~2 s after T0) …
    await vi.advanceTimersByTimeAsync(Math.max(0, T0 + 1_999 - Date.now()));
    expect(postsFor(H)).toHaveLength(1);
    // … then the normal held-retry flush re-forwards H exactly once.
    await vi.advanceTimersByTimeAsync(Math.max(2, T0 + 2_100 - Date.now()));
    for (let i = 0; i < 60 && postsFor(H).length < 2; i += 1) {
      await vi.advanceTimersByTimeAsync(1);
      await Promise.resolve();
    }
    expect(postsFor(H)).toHaveLength(2);
    expect(sendA.mock.calls.map((c) => c[4]).sort()).toEqual([H, P].sort());
    expect(stored().find((r) => r.commandId === H)?.state).toBe("sent");

    await vi.advanceTimersByTimeAsync(60_000);
    a.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE21 item 6 (was GATE20 item 3b): P gets its own wake while H is still inflight behind B's lock — A posts only P; H retries from the held-retry timer", async () => {
  // Two REAL stores share the localStorage backend and the cloned persisted
  // owner, serialized by a small ordered Web Locks shim: B claims H and its
  // POST + instance lock stay PENDING; A tracks H's lease while P sits
  // pending on a SECOND instance. A's lease wake fires at T and its guarded
  // flush QUEUES behind B's lock. While A waits, B's POST is answered HELD.
  // When A finally enters the lock, its FRESH in-lock selection must skip H
  // (held policy at the claim boundary), POST only P, and leave H's next POST
  // to the held-retry clock.
  vi.useFakeTimers();
  const owner = "owner_g20_i3b";
  const heldNow = new Set<string>();
  const queues = new Map<string, Array<() => void>>();
  const originalNavigator = globalThis.navigator;
  const locks = {
    query: async () => ({ held: [] as Array<{ name: string; mode: string }> }),
    request: (name: string, _opts: unknown, cb: () => Promise<unknown>) =>
      new Promise((resolve, reject) => {
        const run = () => {
          heldNow.add(name);
          Promise.resolve()
            .then(cb)
            .then(
              (v) => {
                heldNow.delete(name);
                resolve(v);
                queues.get(name)?.shift()?.();
              },
              (e) => {
                heldNow.delete(name);
                reject(e);
                queues.get(name)?.shift()?.();
              },
            );
        };
        if (heldNow.has(name) || (queues.get(name)?.length ?? 0) > 0) {
          const q = queues.get(name) ?? [];
          q.push(run);
          queues.set(name, q);
        } else {
          run();
        }
      }),
  };
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  sessionStorage.setItem("remuda-outbox-owner", owner);
  let releaseB: (() => void) | null = null;
  try {
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    const { api: apiB, hubStore: b } = await fresh();
    vi.spyOn(apiB, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiB, "screenRead").mockResolvedValue({ lines: [] });
    await bootLive(apiA, a);
    await bootLive(apiB, b);
    const ai = a as unknown as {
      flushAllOutbox: () => Promise<void>;
      outbox: {
        refreshDurable: () => Promise<unknown>;
        pendingLeaseWakeups: () => string[];
        enqueue: (rec: Partial<OutboxRecord> & { commandId: string; clientRequestId: string; instanceId: string; prompt: string }) => Promise<unknown>;
      };
    };
    const bi = b as unknown as { flushAllOutbox: () => Promise<void> };
    await ai.flushAllOutbox();
    await bi.flushAllOutbox();

    const H = "cmd_g20_i3b_h";
    const P = "cmd_g20_i3b_p";
    const INS2 = "ins_g20_i3b_second";
    const J2 = "obj_g20_i3b_journal_second";
    // H pending: B claims it and blocks inside the POST holding the lock.
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: H,
          clientRequestId: `local_${H}`,
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "B is POSTing this one",
          createdAt: Date.now() - 1_000,
          attempts: 0,
          state: "pending",
        } satisfies OutboxRecord,
      ]),
    );
    let sendBCalls = 0;
    const sendB = vi.spyOn(apiB, "instanceSend").mockImplementation(
      ((...args: unknown[]) => {
        // First POST (H) blocks while B holds the instance lock; releaseB
        // answers it HELD. Every later call (the real held-retry re-POST)
        // succeeds immediately so retries settle instead of blocking again.
        if (sendBCalls === 0) {
          sendBCalls += 1;
          return new Promise((resolve) => {
            releaseB = () =>
              resolve({
                relatedCommandIds: [],
                command: {
                  commandId: H,
                  id: H,
                  state: "queued",
                  revision: "1",
                  dispatch: "not-dispatched",
                  resolution: "held",
                },
              });
          });
        }
        sendBCalls += 1;
        const commandId = args[4] as string;
        return Promise.resolve({
          relatedCommandIds: [],
          command: {
            commandId,
            id: commandId,
            state: "accepted",
            revision: "1",
            dispatch: "native-acknowledged",
            resolution: "clear",
          },
        });
      }) as unknown as Api["instanceSend"],
    );
    void bi.flushAllOutbox();
    // Let B's claim transaction commit while its POST stays blocked.
    for (let i = 0; i < 20 && sendB.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(1);
    }
    expect(sendB).toHaveBeenCalledTimes(1);
    expect(sendB.mock.calls[0]?.[4]).toBe(H);

    // A discovers B's live same-owner claim and tracks its deadline. P does
    // NOT exist yet: it is enqueued by A only after B settles H as held.
    await ai.outbox.refreshDurable();
    expect(ai.outbox.pendingLeaseWakeups()).toEqual([H]);
    const claimUntil = (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[]).find(
      (r) => r.commandId === H,
    )?.lease?.until;
    expect(claimUntil).toBeTruthy();

    const sendA = vi.spyOn(apiA, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );

    // Advance to H's deadline: A's fire queues behind B's held instance lock;
    // nothing from A yet.
    await vi.advanceTimersByTimeAsync(Math.max(1, (claimUntil ?? 0) - Date.now()));
    expect(sendA).not.toHaveBeenCalled();

    // Gate 21 item 6: P is enqueued WHILE A's guarded flush is already queued
    // and H is still B's blocked INFLIGHT POST — not after H is settled. P's
    // own real delivery wake (the enqueue reconciliation) fires another
    // guarded flush: its pre-lock read completes while H is inflight, and that
    // flush QUEUES on the same lock. Nothing is sent while B holds it.
    const T0 = Date.now();
    await ai.outbox.enqueue({
      commandId: P,
      clientRequestId: `local_${P}`,
      instanceId: INS2,
      journalId: J2,
      prompt: "A must send this one",
      createdAt: Date.now(),
    });
    for (let i = 0; i < 40; i += 1) await vi.advanceTimersByTimeAsync(1);
    expect(sendA).not.toHaveBeenCalled();
    const storedPre = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(storedPre().find((r) => r.commandId === H)?.state).toBe("inflight");
    expect(storedPre().find((r) => r.commandId === P)?.state).toBe("pending");

    // B's POST now completes HELD while A's flushes are queued. B releases the
    // lock and arms the held-retry clock.
    releaseB!();
    // Let ONLY the blocked POST's held write commit (microtasks, no fake
    // time), then close B before its flush continuation runs another pass
    // over the now-existing P or its held-retry timer fires. A's queued turns
    // are already in the FIFO lock queue and stay alive across B's logout.
    for (let i = 0; i < 600; i += 1) {
      await Promise.resolve();
      const storedH = (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[]).find(
        (r) => r.commandId === H,
      );
      if (storedH?.state === "held") break;
    }
    b.logout();

    // A's queued guarded flushes enter on the FRESH in-lock state: H is HELD
    // there (held policy at the claim boundary), so only P is POSTed. The
    // wake that delivered P existed before H was settled — its own real wake.
    for (let i = 0; i < 120 && sendA.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(1);
      await Promise.resolve();
    }
    const allPosts = () => [...sendA.mock.calls, ...sendB.mock.calls];
    const postsFor = (id: string) => allPosts().filter((c) => c[4] === id);
    expect(sendA).toHaveBeenCalledTimes(1);
    expect(sendA.mock.calls[0]?.[4]).toBe(P);
    const stored = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored().find((r) => r.commandId === P)?.state).toBe("sent");
    expect(stored().find((r) => r.commandId === H)?.state).toBe("held");
    expect(postsFor(H)).toHaveLength(1);

    // H's next POST is A's real held-retry timer (~2 s after T0), exactly
    // once (B's own retry was cancelled at logout).
    await vi.advanceTimersByTimeAsync(Math.max(0, T0 + 1_999 - Date.now()));
    expect(postsFor(H)).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(Math.max(2, T0 + 2_100 - Date.now()));
    for (let i = 0; i < 60 && postsFor(H).length < 2; i += 1) {
      await vi.advanceTimersByTimeAsync(1);
      await Promise.resolve();
    }
    expect(postsFor(H)).toHaveLength(2);
    expect(sendA.mock.calls.map((c) => c[4]).sort()).toEqual([H, P].sort());
    a.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE21 item 1b: a guarded flush whose instance lock request rejects keeps one wake and still delivers", async () => {
  // The deadline/delivery wake's own read lock is granted; the guarded
  // flush's SECOND lock request rejects (a user-agent lock failure). The old
  // catch swallowed it and the finally only cleared suppression — the row's
  // sole wake had fired, so nothing ever re-triggered delivery. The bounded
  // failure count plus the finally's fresh reconciliation must arm one
  // backed-off wake; the next attempts get the lock and POST the row.
  vi.useFakeTimers();
  const owner = "owner_g21_i1b";
  const originalNavigator = globalThis.navigator;
  // Per-name request counter: #1 granted (the fire read), #2 rejected (the
  // flush's delivery lock), #3+ granted.
  const tries = new Map<string, number>();
  const locks = {
    query: async () => ({ held: [] as Array<{ name: string; mode: string }> }),
    request(name: string, _opts: unknown, cb: () => Promise<unknown>) {
      const n = (tries.get(name) ?? 0) + 1;
      tries.set(name, n);
      if (n === 2) return Promise.reject(new Error("navigator.locks.request rejected"));
      return Promise.resolve().then(cb);
    },
  };
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  sessionStorage.setItem("remuda-outbox-owner", owner);
  try {
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    const internal = hubStore as unknown as {
      flushAllOutbox: () => Promise<void>;
      outbox: { enqueue: (rec: Partial<OutboxRecord> & {
        commandId: string;
        clientRequestId: string;
        instanceId: string;
        prompt: string;
      }) => Promise<unknown> };
    };
    await internal.flushAllOutbox();
    expect(send).not.toHaveBeenCalled();

    await internal.outbox.enqueue({
      commandId: "cmd_g21_i1b_r",
      clientRequestId: "local_g21_i1b_r",
      instanceId: INSTANCE,
      journalId: JOURNAL,
      prompt: "deliver me after the rejected lock",
      createdAt: Date.now(),
    });

    // Wake #1 read granted; flush lock #2 rejects → one 250 ms backed-off
    // wake; its fire (#3) and flush lock (#4) deliver.
    for (let i = 0; i < 200 && send.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(5);
      await Promise.resolve();
    }
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[4]).toBe("cmd_g21_i1b_r");
    const stored = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored.find((r) => r.commandId === "cmd_g21_i1b_r")?.state).toBe("sent");
    await vi.advanceTimersByTimeAsync(60_000);
    expect(send).toHaveBeenCalledTimes(1);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});


async function bootItem4(which: "4a" | "4b" | "4c") {
  vi.useFakeTimers();
  const owner = `owner_g20_${which}`;
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: { query: async () => ({ held: [] }) } }, // query, no request
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  sessionStorage.setItem("remuda-outbox-owner", owner);
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  const send = vi.spyOn(api, "instanceSend").mockImplementation(
    (async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => ({
      relatedCommandIds: [],
      command: {
        commandId,
        id: commandId,
        state: "accepted",
        revision: "1",
        dispatch: "native-acknowledged",
        resolution: "clear",
      },
    })) as unknown as Api["instanceSend"],
  );
  await bootLive(api, hubStore);
  const box = (hubStore as unknown as { outbox: unknown }).outbox as {
    refreshDurable: () => Promise<unknown>;
    pendingLeaseWakeups: () => string[];
    readDurable: () => Promise<unknown>;
    patch: (...args: unknown[]) => Promise<unknown>;
    onClaimWake: (() => void) | null;
  };
  const flush = () =>
    (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  await flush();
  expect(send).not.toHaveBeenCalled();

  const cmd = `cmd_g20_${which}_row`;
  const T_MS = 10_000;
  localStorage.setItem(
    OUTBOX_LS_KEY,
    JSON.stringify([
      {
        commandId: cmd,
        clientRequestId: `local_${cmd}`,
        instanceId: INSTANCE,
        journalId: JOURNAL,
        prompt: "recover me after the one-shot failure",
        createdAt: Date.now() - 1_000,
        attempts: 1,
        state: "inflight",
        lease: { owner, until: Date.now() + T_MS },
      } satisfies OutboxRecord,
    ]),
  );
  await box.refreshDurable();
  expect(box.pendingLeaseWakeups()).toEqual([cmd]);

  if (which === "4a") {
    // The TIMER read succeeds; the GUARDED FLUSH's own next durable read is
    // what rejects once. onClaimWake runs after the fire read and before the
    // flush read, so arm the one-shot there.
    const prev = box.onClaimWake;
    let armed = false;
    box.onClaimWake = () => {
      if (!armed) {
        armed = true;
        vi.spyOn(box, "readDurable").mockImplementationOnce(() =>
          Promise.reject(new Error("g20-4a one-shot flush read failure")),
        );
      }
      prev?.();
    };
  } else if (which === "4b") {
    // The timer read (#1) and the flush's pre-lock read (#2) succeed; the
    // IN-LOCK read (#3) rejects once. All later reads (recovery + retry)
    // pass through to the real implementation.
    const realRead = box.readDurable.bind(box) as () => Promise<unknown>;
    let reads = 0;
    vi.spyOn(box, "readDurable").mockImplementation(function (this: unknown) {
      reads += 1;
      if (reads === 3) return Promise.reject(new Error("g20-4b one-shot in-lock read failure"));
      return realRead.call(this);
    });
  } else {
    // The reads succeed; the CLAIM TRANSACTION (box.patch mergeUnlessDone)
    // rejects once. The first patch of the delivery turn is the claim.
    const realPatch = box.patch.bind(box) as (...args: unknown[]) => Promise<unknown>;
    let claims = 0;
    vi.spyOn(box, "patch").mockImplementation((...args: unknown[]) => {
      claims += 1;
      if (claims === 1) return Promise.reject(new Error("g20-4c one-shot claim abort"));
      return realPatch(...args);
    });
  }

  await vi.advanceTimersByTimeAsync(T_MS);
  // The bounded retry is a 250 ms backoff; allow a couple of rounds and pumps.
  for (let i = 0; i < 400 && send.mock.calls.length === 0; i += 1) {
    await vi.advanceTimersByTimeAsync(10);
    await Promise.resolve();
  }
  expect(send).toHaveBeenCalledTimes(1);
  expect(send.mock.calls[0]?.[4]).toBe(cmd);
  const stored = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
  expect(stored.find((r) => r.commandId === cmd)?.state).toBe("sent");
  await vi.advanceTimersByTimeAsync(120_000);
  expect(send).toHaveBeenCalledTimes(1);
  hubStore.logout();
  vi.useRealTimers();
  Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
  sessionStorage.removeItem("remuda-outbox-owner");
}

it("GATE20 item 4a: a one-shot failure of the guarded flush's durable READ still delivers the row", async () => {
  await bootItem4("4a");
});

it("GATE20 item 4b: a one-shot failure of the deliverer's IN-LOCK read still delivers the row", async () => {
  await bootItem4("4b");
});

it("GATE20 item 4c: a one-shot failure of the CLAIM transaction still delivers the row", async () => {
  await bootItem4("4c");
});

it("GATE20 item 4c: a one-shot failure of the CLAIM transaction still delivers the row", async () => {
  await bootItem4("4c");
});

it("GATE20 item 6: two real stores/Outboxes on one backend — B held-and-pagehides behind its lock, A's queued timer sends only P; H retries from the real held-retry timer", async () => {
  // Replaces the old mocked-storage held-row regression: no third mocked read,
  // no hand flushAllOutbox(). TWO real HubStores with real Outboxes share the
  // localStorage backend and the cloned persisted owner, serialized by an
  // ordered Web Locks shim. B claims H, its POST and instance lock stay
  // PENDING. P (A's row, second instance) becomes durable while B is blocked.
  // A's tracked lease timer fires at the deadline and its guarded flush
  // QUEUES behind B's lock. B's POST is then answered HELD and the cloned
  // sibling CLOSES (logout): B never runs another delivery pass, its own
  // held-retry timer is torn down. A's queued turn reads the FRESH held H and
  // the pending P: A sends only P. H's NEXT POST comes from A's real
  // held-retry clock (same id, no attempt budget spent), exactly once.
  vi.useFakeTimers();
  const owner = "owner_g20_i6";
  const heldNow = new Set<string>();
  const queues = new Map<string, Array<() => void>>();
  const originalNavigator = globalThis.navigator;
  const locks = {
    query: async () => ({ held: [] as Array<{ name: string; mode: string }> }),
    request: (name: string, _opts: unknown, cb: () => Promise<unknown>) =>
      new Promise((resolve, reject) => {
        const run = () => {
          heldNow.add(name);
          Promise.resolve()
            .then(cb)
            .then(
              (v) => {
                heldNow.delete(name);
                resolve(v);
                queues.get(name)?.shift()?.();
              },
              (e) => {
                heldNow.delete(name);
                reject(e);
                queues.get(name)?.shift()?.();
              },
            );
        };
        if (heldNow.has(name) || (queues.get(name)?.length ?? 0) > 0) {
          const q = queues.get(name) ?? [];
          q.push(run);
          queues.set(name, q);
        } else {
          run();
        }
      }),
  };
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  sessionStorage.setItem("remuda-outbox-owner", owner);
  let releaseB: (() => void) | null = null;
  try {
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    const { api: apiB, hubStore: b } = await fresh();
    vi.spyOn(apiB, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiB, "screenRead").mockResolvedValue({ lines: [] });
    await bootLive(apiA, a);
    await bootLive(apiB, b);
    const ai = a as unknown as {
      flushAllOutbox: () => Promise<void>;
      outbox: { refreshDurable: () => Promise<unknown>; pendingLeaseWakeups: () => string[] };
    };
    const bi = b as unknown as { flushAllOutbox: () => Promise<void> };
    await ai.flushAllOutbox();
    await bi.flushAllOutbox();

    const H = "cmd_g20_i6_h";
    const P = "cmd_g20_i6_p";
    const INS2 = "ins_g20_i6_second";
    const J2 = "obj_g20_i6_journal_second";
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: H,
          clientRequestId: `local_${H}`,
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "B holds this one",
          createdAt: Date.now() - 1_000,
          attempts: 0,
          state: "pending",
        } satisfies OutboxRecord,
      ]),
    );
    let bCalls = 0;
    vi.spyOn(apiB, "instanceSend").mockImplementation(
      (() => {
        bCalls += 1;
        return new Promise((resolve) => {
          releaseB = () =>
            resolve({
              relatedCommandIds: [],
              command: {
                commandId: H,
                id: H,
                state: "queued",
                revision: "1",
                dispatch: "not-dispatched",
                resolution: "held",
              },
            });
        });
      }) as unknown as Api["instanceSend"],
    );
    void bi.flushAllOutbox();
    for (let i = 0; i < 20 && bCalls === 0; i += 1) await vi.advanceTimersByTimeAsync(1);
    expect(bCalls).toBe(1);

    // A tracks B's live claim; P becomes durable while B's POST is still
    // blocked (B's one delivery turn already selected only H).
    await ai.outbox.refreshDurable();
    expect(ai.outbox.pendingLeaseWakeups()).toEqual([H]);
    const claimUntil = (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[]).find(
      (r) => r.commandId === H,
    )?.lease?.until;
    expect(claimUntil).toBeTruthy();
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        ...(JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[]),
        {
          commandId: P,
          clientRequestId: `local_${P}`,
          instanceId: INS2,
          journalId: J2,
          prompt: "A must send this one",
          createdAt: Date.now() - 1_000,
          attempts: 0,
          state: "pending",
        } satisfies OutboxRecord,
      ]),
    );

    const sendA = vi.spyOn(apiA, "instanceSend").mockImplementation(
      (async (_iid: string, _p: string, _a: unknown, _m: unknown, commandId: string) =>
        ({
          relatedCommandIds: [],
          command: {
            commandId,
            id: commandId,
            state: "accepted",
            revision: "1",
            dispatch: "native-acknowledged",
            resolution: "clear",
          },
        }) as unknown as Awaited<ReturnType<Api["instanceSend"]>>) as unknown as Api["instanceSend"],
    );

    // A's timer fires at the deadline and its guarded flush queues behind B.
    await vi.advanceTimersByTimeAsync(Math.max(1, (claimUntil ?? 0) - Date.now()));
    expect(sendA).not.toHaveBeenCalled();
    expect([...heldNow]).toContain(`remuda-outbox-${INSTANCE}`);

    // B is answered HELD and the cloned sibling's pagehide lands before its
    // next delivery pass: the held write still commits (outbox alive) but B's
    // flush passes abort on the unload flag and a B held-retry cannot POST
    // from a tearing-down page. B itself never drains P.
    const T0 = Date.now();
    releaseB!();
    (b as unknown as { onPageHide: (e: Event) => void }).onPageHide(new Event("pagehide"));
    for (let i = 0; i < 120 && sendA.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(1);
      await Promise.resolve();
    }

    // A sent only P (B's one and only POST was the held H); H stays held.
    expect(sendA.mock.calls.map((c) => c[4])).toEqual([P]);
    expect(bCalls).toBe(1);
    const stored = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored().find((r) => r.commandId === P)?.state).toBe("sent");
    const heldRow = stored().find((r) => r.commandId === H);
    expect(heldRow?.state).toBe("held");
    // The held answer/refund spent no attempt budget; the retry uses same id.
    expect(heldRow?.attempts).toBe(0);

    // Nothing re-POSTs H before A's real held-retry deadline (~2 s) …
    await vi.advanceTimersByTimeAsync(Math.max(0, T0 + 1_999 - Date.now()));
    expect(sendA).toHaveBeenCalledTimes(1);
    // … and exactly ONE H re-POST then runs through the normal held-retry
    // flush (no manual flush in this test), clearing to sent.
    await vi.advanceTimersByTimeAsync(Math.max(2, T0 + 2_100 - Date.now()));
    for (let i = 0; i < 60; i += 1) {
      await vi.advanceTimersByTimeAsync(1);
      await Promise.resolve();
      if (stored().find((r) => r.commandId === H)?.state === "sent") break;
    }
    expect(sendA.mock.calls.map((c) => c[4]).sort()).toEqual([H, P].sort());
    expect(sendA).toHaveBeenCalledTimes(2);
    expect(bCalls).toBe(1);
    expect(stored().find((r) => r.commandId === H)?.state).toBe("sent");
    a.logout();
    b.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE10 item 7: a lease-less done row committed by another tab skips without aborting the flush; B still sends", async () => {
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore);

  // Two rows: A is delivered FIRST; the claim transaction returns it as a
  // lease-less done (another tab journal-confirmed it between cache read and
  // the claim merge). The durable done must be a definitive SKIP, not a
  // claim-abort — otherwise B never sends. B pending.
  localStorage.setItem(
    OUTBOX_LS_KEY,
    JSON.stringify([
      row({
        commandId: "cmd_done_no_lease",
        state: "inflight",
        lease: undefined,
      }),
      row({ commandId: "cmd_after_done" }),
    ]),
  );
  // Flip A to a lease-less done in the MERGE result the claim reads: mock
  // the outbox patch to return the done row for A's claim.
  const store = (hubStore as unknown as {
    outbox?: { patch: (id: string, p: unknown) => Promise<OutboxRecord | null>; cache?: Map<string, OutboxRecord> };
  }).outbox;
  if (store) {
    const origPatch = store.patch.bind(store);
    vi.spyOn(store, "patch").mockImplementation(async (id: string, p: unknown) => {
      if (id === "cmd_done_no_lease") {
        // The durable claim transaction finds another tab's journal-confirmed
        // done and returns it verbatim, discarding this tab's inflight claim.
        return row({ commandId: "cmd_done_no_lease", state: "done", lease: undefined });
      }
      return origPatch(id, p);
    });
  }
  const send = vi.spyOn(api, "instanceSend").mockResolvedValue({
    relatedCommandIds: [],
    command: {
      commandId: "cmd_after_done",
      id: "cmd_after_done",
      state: "accepted",
      revision: "1",
      dispatch: "native-acknowledged",
      resolution: "clear",
    } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
  });

  // Gate 11 item 8f: B must be delivered by the FIRST flush — no second
  // trigger may be needed to get past the lease-less done A.
  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  // Exactly one POST, it is B, and the done row A was never POSTed.
  expect(send).toHaveBeenCalledTimes(1);
  expect(send.mock.calls[0]?.[4]).toBe("cmd_after_done");
  expect(send.mock.calls.some((c) => c[4] === "cmd_done_no_lease")).toBe(false);
  hubStore.logout();
});

it("GATE12 item 6: two aborting load reclaims arm the tracked wakeup that delivers via the REAL store wiring with exactly one POST", async () => {
  // Unlike the Outbox-only test (which installs its own onClaimWake), this
  // drives hubStore end to end: Outbox.load -> both reclaim writes abort ->
  // the tracked wakeup fires at the lease deadline -> the store's registered
  // onClaimWake runs flushAllOutbox -> one instanceSend. Removing the store
  // registration (store.ts onClaimWake) leaves the row pending with zero POSTs.
  vi.useFakeTimers();
  const OWNER_KEY = "remuda-outbox-owner";
  const owner = "owner_g12_item6";
  sessionStorage.setItem(OWNER_KEY, owner);
  const LEASE_MS = 30_000;
  localStorage.setItem(
    OUTBOX_LS_KEY,
    JSON.stringify([
      row({
        commandId: "cmd_wake_store",
        state: "inflight",
        lease: { owner, until: Date.now() + LEASE_MS },
      }),
    ]),
  );
  // Query says holder gone -> same-owner reclaim attempted at load; no
  // request() (jsdom): durable lease fallback path.
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: { locks: { query: async () => ({ held: [] }) } },
  });
  // Fail exactly the first TWO reclaim writes (pending patch); everything
  // after goes through. Capture the ORIGINAL explicitly — the vi spy's
  // wrappedMethod is not available on the jsdom Storage prototype here.
  const originalSetItem = Storage.prototype.setItem;
  let reclaimWrites = 0;
  Storage.prototype.setItem = function (this: Storage, key: string, value: string) {
    if (
      key === OUTBOX_LS_KEY &&
      typeof value === "string" &&
      value.includes("cmd_wake_store") &&
      value.includes('"state":"pending"')
    ) {
      reclaimWrites += 1;
      if (reclaimWrites <= 2) throw new DOMException("one-shot abort", "QuotaExceededError");
    }
    return originalSetItem.call(this, key, value);
  };

  const { api, hubStore } = await fresh();
  // Spy installed BEFORE bootstrap so the boot-time flush (which must POST
  // nothing — the fresh inflight claim is not deliverable) is observed too.
  const send = vi
    .spyOn(api, "instanceSend")
    .mockImplementation((async (_iid, _prompt, _att, _mode, commandId) => ({
      relatedCommandIds: [],
      command: {
        commandId,
        id: commandId,
        state: "accepted",
        revision: "1",
        dispatch: "native-acknowledged",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    })) as Api["instanceSend"]);
  await bootLive(api, hubStore);
  // Both load reclaims aborted; the boot-time flush saw a fresh inflight claim
  // and POSTed nothing.
  expect(reclaimWrites).toBe(2);
  expect(send).not.toHaveBeenCalled();

  // Advance EXACTLY to the lease deadline — no extra wall time. The wake fires
  // at the deadline; drain its microtask chain (reclaim -> flush -> POST)
  // without advancing the clock, so a late delivery cannot pass this test.
  await vi.advanceTimersByTimeAsync(LEASE_MS);
  for (let i = 0; i < 20 && send.mock.calls.length === 0; i += 1) {
    await vi.advanceTimersByTimeAsync(0);
  }
  expect(send).toHaveBeenCalledTimes(1);
  expect(send.mock.calls[0]?.[4]).toBe("cmd_wake_store");
  const stored = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
  const delivered = stored.find((r) => r.commandId === "cmd_wake_store");
  expect(delivered?.state).toBe("sent");
  expect(delivered?.lease).toBeUndefined();

  // No duplicate delivery long afterwards.
  await vi.advanceTimersByTimeAsync(120_000);
  expect(send).toHaveBeenCalledTimes(1);

  vi.useRealTimers();
  Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
  Storage.prototype.setItem = originalSetItem;
  sessionStorage.removeItem(OWNER_KEY);
  hubStore.logout();
});

it("GATE16 item 1: follow(B) while bound to A in recovering adopts mounted B — dead socket arms a deadline that resumes; live socket certifies live", async () => {
  // Gate 15 item 3 routed follow() through bindConnection, whose noteBinding
  // retires A's in-flight resume and clears the watchdog; the mounted branch
  // then returned without adopting B's socket, so the machine was stuck in
  // recovering with ZERO timers. follow() must adopt the mounted follow.
  vi.useFakeTimers();
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore); // mounts A = INSTANCE (journal JOURNAL)
  const store = hubStore as unknown as {
    connection: ConnectionMachine;
    setFollowLiveForTest: (open: boolean, framed: boolean, journalId?: string) => void;
  };

  const JOURNAL_B = "obj_journal_b_g16";
  const INSTANCE_B = "ins_mounted_b_g16";
  const instanceB = {
    id: INSTANCE_B,
    journalId: JOURNAL_B,
    lifecycle: "running",
    revision: "0",
    durableSeq: "0",
  } as never as Awaited<ReturnType<Api["instanceGet"]>>;
  vi.spyOn(api, "instanceGet").mockImplementation(async (id: string) =>
    id === INSTANCE_B ? instanceB : ({} as Awaited<ReturnType<Api["instanceGet"]>>),
  );
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    windowFromSeq: "1",
    durableSeq: "0",
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>);
  // B mounts with an OPEN live socket the first time.
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async () =>
    ({
      subscriptionId: "sub_g16" as never,
      getReadyState: () => 1,
      snapshot: { asOfSeq: "0", fromSeq: null },
      close: () => undefined,
    }) as never,
  );
  // Both A and B are mounted while subscribes resolve live.
  await hubStore.follow(INSTANCE_B);
  await hubStore.follow(INSTANCE);
  const timerNames = () =>
    [...(store.connection as unknown as { timers: Map<string, unknown> }).timers.keys()];

  // --- scenario 1: rebind into recovering with B's socket DEAD ---
  // New subscribes hang (blocked upgrades), so resume reopen attempts stay in
  // flight in recovering rather than certifying live off a fresh socket.
  vi.mocked(api.eventsSubscribe).mockImplementation(
    () => new Promise<never>(() => {}) as never,
  );
  store.setFollowLiveForTest(false, false, JOURNAL);
  store.setFollowLiveForTest(false, false, JOURNAL_B);
  store.connection.dispatch({ type: "close" });
  store.connection.dispatch({ type: "online" }); // beginResume hangs -> recovering
  expect(store.connection.state).toBe("recovering");

  await hubStore.follow(INSTANCE_B); // bind-away retires A's attempt; B dead
  expect(store.connection.state).toBe("recovering");
  // Machine not stuck: the rebind armed a pending bind deadline (zero timers
  // was the gate-16 regression).
  expect(timerNames()).toContain("bind");
  // The bind deadline starts a resume attempt (its reopen hangs); its 20 s
  // watchdog settles quiet stale with armed probe timers — the machine
  // self-drives out of recovering instead of hanging forever.
  await vi.advanceTimersByTimeAsync(15_000); // bind fires -> new recovering attempt
  await Promise.resolve();
  await vi.advanceTimersByTimeAsync(20_001); // watchdog -> quiet stale
  await Promise.resolve();
  await Promise.resolve();
  expect(store.connection.state).toBe("stale");
  expect(timerNames()).toContain("probe");

  // --- scenario 2: rebind into recovering with B's socket LIVE -> live ---
  store.setFollowLiveForTest(true, true, JOURNAL_B);
  store.connection.dispatch({ type: "close" });
  store.connection.dispatch({ type: "online" }); // resume attempt armed
  expect(store.connection.state).toBe("recovering");
  await hubStore.follow(INSTANCE_B); // adopt the live mounted socket
  expect(store.connection.state).toBe("live");
  hubStore.logout();
});

it("GATE18 item 2 (gate 20): aligned deadline wakes on two instances — a held row never POSTs while the OTHER overdue instance is delivered by the same reconciliation", async () => {
  // Two foreign leases share deadline T and BOTH per-command wakes are armed.
  // At T cmd1 has been finished as HELD by a sibling and cmd2's lease is
  // overdue. The one central reconciliation runs from both aligned fires:
  // cmd2 must be delivered (by its own fire or by the 0 ms delivery wake the
  // other fire's snapshot arms — never stranded), cmd1 may POST only from
  // its real held-retry clock, never from the guarded flush at T.
  vi.useFakeTimers();
  const owner = "owner_g18_i2";
  const foreign1 = "owner_g18_i2_crashed_b1";
  const foreign2 = "owner_g18_i2_crashed_b2";
  const INS2 = "ins_g18_i2_second";
  const J2 = "obj_g18_i2_journal_second";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: { query: async () => ({ held: [] }) } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  sessionStorage.setItem("remuda-outbox-owner", owner);
  try {
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi
      .spyOn(api, "instanceSend")
      .mockImplementation((async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"]);
    await bootLive(api, hubStore);
    const internal = hubStore as unknown as {
      ensureOutbox: () => Promise<boolean>;
      flushAllOutbox: () => Promise<void>;
      outbox: { pendingLeaseWakeups: () => string[] };
    };
    expect(await internal.ensureOutbox()).toBe("ok");
    await internal.flushAllOutbox();

    const cmd1 = "cmd_g18_i2_held";
    const cmd2 = "cmd_g18_i2_overdue";
    const T = Date.now() + 10_000;
    const rows = (r1: OutboxRecord, r2: OutboxRecord) =>
      localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([r1, r2]));
    const heldRow = (): OutboxRecord => ({
      commandId: cmd1,
      clientRequestId: "local_g18_i2_1",
      instanceId: INSTANCE,
      journalId: JOURNAL,
      prompt: "becomes held",
      createdAt: Date.now() - 2_000,
      attempts: 1,
      state: "held",
      gotResponse: true,
    });
    const inflight = (id: string, iid: string, jid: string, leaseOwner: string): OutboxRecord => ({
      commandId: id,
      clientRequestId: `local_${id}`,
      instanceId: iid,
      journalId: jid,
      prompt: id,
      createdAt: Date.now() - 1_000,
      attempts: 1,
      state: "inflight",
      lease: { owner: leaseOwner, until: T },
    });

    // Both claims live until T: one per-command deadline wake each.
    rows(inflight(cmd1, INSTANCE, JOURNAL, foreign1), inflight(cmd2, INS2, J2, foreign2));
    await internal.flushAllOutbox();
    expect(internal.outbox.pendingLeaseWakeups().sort()).toEqual([cmd1, cmd2].sort());
    expect(send).not.toHaveBeenCalled();

    // One ms before T: cmd1 has a sibling's HELD answer, cmd2's lease is about
    // to expire. Advance through T and pump the aligned fires + delivery.
    await vi.advanceTimersByTimeAsync(9_999);
    rows(heldRow(), inflight(cmd2, INS2, J2, foreign2));
    await vi.advanceTimersByTimeAsync(1);
    // cmd1's fire reconciles cmd2 (its sibling lease is overdue) and arms its
    // 0 ms delivery wake from INSIDE the fire's awaited read chain. Fake timers
    // do not dispatch a 0 ms timer scheduled in a post-frame microtask on a
    // 0 ms advance, so pump with 1 ms advances (well inside the 2 s held gap).
    for (let i = 0; i < 60 && send.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(1);
      await Promise.resolve();
    }

    // Exactly one POST at T: the overdue instance; the held row waits.
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[4]).toBe(cmd2);
    const stored = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored().find((r) => r.commandId === cmd2)?.state).toBe("sent");
    expect(stored().find((r) => r.commandId === cmd1)?.state).toBe("held");

    // Before the held-retry deadline (2 s after T) nothing re-POSTs held …
    await vi.advanceTimersByTimeAsync(Math.max(0, T + 1_999 - Date.now()));
    expect(send).toHaveBeenCalledTimes(1);
    // … the real held-retry timer then re-forwards cmd1 exactly once.
    await vi.advanceTimersByTimeAsync(2);
    for (let i = 0; i < 40 && send.mock.calls.length < 2; i += 1) await Promise.resolve();
    expect(send).toHaveBeenCalledTimes(2);
    expect(send.mock.calls[1]?.[4]).toBe(cmd1);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(send).toHaveBeenCalledTimes(2);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE18 item 3 (gate 20): a command's deadline fire reconciles ANOTHER command's FOREIGN replacement into the one scheduler — Y adopts T2 and delivers exactly once", async () => {
  // Two inactive claims X, Y share deadline T1. By T1 X is TERMINAL (sent)
  // and Y has been re-claimed by a FOREIGN owner with a later deadline T2 who
  // then crashes. The ONE central reconciliation (from either aligned fire)
  // must adopt Y's authoritative T2 lease — not cancel Y's wake the way the
  // old per-snapshot reconciliation did — and deliver Y with a single POST at
  // T2.
  vi.useFakeTimers();
  const owner = "owner_g18_i3";
  const foreign = "owner_g18_i3_crashed_y";
  const INS_Y = "ins_g18_i3_y";
  const J_Y = "obj_g18_i3_journal_y";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: { query: async () => ({ held: [] }) } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  sessionStorage.setItem("remuda-outbox-owner", owner);
  try {
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi
      .spyOn(api, "instanceSend")
      .mockImplementation((async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"]);
    await bootLive(api, hubStore);
    const internal = hubStore as unknown as {
      ensureOutbox: () => Promise<boolean>;
      flushAllOutbox: () => Promise<void>;
      outbox: {
        ownerId: string;
        refreshDurable: () => Promise<unknown>;
        pendingLeaseWakeups: () => string[];
        leaseWakeAt: (id: string) => number | undefined;
      };
    };
    expect(await internal.ensureOutbox()).toBe("ok");
    await internal.flushAllOutbox();
    expect(send).not.toHaveBeenCalled();

    const cmdX = "cmd_g18_i3_x_terminal";
    const cmdY = "cmd_g18_i3_y_reclaimed";
    const T1 = Date.now() + 2_000;
    const T2 = Date.now() + 20_000;
    const baseRow = (commandId: string, instanceId: string, journalId: string, until: number, leaseOwner = owner): OutboxRecord => ({
      commandId,
      clientRequestId: `local_${commandId}`,
      instanceId,
      journalId,
      prompt: commandId,
      createdAt: Date.now() - 1_000,
      attempts: 1,
      state: "inflight",
      lease: { owner: leaseOwner, until },
    });
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([baseRow(cmdX, INSTANCE, JOURNAL, T1), baseRow(cmdY, INS_Y, J_Y, T1)]),
    );
    await internal.outbox.refreshDurable();
    expect(internal.outbox.pendingLeaseWakeups().sort()).toEqual([cmdX, cmdY].sort());

    // Just before T1: X delivered (terminal) elsewhere; Y re-claimed FOREIGN
    // with deadline T2.
    await vi.advanceTimersByTimeAsync(1_999);
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        { ...baseRow(cmdX, INSTANCE, JOURNAL, T1), state: "sent", gotResponse: true, lease: undefined },
        baseRow(cmdY, INS_Y, J_Y, T2, foreign),
      ]),
    );

    // T1: both aligned fires run; X stops quietly and Y ADOPTS T2 — one wake.
    await vi.advanceTimersByTimeAsync(1);
    for (let i = 0; i < 40; i += 1) await Promise.resolve();
    expect(send).not.toHaveBeenCalled();
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([cmdY]);
    expect(internal.outbox.leaseWakeAt(cmdY)).toBe(T2);

    // Nothing just before T2 …
    await vi.advanceTimersByTimeAsync(17_999);
    expect(send).not.toHaveBeenCalled();
    // … exactly one real POST for Y at the adopted foreign deadline.
    await vi.advanceTimersByTimeAsync(1);
    for (let i = 0; i < 40 && send.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[4]).toBe(cmdY);
    for (let i = 0; i < 30; i += 1) await Promise.resolve();
    const stored = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored.find((r) => r.commandId === cmdY)?.state).toBe("sent");
    await vi.advanceTimersByTimeAsync(120_000);
    expect(send).toHaveBeenCalledTimes(1);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE17 item 1: recovering A -> never-mounted B with a FAILING seed self-drives recovery (timer armed, retry runs)", async () => {
  // Gate 16 fixed only the MOUNTED branch. On the unmounted path bindConnection
  // retires A's attempt and clears its watchdog; if B's initial eventsRead then
  // rejects (or times out) openFollowSocket never runs, so no followBound ever
  // armed anything and the machine sat in recovering with NO timer. The seed
  // must run inside a generation-scoped follow attempt whose failure arms the
  // ordinary probe/reconnect recovery.
  vi.useFakeTimers();
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore); // lists A = INSTANCE (journal JOURNAL)

  const JOURNAL_B = "obj_journal_b_g17_i1";
  const INSTANCE_B = "ins_unmounted_b_g17_i1";
  vi.spyOn(api, "instanceGet").mockImplementation(async (id: string) =>
    id === INSTANCE_B
      ? ({
          id: INSTANCE_B,
          journalId: JOURNAL_B,
          lifecycle: "running",
          revision: "0",
          durableSeq: "0",
        } as never as Awaited<ReturnType<Api["instanceGet"]>>)
      : ({} as Awaited<ReturnType<Api["instanceGet"]>>),
  );
  const emptyRead = {
    events: [],
    windowFromSeq: "1",
    durableSeq: "0",
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>;
  // B's journal is down for the whole test: seed AND probe reject.
  const eventsRead = vi.spyOn(api, "eventsRead").mockImplementation(async (args) => {
    if (args.journalId === JOURNAL_B) throw new Error("B journal unreachable");
    return emptyRead;
  });
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    async () =>
      ({
        subscriptionId: "sub_g17i1" as never,
        getReadyState: () => 1,
        snapshot: { asOfSeq: "0", fromSeq: null },
        close: () => undefined,
      }) as never,
  );

  // Mount A live (a framed socket).
  await hubStore.follow(INSTANCE);
  const store = hubStore as unknown as {
    connection: ConnectionMachine;
    setFollowLiveForTest: (open: boolean, framed: boolean, journalId?: string) => void;
  };

  // A goes recovering: a resume whose reopen hangs (blocked upgrade).
  vi.mocked(api.eventsSubscribe).mockImplementation(() => new Promise<never>(() => {}) as never);
  store.setFollowLiveForTest(false, false, JOURNAL);
  store.connection.dispatch({ type: "close" });
  store.connection.dispatch({ type: "online" });
  expect(store.connection.state).toBe("recovering");

  // Navigate to B, NEVER mounted before. bind-away retires A; B's seed fails.
  const bReads = () => eventsRead.mock.calls.filter((c) => c[0].journalId === JOURNAL_B).length;
  await expect(hubStore.follow(INSTANCE_B)).rejects.toThrow("B journal unreachable");
  // The failure settled through the real probe (B's probe read rejects too):
  // loud offline WITH a reconnect clock — never a timer-less recovering.
  for (let i = 0; i < 5; i += 1) await Promise.resolve();
  const timerNames = () =>
    [...(store.connection as unknown as { timers: Map<string, unknown> }).timers.keys()];
  expect(store.connection.state).toBe("offline");
  expect(timerNames()).toContain("reconnect");
  expect(bReads()).toBeGreaterThanOrEqual(2); // seed + probe

  // The backoff fires on its OWN: the real resume action reopens B (via the
  // resume chain -> reopenFollow -> follow), which seeds B again. No UI event.
  const readsBeforeRetry = bReads();
  await vi.advanceTimersByTimeAsync(3_000);
  for (let i = 0; i < 10; i += 1) await Promise.resolve();
  expect(bReads()).toBeGreaterThan(readsBeforeRetry);
  // And the machine is still not stranded: after the retried seed fails it
  // arms another reconnect clock.
  expect(timerNames()).toContain("reconnect");
  vi.useRealTimers();
  hubStore.logout();
});

it("GATE18 item 6 (real store): an offline-retired attempt's delayed CATCH-UP rejection never crosses a binding onto adopted live B — the subscribe succeeds; the catch-up after openFollowSocket is what rejects", async () => {
  // Gate-19 acceptance: the previous version of this test rejected the
  // SUBSCRIBE (eventsSubscribe #2 hung) while every eventsRead succeeded, so
  // it never reached the path it claimed — the delayed rejection of
  // resumeAfterReconnect's bounded CATCH-UP read (store.ts reopenFollow:
  // openFollowSocket THEN client.resumeAfterReconnect). Here subscribe #2
  // really opens (its snapshot frames), the socket genuinely closes mid
  // attempt through the REAL subscribe onClose hook, a browser offline event
  // retires the attempt, navigation adopts the MOUNTED B live, and only THEN
  // does A's pending catch-up read (limit 128) reject. The stale completion
  // is dropped on the binding-generation guard: B stays live and no
  // settlement probe or reopen runs for either journal.
  vi.useFakeTimers();
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore); // lists A = INSTANCE (journal JOURNAL)

  const JOURNAL_B = "obj_journal_b_g18_i6";
  const INSTANCE_B = "ins_mounted_b_g18_i6";
  vi.spyOn(api, "instanceGet").mockImplementation(async (id: string) =>
    id === INSTANCE_B
      ? ({
          id: INSTANCE_B,
          journalId: JOURNAL_B,
          lifecycle: "running",
          revision: "0",
          durableSeq: "0",
        } as never as Awaited<ReturnType<Api["instanceGet"]>>)
      : ({} as Awaited<ReturnType<Api["instanceGet"]>>),
  );
  const emptyRead = {
    events: [],
    windowFromSeq: "1",
    durableSeq: "0",
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>;
  let catchUpReads = 0;
  let rejectCatchUp!: (err: Error) => void;
  vi.spyOn(api, "eventsRead").mockImplementation(
    (async (args: Parameters<Api["eventsRead"]>[0]) => {
      // The resume catch-up is the bounded limit-128 read AFTER the socket
      // opened. A's FIRST one stays pending until the test rejects it across
      // the binding; every other read (seeds/probes/later catch-ups) succeeds.
      if (args.limit === 128 && args.journalId === JOURNAL) {
        catchUpReads += 1;
        if (catchUpReads === 1) {
          await new Promise<never>((_resolve, reject) => {
            rejectCatchUp = reject;
          });
        }
      }
      return emptyRead;
    }) as Api["eventsRead"],
  );
  let aSubscribeCalls = 0;
  let bSubscribeCalls = 0;
  let closeSub2: (() => void) | null = null;
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (
      journalId: string,
      _afterSeq: unknown,
      _onBatch: unknown,
      _onGap: unknown,
      hooks?: { onOpen?: () => void; onClose?: () => void; onFrame?: () => void },
    ) => {
      if (journalId === JOURNAL) {
        aSubscribeCalls += 1;
        // Subscribe #2 REALLY opens; expose its genuine close hook.
        if (aSubscribeCalls === 2) closeSub2 = () => hooks?.onClose?.();
      } else {
        bSubscribeCalls += 1;
      }
      return {
        subscriptionId: `sub_g18i6_${journalId}_${aSubscribeCalls + bSubscribeCalls}` as never,
        getReadyState: () => 1,
        snapshot: { asOfSeq: "0", fromSeq: null },
        close: () => undefined,
      };
    }) as unknown as Api["eventsSubscribe"],
  );

  // Mount A live, then B, then return to A — both journals are mounted.
  await hubStore.follow(INSTANCE);
  await hubStore.follow(INSTANCE_B);
  expect(bSubscribeCalls).toBe(1);
  await hubStore.follow(INSTANCE);
  const store = hubStore as unknown as {
    connection: ConnectionMachine;
    connectionBoundJournal: string;
    setFollowLiveForTest: (open: boolean, framed: boolean, journalId?: string) => void;
  };
  store.setFollowLiveForTest(true, true, JOURNAL);
  store.setFollowLiveForTest(true, true, JOURNAL_B);
  expect(aSubscribeCalls).toBe(1);

  // A's link dies; the real online resume opens subscribe #2 (snapshot frames
  // it live) and then blocks in its catch-up read.
  store.setFollowLiveForTest(false, false, JOURNAL);
  store.connection.dispatch({ type: "online" });
  for (let i = 0; i < 30 && aSubscribeCalls < 2; i += 1) await Promise.resolve();
  expect(aSubscribeCalls).toBe(2);
  for (let i = 0; i < 30 && catchUpReads < 1; i += 1) await Promise.resolve();
  expect(catchUpReads).toBe(1);
  expect(store.connection.state).toBe("live"); // snapshot-certified while read pending

  // The follow socket genuinely closes mid-attempt (REAL subscribe hook ->
  // machine latch), then a browser offline event retires the attempt.
  (closeSub2 as (() => void) | null)?.();
  window.dispatchEvent(new Event("offline"));
  expect(store.connection.state).toBe("offline");

  // Navigate to the MOUNTED B: real bind + adoption certifies B live.
  await hubStore.follow(INSTANCE_B);
  expect(store.connectionBoundJournal).toBe(JOURNAL_B);
  expect(store.connection.state).toBe("live");

  // A's delayed CATCH-UP read now rejects across the binding.
  rejectCatchUp(new Error("A catch-up lost the race"));
  for (let i = 0; i < 30; i += 1) await Promise.resolve();
  expect(store.connection.state).toBe("live");

  // The stale completion was dropped BEFORE any settlement: no REST probe for
  // either journal, no retry of A's catch-up, and no subscribe reopened.
  const probes = () =>
    vi.mocked(api.eventsRead).mock.calls.filter((c) => c[0]?.limit === 1).length;
  expect(probes()).toBe(0);
  expect(catchUpReads).toBe(1);
  expect(aSubscribeCalls).toBe(2);
  expect(bSubscribeCalls).toBe(1);

  // Still live with nothing reopened well past the rejection.
  await vi.advanceTimersByTimeAsync(10_000);
  for (let i = 0; i < 20; i += 1) await Promise.resolve();
  expect(store.connection.state).toBe("live");
  expect(probes()).toBe(0);
  expect(catchUpReads).toBe(1);
  expect(aSubscribeCalls).toBe(2);
  expect(bSubscribeCalls).toBe(1);
  vi.useRealTimers();
  hubStore.logout();
});

it("GATE19 item 6b (real store): a same-binding offline-retired rejection is CONSUMED — settlement probe + reconnect clock — then an explicit online after the retry deadline runs a REAL resume + subscribe and certifies live", async () => {
  // Replaces the setupTracked machine unit test. Same binding throughout: the
  // reopen subscribe really opens, its socket genuinely closes while its
  // catch-up read (limit 128) is pending, a browser offline event retires the
  // attempt, and the delayed catch-up then REJECTS. Because the binding did
  // not move, the rejection must be CONSUMED (not dropped): the settlement
  // probe runs, REST being down keeps the loud offline reconnect clock; past
  // the retry deadline an explicit online starts a REAL second resume whose
  // subscribe + catch-up succeed and certify live.
  vi.useFakeTimers();
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore); // lists A = INSTANCE (journal JOURNAL)

  const emptyRead = {
    events: [],
    windowFromSeq: "1",
    durableSeq: "0",
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>;
  let catchUpReads = 0;
  let rejectCatchUp!: (err: Error) => void;
  vi.spyOn(api, "eventsRead").mockImplementation(
    (async (args: Parameters<Api["eventsRead"]>[0]) => {
      // Settlement probes always fail (REST down for follow reads).
      if (args.limit === 1) throw new Error("probe unreachable");
      if (args.limit === 128) {
        catchUpReads += 1;
        // The FIRST catch-up (resume #1) blocks and is rejected after the
        // offline retirement; every later catch-up (resume #2) succeeds.
        if (catchUpReads === 1) {
          await new Promise<never>((_resolve, reject) => {
            rejectCatchUp = reject;
          });
        }
      }
      return emptyRead;
    }) as Api["eventsRead"],
  );
  let subscribes = 0;
  let closeSub2: (() => void) | null = null;
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (
      _journalId: string,
      _afterSeq: unknown,
      _onBatch: unknown,
      _onGap: unknown,
      hooks?: { onOpen?: () => void; onClose?: () => void; onFrame?: () => void },
    ) => {
      subscribes += 1;
      if (subscribes === 2) closeSub2 = () => hooks?.onClose?.();
      return {
        subscriptionId: `sub_g19_i6b_${subscribes}` as never,
        getReadyState: () => 1,
        snapshot: { asOfSeq: "0", fromSeq: null },
        close: () => undefined,
      };
    }) as unknown as Api["eventsSubscribe"],
  );

  await hubStore.follow(INSTANCE);
  expect(subscribes).toBe(1);
  const store = hubStore as unknown as {
    connection: ConnectionMachine;
    setFollowLiveForTest: (open: boolean, framed: boolean, journalId?: string) => void;
  };
  store.setFollowLiveForTest(true, true, JOURNAL);
  const timerNames = () =>
    [...(store.connection as unknown as { timers: Map<string, unknown> }).timers.keys()];
  const probes = () =>
    vi.mocked(api.eventsRead).mock.calls.filter((c) => c[0]?.limit === 1).length;

  // Link dies; resume #1 opens subscribe #2 and blocks in its catch-up read.
  store.setFollowLiveForTest(false, false, JOURNAL);
  store.connection.dispatch({ type: "online" });
  for (let i = 0; i < 40 && subscribes < 2; i += 1) await Promise.resolve();
  expect(subscribes).toBe(2);
  for (let i = 0; i < 40 && catchUpReads < 1; i += 1) await Promise.resolve();
  expect(catchUpReads).toBe(1);

  // Same-binding socket close (latched) then browser offline retirement.
  (closeSub2 as (() => void) | null)?.();
  window.dispatchEvent(new Event("offline"));
  expect(store.connection.state).toBe("offline");

  // The delayed catch-up rejection is CONSUMED: the settlement probe runs and
  // fails, leaving the loud offline state with a reconnect clock.
  rejectCatchUp(new Error("catch-up lost"));
  for (let i = 0; i < 40 && !timerNames().includes("reconnect"); i += 1) {
    await Promise.resolve();
  }
  expect(probes()).toBe(1);
  expect(store.connection.state).toBe("offline");
  expect(timerNames()).toContain("reconnect");

  // Past the retry deadline an explicit online starts a REAL second resume:
  // a new follow subscribe and a real catch-up that succeeds. Cancel the
  // autonomous reconnect handle so the foreground online is the sole driver
  // under observation (the clock's existence was just proven above); let wall
  // time cross the deadline first.
  {
    const timers = (store.connection as unknown as {
      timers: Map<string, ReturnType<typeof setTimeout>>;
    }).timers;
    const h = timers.get("reconnect");
    if (h) clearTimeout(h);
    timers.delete("reconnect");
  }
  await vi.advanceTimersByTimeAsync(30_000);
  expect(subscribes).toBe(2); // nothing resumed autonomously
  store.connection.dispatch({ type: "online" });
  for (let i = 0; i < 100 && store.connection.state !== "live"; i += 1) {
    await vi.advanceTimersByTimeAsync(0);
  }
  expect(store.connection.state).toBe("live");
  expect(subscribes).toBe(3);
  expect(catchUpReads).toBe(2);
  // The reconnect tail is gone; the frame watchdog owns the link.
  expect(timerNames()).toContain("stale");
  expect(timerNames()).not.toContain("reconnect");
  expect(timerNames()).not.toContain("watchdog");
  vi.useRealTimers();
  hubStore.logout();
});

it("GATE19 item 5 (real store): a follow socket that genuinely closes while the resume catch-up read is pending never certifies live when that catch-up later succeeds — recovery reopens for real", async () => {
  // Reconnect opens the follow socket (subscribe snapshot frames it live),
  // the socket genuinely closes WHILE resumeAfterReconnect's REST read is in
  // flight (closedDuringResume latched through the REAL eventsSubscribe
  // onClose), and the delayed catch-up then SUCCEEDS. The successful read
  // certifies journal catch-up, not the link: the machine must leave live,
  // run the dead-link settlement, and reopen a REAL follow subscribe.
  vi.useFakeTimers();
  try {
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore); // lists A = INSTANCE (journal JOURNAL)

  const emptyRead = {
    events: [],
    windowFromSeq: "1",
    durableSeq: "0",
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>;

  // The FIRST resume catch-up read (limit 128) blocks on a gate; while it is
  // pending it dispatches the genuine close of the reopen subscribe. The
  // second resume read (the real recovery reopen) resolves immediately.
  let resumeReads = 0;
  let closeSub2: (() => void) | null = null;
  let closeDispatched = false;
  let releaseResume: () => void = () => undefined;
  const resumeGate = new Promise<void>((resolve) => {
    releaseResume = resolve;
  });
  vi.spyOn(api, "eventsRead").mockImplementation(
    (async (args: Parameters<Api["eventsRead"]>[0]) => {
      if (args.limit === 128) {
        resumeReads += 1;
        if (resumeReads === 1) {
          // Socket genuinely closes WHILE this catch-up read is pending.
          closeSub2?.();
          closeDispatched = true;
          await resumeGate;
        }
      }
      return emptyRead;
    }) as Api["eventsRead"],
  );

  let subscribes = 0;
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (
      _journalId: string,
      _afterSeq: unknown,
      _onBatch: unknown,
      _onGap: unknown,
      hooks?: { onOpen?: () => void; onClose?: () => void; onFrame?: () => void },
    ) => {
      subscribes += 1;
      if (subscribes === 2) {
        const close = hooks?.onClose;
        closeSub2 = () => close?.();
      }
      return {
        subscriptionId: `sub_g19_i5_${subscribes}` as never,
        getReadyState: () => 1,
        snapshot: { asOfSeq: "0", fromSeq: null },
        close: () => undefined,
      };
    }) as unknown as Api["eventsSubscribe"],
  );

  // Initial real mount: seed + subscribe #1, framed live.
  await hubStore.follow(INSTANCE);
  expect(subscribes).toBe(1);
  const store = hubStore as unknown as {
    connection: ConnectionMachine;
    setFollowLiveForTest: (open: boolean, framed: boolean, journalId?: string) => void;
  };
  store.setFollowLiveForTest(true, true, JOURNAL);

  // Record every link transition from here on.
  const states: string[] = [];
  hubStore.subscribe(() => {
    const s = hubStore.getSnapshot() as unknown as { connection?: string };
    const last = states.at(-1);
    if (s.connection && s.connection !== last) states.push(s.connection);
  });

  // Link dies; a real online-driven resume opens subscribe #2; its snapshot
  // frames live while the catch-up read is still pending.
  store.setFollowLiveForTest(false, false, JOURNAL);
  store.connection.dispatch({ type: "online" });
  for (let i = 0; i < 50 && subscribes < 2; i += 1) await Promise.resolve();
  expect(subscribes).toBe(2);
  for (let i = 0; i < 50 && !closeDispatched; i += 1) await Promise.resolve();
  expect(closeDispatched).toBe(true);
  // Catch-up still pending; the snapshot had optimistically framed it live.
  expect(store.connection.state).toBe("live");

  // The delayed catch-up SUCCEEDS. The completion must not certify live:
  // the dead-link settlement runs and the machine drops to quiet stale
  // (REST reachable) WITHOUT reopening yet — still only two subscribes.
  releaseResume();
  for (let i = 0; i < 100 && store.connection.state !== "stale"; i += 1) {
    await Promise.resolve();
  }
  expect(store.connection.state).toBe("stale");
  expect(subscribes).toBe(2);
  // The successful resume did not keep the dead-link "live" label: after the
  // latched-close live it transited recovering -> stale.
  const firstLive = states.indexOf("live");
  expect(firstLive).toBeGreaterThanOrEqual(0);
  expect(states.slice(firstLive + 1)).toContain("stale");

  // Recovery proceeds on the real stale-probe clock: the probe succeeds and
  // the quiet reopen opens a REAL follow subscribe (#3) with a real catch-up;
  // the NEW socket's own snapshot is what re-certifies live.
  await vi.advanceTimersByTimeAsync(REST_PROBE_MS);
  for (let i = 0; i < 100 && subscribes < 3; i += 1) {
    await vi.advanceTimersByTimeAsync(0);
  }
  expect(subscribes).toBe(3);
  for (let i = 0; i < 100 && store.connection.state !== "live"; i += 1) {
    await vi.advanceTimersByTimeAsync(0);
  }
  expect(store.connection.state).toBe("live");
  expect(resumeReads).toBe(2); // the reopen's catch-up ran for real too
  hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("GATE20 item 5 (real store): offline retires the attempt, THEN its reopen socket genuinely closes, THEN the pending catch-up succeeds — never certified live", async () => {
  // Gate 19 item 5 covered close -> offline. This is the inverse ordering the
  // reviewer found: browser OFFLINE arrives first (the attempt is retired with
  // resumeRetiredOffline while resumeAfterReconnect's REST read is pending),
  // the follow socket the attempt opened genuinely CLOSES only afterwards, and
  // the delayed catch-up then SUCCEEDS before the reconnect timer runs. The
  // success proves journal catch-up, not a live link: the close must stay
  // latched for the offline-retired attempt and the completion takes the dead-
  // link settlement (quiet stale; a REAL reopen re-certifies), never "live".
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    await bootLive(api, hubStore);

    const emptyRead = {
      events: [],
      windowFromSeq: "1",
      durableSeq: "0",
      reachedAfterSeq: true,
    } as Awaited<ReturnType<Api["eventsRead"]>>;

    let resumeReads = 0;
    let closeSub2: (() => void) | null = null;
    let closeDispatched = false;
    let releaseResume: () => void = () => undefined;
    let releaseOrderGate: () => void = () => undefined;
    const resumeGate = new Promise<void>((resolve) => {
      releaseResume = resolve;
    });
    // The close is allowed ONLY after the test dispatched browser `offline`,
    // forcing the offline -> close ordering this regression targets.
    const orderGate = new Promise<void>((resolve) => {
      releaseOrderGate = resolve;
    });
    vi.spyOn(api, "eventsRead").mockImplementation(
      (async (args: Parameters<Api["eventsRead"]>[0]) => {
        if (args.limit === 128) {
          resumeReads += 1;
          if (resumeReads === 1) {
            await orderGate;
            // Socket genuinely closes WHILE this catch-up read is pending,
            // but strictly AFTER the browser offline event retired the attempt.
            closeSub2?.();
            closeDispatched = true;
            await resumeGate;
          }
        }
        return emptyRead;
      }) as Api["eventsRead"],
    );

    let subscribes = 0;
    vi.spyOn(api, "eventsSubscribe").mockImplementation(
      (async (
        _journalId: string,
        _afterSeq: unknown,
        _onBatch: unknown,
        _onGap: unknown,
        hooks?: { onOpen?: () => void; onClose?: () => void; onFrame?: () => void },
      ) => {
        subscribes += 1;
        if (subscribes === 2) {
          const close = hooks?.onClose;
          closeSub2 = () => close?.();
        }
        return {
          subscriptionId: `sub_g20_i5_${subscribes}` as never,
          getReadyState: () => 1,
          snapshot: { asOfSeq: "0", fromSeq: null },
          close: () => undefined,
        };
      }) as unknown as Api["eventsSubscribe"],
    );

    // Initial real mount: subscribe #1, framed live.
    await hubStore.follow(INSTANCE);
    expect(subscribes).toBe(1);
    const store = hubStore as unknown as {
      connection: ConnectionMachine;
      setFollowLiveForTest: (open: boolean, framed: boolean, journalId?: string) => void;
    };
    store.setFollowLiveForTest(true, true, JOURNAL);

    // Link dies; an online-driven resume opens subscribe #2; its snapshot
    // frames live while the first catch-up read blocks on the gate.
    store.setFollowLiveForTest(false, false, JOURNAL);
    store.connection.dispatch({ type: "online" });
    for (let i = 0; i < 50 && subscribes < 2; i += 1) await Promise.resolve();
    expect(subscribes).toBe(2);
    // Let subscribe #2's snapshot frame the optimistic live state while the
    // gated catch-up read is still pending (close still held back).
    for (let i = 0; i < 50 && store.connection.state !== "live"; i += 1) {
      await Promise.resolve();
    }
    expect(store.connection.state).toBe("live");

    // Ordering under test: browser OFFLINE retires the attempt first …
    store.connection.dispatch({ type: "offline" });
    for (let i = 0; i < 20; i += 1) await Promise.resolve();
    expect(store.connection.state).toBe("offline");
    expect(closeDispatched).toBe(false);
    // … allow the pending read's real onClose to land THEN …
    releaseOrderGate();
    for (let i = 0; i < 50 && !closeDispatched; i += 1) await Promise.resolve();
    expect(closeDispatched).toBe(true);
    expect(store.connection.state).toBe("offline");
    expect(subscribes).toBe(2);
    // … and the pending catch-up SUCCEEDS (no fake time passes: the reconnect
    // timer has not run).
    releaseResume();
    for (let i = 0; i < 100 && store.connection.state === "offline"; i += 1) {
      await Promise.resolve();
    }
    // Dead-link settlement: quiet stale on the successful REST read, never the
    // "live" label over a socket that already closed.
    expect(store.connection.state).toBe("stale");
    expect(subscribes).toBe(2);

    // Recovery reopens for REAL on the stale-probe clock: subscribe #3 with a
    // real catch-up, and only the NEW socket's snapshot re-certifies live.
    await vi.advanceTimersByTimeAsync(REST_PROBE_MS);
    for (let i = 0; i < 100 && subscribes < 3; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(subscribes).toBe(3);
    for (let i = 0; i < 100 && store.connection.state !== "live"; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(store.connection.state).toBe("live");
    expect(resumeReads).toBe(2);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("GATE18 item 5: a duplicate seed that finds the journal mounted with a DEAD socket ends the current-binding attempt instead of stranding it on the 20 s watchdog", async () => {
  // follow(B) attempt 1 (unmounted seed in flight) -> navigate to mounted live
  // A (retries/retries attempt 1) -> follow(B) again before the seed returns
  // (attempt 2) -> attempt 1's seed returns FIRST: it journals.set B, then its
  // openFollowSocket rejects (failure dropped: stale generation). Attempt 2's
  // seed then returns to a mounted-but-dead B: the journals.has branch used to
  // return WITHOUT followAttemptEnd, leaving attempt 2 in recovering with no
  // timer until the 20 s watchdog. It must end the attempt as failed so the
  // real probe/reopen path self-drives before the watchdog.
  vi.useFakeTimers();
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore); // lists A = INSTANCE (journal JOURNAL)

  const JOURNAL_B = "obj_journal_b_g18_i5";
  const INSTANCE_B = "ins_unmounted_b_g18_i5";
  vi.spyOn(api, "instanceGet").mockImplementation(async (id: string) =>
    id === INSTANCE_B
      ? ({
          id: INSTANCE_B,
          journalId: JOURNAL_B,
          lifecycle: "running",
          revision: "0",
          durableSeq: "0",
        } as never as Awaited<ReturnType<Api["instanceGet"]>>)
      : ({} as Awaited<ReturnType<Api["instanceGet"]>>),
  );
  const emptyRead = {
    events: [],
    windowFromSeq: "1",
    durableSeq: "0",
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>;
  // The first two B seeds are externally ordered; later B reads (the settle
  // probe, the reopen catch-up) answer immediately.
  const seedResolvers: Array<() => void> = [];
  let bSeedCalls = 0;
  vi.spyOn(api, "eventsRead").mockImplementation((async (args) => {
    if (args.journalId !== JOURNAL_B) return emptyRead;
    bSeedCalls += 1;
    if (bSeedCalls <= 2) {
      return await new Promise<Awaited<ReturnType<Api["eventsRead"]>>>((resolve) => {
        seedResolvers[bSeedCalls - 1] = () => resolve(emptyRead);
      });
    }
    return emptyRead;
  }) as Api["eventsRead"]);
  // B's FIRST subscribe (attempt 1's late open) is blocked; every later one
  // opens live with an immediate snapshot. A's subscribe always succeeds.
  let bSubscribeCalls = 0;
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (journalId: string) => {
      if (journalId === JOURNAL_B) {
        bSubscribeCalls += 1;
        if (bSubscribeCalls === 1) throw new Error("attempt 1 follow blocked");
      }
      return {
        subscriptionId: `sub_g18i5_${bSubscribeCalls}` as never,
        getReadyState: () => 1,
        snapshot: { asOfSeq: "0", fromSeq: null },
        close: () => undefined,
      };
    }) as unknown as Api["eventsSubscribe"],
  );

  // Mount A live (a framed socket).
  await hubStore.follow(INSTANCE);
  const store = hubStore as unknown as {
    connection: ConnectionMachine;
    setFollowLiveForTest: (open: boolean, framed: boolean, journalId?: string) => void;
  };
  store.setFollowLiveForTest(true, true, JOURNAL);
  const timerNames = () =>
    [...(store.connection as unknown as { timers: Map<string, unknown> }).timers.keys()];

  // Attempt 1: follow(B) while its seed is still pending.
  const followB1 = hubStore.follow(INSTANCE_B);
  void followB1.catch(() => undefined);
  for (let i = 0; i < 10; i += 1) await Promise.resolve();
  expect(bSeedCalls).toBe(1);

  // Navigate to the MOUNTED live A (retries attempt 1, adopts A's socket).
  await hubStore.follow(INSTANCE);
  expect(store.connection.state).toBe("live");

  // Attempt 2: follow(B) again before attempt 1's seed returns.
  const followB2 = hubStore.follow(INSTANCE_B);
  void followB2.catch(() => undefined);
  for (let i = 0; i < 10; i += 1) await Promise.resolve();
  expect(bSeedCalls).toBe(2);
  expect(store.connection.state).toBe("recovering");

  // Attempt 1's seed returns: it journals.set B and its open is rejected; the
  // failure is dropped (generation stale) and followB1 rejects.
  seedResolvers[0]!();
  await expect(followB1).rejects.toThrow("attempt 1 follow blocked");
  for (let i = 0; i < 10; i += 1) await Promise.resolve();
  expect(bSubscribeCalls).toBe(1);
  expect(store.connection.state).toBe("recovering");

  // Attempt 2's seed returns to a mounted-but-dead B.
  seedResolvers[1]!();
  await followB2;
  for (let i = 0; i < 20; i += 1) await Promise.resolve();

  // FIX: the attempt ended through the NORMAL failure path — quiet stale with
  // the probe/reopen clocks armed, NOT a timer-less recovering waiting on the
  // 20 s watchdog.
  expect(store.connection.state).toBe("stale");
  expect(timerNames()).toContain("probe");
  expect(timerNames()).not.toContain("watchdog");
  expect(bSubscribeCalls).toBe(1);

  // The quiet probe fires well before the 20 s watchdog and reopens B for
  // real: a second subscribe succeeds, a snapshot frame certifies live.
  await vi.advanceTimersByTimeAsync(REST_PROBE_MS);
  for (let i = 0; i < 30; i += 1) await vi.advanceTimersByTimeAsync(0);
  expect(bSubscribeCalls).toBe(2);
  expect(store.connection.state).toBe("live");

  // No competing reopen from the 20 s watchdog (the mock socket is silent, so
  // the frame watchdog's own ordinary stale cycle owns anything past 30 s).
  await vi.advanceTimersByTimeAsync(10_000);
  expect(bSubscribeCalls).toBe(2);
  expect(store.connection.state).toBe("live");
  vi.useRealTimers();
  hubStore.logout();
});

it("GATE17 item 5a: repeated live A -> mounted-B navigation opens ZERO additional subscribes and re-seeds nothing", async () => {
  // The gate-16 live regression test ended still bound to B. Recreate the
  // live A->B transition on its own and prove adoption reopens nothing: a live
  // mounted rebind certifies the existing socket and neither the resume action
  // (no eventsSubscribe) nor a new seed (no eventsRead for either journal)
  // runs.
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore); // A = INSTANCE / JOURNAL

  const JOURNAL_B = "obj_journal_b_g17_5a";
  const INSTANCE_B = "ins_mounted_b_g17_5a";
  vi.spyOn(api, "instanceGet").mockImplementation(async (id: string) =>
    id === INSTANCE_B
      ? ({
          id: INSTANCE_B,
          journalId: JOURNAL_B,
          lifecycle: "running",
          revision: "0",
          durableSeq: "0",
        } as never as Awaited<ReturnType<Api["instanceGet"]>>)
      : ({} as Awaited<ReturnType<Api["instanceGet"]>>),
  );
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    windowFromSeq: "1",
    durableSeq: "0",
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>);
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(
    async () =>
      ({
        subscriptionId: "sub_g17_5a" as never,
        getReadyState: () => 1,
        snapshot: { asOfSeq: "0", fromSeq: null },
        close: () => undefined,
      }) as never,
  );

  const store = hubStore as unknown as {
    connection: ConnectionMachine;
    setFollowLiveForTest: (open: boolean, framed: boolean, journalId?: string) => void;
  };
  // Mount B first, then return to A, so A->B below lands on the MOUNTED branch.
  await hubStore.follow(INSTANCE_B);
  await hubStore.follow(INSTANCE);
  store.setFollowLiveForTest(true, true, JOURNAL);
  store.setFollowLiveForTest(true, true, JOURNAL_B);

  const reads = vi.mocked(api.eventsRead);
  for (const to of [INSTANCE_B, INSTANCE, INSTANCE_B, INSTANCE, INSTANCE_B]) {
    reads.mockClear();
    subscribe.mockClear();
    await hubStore.follow(to);
    expect(store.connection.state).toBe("live");
    expect(subscribe).not.toHaveBeenCalled();
    expect(reads).not.toHaveBeenCalled();
  }
  hubStore.logout();
});

it("GATE16 item 4c: an accepted delivery stamped during offline recovery is caught up via the REAL path after navigating away to mounted B", async () => {
  // Replaces the gate-15 version that injected debt through private
  // recordOwed. Here the debt forms the way production forms it: a pending row
  // is delivered while the bound session A is in recovering/offline (its
  // follow does not own catch-up), so the drain chains recordOwed. Navigating
  // to already-mounted B then services the abandoned debt (eventsRead A).
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore); // mounts A = INSTANCE (journal JOURNAL)
  const store = hubStore as unknown as {
    connection: ConnectionMachine;
    setFollowLiveForTest: (open: boolean, framed: boolean, journalId?: string) => void;
    reconcileOwed: Map<string, unknown>;
  };

  const JOURNAL_B = "obj_journal_b";
  const INSTANCE_B = "ins_mounted_b";
  const instanceB = {
    id: INSTANCE_B,
    journalId: JOURNAL_B,
    lifecycle: "running",
    revision: "0",
    durableSeq: "0",
  } as never as Awaited<ReturnType<Api["instanceGet"]>>;
  vi.spyOn(api, "instanceGet").mockImplementation(async (id: string) =>
    id === INSTANCE_B ? instanceB : ({} as Awaited<ReturnType<Api["instanceGet"]>>),
  );
  const eventsReadSpy = vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    windowFromSeq: "1",
    durableSeq: "0",
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>);
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async () =>
    ({
      subscriptionId: "sub_b_c" as never,
      getReadyState: () => 1,
      snapshot: { asOfSeq: "0", fromSeq: null },
      close: () => undefined,
    }) as never,
  );
  // A's POST succeeds (the row reaches the Hub); its deferred journal
  // catch-up is what must later be serviced.
  vi.spyOn(api, "instanceSend").mockImplementation(
    (async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) =>
      ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
  );

  // Mount B first, return to A, so the navigation lands on the mounted branch.
  await hubStore.follow(INSTANCE_B);
  await hubStore.follow(INSTANCE);

  // Seed a pending row and take A's link OFFLINE: the follow no longer owns
  // catch-up (followOwnsCatchupNow checks offline/recovering + bound journal).
  localStorage.setItem(
    OUTBOX_LS_KEY,
    JSON.stringify([
      {
        commandId: "cmd_owed_real",
        clientRequestId: "local_owed",
        instanceId: INSTANCE,
        journalId: JOURNAL,
        prompt: "owed",
        createdAt: Date.now() - 1_000,
        attempts: 0,
        state: "pending",
      } satisfies OutboxRecord,
    ]),
  );
  store.setFollowLiveForTest(false, false, JOURNAL);
  store.connection.dispatch({ type: "close" });
  store.connection.dispatch({ type: "offline" });

  // The REAL delivery path: POST succeeds while A is offline; the post-drain
  // chain sees the follow does not own catch-up and records debt.
  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  await Promise.resolve();
  await Promise.resolve();
  expect(store.reconcileOwed.has(INSTANCE)).toBe(true);

  // Navigate to the already-mounted B. bindConnection bind-away services A's
  // abandoned debt via a bounded journal catch-up (eventsRead on A's journal).
  eventsReadSpy.mockClear();
  await hubStore.follow(INSTANCE_B);
  await vi.waitFor(
    () => expect(eventsReadSpy.mock.calls.some((c) => c[0].journalId === JOURNAL)).toBe(true),
    { timeout: 2_000 },
  );
  await vi.waitFor(() => expect(store.reconcileOwed.has(INSTANCE)).toBe(false), { timeout: 2_000 });
  hubStore.logout();
});

it("GATE15 item 3: navigating away from mounted A (deferred REST catch-up owed) to mounted B runs A's catch-up via the real follow() path", async () => {
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore); // mounts A = INSTANCE (journal JOURNAL)

  const JOURNAL_B = "obj_journal_b";
  const INSTANCE_B = "ins_mounted_b";
  const instanceB = {
    id: INSTANCE_B,
    journalId: JOURNAL_B,
    lifecycle: "running",
    revision: "0",
    durableSeq: "0",
  } as never as Awaited<ReturnType<Api["instanceGet"]>>;
  vi.spyOn(api, "instanceGet").mockImplementation(async (id: string) =>
    id === INSTANCE_B ? instanceB : ({} as Awaited<ReturnType<Api["instanceGet"]>>),
  );
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    windowFromSeq: "1",
    durableSeq: "0",
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>);
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async () =>
    ({
      subscriptionId: "sub_b" as never,
      getReadyState: () => 1,
      snapshot: { asOfSeq: "0", fromSeq: null },
      close: () => undefined,
    }) as never,
  );

  // First mount B, then return to A, so the test navigation below lands on the
  // mounted-journal early-return branch (the branch that used to skip
  // bindConnection).
  await hubStore.follow(INSTANCE_B);
  await hubStore.follow(INSTANCE);

  // A owes a deferred catch-up: its accepted response arrived during
  // reconnect backoff and was stamped via recordOwed rather than read inline.
  const storeInternal = hubStore as unknown as {
    recordOwed: (id: string) => void;
    reconcileOwed: Map<string, unknown>;
  };
  storeInternal.recordOwed(INSTANCE);
  expect(storeInternal.reconcileOwed.has(INSTANCE)).toBe(true);

  // Navigate to the already-mounted B (as SessionPage does). With the fix
  // follow() routes through bindConnection, which sees the bind-away A→B and
  // services A's abandoned debt immediately.
  const journalReadsAfterNav: string[] = [];
  vi.mocked(api.eventsRead).mockImplementation(async (args) => {
    journalReadsAfterNav.push(args.journalId);
    return {
      events: [],
      windowFromSeq: "1",
      durableSeq: "0",
      reachedAfterSeq: true,
    } as Awaited<ReturnType<Api["eventsRead"]>>;
  });
  await hubStore.follow(INSTANCE_B);
  await vi.waitFor(
    () => expect(journalReadsAfterNav).toContain(JOURNAL),
    { timeout: 2_000 },
  );
  // Debt honoured after the catch-up completed.
  await vi.waitFor(() => expect(storeInternal.reconcileOwed.has(INSTANCE)).toBe(false), { timeout: 2_000 });
  hubStore.logout();
});

it("discovers and delivers a row another tab persisted after this tab loaded", async () => {
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore);
  // No rows when this tab's outbox cache loaded.
  expect(JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]")).toHaveLength(0);

  // Another tab enqueues after load: the row exists only in the durable store.
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([row({ commandId: "cmd_late_foreign" })]));
  const send = vi
    .spyOn(api, "instanceSend")
    .mockResolvedValue({
      relatedCommandIds: [],
      command: {
        commandId: "cmd_late_foreign",
        id: "cmd_late_foreign",
        state: "accepted",
        revision: "1",
        dispatch: "native-acknowledged",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    });

  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  expect(send).toHaveBeenCalledTimes(1);
  expect(send.mock.calls[0]?.[4]).toBe("cmd_late_foreign");
  hubStore.logout();
});

it("wakes up at a crashed tab's lease expiry and then delivers the inflight row", async () => {
  vi.useFakeTimers();
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore);

  // Another tab POSTed and crashed while the row was in flight under a live
  // lease expiring in 2 s.
  localStorage.setItem(
    OUTBOX_LS_KEY,
    JSON.stringify([
      row({
        commandId: "cmd_crashed_owner",
        state: "inflight",
        lease: { owner: "owner_other_tab", until: Date.now() + 2_000 },
      }),
    ]),
  );
  const send = vi.spyOn(api, "instanceSend").mockRejectedValue(new Error("must not deliver yet"));

  const flush = hubStore as unknown as { flushAllOutbox: () => Promise<void> };
  await flush.flushAllOutbox();
  // Lease still live: the row is not deliverable and no POST happens.
  expect(send).not.toHaveBeenCalled();

  // The expiry wakeup fires with no UI event; let the deliverer run.
  send.mockResolvedValue({
    relatedCommandIds: [],
    command: {
      commandId: "cmd_crashed_owner",
      id: "cmd_crashed_owner",
      state: "accepted",
      revision: "1",
      dispatch: "native-acknowledged",
      resolution: "clear",
    } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
  });
  await vi.advanceTimersByTimeAsync(2_500);
  expect(send).toHaveBeenCalledTimes(1);
  expect(send.mock.calls[0]?.[4]).toBe("cmd_crashed_owner");
  vi.useRealTimers();
  hubStore.logout();
});

it("a row another tab retracts before the inflight claim commits is never POSTed", async () => {
  // c-reconnfu round 3 item 3: the durable claim (box.patch →
  // mergeUnlessDone) resolves null when another tab's retract deleted the
  // row in the claim's window. The old code only checked for a preserved
  // `done`; a null claim still fell through to instanceSend — a POST for a
  // command the user retracted.
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore);

  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([row({ commandId: "cmd_retracted_first" })]));
  const send = vi
    .spyOn(api, "instanceSend")
    .mockResolvedValue({
      relatedCommandIds: [],
      command: {
        commandId: "cmd_retracted_first",
        id: "cmd_retracted_first",
        state: "accepted",
        revision: "1",
        dispatch: "native-acknowledged",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    });

  // Model the cross-tab retract landing INSIDE the claim window: the
  // deliverer already read the row as deliverable; right when the inflight
  // lease claim's merge transaction runs, the durable row is gone, so the
  // REAL merge resolves null (missing row), exactly like the IDB backend.
  const box = (hubStore as unknown as { outbox: import("./outbox").Outbox }).outbox;
  const realPatch = box.patch.bind(box);
  vi.spyOn(box, "patch").mockImplementation(async (id, patch) => {
    if (patch.state === "inflight") await box.remove(id);
    return realPatch(id, patch);
  });

  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  await new Promise((r) => setTimeout(r, 10));
  expect(send).not.toHaveBeenCalled();
  hubStore.logout();
});

it("a retracted first row does not strand the next queued row of the instance (drain continues)", async () => {
  // c-reconnfu round 4 item 1: rows [A, B] are read deliverable inside one
  // lock turn; A is retracted inside its inflight claim (null claim → no POST).
  // The drain must distinguish a skipped/deleted row from a failed delivery and
  // continue to B in the SAME bounded turn — the old code returned null from
  // deliverOneRow, the flush broke with deliveredNew=false, and B sat pending
  // with no retry timer.
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore);

  const base = Date.now();
  localStorage.setItem(
    OUTBOX_LS_KEY,
    JSON.stringify([
      row({ commandId: "cmd_a_retracted", clientRequestId: "local_a", createdAt: base - 2_000 }),
      row({ commandId: "cmd_b_delivered", clientRequestId: "local_b", createdAt: base - 1_000 }),
    ]),
  );
  const send = vi
    .spyOn(api, "instanceSend")
    .mockResolvedValue({
      relatedCommandIds: [],
      command: {
        commandId: "cmd_b_delivered",
        id: "cmd_b_delivered",
        state: "accepted",
        revision: "1",
        dispatch: "native-acknowledged",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    });

  // Only A is retracted inside its claim window; B's claim must commit normally.
  const box = (hubStore as unknown as { outbox: import("./outbox").Outbox }).outbox;
  const realPatch = box.patch.bind(box);
  vi.spyOn(box, "patch").mockImplementation(async (id, patch) => {
    if (id === "cmd_a_retracted" && patch.state === "inflight") await box.remove(id);
    return realPatch(id, patch);
  });

  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  await vi.waitFor(() => expect(send).toHaveBeenCalledTimes(1));
  // B POSTed exactly once, A never did.
  expect(send.mock.calls[0]?.[4]).toBe("cmd_b_delivered");
  expect(send.mock.calls.some((c) => c[4] === "cmd_a_retracted")).toBe(false);
  hubStore.logout();
});

it("an aborted claim on the first queued row keeps FIFO: a later row is not POSTed first", async () => {
  // c-reconnfu gate 6 item 4b: rows [A, B] are read deliverable in one lock
  // turn. A's durable inflight claim TRANSACTION ABORTS (storage error) — A is
  // still first and still deliverable, unlike a retracted (null) row. The
  // drain must STOP the turn rather than POST B ahead of A; after storage
  // recovers the next flush POSTs A first.
  const { api, hubStore } = await fresh();
  await bootLive(api, hubStore);

  const base = Date.now();
  localStorage.setItem(
    OUTBOX_LS_KEY,
    JSON.stringify([
      row({ commandId: "cmd_a_abort", clientRequestId: "local_a", createdAt: base - 2_000 }),
      row({ commandId: "cmd_b_wait", clientRequestId: "local_b", createdAt: base - 1_000 }),
    ]),
  );
  const send = vi
    .spyOn(api, "instanceSend")
    .mockResolvedValue({
      relatedCommandIds: [],
      command: {
        commandId: "cmd_ok",
        id: "cmd_ok",
        state: "accepted",
        revision: "1",
        dispatch: "native-acknowledged",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    });

  // A's claim aborts; B is never reached for a claim in this turn.
  const box = (hubStore as unknown as { outbox: import("./outbox").Outbox }).outbox;
  const realPatch = box.patch.bind(box);
  const patchSpy = vi.spyOn(box, "patch");
  patchSpy.mockImplementation(async (id, patch) => {
    if (id === "cmd_a_abort" && patch.state === "inflight") {
      throw new Error("IndexedDB transaction aborted");
    }
    return realPatch(id, patch);
  });

  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  // No POST at all this turn — B must not jump ahead of the still-pending A.
  expect(send).not.toHaveBeenCalled();

  // Storage recovers: the next flush claims A normally and POSTs it FIRST.
  patchSpy.mockImplementation(realPatch);
  send.mockImplementation((async (_iid, _prompt, _att, _mode, commandId) => ({
    relatedCommandIds: [],
    command: {
      commandId,
      id: commandId,
      state: "accepted",
      revision: "1",
      dispatch: "native-acknowledged",
      resolution: "clear",
    } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
  })) as Api["instanceSend"]);
  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  await vi.waitFor(() => expect(send).toHaveBeenCalled());
  expect(send.mock.calls[0]?.[4]).toBe("cmd_a_abort");
  hubStore.logout();
});
it("GATE23 item 4 (rebuilds gate-21 item-1a): two real stores — B blocks on S holding IY; A's parked flush loses R's claim while covered and its recovery is consumed suppressed; after B releases and T is really retracted, the scoped epilogue re-arms R and A's real timer delivers it", async () => {
  // Rebuilt per gate-23 RULES: the gate-22 version REPLACED flushAllOutbox on
  // A and choreographed flush #1/#2 by ERROR STACK ("fireLeaseWake") and call
  // ordinals, and gated storage.all by stack string. Here flushAllOutbox,
  // onClaimWake and every delivery callback are the REAL methods; the harness
  // controls only (a) B's API-boundary POST via a promise gate and (b) A's
  // real claim transaction by command id and DURABLE STATE. Every durable row
  // comes from a real enqueue/claim flow.
  //
  // B sends S (IY); its real POST blocks holding the IY Web Lock. A really
  // enqueues R (IX) and T (IY) while B holds that lock; the claim-fault is
  // armed on A's real storage BEFORE the enqueues. The rows' registered
  // deliver wakes fire: R's wake (its IX lock is free) starts the real guarded
  // flush which covers IX+IY in one pass; the IY turn queues behind B while
  // R's claim genuinely aborts (injected at A's real merge, while S is durably
  // not sent), and the bounded 250 ms recovery is consumed under the live pass
  // suppression. T's own fire queues on the held IY lock. The user REALLY
  // retracts T (durable delete). When B releases, S settles and B closes
  // before its pass 1 (its queued turns read empty behind A's); A's queued IY
  // turn finds T gone and posts nothing; the pass posts nothing at all.
  // Pre-gate-21 its finally only CLEARED suppression and R stranded; the
  // scoped epilogue now releases and re-reads, arming R's one wake — R is
  // delivered by A's real timer. Final calls exactly B:[S] A:[R].
  vi.useFakeTimers();
  vi.setSystemTime(900_000);
  const ownerA = "owner_g23_i4_a";
  const ownerB = "owner_g23_i4_b";
  const IX = "ins_g23_i4_x";
  const IY = "ins_g23_i4_y";
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: shim.locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  const accepted = (commandId: string) => ({
    relatedCommandIds: [],
    command: {
      commandId,
      id: commandId,
      state: "accepted",
      revision: "1",
      dispatch: "native-acknowledged",
      resolution: "clear",
    },
  });
  try {
    // B boots first and sends S: real claim, POST pending inside IY.
    sessionStorage.setItem("remuda-outbox-owner", ownerB);
    const { api: apiB, hubStore: b } = await fresh();
    vi.spyOn(apiB, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiB, "screenRead").mockResolvedValue({ lines: [] });
    const sendB = vi.spyOn(apiB, "instanceSend");
    await bootLive(apiB, b);
    let releaseB: (() => void) | null = null;
    const bGate = new Promise<void>((resolve) => {
      releaseB = resolve;
    });
    sendB.mockImplementation(
      (async (iid: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => {
        if (iid === IY) {
          await bGate; // S blocked holding IY
          return accepted(commandId ?? "cmd_x");
        }
        throw new Error("g23-i4 B never POSTs another instance");
      }) as unknown as Api["instanceSend"],
    );
    void b.send(IY, "g23 4 S");
    for (let i = 0; i < 2_000 && sendB.mock.calls.length === 0; i += 1) await Promise.resolve();
    const sId = sendB.mock.calls[0]?.[4] as string;
    expect(sId).toBeTruthy();

    // A boots (its empty boot flush posts nothing) and goes OFFLINE.
    sessionStorage.setItem("remuda-outbox-owner", ownerA);
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    const sendA = vi.spyOn(apiA, "instanceSend").mockImplementation(
      (async (_iid: string, _p: string, _x?: unknown, _m?: string, commandId?: string) =>
        accepted(commandId ?? "cmd_x")) as unknown as Api["instanceSend"],
    );
    await bootLive(apiA, a);
    await pump(500);

    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    const rowOf = (prompt: string) => storedRows().find((r) => r.prompt === prompt);

    // Fault at A's REAL claim transaction, armed BEFORE any enqueue: every R
    // claim aborts BEFORE commit while S is durably not sent. Gated solely on
    // the real durable row (prompt lookup) and durable state — no stack
    // inspection, no ordinals, no storage writes.
    const aStorage = (
      a as unknown as {
        outbox: {
          storage: {
            mergeUnlessDone: (
              id: string,
              patch: Partial<OutboxRecord>,
            ) => Promise<OutboxRecord | null>;
          };
        };
      }
    ).outbox.storage;
    const realMergeA = aStorage.mergeUnlessDone.bind(aStorage);
    let claimFaults = 0;
    aStorage.mergeUnlessDone = vi.fn(async (id, patch) => {
      const existing = storedRows().find((r) => r.commandId === id);
      if (
        id.startsWith("cmd_") &&
        (patch as { state?: string }).state === "inflight" &&
        existing?.prompt === "g23 4 R" &&
        rowOf("g23 4 S")?.state !== "sent"
      ) {
        claimFaults += 1;
        throw new Error("g23-i4 R claim abort while S is unsent");
      }
      return await realMergeA(id, patch);
    });

    // R and T are really enqueued (awaited); their registered deliver wakes
    // are the only triggers, and R's real claims hit the fault.
    expect(await a.send(IX, "g23 4 R")).toBe(true);
    expect(await a.send(IY, "g23 4 T")).toBe(true);
    await pump(500);
    const rId = rowOf("g23 4 R")!.commandId;
    const tBubble = a.getSnapshot().bubbles.find((x) => x.text === "g23 4 T")!;
    expect(rowOf("g23 4 T")?.state).toBe("pending");
    expect(sendA).not.toHaveBeenCalled();

    // R's real wake fires: the real flush covers IX+IY; R's claim aborts and
    // the IY turn queues behind blocked B. T's real fire queues on the lock.
    await vi.advanceTimersByTimeAsync(1);
    await pump(2_000);
    expect(claimFaults).toBeGreaterThanOrEqual(1);

    // Advance past the bounded recovery while B stays blocked: the 250 ms read
    // is consumed under the live pass suppression — it arms nothing, and no
    // POST runs. R never committed a claim (still lease-less, one attempt).
    await vi.advanceTimersByTimeAsync(2_000);
    await pump(2_000);
    expect(sendA).not.toHaveBeenCalled();
    expect(sendB).toHaveBeenCalledTimes(1);
    expect(rowOf("g23 4 R")?.state).toBe("pending");
    expect(rowOf("g23 4 R")?.lease).toBeUndefined();
    const aBox = (a as unknown as { outbox: { pendingLeaseWakeups: () => string[] } }).outbox;
    expect(aBox.pendingLeaseWakeups()).not.toContain(rId);

    // The user REALLY retracts queued T while the IY turn is queued.
    await a.retract(tBubble.clientRequestId);
    await pump(1_000);
    expect(rowOf("g23 4 T")).toBeUndefined();
    expect(sendA).not.toHaveBeenCalled();

    // Release B: S settles; A's queued IY turns read T gone and post nothing;
    // the pass posted nothing at all. Its scoped epilogue re-arms R's one
    // wake (the claim fault clears once S is sent), and A's REAL timer
    // delivers R under the same command id.
    releaseB!();
    // Let B's REAL flow settle S durably; B's pass-1 turns are queued on the
    // FIFO Web Lock BEHIND A's turn that parked during the block. Close B the
    // instant S commits: its queued turns then read an empty selection (and
    // the flush loop bails on the replaced outbox), so B never drains R.
    for (let i = 0; i < 2_000 && rowOf("g23 4 S")?.state !== "sent"; i += 1) {
      await Promise.resolve();
    }
    b.logout();
    // A's parked turn completes, the scoped epilogue re-arms R (claim fault
    // cleared once S is sent), and A's REAL timer delivers R.
    for (let i = 0; i < 400 && sendA.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(25);
      await Promise.resolve();
    }
    expect(sendB.mock.calls.map((c) => c[4])).toEqual([sId]);
    expect(sendA.mock.calls.map((c) => c[4])).toEqual([rId]);
    expect(rowOf("g23 4 S")?.state).toBe("sent");
    expect(rowOf("g23 4 R")?.state).toBe("sent");
    expect(rowOf("g23 4 T")).toBeUndefined();

    // Far past every deadline: exactly the combined calls, no duplicates.
    await vi.advanceTimersByTimeAsync(120_000);
    await pump(2_000);
    expect(sendB.mock.calls.map((c) => c[4])).toEqual([sId]);
    expect(sendA.mock.calls.map((c) => c[4])).toEqual([rId]);
    a.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE23 item 5 (rebuilds gate-21 item-2): B's REAL blocked claims leave X with a 10 s lease and Y with a 30 s lease; B finishes X and restores Y pending before A boots; advancing only to X's deadline, A delivers Y 20 s before Y's own deadline", async () => {
  // Rebuilt per gate-23 RULES: the gate-22 version SEEDED two handcrafted
  // inflight rows/leases in localStorage. Here EVERY row and lease comes from
  // B's REAL enqueue/claim flow. B runs without Web Locks (durable row lease
  // is the single-deliverer boundary): its real X claim at t0 blocks in the
  // POST with a 30 s lease; fake time advances 20 s; B's real Y claim blocks
  // with a fresh 30 s lease (deadline t+50 s). A then boots WITH Web Locks —
  // B never held any — so its real boot flush reads immediately: both rows
  // carry B's LIVE row leases (X deadline t+30 s, Y deadline t+50 s), nothing
  // is deliverable, and the central reconciliation arms both deadline wakes.
  // B's blocked POSTs then settle for real: X accepted (sent); Y fails at the
  // API boundary (POST rejects AND status GET rejects) and is restored
  // lease-less pending, all near t+20 s; B closes with its pinned jittered
  // retry cancelled. Advancing fake time ONLY to X's deadline: A's real X fire
  // is the trigger — its authoritative read sees X sent and Y PENDING and
  // arms Y's one 0 ms wake, whose real fire hands A's guarded flush Y, ~20 s
  // before Y's own lease deadline. Combined API calls exactly B:[X, Y] A:[Y].
  vi.useFakeTimers();
  vi.setSystemTime(1_000_000);
  const ownerA = "owner_g23_i5_a";
  const ownerB = "owner_g23_i5_b";
  const IX = "ins_g23_i5_x";
  const IY = "ins_g23_i5_y";
  const originalNavigator = globalThis.navigator;
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  // No Web Locks for B: the durable lease path. Switched to the FIFO shim
  // before A boots.
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: undefined },
  });
  try {
    sessionStorage.setItem("remuda-outbox-owner", ownerB);
    const { api: apiB, hubStore: b } = await fresh();
    vi.spyOn(apiB, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiB, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(apiB, "instanceCommandStatus").mockRejectedValue(new Error("g23-i5 status network down"));
    let releaseX: (() => void) | null = null;
    const xGate = new Promise<void>((resolve) => {
      releaseX = resolve;
    });
    let releaseY: (() => void) | null = null;
    const yGate = new Promise<void>((resolve) => {
      releaseY = resolve;
    });
    const sendB = vi.spyOn(apiB, "instanceSend").mockImplementation(
      (async (_iid: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) => {
        if (prompt === "g23 5 x") {
          await xGate;
          return {
            relatedCommandIds: [],
            command: {
              commandId,
              id: commandId,
              state: "accepted",
              revision: "1",
              dispatch: "native-acknowledged",
              resolution: "clear",
            },
          };
        }
        await yGate;
        throw new Error("g23 5 boundary POST failure");
      }) as unknown as Api["instanceSend"],
    );
    // Pin B's store-retry jitter high; B closes before the retry can fire.
    vi.spyOn(Math, "random").mockReturnValue(0.999);
    await bootLive(apiB, b);
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    const row = (id: string) => storedRows().find((r) => r.commandId === id);

    // X's real claim at t0: POST blocked, live 30 s lease.
    void b.send(IX, "g23 5 x");
    for (let i = 0; i < 2_000 && sendB.mock.calls.length === 0; i += 1) await Promise.resolve();
    const xId = sendB.mock.calls[0]?.[4] as string;
    await pump(500);
    expect(row(xId)?.state).toBe("inflight");
    expect(row(xId)?.lease?.owner).toBe(ownerB);
    expect(row(xId)!.lease!.until - Date.now()).toBeGreaterThan(25_000);

    // 20 s pass while X is still blocked (gates do not move with fake time).
    await vi.advanceTimersByTimeAsync(20_000);

    // Y's real claim at t+20 s: fresh live 30 s lease, deadline t+50 s.
    void b.send(IY, "g23 5 y");
    for (let i = 0; i < 2_000 && sendB.mock.calls.length < 2; i += 1) await Promise.resolve();
    const yId = sendB.mock.calls[1]?.[4] as string;
    await pump(500);
    expect(row(yId)?.state).toBe("inflight");
    expect(row(yId)!.lease!.until - Date.now()).toBeGreaterThan(25_000);
    expect(sendB.mock.calls.map((c) => c[4])).toEqual([xId, yId]);

    // A boots WITH Web Locks (B never held any): its real flush reads both
    // live B leases, posts nothing and tracks both lease deadlines.
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      writable: true,
      value: { locks: fifoWebLocks().locks },
    });
    sessionStorage.setItem("remuda-outbox-owner", ownerA);
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    const sendA = vi.spyOn(apiA, "instanceSend").mockImplementation(
      (async (_iid: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(apiA, a);
    const aOutbox = (
      a as unknown as { outbox: { pendingLeaseWakeups: () => string[] } }
    ).outbox;
    await pump(1_000);
    expect(sendA).not.toHaveBeenCalled();
    expect(aOutbox.pendingLeaseWakeups().sort()).toEqual([xId, yId].sort());

    // B's real flows settle: X accepted; Y boundary-fails (POST + status GET)
    // and returns lease-less pending — all near t+20 s — then B closes.
    releaseX!();
    for (let i = 0; i < 2_000 && row(xId)?.state !== "sent"; i += 1) await Promise.resolve();
    releaseY!();
    for (let i = 0; i < 4_000; i += 1) {
      if (sendB.mock.calls.length >= 2 && row(yId)?.state === "pending") break;
      await Promise.resolve();
    }
    b.logout();
    expect(sendB.mock.calls.map((c) => c[4])).toEqual([xId, yId]);
    expect(row(xId)?.state).toBe("sent");
    expect(row(yId)?.state).toBe("pending");
    expect(row(yId)?.lease).toBeUndefined();
    expect(sendA).not.toHaveBeenCalled();

    // Advance ONLY to X's tracked deadline (t+30 s): A's real X fire reads Y
    // pending and its reconciliation arms Y's one immediate wake; A delivers
    // Y ~20 s before Y's own lease deadline, never POSTing X.
    await vi.advanceTimersByTimeAsync(10_000);
    for (let i = 0; i < 200 && sendA.mock.calls.length === 0; i += 1) {
      await Promise.resolve();
      await vi.advanceTimersByTimeAsync(1);
    }
    expect(sendA).toHaveBeenCalledTimes(1);
    expect(sendA.mock.calls[0]?.[4]).toBe(yId);
    expect(row(yId)?.state).toBe("sent");
    expect(Date.now()).toBeLessThan(1_000_000 + 40_000);
    expect(sendA.mock.calls.some((c) => c[4] === xId)).toBe(false);

    // Far past every deadline: exactly A's one POST and B's two calls.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(sendA).toHaveBeenCalledTimes(1);
    expect(sendB).toHaveBeenCalledTimes(2);
    a.logout();
  } finally {
    // Independent cleanup (grok gate 25 item 2): restore the leaked globals
    // FIRST, each in its own guarded block, so one teardown call throwing can
    // never prevent the navigator override or the Math.random jitter pin from
    // being restored for later tests in this process.
    const restore = (fn: () => void) => {
      try {
        fn();
      } catch {
        /* one cleanup failing must not skip the rest */
      }
    };
    restore(() =>
      Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator }),
    );
    restore(() => vi.mocked(Math.random).mockRestore());
    restore(() => vi.useRealTimers());
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});


it("GATE22 item 6: a flush queued across logout/reinit releases its tokens and epilogue on the ISSUING box — the new epoch's outbox gets no stale tokens or reconcile and delivers the row itself", async () => {
  // flushAllOutbox captures the current outbox in a local `box`. Before gate
  // 22 item 6 a flush queued on another instance's Web Lock across a
  // logout/reinit ran its finally against the CURRENT this.outbox — the NEW
  // box — applying the disposed epoch's scoped suppression releases and a
  // fresh-read reconciliation to the new auth epoch. The continuation also
  // used to keep looping after the box was replaced. The fix bails every
  // continuation once this.outbox !== box and releases tokens/epilogue on
  // the issuing (disposed) box only; the new epoch independently boots and
  // delivers the durable row once.
  vi.useFakeTimers();
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: shim.locks },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  const I = "ins_g22_i6_reinit";
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g22_i6_reinit");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();

    // Hold the instance lock ahead of the store's flush with a real queued turn.
    let releaseBlocker: (() => void) | null = null;
    const blockerGate = new Promise<void>((resolve) => {
      releaseBlocker = resolve;
    });
    const boxOf = () =>
      (
        hubStore as unknown as {
          outbox: { withInstanceLock: (i: string, f: () => Promise<number>) => Promise<number> } | null;
        }
      ).outbox;
    void boxOf()!.withInstanceLock(I, async () => {
      await blockerGate;
      return 0;
    });
    await pump(200);

    // Enqueue a row and start the store's real flush — it queues on the lock.
    await hubStore.send(I, "gate22 6 reinit row");
    const staleFlush = (
      hubStore as unknown as { flushAllOutbox: () => Promise<void> }
    ).flushAllOutbox();
    await pump(200);
    expect(send).not.toHaveBeenCalled();

    // Logout (disposes the issuing box) and REINIT while the blocker still
    // holds the lock: the stale flush is queued; the new epoch's boot flush
    // queues behind it on the same Web Lock.
    hubStore.logout();
    expect(boxOf()).toBeNull();
    const reopened = await (
      hubStore as unknown as { ensureOutbox: () => Promise<boolean> }
    ).ensureOutbox();
    expect(reopened).toBe("ok");
    const newBox = boxOf()!;
    const newEnd = vi.spyOn(
      newBox as unknown as { endDeliveryPasses: (t: Iterable<number>) => Promise<void> },
      "endDeliveryPasses",
    );

    // Release: the stale (old-box) turn runs first and must finish entirely on
    // its ISSUING box — its disposed snapshot read posts nothing and its
    // finally never calls the new box's epilogue. The new boot flush then
    // delivers the row and runs exactly ONE epilogue (its own token).
    releaseBlocker!();
    await staleFlush.catch(() => undefined);
    for (let i = 0; i < 300 && send.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(10);
    }
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[1]).toBe("gate22 6 reinit row");
    // Exactly the new epoch's boot-flush epilogue — a stale crossing would add
    // a second endDeliveryPasses call here (pre-fix: 2).
    expect(newEnd).toHaveBeenCalledTimes(1);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE23 item 3: a successful DIRECT STEER records progress at the shared POST boundary, clears the instance's exhaustion and lets the real timer deliver the earlier pending row", async () => {
  // Exhaustion is per instance and total, and ANY successful delivery for the
  // instance — flush, timer OR direct steer — records progress at the shared
  // POST boundary and clears it. P is enqueued while offline and the Web Lock
  // is down: the real timer wakes spend the bounded budget (persistent lock
  // rejections, reads succeed) and the instance surfaces exhausted with no
  // armed wake. The locks then recover and a live steer S is POSTed through
  // the DIRECT route (no flush pass): pre-fix only the guarded flush recorded
  // progress, so the latch stayed, the steer epilogue armed nothing for P and
  // it stranded; post-fix the shared boundary clears the latch, the steer
  // epilogue re-arms P's one deliver wake and A's REAL timer delivers P — no
  // manual flush anywhere.
  vi.useFakeTimers();
  vi.setSystemTime(950_000);
  const I = "ins_g23_i3";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    request: () => Promise.reject(new Error("g23 i3 locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = bad;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: {
      get locks() {
        return locks;
      },
    },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g23_i3");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, _p: string, _x?: unknown, _m?: unknown, commandId?: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    const boxOf = () =>
      (
        hubStore as unknown as {
          outbox: {
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];

    // 1) Offline + locks down: enqueue P through the real send. No flush runs
    //    while offline; the enqueue epilogue arms the one deliver wake.
    hubStore.setConnectionStateForTest("offline");
    locks = bad;
    expect(await hubStore.send(I, "gate23 3 P")).toBe(true);
    await pump(200);
    const pId = storedRows().find((r) => r.prompt === "gate23 3 P")!.commandId;
    expect(send).not.toHaveBeenCalled();

    // 2) The real timer wakes spend the bounded budget: lock requests reject
    //    while reads succeed — exhaustion surfaced, every wake cancelled.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(boxOf()!.pendingLeaseWakeups()).toEqual([]);
    expect(send).not.toHaveBeenCalled();
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("pending");

    // 3) Locks recover and the link is live: POST the steer S through the
    //    DIRECT route (no guarded flush pass).
    locks = good;
    hubStore.setConnectionStateForTest("live");
    const accepted = await hubStore.send(I, "gate23 3 S", [], [], "steer");
    expect(accepted).toBe(true);
    await pump(500);
    expect(send).toHaveBeenCalledTimes(1);
    const sId = send.mock.calls[0]?.[4] as string;
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("sent");

    // 4) The successful steer cleared the instance's exhaustion at the shared
    //    POST boundary; the steer epilogue re-armed P and the REAL timer now
    //    delivers it automatically — no manual flush.
    for (let i = 0; i < 400 && send.mock.calls.length < 2; i += 1) {
      await vi.advanceTimersByTimeAsync(10);
      await Promise.resolve();
    }
    expect(send).toHaveBeenCalledTimes(2);
    expect(send.mock.calls.map((c) => c[4])).toEqual([sId, pId]);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("sent");
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(false);
    expect(boxOf()!.pendingLeaseWakeups()).toEqual([]);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE24 item 1: a REJECTED direct steer leaves exhaustion latched and arms no wakes; a later ACCEPTED steer clears it and the older row is delivered by the real timer", async () => {
  // Gate 24 narrowed the shared POST boundary: only a Hub-ACCEPTED POST
  // records progress. P spends the bounded budget against persistently
  // rejecting Web Locks (reads succeed). After recovery a direct steer S1 is
  // REJECTED by the Hub: pre-fix the steer's finally cleared exhaustion
  // anyway, its epilogue re-armed P and the rejected outcome masqueraded as
  // delivery health. Post-fix exhaustion stays latched with no wakes; a later
  // ACCEPTED steer S2 clears it at the boundary and A's REAL armed timer
  // delivers P. The rejected S1 is POSTed exactly once (terminal, no retry).
  vi.useFakeTimers();
  vi.setSystemTime(960_000);
  const I = "ins_g24_i1";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: () => Promise.reject(new Error("g24 i1 locks down")) as Promise<any>,
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = bad;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: {
      get locks() {
        return locks;
      },
    },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g24_i1");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const acceptedResult = (commandId: string) => ({
      relatedCommandIds: [],
      command: {
        commandId,
        id: commandId,
        state: "accepted",
        revision: "1",
        dispatch: "native-acknowledged",
        resolution: "clear",
      },
    });
    const rejectedResult = (commandId: string) => ({
      relatedCommandIds: [],
      command: {
        commandId,
        id: commandId,
        state: "settled",
        revision: "1",
        dispatch: "native-acknowledged",
        resolution: "clear",
        settlement: { outcome: "rejected", reason: "g24 i1 steer rejected" },
      },
    });
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) =>
        prompt === "g24 1 reject"
          ? rejectedResult(commandId ?? "cmd_x")
          : acceptedResult(commandId ?? "cmd_x")) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    const boxOf = () =>
      (
        hubStore as unknown as {
          outbox: {
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];

    // 1) Locks down + offline: real enqueue P; real timer wakes spend the
    //    bounded budget — exhausted, no wakes, P untouched.
    hubStore.setConnectionStateForTest("offline");
    locks = bad;
    expect(await hubStore.send(I, "g24 1 P")).toBe(true);
    await pump(200);
    const pId = storedRows().find((r) => r.prompt === "g24 1 P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(boxOf()!.pendingLeaseWakeups()).toEqual([]);
    expect(send).not.toHaveBeenCalled();

    // 2) Locks recover, link live: a direct steer the Hub REJECTS.
    locks = good;
    hubStore.setConnectionStateForTest("live");
    const rejectedSteer = await hubStore.send(I, "g24 1 reject", [], [], "steer");
    expect(rejectedSteer).toBe(false);
    await pump(500);
    const rjId = send.mock.calls[0]?.[4] as string;
    expect(send).toHaveBeenCalledTimes(1);
    expect(storedRows().find((r) => r.commandId === rjId)?.state).toBe("rejected");

    // 3) Ruling: rejection records NO progress — exhaustion stays latched and
    //    the steer epilogue arms nothing for P. Time passes: no retry POST for
    //    the rejected steer and P is still pending with no wake.
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(boxOf()!.pendingLeaseWakeups()).toEqual([]);
    await vi.advanceTimersByTimeAsync(30_000);
    await pump(1_000);
    expect(send).toHaveBeenCalledTimes(1);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("pending");

    // 4) A direct steer the Hub ACCEPTS clears the exhaustion at the shared
    //    boundary; its epilogue re-arms P and A's REAL timer delivers P.
    const acceptedSteer = await hubStore.send(I, "g24 1 accept", [], [], "steer");
    expect(acceptedSteer).toBe(true);
    for (let i = 0; i < 400 && send.mock.calls.length < 3; i += 1) {
      await vi.advanceTimersByTimeAsync(10);
      await Promise.resolve();
    }
    const okId = send.mock.calls[1]?.[4] as string;
    expect(send.mock.calls.map((c) => c[4])).toEqual([rjId, okId, pId]);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("sent");
    expect(storedRows().find((r) => r.commandId === rjId)?.state).toBe("rejected");
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(false);

    // Far past every deadline: the rejected steer never re-POSTs.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(send.mock.calls.map((c) => c[4])).toEqual([rjId, okId, pId]);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE24 item 2 (rebuilds gate-23 item-2): a real HubStore with two real enqueues and the REAL onClaimWake — persistent timer-path lock rejections while reads succeed stop at the cap with no wakes and no further requests", async () => {
  // Rebuilt per gate-24 RULES: the gate-23 version hand-seeded both records
  // with store.put(rec(...)) and replaced onClaimWake with a vi.fn. Here
  // everything runs through the real HubStore: both rows are created by real
  // offline send() enqueues, the store's real registered onClaimWake drives
  // its real guarded flush, and the ONLY fault is injected at
  // navigator.locks.request (persistent rejection) — durable reads keep
  // succeeding. The store boots and settles its empty boot flush with locks
  // UP; the failing lock backend is installed only afterwards, so every
  // counted request is a real timer-path delivery attempt. At the shared
  // per-instance cap the central reconciliation cancels every wake, the
  // instance is exhausted, no guarded flush ever POSTs and no further lock
  // request is ever issued.
  vi.useFakeTimers();
  vi.setSystemTime(970_000);
  const I = "ins_g24_i2";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  let requestCount = 0;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: vi.fn((): Promise<any> => {
      requestCount += 1;
      return Promise.reject(new Error("g24 i2 locks down"));
    }),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = good;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: {
      get locks() {
        return locks;
      },
    },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g24_i2");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "accepted",
          revision: "1",
          dispatch: "native-acknowledged",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    // Settle the empty boot flush while locks are UP — it must not count.
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    const boxOf = () =>
      (
        hubStore as unknown as {
          outbox: {
            onClaimWake: (() => void) | null;
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;
    // The store's REAL wake handoff is registered (never replaced).
    expect(typeof boxOf()!.onClaimWake).toBe("function");
    expect(requestCount).toBe(0);

    // Locks go DOWN; two rows really enqueued while offline — no flush, only
    // the registered deliver wakes.
    hubStore.setConnectionStateForTest("offline");
    locks = bad;
    expect(await hubStore.send(I, "g24 2 P1")).toBe(true);
    expect(await hubStore.send(I, "g24 2 P2")).toBe(true);
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    await pump(300);
    const id1 = storedRows().find((r) => r.prompt === "g24 2 P1")!.commandId;
    const id2 = storedRows().find((r) => r.prompt === "g24 2 P2")!.commandId;
    expect(boxOf()!.pendingLeaseWakeups().sort()).toEqual([id1, id2].sort());
    expect(send).not.toHaveBeenCalled();

    // The real timer wakes spend the SHARED per-instance budget: lock
    // requests reject while every read succeeds.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(requestCount).toBe(CLAIM_WAKE_MAX_FAILURES);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(boxOf()!.pendingLeaseWakeups()).toEqual([]);
    expect(send).not.toHaveBeenCalled();
    expect(storedRows().find((r) => r.commandId === id1)?.state).toBe("pending");
    expect(storedRows().find((r) => r.commandId === id2)?.state).toBe("pending");

    // Long after every bounded delay: quiescence — no further requests, no
    // re-armed wake, no POST.
    const requestsAtQuiesce = requestCount;
    await vi.advanceTimersByTimeAsync(300_000);
    expect(requestCount).toBe(requestsAtQuiesce);
    expect(boxOf()!.pendingLeaseWakeups()).toEqual([]);
    expect(send).not.toHaveBeenCalled();

    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

/** GATE25 item 1 harness bits: the queued+forwarded+reconciling Hub answer. */
function g25ReconcilingAnswer(commandId: string) {
  return {
    relatedCommandIds: [],
    command: {
      commandId,
      id: commandId,
      revision: "1",
      state: "queued",
      dispatch: "transport-written",
      resolution: "reconciling",
    },
  };
}
type G25Status = Awaited<ReturnType<Api["instanceCommandStatus"]>>;
const g25Status = (commandId: string, shape: "reconciling" | "accepted" | "rejected"): G25Status => {
  const base = {
    commandId,
    instanceId: "ins_g25_i1",
    state: shape === "accepted" ? "accepted" : shape === "rejected" ? "settled" : "queued",
    resolution: shape === "reconciling" ? "reconciling" : "clear",
    forwarded: true,
  };
  return (shape === "rejected"
    ? { ...base, settlement: { outcome: "rejected" as const, reason: "g25 rejected" } }
    : base) as unknown as G25Status;
};

it("GATE25 item 1 (positive): an exhausted instance's reconciling steer is accepted by the delayed GET — progress on the originating outbox + fresh epilogue deliver the older row by its real timer", async () => {
  // The POST answers queued+forwarded+RECONCILING (Node ack timeout): no
  // acceptance yet, so exhaustion must stay latched. The async command-status
  // GET first still reports reconciling, then ACCEPTED. Pre-fix the delayed
  // acceptance only patched the steer row to sent — it never recorded progress
  // nor re-ran the delivery epilogue, so the older exhausted row stayed
  // stranded. Post-fix the originating, still-current outbox records progress
  // and its fresh epilogue re-arms P; the real timer delivers P.
  vi.useFakeTimers();
  vi.setSystemTime(980_000);
  const I = "ins_g25_i1";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g25 locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = bad;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g25_i1");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) =>
        prompt === "g25 1 S"
          ? g25ReconcilingAnswer(commandId ?? "cmd_x")
          : {
              relatedCommandIds: [],
              command: {
                commandId, id: commandId, state: "accepted", revision: "1",
                dispatch: "native-acknowledged", resolution: "clear",
              },
            }) as unknown as Api["instanceSend"],
    );
    let gets = 0;
    vi.spyOn(api, "instanceCommandStatus").mockImplementation(
      (async (_iid: string, commandId: string) => {
        gets += 1;
        // GET 1 still reconciling; GET 2 authoritative acceptance.
        return g25Status(commandId, gets === 1 ? "reconciling" : "accepted");
      }) as Api["instanceCommandStatus"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    const boxOf = () =>
      (
        hubStore as unknown as {
          outbox: {
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];

    // P spends the bounded budget with locks down (reads succeed).
    hubStore.setConnectionStateForTest("offline");
    locks = bad;
    expect(await hubStore.send(I, "g25 1 P")).toBe(true);
    await pump(200);
    const pId = storedRows().find((r) => r.prompt === "g25 1 P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(send).not.toHaveBeenCalled();

    // Locks recover: the steer POSTs and answers reconciling — NOT accepted.
    locks = good;
    hubStore.setConnectionStateForTest("live");
    const steerAccepted = await hubStore.send(I, "g25 1 S", [], [], "steer");
    expect(steerAccepted).toBe(false); // reconciling is not an interrupt receipt
    await pump(500);
    const sId = send.mock.calls[0]?.[4] as string;
    expect(send).toHaveBeenCalledTimes(1);
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("reconciling");

    // The async GET loop starts with the POST; its first poll (still
    // reconciling) completes on the microtask queue without any timer moving.
    // Uncertain → exhaustion latched, P uncovered, no second POST.
    for (let i = 0; i < 1_000 && gets < 1; i += 1) await Promise.resolve();
    expect(gets).toBe(1);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(send).toHaveBeenCalledTimes(1);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("pending");

    // The 1 s poll wait elapses: GET 2 is accepted. Progress clears the latch
    // on the originating outbox and the fresh epilogue re-arms P — A's REAL
    // timer delivers it.
    await vi.advanceTimersByTimeAsync(1_000);
    for (let i = 0; i < 400 && send.mock.calls.length < 2; i += 1) {
      await vi.advanceTimersByTimeAsync(10);
      await Promise.resolve();
    }
    expect(send.mock.calls.map((c) => c[4])).toEqual([sId, pId]);
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("sent");
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("sent");
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(false);

    // Well after every deadline: the steer never re-POSTs.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(send.mock.calls.map((c) => c[4])).toEqual([sId, pId]);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE25 item 1 (negative: delayed GET rejects): a reconciling steer later rejected by the GET keeps exhaustion latched and never delivers the older row", async () => {
  vi.useFakeTimers();
  vi.setSystemTime(981_000);
  const I = "ins_g25_i1n1";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g25 n1 locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = bad;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g25_n1");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) =>
        prompt === "g25 n1 S"
          ? g25ReconcilingAnswer(commandId ?? "cmd_x")
          : {
              relatedCommandIds: [],
              command: {
                commandId, id: commandId, state: "accepted", revision: "1",
                dispatch: "native-acknowledged", resolution: "clear",
              },
            }) as unknown as Api["instanceSend"],
    );
    let gets = 0;
    vi.spyOn(api, "instanceCommandStatus").mockImplementation(
      (async (_iid: string, commandId: string) => {
        gets += 1;
        return g25Status(commandId, gets === 1 ? "reconciling" : "rejected");
      }) as Api["instanceCommandStatus"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    const boxOf = () =>
      (
        hubStore as unknown as {
          outbox: {
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];

    hubStore.setConnectionStateForTest("offline");
    locks = bad;
    expect(await hubStore.send(I, "g25 n1 P")).toBe(true);
    await pump(200);
    const pId = storedRows().find((r) => r.prompt === "g25 n1 P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);

    locks = good;
    hubStore.setConnectionStateForTest("live");
    expect(await hubStore.send(I, "g25 n1 S", [], [], "steer")).toBe(false);
    await pump(500);
    const sId = send.mock.calls[0]?.[4] as string;

    // GET 1 uncertain, GET 2 rejected: no progress ever.
    await vi.advanceTimersByTimeAsync(3_000);
    await pump(1_000);
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("rejected");
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(boxOf()!.pendingLeaseWakeups()).toEqual([]);
    expect(send).toHaveBeenCalledTimes(1);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("pending");

    // Far past every deadline: P stays stranded, rejected steer not re-POSTed.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(send).toHaveBeenCalledTimes(1);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("pending");
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE25 item 1 (negative: GET never resolves): a reconciling steer whose GET stays uncertain to the deadline ends unknown and leaves exhaustion latched", async () => {
  vi.useFakeTimers();
  vi.setSystemTime(982_000);
  const I = "ins_g25_i1n2";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g25 n2 locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = bad;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g25_n2");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) =>
        prompt === "g25 n2 S"
          ? g25ReconcilingAnswer(commandId ?? "cmd_x")
          : {
              relatedCommandIds: [],
              command: {
                commandId, id: commandId, state: "accepted", revision: "1",
                dispatch: "native-acknowledged", resolution: "clear",
              },
            }) as unknown as Api["instanceSend"],
    );
    vi.spyOn(api, "instanceCommandStatus").mockImplementation(
      (async (_iid: string, commandId: string) => g25Status(commandId, "reconciling")) as Api["instanceCommandStatus"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    const boxOf = () =>
      (
        hubStore as unknown as {
          outbox: {
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];

    hubStore.setConnectionStateForTest("offline");
    locks = bad;
    expect(await hubStore.send(I, "g25 n2 P")).toBe(true);
    await pump(200);
    const pId = storedRows().find((r) => r.prompt === "g25 n2 P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);

    locks = good;
    hubStore.setConnectionStateForTest("live");
    expect(await hubStore.send(I, "g25 n2 S", [], [], "steer")).toBe(false);
    await pump(500);
    const sId = send.mock.calls[0]?.[4] as string;

    // Through the full 30 s GET deadline the answer stays uncertain: the steer
    // ends unknown, exhaustion remains latched and P is never delivered.
    await vi.advanceTimersByTimeAsync(35_000);
    await pump(2_000);
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("unknown");
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(send).toHaveBeenCalledTimes(1);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("pending");

    await vi.advanceTimersByTimeAsync(120_000);
    expect(send).toHaveBeenCalledTimes(1);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("pending");
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

/** GATE26 helpers: follow subscription shape, journal event, status records. */
function g26Subscription() {
  return {
    subscriptionId: "sub_g26",
    journalId: JOURNAL,
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
    getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_g26",
      asOfSeq: "0",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetrainedSeq: "0", complete: true },
    },
  };
}
type G26Batch = {
  subscriptionId: string;
  journalId: string;
  fromSeq: string;
  toSeq: string;
  durableSeq: string;
  events: unknown[];
};
const g26JournalMessage = (commandId: string, seq: string): unknown => ({
  kind: "message",
  eventId: `evt_g26_${seq}`,
  journalId: JOURNAL,
  instanceId: "ins_g26",
  seq,
  payload: { role: "user", commandId },
});

it("GATE26 item 1a: journal settles the reconciling steer done BEFORE the status GET — the acceptance observation clears exhaustion and the older row is delivered by the real timer", async () => {
  // The status GET stays parked while the live follow receives the
  // command-confirming journal message. Pre-gate-26 only the GET loop could
  // record progress, so a journal acceptance before any GET verdict left the
  // instance exhausted. Post-fix the journal observation is itself an
  // authoritative acceptance routed through the single helper.
  vi.useFakeTimers();
  vi.setSystemTime(990_000);
  const I = "ins_g26_a";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g26 a locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = bad;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g26_a");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) =>
        prompt === "g26 a S"
          ? g25ReconcilingAnswer(commandId ?? "cmd_x")
          : {
              relatedCommandIds: [],
              command: {
                commandId, id: commandId, state: "accepted", revision: "1",
                dispatch: "native-acknowledged", resolution: "clear",
              },
            }) as unknown as Api["instanceSend"],
    );
    let releaseGet: (r: unknown) => void = () => undefined;
    vi.spyOn(api, "instanceCommandStatus").mockImplementation(
      () =>
        new Promise((resolve) => {
          releaseGet = resolve as (r: unknown) => void;
        }) as never,
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await hubStore.follow(I);
    const subscribe = vi.mocked(api.eventsSubscribe);
    for (let i = 0; i < 2_000 && !subscribe.mock.calls[0]?.[2]; i += 1) await Promise.resolve();
    const onBatch = subscribe.mock.calls[0]?.[2] as (b: G26Batch) => void;
    const boxOf = () =>
      (
        hubStore as unknown as {
          outbox: {
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];

    hubStore.setConnectionStateForTest("offline");
    locks = bad;
    expect(await hubStore.send(I, "g26 a P")).toBe(true);
    await pump(200);
    const pId = storedRows().find((r) => r.prompt === "g26 a P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);

    locks = good;
    hubStore.setConnectionStateForTest("live");
    expect(await hubStore.send(I, "g26 a S", [], [], "steer")).toBe(false);
    await pump(500);
    const sId = send.mock.calls[0]?.[4] as string;
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("reconciling");

    // The journal settles S done while the GET is STILL parked.
    onBatch({
      subscriptionId: "sub_g26",
      journalId: JOURNAL,
      fromSeq: "1",
      toSeq: "1",
      durableSeq: "1",
      events: [g26JournalMessage(sId, "1")],
    });
    for (let i = 0; i < 400 && send.mock.calls.length < 2; i += 1) {
      await vi.advanceTimersByTimeAsync(10);
      await Promise.resolve();
    }
    expect(send.mock.calls.map((c) => c[4])).toEqual([sId, pId]);
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("done");
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("sent");
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(false);

    // Release the parked GET (still-reconciling): the loop top sees done and
    // returns without downgrading it or re-POSTing the steer.
    releaseGet(g25Status(sId, "reconciling"));
    await pump(500);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(send.mock.calls.map((c) => c[4])).toEqual([sId, pId]);
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("done");
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});
it("GATE26 item 1b: another tab's journal settle commits done DURING A's accepted GET — the dropped-patch GET verdict is the ONLY progress source for A and delivers P", async () => {
  // Gate 27 item 3 hardening: the journal-done and the accepted GET can both
  // record progress, so the old version of this test stayed green even when
  // the GET path stopped recording (the journal observation covered for it).
  // Here the done commit comes from a SECOND, real HubStore B's journal
  // settle on the shared backend while B is PAGE-HIDDEN: B retires S to
  // durably done (journal evidence always applies; B loaded S at boot), but
  // B never issued S and a hidden box neither records progress nor
  // reconciles — so for A the journal cannot be the source. A never receives
  // the event. When A's parked GET then resolves accepted, its local sent
  // patch is dropped (stored row already done); ONLY the GET-path recording
  // clears A's exhaustion and P is delivered by A's real timer. B never
  // POSTs.
  vi.useFakeTimers();
  vi.setSystemTime(991_000);
  const I = "ins_g26_b";
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  const good = shim.locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g26 b locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = good;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    // A boots with locks UP and sends the reconciling steer; its accepted GET
    // parks.
    sessionStorage.setItem("remuda-outbox-owner", "owner_g26_b_a");
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(apiA, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(apiA, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const sendA = vi.spyOn(apiA, "instanceSend").mockImplementation(
      (async (_iid: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) =>
        prompt === "g26 b S"
          ? g25ReconcilingAnswer(commandId ?? "cmd_x")
          : {
              relatedCommandIds: [],
              command: {
                commandId, id: commandId, state: "accepted", revision: "1",
                dispatch: "native-acknowledged", resolution: "clear",
              },
            }) as unknown as Api["instanceSend"],
    );
    let releaseGet: (r: unknown) => void = () => undefined;
    vi.spyOn(apiA, "instanceCommandStatus").mockImplementation(
      () =>
        new Promise((resolve) => {
          releaseGet = resolve as (r: unknown) => void;
        }) as never,
    );
    await bootLive(apiA, a);
    await (a as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    expect(await a.send(I, "g26 b S", [], [], "steer")).toBe(false);
    await pump(500);
    const sId = sendA.mock.calls[0]?.[4] as string;
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("reconciling");

    // B boots second: its boot flush reads the shared backend (S is
    // reconciling/non-deliverable, so it POSTs nothing) and its restored
    // reconciling GET parks forever. Then B is PAGE-HIDDEN.
    sessionStorage.setItem("remuda-outbox-owner", "owner_g26_b_b");
    const { api: apiB, hubStore: b } = await fresh();
    vi.spyOn(apiB, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiB, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(apiB, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(apiB, "eventsSubscribe").mockResolvedValue(
      { ...g26Subscription(), subscriptionId: "sub_g26_b" } as never,
    );
    vi.spyOn(apiB, "instanceCommandStatus").mockImplementation(
      () => new Promise(() => {}) as never,
    );
    const sendB = vi.spyOn(apiB, "instanceSend");
    await bootLive(apiB, b);
    await pump(1_000);
    await b.follow(I);
    const subscribeB = vi.mocked(apiB.eventsSubscribe);
    for (let i = 0; i < 2_000 && !subscribeB.mock.calls[0]?.[2]; i += 1) await Promise.resolve();
    const onBatchB = subscribeB.mock.calls[0]?.[2] as (batch: G26Batch) => void;
    (b as unknown as { onPageHide: (e?: Event) => void }).onPageHide(new Event("pagehide"));
    expect((b as unknown as { pageIsUnloadingForTest: boolean }).pageIsUnloadingForTest).toBe(true);
    expect(sendB).not.toHaveBeenCalled();

    const boxOfA = () =>
      (
        a as unknown as {
          outbox: {
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;

    // Web Locks go down: A enqueues P while offline and its real timer wakes
    // exhaust the instance. B is hidden — it never reconciles or POSTs.
    a.setConnectionStateForTest("offline");
    locks = bad;
    expect(await a.send(I, "g26 b P")).toBe(true);
    await pump(200);
    const pId = storedRows().find((r) => r.prompt === "g26 b P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOfA()!.isDeliveryExhausted(I)).toBe(true);
    expect(sendA).toHaveBeenCalledTimes(1);
    expect(sendB).not.toHaveBeenCalled();

    // Locks recover (A is back live). B's REAL journal settle commits S done
    // WHILE A's GET is parked: the hidden B retires the row but records and
    // reconciles nothing.
    locks = good;
    a.setConnectionStateForTest("live");
    onBatchB({
      subscriptionId: "sub_g26_b",
      journalId: JOURNAL,
      fromSeq: "1",
      toSeq: "1",
      durableSeq: "1",
      events: [g26JournalMessage(sId, "1")],
    });
    await pump(2_000);
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("done");
    // A never saw the journal event: still exhausted, P uncovered.
    expect(boxOfA()!.isDeliveryExhausted(I)).toBe(true);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("pending");
    expect(sendA).toHaveBeenCalledTimes(1);
    expect(sendB).not.toHaveBeenCalled();

    // A's parked GET resolves ACCEPTED: its sent patch is dropped against the
    // committed done, yet the GET verdict alone records progress on A — the
    // epilogue re-arms P and A's real timer delivers it.
    releaseGet(g25Status(sId, "accepted"));
    for (let i = 0; i < 400 && sendA.mock.calls.length < 2; i += 1) {
      await vi.advanceTimersByTimeAsync(10);
      await Promise.resolve();
    }
    expect(sendA.mock.calls.map((c) => c[4])).toEqual([sId, pId]);
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("done");
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("sent");
    expect(boxOfA()!.isDeliveryExhausted(I)).toBe(false);
    // B's hidden box never POSTed anything.
    expect(sendB).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(120_000);
    expect(sendA.mock.calls.map((c) => c[4])).toEqual([sId, pId]);
    expect(sendB).not.toHaveBeenCalled();
    a.logout();
    b.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE26 item 1c: a Hub-rejected steer stays rejected when journal traffic arrives — rejection and non-confirming journal events never clear exhaustion", async () => {
  vi.useFakeTimers();
  vi.setSystemTime(992_000);
  const I = "ins_g26_c";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g26 c locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = bad;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g26_c");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) =>
        prompt === "g26 c S"
          ? g25ReconcilingAnswer(commandId ?? "cmd_x")
          : {
              relatedCommandIds: [],
              command: {
                commandId, id: commandId, state: "accepted", revision: "1",
                dispatch: "native-acknowledged", resolution: "clear",
              },
            }) as unknown as Api["instanceSend"],
    );
    // Both the verdict GET and its double-confirm GET report rejected.
    vi.spyOn(api, "instanceCommandStatus").mockImplementation(
      (async (_iid: string, commandId: string) => g25Status(commandId, "rejected")) as Api["instanceCommandStatus"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await hubStore.follow(I);
    const subscribe = vi.mocked(api.eventsSubscribe);
    for (let i = 0; i < 2_000 && !subscribe.mock.calls[0]?.[2]; i += 1) await Promise.resolve();
    const onBatch = subscribe.mock.calls[0]?.[2] as (b: G26Batch) => void;
    const boxOf = () =>
      (
        hubStore as unknown as {
          outbox: {
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];

    hubStore.setConnectionStateForTest("offline");
    locks = bad;
    expect(await hubStore.send(I, "g26 c P")).toBe(true);
    await pump(200);
    const pId = storedRows().find((r) => r.prompt === "g26 c P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);

    locks = good;
    hubStore.setConnectionStateForTest("live");
    expect(await hubStore.send(I, "g26 c S", [], [], "steer")).toBe(false);
    await pump(1_000);
    await vi.advanceTimersByTimeAsync(1_000);
    const sId = send.mock.calls[0]?.[4] as string;
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("rejected");
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(send).toHaveBeenCalledTimes(1);

    // Journal traffic for the rejected S: a non-message observation with its
    // id settles nothing; then a late commandId MESSAGE arrives after the Hub
    // rejection — neither may record progress.
    onBatch({
      subscriptionId: "sub_g26",
      journalId: JOURNAL,
      fromSeq: "1",
      toSeq: "2",
      durableSeq: "2",
      events: [
        { kind: "opaque", eventId: "evt_g26_o", journalId: JOURNAL, instanceId: I, seq: "1", payload: { commandId: sId } },
        g26JournalMessage(sId, "2"),
      ],
    });
    // No acceptance observation happened: with NO timer advanced past the
    // batch (a later user/online reconnect flush is a separate, legitimate
    // fresh trigger outside this ruling), rejection still owns the instance —
    // P is uncovered, exhausted, and the steer is not re-POSTed.
    await pump(2_000);
    expect(send).toHaveBeenCalledTimes(1);
    expect(storedRows().find((r) => r.commandId === sId)?.state).toBe("rejected");
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("pending");
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(boxOf()!.pendingLeaseWakeups()).toEqual([]);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE27 item 1: a second acceptance observation of the SAME command is a no-op — failures accrued after the first acceptance survive a late journal settle", async () => {
  // An accepted POST records progress once. The instance then earns NEW
  // delivery failures (a later pending row hammers the down Web Lock to the
  // cap). When a journal settle for the SAME already-accepted command id
  // arrives late, pre-gate-27 the helper cleared the instance's books a
  // second time and wiped the new backoff/exhaustion; post-fix the second
  // observation is a no-op and the failures stay exactly as accrued.
  vi.useFakeTimers();
  vi.setSystemTime(1_000_000);
  const I = "ins_g27_i1";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g27 i1 locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = bad;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g27_i1");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: {
          commandId, id: commandId, state: "accepted", revision: "1",
          dispatch: "native-acknowledged", resolution: "clear",
        },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await hubStore.follow(I);
    const subscribe = vi.mocked(api.eventsSubscribe);
    for (let i = 0; i < 2_000 && !subscribe.mock.calls[0]?.[2]; i += 1) await Promise.resolve();
    const onBatch = subscribe.mock.calls[0]?.[2] as (b: G26Batch) => void;
    const boxOf = () =>
      (
        hubStore as unknown as {
          outbox: {
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];

    // 1) The accepted POST records progress the first time.
    locks = good;
    hubStore.setConnectionStateForTest("live");
    expect(await hubStore.send(I, "g27 1 C")).toBe(true);
    await pump(500);
    const cId = send.mock.calls[0]?.[4] as string;
    expect(storedRows().find((r) => r.commandId === cId)?.state).toBe("sent");
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(false);

    // 2) Locks go down; a NEW pending row spends the bounded budget — fresh
    //    failures accrue and the instance is exhausted again.
    locks = bad;
    hubStore.setConnectionStateForTest("offline");
    expect(await hubStore.send(I, "g27 1 P2")).toBe(true);
    await pump(200);
    const p2Id = storedRows().find((r) => r.prompt === "g27 1 P2")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(storedRows().find((r) => r.commandId === p2Id)?.state).toBe("pending");
    const callsAtExhaustion = send.mock.calls.length;

    // 3) A late journal settle of the SAME, already-accepted command id: the
    //    second acceptance observation must be a no-op. The row is sent (not
    //    re-patched), the latch and P2's cover are untouched.
    onBatch({
      subscriptionId: "sub_g26",
      journalId: JOURNAL,
      fromSeq: "1",
      toSeq: "1",
      durableSeq: "1",
      events: [g26JournalMessage(cId, "1")],
    });
    await pump(2_000);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(boxOf()!.pendingLeaseWakeups()).toEqual([]);
    expect(send.mock.calls.length).toBe(callsAtExhaustion);
    expect(storedRows().find((r) => r.commandId === p2Id)?.state).toBe("pending");
    // Journal evidence still retires the already-accepted row to done — the
    // no-op is only the PROGRESS side (latch/wakes), not the durable patch.
    expect(storedRows().find((r) => r.commandId === cId)?.state).toBe("done");

    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});
it("GATE27 item 2: an OLD-epoch acceptance arriving only AFTER logout/reinit (the command was issued but never accepted) never credits the new epoch", async () => {
  // codex gate-29 refinement: epoch A ISSUES C (its claim commits, stamping
  // the issuer) but C is never accepted — the POST hangs inflight. Logout/
  // reinit creates epoch B, which exhausts its own instance on P while C's
  // live same-owner lease keeps B from claiming it. The stale epoch-A
  // subscription delivers C's accepted journal settle ONLY THEN. The durable
  // CAS compares C's issuer (A) with the current epoch (B): the row retires
  // to done (journal evidence always applies) but progressRecorded never
  // flips, B's exhaustion/backoff are untouched and P stays stranded.
  vi.useFakeTimers();
  vi.setSystemTime(1_010_000);
  const I = "ins_g27_i2";
  const oldI = "ins_g27_i2_old";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g27 i2 locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = good;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
  const rowOf = (id: string) => storedRows().find((r) => r.commandId === id);
  try {
    // Epoch A: follow (capture subscription); C's POST parks forever so the
    // command is claimed (issuer = A) but never accepted before logout.
    sessionStorage.setItem("remuda-outbox-owner", "owner_g27_i2");
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(apiA, "instanceGet").mockImplementation(
      (async (id: string) => ({ id, journalId: JOURNAL })) as Api["instanceGet"],
    );
    vi.spyOn(apiA, "instanceList").mockResolvedValue({
      items: [{ id: I, journalId: JOURNAL, revision: "1", durableSeq: "0", lifecycle: "running" } as never],
      nextCursor: null,
    });
    vi.spyOn(apiA, "eventsSubscribe").mockResolvedValue(
      { ...g26Subscription(), subscriptionId: "sub_g27_i2_a" } as never,
    );
    const sendA = vi.spyOn(apiA, "instanceSend");
    sendA.mockImplementation(
      (() => new Promise(() => { /* C's POST never resolves: issued, not accepted */ })) as Api["instanceSend"],
    );
    await bootLive(apiA, a);
    await (a as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await a.follow(oldI);
    const subscribeA = vi.mocked(apiA.eventsSubscribe);
    for (let i = 0; i < 2_000 && !subscribeA.mock.calls[0]?.[2]; i += 1) await Promise.resolve();
    const onBatchA = subscribeA.mock.calls[0]?.[2] as (b: G26Batch) => void;

    // Online ordinary send: the claim commits and the POST then hangs.
    a.setConnectionStateForTest("live");
    await a.send(oldI, "g27 2 C");
    await pump(2_000);
    const cId = sendA.mock.calls[0]?.[4] as string;
    expect(sendA).toHaveBeenCalledTimes(1);
    expect(rowOf(cId)?.state).toBe("inflight");
    const lifeA = rowOf(cId)?.issuerLifetime;
    expect(lifeA).toBeTruthy();
    expect(rowOf(cId)?.progressRecorded).toBeFalsy();

    // Logout BEFORE any acceptance: the outbox is disposed (its pending POST
    // dies with it); reinit creates a NEW outbox with a new lifetime id.
    a.logout();
    const reopened = await (
      a as unknown as { ensureOutbox: () => Promise<boolean> }
    ).ensureOutbox();
    expect(reopened).toBe("ok");
    const lifeB = (a as unknown as { outbox: { lifetime: string } }).outbox.lifetime;
    expect(lifeB).not.toBe(lifeA);
    // B's boot flush cannot touch C: B is a different lifetime issuer and C
    // carries a live same-owner lease (TTL 30 s), so the claim stops at
    // claim-aborted.
    locks = good;
    a.setConnectionStateForTest("live");
    await (a as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await pump(1_000);
    expect(rowOf(cId)?.state).toBe("inflight");
    expect(rowOf(cId)?.issuerLifetime).toBe(lifeA);
    expect(sendA).toHaveBeenCalledTimes(1);
    const boxOfB = () =>
      (
        a as unknown as {
          outbox: {
            isDeliveryExhausted: (i: string) => boolean;
            pendingLeaseWakeups: () => string[];
          } | null;
        }
      ).outbox;

    // B exhausts its OWN instance on P with the locks down.
    locks = bad;
    a.setConnectionStateForTest("offline");
    expect(await a.send(I, "g27 2 P")).toBe(true);
    await pump(200);
    const pId = storedRows().find((r) => r.prompt === "g27 2 P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOfB()!.isDeliveryExhausted(I)).toBe(true);
    expect(boxOfB()!.pendingLeaseWakeups()).toEqual([]);

    // The stale outbox-A subscription settles C accepted/done AFTER reinit.
    onBatchA({
      subscriptionId: "sub_g27_i2_a",
      journalId: JOURNAL,
      fromSeq: "1",
      toSeq: "1",
      durableSeq: "1",
      events: [g26JournalMessage(cId, "1")],
    });
    await pump(2_000);
    await vi.advanceTimersByTimeAsync(1_000);

    // Journal evidence retires C to durable done, but C is NOT credited to
    // outbox B: the flag never flips (issuer lifetime A ≠ B) and B's books
    // stand.
    expect(rowOf(cId)?.state).toBe("done");
    expect(rowOf(cId)?.issuerLifetime).toBe(lifeA);
    expect(rowOf(cId)?.progressRecorded).toBeFalsy();
    expect(boxOfB()!.isDeliveryExhausted(I)).toBe(true);
    expect(boxOfB()!.pendingLeaseWakeups()).toEqual([]);
    expect(rowOf(pId)?.state).toBe("pending");
    expect(sendA).toHaveBeenCalledTimes(1);
    a.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

/** GATE28: read the durable outbox rows straight from localStorage. */
function g28Rows(): Array<OutboxRecord & { progressRecorded?: boolean }> {
  return JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<OutboxRecord & {
    progressRecorded?: boolean;
  }>;
}
const g28Row = (id: string) => g28Rows().find((r) => r.commandId === id);

it("GATE28 a: a claim that fails before commit stamps no issuer — the stale command's acceptance on the replacement epoch records nothing", async () => {
  // In epoch A the row C's REAL claim transaction aborts BEFORE commit (fault
  // gated by command id + durable state): the issuer epoch is part of that
  // write, so the row stays without an issuer. Logout/reinit creates epoch B,
  // which exhausts its own instance on P. The stale epoch-A subscription then
  // delivers C's accepted journal settle: the durable CAS sees no matching
  // issuer on the new epoch and does NOT flip — C retires to done but B stays
  // exhausted and P stays uncovered.
  vi.useFakeTimers();
  vi.setSystemTime(1_020_000);
  const I = "ins_g28_a";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g28 a locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = good;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    // Epoch A: follow (capture subscription), enqueue C, make its real claim
    // abort before commit while locks are up.
    sessionStorage.setItem("remuda-outbox-owner", "owner_g28_a_a");
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(apiA, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(apiA, "eventsSubscribe").mockResolvedValue({ ...g26Subscription(), subscriptionId: "sub_g28_a" } as never);
    const sendA = vi.spyOn(apiA, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [], command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(apiA, a);
    await (a as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await a.follow(I);
    const subscribeA = vi.mocked(apiA.eventsSubscribe);
    for (let i = 0; i < 2_000 && !subscribeA.mock.calls[0]?.[2]; i += 1) await Promise.resolve();
    const onBatchA = subscribeA.mock.calls[0]?.[2] as (b: G26Batch) => void;
    a.setConnectionStateForTest("live");
    expect(await a.send(I, "g28 a C")).toBe(true);
    const cId = g28Rows().find((r) => r.prompt === "g28 a C")!.commandId;
    const aStorage = (a as unknown as { outbox: { storage: { mergeUnlessDone: (id: string, p: Partial<OutboxRecord>) => Promise<OutboxRecord | null> } } }).outbox.storage;
    const realMergeA = aStorage.mergeUnlessDone.bind(aStorage);
    let claimFaults = 0;
    aStorage.mergeUnlessDone = vi.fn(async (id, patch) => {
      if (id === cId && (patch as { state?: string }).state === "inflight") {
        claimFaults += 1;
        throw new Error("g28 a claim pre-commit abort");
      }
      return await realMergeA(id, patch);
    });
    // Kick the claim while a blocker holds the instance lock; simplest is the
    // real boot/online flush after a second send — drive one flush directly.
    void (a as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await pump(2_000);
    await vi.advanceTimersByTimeAsync(1_000);
    await pump(1_000);
    expect(claimFaults).toBeGreaterThan(0);
    expect(sendA).not.toHaveBeenCalled();
    // The failed claim stamped NOTHING.
    expect(g28Row(cId)?.issuerLifetime).toBeUndefined();
    expect(g28Row(cId)?.state).toBe("pending");

    // Logout disposes the outbox; reinit creates a new outbox (new lifetime).
    a.logout();
    const reopened = await (a as unknown as { ensureOutbox: () => Promise<boolean> }).ensureOutbox();
    expect(reopened).toBe("ok");
    const boxB = () => (a as unknown as { outbox: { isDeliveryExhausted: (i: string) => boolean; pendingLeaseWakeups: () => string[] } | null }).outbox;

    // B exhausts its own instance on P with the locks down.
    locks = bad;
    a.setConnectionStateForTest("offline");
    expect(await a.send(I, "g28 a P")).toBe(true);
    await pump(200);
    const pRowId = g28Rows().find((r) => r.prompt === "g28 a P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxB()!.isDeliveryExhausted(I)).toBe(true);

    // Locks recover and ONE flush runs: C is first in FIFO but its real claim
    // aborts BEFORE commit (fault re-armed on B's storage), so the flush stops
    // and nothing POSTs. Gate 27's selection-time mark wrongly assigns C to B
    // here; gate 28 stamps nothing because no claim committed.
    const bStorage = (a as unknown as { outbox: { storage: { mergeUnlessDone: (id: string, p: Partial<OutboxRecord>) => Promise<OutboxRecord | null> } } }).outbox.storage;
    const realMergeB = bStorage.mergeUnlessDone.bind(bStorage);
    let bClaimFaults = 0;
    bStorage.mergeUnlessDone = vi.fn(async (id, patch) => {
      if (id === cId && (patch as { state?: string }).state === "inflight") {
        bClaimFaults += 1;
        throw new Error("g28 a B claim pre-commit abort");
      }
      return await realMergeB(id, patch);
    });
    locks = good;
    await (a as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await pump(2_000);
    expect(bClaimFaults).toBeGreaterThan(0);
    expect(sendA).not.toHaveBeenCalled();
    expect(g28Row(cId)?.issuerLifetime).toBeUndefined();
    expect(g28Row(pRowId)?.state).toBe("pending");

    // The stale epoch-A subscription settles C accepted/done.
    onBatchA({
      subscriptionId: "sub_g28_a",
      journalId: JOURNAL,
      fromSeq: "1",
      toSeq: "1",
      durableSeq: "1",
      events: [g26JournalMessage(cId, "1")],
    });
    await pump(2_000);
    await vi.advanceTimersByTimeAsync(1_000);
    // Journal evidence retires C, but its acceptance recorded nothing on B.
    expect(g28Row(cId)?.state).toBe("done");
    expect(g28Row(cId)?.progressRecorded).toBeFalsy();
    expect(boxB()!.isDeliveryExhausted(I)).toBe(true);
    expect(boxB()!.pendingLeaseWakeups()).toEqual([]);
    expect(g28Row(pRowId)?.state).toBe("pending");
    expect(sendA).not.toHaveBeenCalled();
    a.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});
it("GATE28 b: POST → status GET → journal for the SAME command records progress exactly once, with more accepted commands than any cache could hold between them and later failures preserved", { timeout: 60_000 }, async () => {

  // codex gate-29 NOT MET: the acceptance sequence must be POST, then the
  // command-status GET, then a JOURNAL settle of the SAME command id — and
  // enough OTHER commands accepted in between (more than the old 2 000-row
  // in-memory tracking bound) that only durable row state can remember the
  // first recording. Progress for C happens exactly once (the GET); the later
  // journal observation and the filler volume change nothing; failures the
  // instance earns after the GET survive the journal.
  vi.useFakeTimers();
  vi.setSystemTime(1_030_000);
  const I = "ins_g28_b";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g28 b locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = good;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  // More rows than the removed in-memory sets' eviction bound.
  const FILLER = 600;
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g28_b");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) =>
        prompt === "g28 b C"
          ? g25ReconcilingAnswer(commandId ?? "cmd_x")
          : {
              relatedCommandIds: [],
              command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" },
            }) as unknown as Api["instanceSend"],
    );
    // GET 1 still reconciling; GET 2 accepted; later GETs stay accepted.
    let gets = 0;
    vi.spyOn(api, "instanceCommandStatus").mockImplementation(
      (async (_i: string, commandId: string) =>
        g25Status(commandId, gets++ === 0 ? "reconciling" : "accepted")) as Api["instanceCommandStatus"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await hubStore.follow(I);
    const subscribe = vi.mocked(api.eventsSubscribe);
    for (let i = 0; i < 2_000 && !subscribe.mock.calls[0]?.[2]; i += 1) await Promise.resolve();
    const onBatch = subscribe.mock.calls[0]?.[2] as (b: G26Batch) => void;
    const boxHandle = () =>
      hubStore as unknown as {
        outbox: {
          isDeliveryExhausted: (i: string) => boolean;
          pendingLeaseWakeups: () => string[];
        } | null;
      };
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    // Spy on the REAL acceptance CAS (no bypass): the filler POSTs and C's GET
    // acceptance run through production; the final journal observation of C
    // must invoke the CAS and get false (the flag is already set by the GET).
    const casStorage = (hubStore as unknown as {
      outbox: { storage: { casProgressRecorded: (id: string, l: string) => Promise<boolean> } };
    }).outbox.storage;
    const realCas = casStorage.casProgressRecorded.bind(casStorage);
    const casCalls = vi.fn(async (id: string, l: string) => realCas(id, l));
    casStorage.casProgressRecorded = casCalls;

    // 1) POST: the steer answers queued+forwarded+reconciling — no acceptance.
    hubStore.setConnectionStateForTest("live");
    expect(await hubStore.send(I, "g28 b C", [], [], "steer")).toBe(false);
    for (let i = 0; i < 1_000 && gets < 1; i += 1) await Promise.resolve();
    const cId = send.mock.calls[0]?.[4] as string;
    expect(g28Row(cId)?.state).toBe("reconciling");
    expect(g28Row(cId)?.progressRecorded).toBeFalsy();

    // 2) status GET confirms accepted — the ONE progress recording.
    await vi.advanceTimersByTimeAsync(1_000);
    for (let i = 0; i < 1_000; i += 1) await Promise.resolve();
    expect(g28Row(cId)?.state).toBe("sent");
    expect(g28Row(cId)?.progressRecorded).toBe(true);
    expect(boxHandle().outbox!.isDeliveryExhausted(I)).toBe(false);

    // 3) More OTHER accepted deliveries than any small in-memory cache could
    //    practically hold. Every one is a REAL delivery through the store: an
    //    ordinary send (storage-first enqueue) → the guarded flushAllOutbox
    //    commits the claim (stamping this outbox's lifetime) → the accepted
    //    API POST → the sent patch → the acceptance helper's durable CAS.
    //    Each filler is on its own instance so none touches C's. The send mock
    //    accepts every non-C prompt.
    for (let i = 0; i < FILLER; i += 1) {
      const iid = `ins_g28_b_filler_${i % 40}`;
      // eslint-disable-next-line no-await-in-loop
      expect(await hubStore.send(iid, `filler ${i}`)).toBe(true);
      // eslint-disable-next-line no-await-in-loop
      await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    }
    // C plus every filler was really POSTed and recorded once.
    expect(send.mock.calls).toHaveLength(FILLER + 1);
    expect(storedRows().filter((r) => r.progressRecorded)).toHaveLength(FILLER + 1);
    expect(g28Row(cId)?.progressRecorded).toBe(true);

    // 4) Fresh delivery failures accrue on C's instance (a later row Q hammers
    //    the down lock to the cap).
    locks = bad;
    hubStore.setConnectionStateForTest("offline");
    expect(await hubStore.send(I, "g28 b Q")).toBe(true);
    await pump(200);
    const qId = storedRows().find((r) => r.prompt === "g28 b Q")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxHandle().outbox!.isDeliveryExhausted(I)).toBe(true);
    const callsBeforeJournal = send.mock.calls.length;

    // 5) JOURNAL settle of the SAME command C (the third acceptance
    //    observation): it really reaches the production CAS and returns false
    //    (flag already set by the accepted GET) → no second recording; Q's
    //    exhaustion/backoff and C's one POST are untouched.
    const casBefore = casCalls.mock.calls.length;
    onBatch({
      subscriptionId: "sub_g26",
      journalId: JOURNAL,
      fromSeq: "1",
      toSeq: "1",
      durableSeq: "1",
      events: [g26JournalMessage(cId, "1")],
    });
    await pump(2_000);
    await vi.advanceTimersByTimeAsync(1_000);
    // The journal observation invoked the REAL CAS for C...
    expect(casCalls.mock.calls.length).toBeGreaterThan(casBefore);
    // ...and its CAS result is false (flag already set by the accepted GET).
    const cResults = await Promise.all(
      casCalls.mock.results
        .filter((_, i) => casCalls.mock.calls[i][0] === cId)
        .map((r) => r.value),
    );
    expect(cResults).toContain(false);
    expect(g28Row(cId)?.state).toBe("done");
    expect(g28Row(cId)?.progressRecorded).toBe(true);
    expect(boxHandle().outbox!.isDeliveryExhausted(I)).toBe(true);
    expect(boxHandle().outbox!.pendingLeaseWakeups()).toEqual([]);
    expect(send.mock.calls.length).toBe(callsBeforeJournal);
    expect(send.mock.calls.filter((c) => c[4] === cId)).toHaveLength(1);
    expect(g28Row(qId)?.state).toBe("pending");
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE28 c: cancelled navigation (pagehide) between the claim and the accepted GET still records progress once after BFCache restore", async () => {
  // C claims (issuer stamped) and answers reconciling — no acceptance yet.
  // The page is genuinely hidden (the old in-memory sets were cleared right
  // here), then BFCache-restored. The parked GET then reports accepted: with
  // durable row state the flag still flips exactly once and P is delivered.
  vi.useFakeTimers();
  vi.setSystemTime(1_040_000);
  const I = "ins_g28_c";
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: shim.locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g28_c");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) =>
        prompt === "g28 c C"
          ? g25ReconcilingAnswer(commandId ?? "cmd_x")
          : { relatedCommandIds: [], command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" } }) as unknown as Api["instanceSend"],
    );
    let gets = 0;
    vi.spyOn(api, "instanceCommandStatus").mockImplementation(
      (async (_i: string, commandId: string) => g25Status(commandId, gets++ === 0 ? "reconciling" : "accepted")) as Api["instanceCommandStatus"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    const box = () => (hubStore as unknown as { outbox: { isDeliveryExhausted: (i: string) => boolean } | null }).outbox;

    // Exhaust P first (locks down), then a reconciling steer with locks up.
    const bad = { query: async () => ({ held: [] as Array<{ name: string }> }), request: () => Promise.reject(new Error("g28 c locks down")) as Promise<never> };
    let locks: unknown = bad;
    Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { get locks() { return locks; } } });
    hubStore.setConnectionStateForTest("offline");
    expect(await hubStore.send(I, "g28 c P")).toBe(true);
    await pump(200);
    const pId = g28Rows().find((r) => r.prompt === "g28 c P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(box()!.isDeliveryExhausted(I)).toBe(true);

    locks = shim.locks;
    hubStore.setConnectionStateForTest("live");
    expect(await hubStore.send(I, "g28 c C", [], [], "steer")).toBe(false);
    for (let i = 0; i < 1_000 && gets < 1; i += 1) await Promise.resolve();
    const cId = send.mock.calls[0]?.[4] as string;
    expect(g28Row(cId)?.state).toBe("reconciling");
    expect(g28Row(cId)?.issuerLifetime).toBeTruthy();
    expect(g28Row(cId)?.progressRecorded).toBeFalsy();

    // Genuine page navigation away and BFCache restore before the GET settles.
    (hubStore as unknown as { onPageHide: (e?: Event) => void }).onPageHide(new Event("pagehide"));
    expect((hubStore as unknown as { pageIsUnloadingForTest: boolean }).pageIsUnloadingForTest).toBe(true);
    (hubStore as unknown as { onConnPageshow: (e: PageTransitionEvent) => void }).onConnPageshow(
      { persisted: true } as PageTransitionEvent,
    );
    expect((hubStore as unknown as { pageIsUnloadingForTest: boolean }).pageIsUnloadingForTest).toBe(false);

    // GET 2 accepted: the durable CAS survives the teardown — records once.
    await vi.advanceTimersByTimeAsync(1_000);
    for (let i = 0; i < 400 && send.mock.calls.length < 2; i += 1) {
      await vi.advanceTimersByTimeAsync(10);
      await Promise.resolve();
    }
    expect(send.mock.calls.map((c) => c[4])).toEqual([cId, pId]);
    expect(g28Row(cId)?.progressRecorded).toBe(true);
    expect(g28Row(pId)?.state).toBe("sent");
    expect(box()!.isDeliveryExhausted(I)).toBe(false);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE28 d / GATE30 single-writer: after a reload of the same login a restored reconciling row's acceptance is NOT credited to the new page instance — its exhaustion stands", async () => {
  // Not a logout: the auth EPOCH persists in localStorage, but a reload is a
  // new PAGE INSTANCE with a fresh outbox lifetime id (c-reconnfu gate 30
  // single-writer ruling). The restored reconciling row C was issued by the
  // previous page lifetime; when its restarted GET loop now reports accepted,
  // the new outbox is NOT the issuer — the sent patch applies, but the CAS
  // never flips and the new page's own exhaustion (on P) is untouched.
  // Documented/accepted: only the issuing outbox records that acceptance.
  vi.useFakeTimers();
  vi.setSystemTime(1_050_000);
  const I = "ins_g28_d";
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: shim.locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g28_d");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, prompt: string, _x?: unknown, _m?: string, commandId?: string) =>
        prompt === "g28 d C"
          ? g25ReconcilingAnswer(commandId ?? "cmd_x")
          : { relatedCommandIds: [], command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" } }) as unknown as Api["instanceSend"],
    );
    // Every status GET parks. releaseGet() resolves only the MOST RECENT
    // waiter: in a real reload the OLD document (and its parked GET) is dead,
    // so its promise must never settle here — only the recreated page's GET
    // receives the accepted verdict.
    const getWaiters: Array<(r: unknown) => void> = [];
    const releaseGet = (r: unknown) => {
      const latest = getWaiters.pop();
      latest?.(r);
    };
    vi.spyOn(api, "instanceCommandStatus").mockImplementation(
      async (_i: string, commandId: string) => {
        await new Promise((resolve) => { getWaiters.push(resolve); });
        return g25Status(commandId, "accepted");
      },
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();

    // Exhaust P (locks down) before the steer.
    const bad = { query: async () => ({ held: [] as Array<{ name: string }> }), request: () => Promise.reject(new Error("g28 d locks down")) as Promise<never> };
    let locks: unknown = bad;
    Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { get locks() { return locks; } } });
    hubStore.setConnectionStateForTest("offline");
    expect(await hubStore.send(I, "g28 d P")).toBe(true);
    await pump(200);
    const pId = g28Rows().find((r) => r.prompt === "g28 d P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);

    locks = shim.locks;
    hubStore.setConnectionStateForTest("live");
    expect(await hubStore.send(I, "g28 d C", [], [], "steer")).toBe(false);
    await pump(500);
    // C is issued; take the Web Locks back DOWN before the reload so the new
    // page's reconnect flush cannot deliver P itself (the latch + failed
    // locks are the only thing standing between P and delivery here).
    locks = bad;
    const cId = send.mock.calls[0]?.[4] as string;
    const lifeBeforeReload = g28Row(cId)?.issuerLifetime;
    expect(lifeBeforeReload).toBeTruthy();
    expect(g28Row(cId)?.state).toBe("reconciling");
    expect(g28Row(cId)?.progressRecorded).toBeFalsy();

    // RELOAD of the same login: drop the in-memory outbox and re-init WITHOUT
    // clearing the session/epoch (unlike logout).
    const reloader = hubStore as unknown as {
      outbox: unknown;
      outboxInit: unknown;
      connection: { dispose: () => void } | null;
      connectionBoundTo: unknown;
      connectionBoundJournal: unknown;
      connectionBindGen: number;
      initConnection: (offline?: boolean) => Promise<void>;
    };
    // Drop the connection machine too so initConnection rebuilds it (a reload).
    // Same login, but the new page gets a NEW outbox lifetime.
    reloader.connection?.dispose();
    reloader.connection = null;
    reloader.connectionBoundTo = null;
    reloader.connectionBoundJournal = null;
    reloader.connectionBindGen = 0;
    reloader.outbox = null;
    reloader.outboxInit = null;
    // A real reload destroys the previous JS context (old reconciliation loop
    // and outbox) outright; in-process we drop its in-flight marker so the new
    // page starts the GET loop bound to the NEW outbox (new lifetime). The
    // orphaned old loop (if its parked GET resolves) is rejected by the origin
    // check.
    (hubStore as unknown as { reconcilingInFlight: Set<string> }).reconcilingInFlight.delete(cId);
    await reloader.initConnection();
    // Let the recreated connection machine certify live against the mocked
    // Hub (probe + ready follow) as it would on a real reload.
    await pump(3_000);

    // The restored GET reconciliation reports accepted: the sent patch lands,
    // but the lifetime CAS fails — the new page is a different outbox.
    releaseGet(g25Status(cId, "accepted"));
    await pump(3_000);
    await vi.advanceTimersByTimeAsync(2_000);
    expect(send.mock.calls.map((c) => c[4])).toEqual([cId]); // P never POSTed
    expect(g28Row(cId)?.state).toBe("sent");
    expect(g28Row(cId)?.progressRecorded).toBeFalsy(); // not credited
    expect(g28Row(cId)?.issuerLifetime).toBe(lifeBeforeReload);
    // P stays pending with the locks down: the rejected CAS ran no epilogue,
    // so no wake was re-armed that could deliver it (in-memory exhaustion is
    // per-page and intentionally not carried across a reload; the durable flag
    // above is the single-writer guarantee).
    expect(g28Row(pId)?.state).toBe("pending");
    expect(send.mock.calls.map((c) => c[4])).toEqual([cId]);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE29 item 2: one acceptance CAS storage failure keeps the acceptance pending — the bounded retry flips it once and delivers the older row", async () => {
  // The Hub accepts the steer (POST verdict) but the FIRST durable CAS write
  // fails. Pre-gate-29 the error was swallowed and the acceptance was lost for
  // good (GET loop done, journal never replays); post-fix the observed
  // acceptance retries on the 250 ms bounded chain, flips exactly once and the
  // fresh epilogue delivers the exhausted older row.
  vi.useFakeTimers();
  vi.setSystemTime(1_060_000);
  const I = "ins_g29_i2";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g29 i2 locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = bad;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g29_i2");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    const boxHandle = () =>
      hubStore as unknown as {
        outbox: {
          storage: { casProgressRecorded: (id: string, lifetime: string) => Promise<boolean> };
          isDeliveryExhausted: (i: string) => boolean;
        } | null;
      };
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];

    // Exhaust P with the locks down.
    hubStore.setConnectionStateForTest("offline");
    expect(await hubStore.send(I, "g29 i2 P")).toBe(true);
    await pump(200);
    const pId = storedRows().find((r) => r.prompt === "g29 i2 P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxHandle().outbox!.isDeliveryExhausted(I)).toBe(true);

    // First CAS write fails once; the second delegates to the real backend.
    const storage = boxHandle().outbox!.storage;
    const realCas = storage.casProgressRecorded.bind(storage);
    // CASes of the steer (any id but P); the first steer CAS fails once.
    let casCalls = 0;
    storage.casProgressRecorded = vi.fn(async (id: string, lifetime: string) => {
      if (id !== pId) {
        casCalls += 1;
        if (casCalls === 1) throw new Error("g29 i2 CAS write aborted");
      }
      return await realCas(id, lifetime);
    });

    // The accepted steer records progress; the CAS fails once.
    locks = good;
    hubStore.setConnectionStateForTest("live");
    expect(await hubStore.send(I, "g29 i2 S", [], [], "steer")).toBe(true);
    const sId = send.mock.calls[0]?.[4] as string;
    await pump(1_000);
    expect(casCalls).toBe(1);
    // Failed first attempt: still exhausted, P not delivered yet.
    expect(boxHandle().outbox!.isDeliveryExhausted(I)).toBe(true);
    expect(send).toHaveBeenCalledTimes(1);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("pending");

    // The bounded retry fires after the 250 ms backoff: second CAS commits.
    await vi.advanceTimersByTimeAsync(300);
    for (let i = 0; i < 400 && send.mock.calls.length < 2; i += 1) {
      await vi.advanceTimersByTimeAsync(10);
      await Promise.resolve();
    }
    expect(casCalls).toBe(2);
    expect(send.mock.calls.map((c) => c[4])).toEqual([sId, pId]);
    expect(storedRows().find((r) => r.commandId === sId)?.progressRecorded).toBe(true);
    expect(storedRows().find((r) => r.commandId === pId)?.state).toBe("sent");
    expect(boxHandle().outbox!.isDeliveryExhausted(I)).toBe(false);

    // Exactly once: no further CAS retries or P re-POSTs.
    await vi.advanceTimersByTimeAsync(10_000);
    expect(casCalls).toBe(2);
    expect(send.mock.calls.filter((c) => c[4] === pId)).toHaveLength(1);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});it("GATE31 item 5: the issuing tab records the accepted command exactly once; the foreign same-epoch tab's rejection leaves its exhaustion intact even after the issuer succeeds", async () => {
  // Two page lifetimes, one auth epoch, one backend. The ISSUER A sends steer
  // S; its first progress CAS fails and its bounded retry is pending. Tab B
  // (different lifetime, S in cache) has independently exhausted its own
  // instance on Q. While the flag is still unset B receives S's acceptance —
  // it is rejected (lifetime mismatch) and its exhaustion stands. Then A's
  // retry commits: the flag flips exactly once (the issuer records), S is
  // delivered once, and B's exhaustion STILL survives because A's epilogue is
  // local to A.
  vi.useFakeTimers();
  vi.setSystemTime(1_080_000);
  const I = "ins_g31_i5";
  const originalNavigator = globalThis.navigator;
  const good = fifoWebLocks().locks;
  const bad = {
    query: async () => ({ held: [] as Array<{ name: string }> }),
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    request: (): Promise<any> => Promise.reject(new Error("g31 i5 locks down")),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let locks: { query: () => Promise<unknown>; request: (...args: any[]) => Promise<unknown> } = good;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { get locks() { return locks; } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  const rowsOf = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
  try {
    // Issuer A boots and fails its FIRST CAS once; later attempts commit.
    sessionStorage.setItem("remuda-outbox-owner", "owner_g31_i5_a");
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(apiA, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(apiA, "eventsSubscribe").mockResolvedValue({ ...g26Subscription(), subscriptionId: "sub_g31_i5" } as never);
    const sendA = vi.spyOn(apiA, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(apiA, a);
    await (a as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await a.follow(I);
    const aStorage = (a as unknown as { outbox: { storage: { casProgressRecorded: (id: string, l: string) => Promise<boolean> } } }).outbox.storage;
    const realCasA = aStorage.casProgressRecorded.bind(aStorage);
    let aCasCalls = 0;
    // First CAS rejects (creates the pending retry); EVERY later CAS parks
    // until releaseA() — so the issuer retry cannot commit during B's phase.
    let releaseA: () => void = () => undefined;
    const aGate = new Promise<void>((resolve) => {
      releaseA = () => resolve();
    });
    aStorage.casProgressRecorded = vi.fn(async (id: string, l: string) => {
      aCasCalls += 1;
      if (aCasCalls === 1) throw new Error("g31 i5 A CAS first failure");
      await aGate;
      return await realCasA(id, l);
    });
    a.setConnectionStateForTest("live");
    expect(await a.send(I, "g31 i5 S", [], [], "steer")).toBe(true);
    await pump(1_000);
    const sId = sendA.mock.calls[0]?.[4] as string;
    const sRow = () => rowsOf().find((r) => r.commandId === sId)!;
    expect(sRow().state).toBe("sent");
    expect(sRow().progressRecorded).toBeFalsy(); // pending the bounded retry
    expect(aCasCalls).toBe(1);
    const issuerLife = sRow().issuerLifetime;

    // Foreign tab B boots (S loads into cache) and independently exhausts its
    // own instance on Q with the locks down.
    sessionStorage.setItem("remuda-outbox-owner", "owner_g31_i5_b");
    const { api: apiB, hubStore: b } = await fresh();
    vi.spyOn(apiB, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(apiB, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(apiB, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(apiB, "eventsSubscribe").mockResolvedValue({ ...g26Subscription(), subscriptionId: "sub_g31_i5_b" } as never);
    const sendB = vi.spyOn(apiB, "instanceSend");
    await bootLive(apiB, b);
    await (b as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await b.follow(I);
    const subscribeB = vi.mocked(apiB.eventsSubscribe);
    for (let i = 0; i < 2_000 && !subscribeB.mock.calls[0]?.[2]; i += 1) await Promise.resolve();
    const onBatchB = subscribeB.mock.calls[0]?.[2] as (b2: G26Batch) => void;
    const bBox = () => (b as unknown as { outbox: { isDeliveryExhausted: (i: string) => boolean; pendingLeaseWakeups: () => string[] } | null }).outbox;

    locks = bad;
    b.setConnectionStateForTest("offline");
    expect(await b.send(I, "g31 i5 Q")).toBe(true);
    await pump(200);
    const qId = rowsOf().find((r) => r.prompt === "g31 i5 Q")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(bBox()!.isDeliveryExhausted(I)).toBe(true);

    // B receives S's acceptance BEFORE A's retry fires (well inside the 250 ms
    // backoff). Lifetime mismatch → CAS not flipped, B's latch intact.
    onBatchB({
      subscriptionId: "sub_g31_i5_b",
      journalId: JOURNAL,
      fromSeq: "1",
      toSeq: "1",
      durableSeq: "1",
      events: [g26JournalMessage(sId, "1")],
    });
    await pump(1_000);
    expect(sRow().progressRecorded).toBeFalsy();
    expect(bBox()!.isDeliveryExhausted(I)).toBe(true);
    expect(sendB).not.toHaveBeenCalled();

    // Let the bounded retry timer fire (it parks on the gate), then release
    // the ISSUER's CAS: the flag flips exactly once.
    await vi.advanceTimersByTimeAsync(300);
    await pump(500);
    expect(aCasCalls).toBe(2); // the parked retry attempt is now in flight
    releaseA();
    await pump(1_000);
    expect(aCasCalls).toBe(2); // one failure + one success, no more
    expect(sRow().progressRecorded).toBe(true);
    expect(sRow().issuerLifetime).toBe(issuerLife);
    expect(sendA.mock.calls.map((c) => c[4])).toEqual([sId]); // delivered once

    // A's success must NOT have crossed tabs: B is still exhausted, Q still
    // pending, B never POSTed, and a replayed batch at B is still a no-op.
    expect(bBox()!.isDeliveryExhausted(I)).toBe(true);
    expect(bBox()!.pendingLeaseWakeups()).toEqual([]);
    expect(rowsOf().find((r) => r.commandId === qId)?.state).toBe("pending");
    expect(sendB).not.toHaveBeenCalled();
    onBatchB({
      subscriptionId: "sub_g31_i5_b",
      journalId: JOURNAL,
      fromSeq: "2",
      toSeq: "2",
      durableSeq: "2",
      events: [g26JournalMessage(sId, "2")],
    });
    await pump(500);
    expect(sendB).not.toHaveBeenCalled();
    // No third CAS for S anywhere.
    expect(aCasCalls).toBe(2);
    a.logout();
    b.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});;

it("GATE32 item 1a: logout/reinit disposes the Outbox — the new outbox has a fresh lifetime and an old command's acceptance never credits it", async () => {
  // The same PAGE logs out and reinits: the disposed outbox's lifetime dies,
  // the new outbox gets a different lifetime. A command accepted after the
  // change (journal arrives on the new page) retires to done but its CAS
  // compares lifetimes and never flips, so the new outbox's own exhausted
  // instance is untouched.
  vi.useFakeTimers();
  vi.setSystemTime(1_090_000);
  const I = "ins_g32_a";
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: shim.locks },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g32_a");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend");
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await hubStore.follow(I);
    const onBatch = vi.mocked(api.eventsSubscribe).mock.calls[0]?.[2] as (b: G26Batch) => void;
    const boxOf = () => (hubStore as unknown as { outbox: { lifetime: string; isDeliveryExhausted: (i: string) => boolean; pendingLeaseWakeups: () => string[] } | null }).outbox;

    // Old outbox issues C; the POST hangs (issued, never accepted).
    send.mockImplementation(
      (() => new Promise(() => { /* hung: issued, not accepted */ })) as Api["instanceSend"],
    );
    hubStore.setConnectionStateForTest("live");
    await hubStore.send(I, "g32 a C");
    await pump(2_000);
    const rows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    const cId = send.mock.calls[0]?.[4] as string;
    const oldLife = boxOf()!.lifetime;
    expect(rows().find((r) => r.commandId === cId)?.issuerLifetime).toBe(oldLife);

    // Logout then reinit (same page): a NEW outbox with a NEW lifetime.
    hubStore.logout();
    const reopened = await (hubStore as unknown as { ensureOutbox: () => Promise<boolean> }).ensureOutbox();
    expect(reopened).toBe("ok");
    const newLife = boxOf()!.lifetime;
    expect(newLife).not.toBe(oldLife);

    // New outbox independently exhausts its own instance on P (locks down).
    const bad = { query: async () => ({ held: [] as Array<{ name: string }> }), request: () => Promise.reject(new Error("g32 a locks")) as Promise<never> };
    let locks: unknown = shim.locks;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      writable: true,
      value: { get locks() { return locks; } },
    });
    locks = bad;
    hubStore.setConnectionStateForTest("offline");
    expect(await hubStore.send(I, "g32 a P")).toBe(true);
    await pump(200);
    const pId = rows().find((r) => r.prompt === "g32 a P")!.commandId;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);

    // The old command's accepted journal settle arrives on the NEW page:
    // done patch retires C, but the lifetime CAS fails — no credit.
    onBatch({
      subscriptionId: "sub_g32",
      journalId: JOURNAL,
      fromSeq: "1",
      toSeq: "1",
      durableSeq: "1",
      events: [g26JournalMessage(cId, "1")],
    });
    await pump(2_000);
    expect(rows().find((r) => r.commandId === cId)?.state).toBe("done");
    expect(rows().find((r) => r.commandId === cId)?.progressRecorded).toBeFalsy();
    expect(rows().find((r) => r.commandId === cId)?.issuerLifetime).toBe(oldLife);
    expect(boxOf()!.isDeliveryExhausted(I)).toBe(true);
    expect(rows().find((r) => r.commandId === pId)?.state).toBe("pending");
    expect(send).toHaveBeenCalledTimes(1);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE32 item 1b: a peer tab's session removal observed via storage disposes THIS outbox — its pending acceptance retry never records", async () => {
  // One store with a pending acceptance CAS retry. Another document removes
  // the device session (logout in a sibling tab) → the storage event disposes
  // the outbox BEFORE the retry fires, so the retry sees a disposed/non-live
  // outbox and the flag never flips.
  vi.useFakeTimers();
  vi.setSystemTime(1_091_000);
  const I = "ins_g32_b";
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: shim.locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g32_b");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await hubStore.follow(I);
    hubStore.setConnectionStateForTest("live");

    // Fail the first CAS once → bounded retry is scheduled (fires at 250 ms).
    const storage = (hubStore as unknown as { outbox: { storage: { casProgressRecorded: (id: string, l: string) => Promise<boolean> } } }).outbox.storage;
    const realCas = storage.casProgressRecorded.bind(storage);
    let casCalls = 0;
    storage.casProgressRecorded = vi.fn(async (id: string, l: string) => {
      casCalls += 1;
      if (casCalls === 1) throw new Error("g32 b CAS first failure");
      return realCas(id, l);
    });
    expect(await hubStore.send(I, "g32 b S", [], [], "steer")).toBe(true);
    await pump(1_000);
    const cId = send.mock.calls[0]?.[4] as string;
    const rows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(rows().find((r) => r.commandId === cId)?.progressRecorded).toBeFalsy();
    expect(casCalls).toBe(1);

    // A SIBLING tab logs out (storage events fire only in other documents):
    // the device session key is replaced/cleared.
    window.dispatchEvent(
      new StorageEvent("storage", { key: "runtime.device-session", oldValue: '{"deviceId":"x"}', newValue: null }),
    );
    // The outbox is disposed by the peer-auth watcher.
    expect((hubStore as unknown as { outbox: unknown }).outbox).toBeNull();

    // Advance past every bounded retry: no retry CAS from the disposed outbox.
    await vi.advanceTimersByTimeAsync(120_000);
    await pump(500);
    expect(casCalls).toBe(1);
    expect(rows().find((r) => r.commandId === cId)?.progressRecorded).toBeFalsy();
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE33 item 1: every session mutation (logout) bumps the auth generation and disposes the current Outbox — a new outbox gets a fresh lifetime and an old acceptance never credits it", async () => {
  // Public-path assertion of the single identity-change helper: with a live
  // outbox (issuer lifetime L1), logout must bump authGen and dispose it; the
  // post-logout ensureOutbox creates a NEW outbox (L2). A durable command
  // issued under L1 that settles after the change never flips L2's flag.
  vi.useFakeTimers();
  vi.setSystemTime(1_100_000);
  const I = "ins_g33_1";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: fifoWebLocks().locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g33_1");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (() => new Promise(() => { /* hung: issued, never accepted */ })) as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await hubStore.follow(I);
    const boxAt = () => (hubStore as unknown as { outbox: { lifetime: string } | null }).outbox;
    const genAt = () => (hubStore as unknown as { authGenForTest: number }).authGenForTest;
    const l1 = boxAt()!.lifetime;
    const gen1 = genAt();

    hubStore.setConnectionStateForTest("live");
    await hubStore.send(I, "g33 1 C");
    await pump(2_000);
    const cId = send.mock.calls[0]?.[4] as string;
    const rows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(rows().find((r) => r.commandId === cId)?.issuerLifetime).toBe(l1);

    // logout: the SINGLE identity-change helper bumps gen and disposes.
    hubStore.logout();
    expect(genAt()).toBe(gen1 + 1);
    expect(boxAt()).toBeNull();

    // reinit under the new identity: a fresh outbox/lifetime.
    const reopened = await (hubStore as unknown as { ensureOutbox: () => Promise<boolean> }).ensureOutbox();
    expect(reopened).toBe("ok");
    const l2 = boxAt()!.lifetime;
    expect(l2).not.toBe(l1);

    // L1 command settling after the change retires to done but does not flip
    // L2's progressRecorded.
    const onBatch = vi.mocked(api.eventsSubscribe).mock.calls[0]?.[2] as (b: G26Batch) => void;
    onBatch({
      subscriptionId: "sub_g33_1", journalId: JOURNAL, fromSeq: "1", toSeq: "1", durableSeq: "1",
      events: [g26JournalMessage(cId, "1")],
    });
    await pump(2_000);
    expect(rows().find((r) => r.commandId === cId)?.state).toBe("done");
    expect(rows().find((r) => r.commandId === cId)?.progressRecorded).toBeFalsy();
    expect(rows().find((r) => r.commandId === cId)?.issuerLifetime).toBe(l1);
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE33 item 2: an Outbox initializer whose auth generation goes stale before it finishes installs NO box and flushes NOTHING; the new identity gets a fresh outbox", async () => {
  // Start ensureOutbox with a gate holding the storage-open, change identity
  // (helper bumps gen + invalidates) before release, then let the old
  // initializer finish: it must dispose its box, leave this.outbox null and
  // not flush. A fresh ensureOutbox under the current gen installs a new box.
  vi.useFakeTimers();
  vi.setSystemTime(1_110_000);
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: fifoWebLocks().locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 200) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g33_2");
    const { hubStore } = await fresh();
    const h = hubStore as unknown as {
      outbox: { lifetime: string } | null;
      outboxInit: Promise<boolean> | null;
      authGenForTest: number;
      setOutboxOpenGateForTest: (g: ((r: () => void) => void) | null) => void;
      applyAuthIdentityChangeForTest: () => void;
      ensureOutbox: () => Promise<boolean>;
      flushAllOutbox: () => Promise<void>;
    };

    // Gate the FIRST initializer before its storage-open resolves.
    let releaseOld: () => void = () => undefined;
    h.setOutboxOpenGateForTest((resolve) => {
      releaseOld = resolve;
    });
    const firstInit = h.ensureOutbox();
    await pump(50);
    expect(h.outbox).toBeNull(); // still opening

    // Identity changes while the initializer is in flight: gen bumps, the
    // pending init is invalidated (outboxInit cleared).
    h.applyAuthIdentityChangeForTest();
    const genAfter = h.authGenForTest;
    expect(h.outboxInit).toBeNull();

    // Let the obsolete initializer finish: it must install nothing.
    releaseOld();
    await expect(firstInit).resolves.toBe("obsolete");
    await pump(100);
    expect(h.outbox).toBeNull();

    // The CURRENT identity's initializer (no gate) installs a fresh outbox.
    h.setOutboxOpenGateForTest(null);
    const ok = await h.ensureOutbox();
    expect(ok).toBe("ok");
    expect(h.outbox).not.toBeNull();
    expect(h.authGenForTest).toBe(genAfter);
    // It is a live, usable outbox (a flush runs without throwing).
    await h.flushAllOutbox();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE34 item 1: once a public send() commits C durably, an auth disposal before the POST never falls back to an unkeyed direct POST — C is delivered at most once, by the current box", async () => {
  vi.useFakeTimers();
  vi.setSystemTime(1_120_000);
  const I = "ins_g34_1";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: fifoWebLocks().locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g34_1");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    await hubStore.follow(I);
    // Start live just long enough to boot, then go offline so the pending row
    // is never auto-flushed before the login transition.
    hubStore.setConnectionStateForTest("live");
    const h0 = hubStore as unknown as { flushAllOutbox: () => Promise<void> };
    await h0.flushAllOutbox();
    hubStore.setConnectionStateForTest("offline");
    const h = hubStore as unknown as {
      logout: () => void;
      send: (i: string, p: string) => Promise<boolean>;
      ensureOutbox: () => Promise<string>;
      setOutboxEnqueueGateForTest: (g: ((r: () => void) => void) | null) => void;
      flushAllOutbox: () => Promise<void>;
    };

    // Gate the durable enqueue write so send() parks after it minted C but
    // before C's storage.put commits.
    let releaseEnqueue: () => void = () => undefined;
    let enqueueStarted = false;
    h.setOutboxEnqueueGateForTest((resolve) => {
      enqueueStarted = true;
      releaseEnqueue = resolve;
    });
    const sendPaused = h.send(I, "g34 1 C");
    for (let i = 0; i < 2_000 && !enqueueStarted; i += 1) await Promise.resolve();
    expect(enqueueStarted).toBe(true);

    // Auth identity changes while C's commit is in flight.
    h.logout();
    releaseEnqueue(); // C now commits into durable storage
    const result = await sendPaused;
    // The committed command must NOT take the direct unkeyed fallback: the
    // action does not execute from this call.
    expect(result).toBe(false);
    h.setOutboxEnqueueGateForTest(null);
    await pump(500);
    expect(send).not.toHaveBeenCalled();

    // Re-auth implicitly via a fresh outbox (simulating the post-change page):
    // ensureOutbox installs the current-generation box and the flush delivers
    // the committed C EXACTLY once, carrying its own command id.
    const status = await h.ensureOutbox();
    expect(status).toBe("ok");
    await h.flushAllOutbox();
    await pump(2_000);
    expect(send).toHaveBeenCalledTimes(1);
    const postedId = send.mock.calls[0]?.[4];
    expect(postedId?.startsWith("cmd_")).toBe(true);
    const rows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(rows().some((r) => r.commandId === postedId)).toBe(true);
    // Never a second POST.
    await h.flushAllOutbox();
    await pump(500);
    expect(send).toHaveBeenCalledTimes(1);
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});;

it("GATE34 item 2 (Fence B): a flush delivery whose box is disposed while awaiting its claim never POSTs; the row is released and delivered once by the current box", async () => {
  vi.useFakeTimers();
  vi.setSystemTime(1_130_000);
  const I = "ins_g34_2";
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: shim.locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g34_2");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" },
      })) as unknown as Api["instanceSend"],
    );
    await bootLive(api, hubStore);
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await hubStore.follow(I);
    hubStore.setConnectionStateForTest("live");

    // Enqueue a pending row but make the CLAIM write block at the storage
    // backend, so the delivery parks inside deliverOutboxRecord awaiting the
    // claim.
    const outbox0 = (hubStore as unknown as { outbox: { storage: Record<string, unknown> } }).outbox;
    const storage = outbox0.storage as {
      mergeUnlessDone: (id: string, p: unknown) => Promise<unknown>;
      all: () => Promise<OutboxRecord[]>;
    };
    const realMerge = storage.mergeUnlessDone.bind(storage);
    let commandId = "";
    let releaseClaim: () => void = () => undefined;
    let claimHeld = false;
    storage.mergeUnlessDone = vi.fn(async (id: string, patch: unknown) => {
      if (!claimHeld && (patch as { state?: string }).state === "inflight") {
        claimHeld = true;
        await new Promise<void>((resolve) => { releaseClaim = resolve; });
      }
      return realMerge(id, patch);
    });
    expect(await hubStore.send(I, "g34 2 C")).toBe(true);
    await pump(500);
    for (let i = 0; i < 2_000 && !claimHeld; i += 1) await Promise.resolve();
    expect(claimHeld).toBe(true);
    commandId = send.mock.calls.length ? "" : commandId; // no POST yet
    const rows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    commandId = rows().find((r) => r.prompt === "g34 2 C")!.commandId;
    expect(send).not.toHaveBeenCalled(); // parked at the claim, pre-POST

    // Dispose the originating box WHILE the claim await is parked (auth change).
    (hubStore as unknown as { applyAuthIdentityChangeForTest: () => void }).applyAuthIdentityChangeForTest();
    releaseClaim(); // the claim continuation now resumes on a disposed box
    await pump(2_000);
    // Fence B: L1's continuation must NOT POST even though the claim completed.
    expect(send).not.toHaveBeenCalled();

    // The current-generation box claims the released row (released by L1's
    // finally) and delivers it exactly once.
    const ok = await (hubStore as unknown as { ensureOutbox: () => Promise<string> }).ensureOutbox();
    expect(ok).toBe("ok");
    await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await pump(3_000);
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[4]).toBe(commandId);
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE34 item 3 (Fence A): during a login whose body is delayed, no old-lifetime outbox can POST; a fresh box is initialized only after the body settles (success or failure)", async () => {
  vi.useFakeTimers();
  vi.setSystemTime(1_140_000);
  const I = "ins_g34_3";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: fifoWebLocks().locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g34_3");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" },
      })) as unknown as Api["instanceSend"],
    );
    // hello succeeds (cookie session) for the pre-login boot.
    vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
    vi.spyOn(api, "instanceList").mockResolvedValue({ items: [] } as never);
    vi.spyOn(api, "hostList").mockResolvedValue({ items: [] } as never);
    vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
    vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as never);
    vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as never);
    await bootLive(api, hubStore);
    await hubStore.follow(I);
    // Start live just long enough to boot, then go offline so the pending row
    // is never auto-flushed before the login transition.
    hubStore.setConnectionStateForTest("live");
    const h0 = hubStore as unknown as { flushAllOutbox: () => Promise<void> };
    await h0.flushAllOutbox();
    hubStore.setConnectionStateForTest("offline");
    const h = hubStore as unknown as {
      login: (k: "bootstrap", s: string, d: string) => Promise<void>;
      ensureOutbox: () => Promise<string>;
      flushAllOutbox: () => Promise<void>;
      authTransitionOpenForTest: boolean;
      outbox: { lifetime: string } | null;
    };
    const oldLife = h.outbox!.lifetime;

    // Delay the login BODY (headers would already have set the new cookie).
    let releaseLogin: () => void = () => undefined;
    vi.spyOn(api, "login").mockImplementation(
      () => new Promise((resolve) => {
        releaseLogin = () =>
          resolve({ deviceId: "dev_g34_3_new", token: "tok_new", name: "new" });
      }),
    );

    // Enqueue a pending row on the OLD outbox while OFFLINE (so it stays
    // pending — nothing POSTs yet), then start the login (which disposes the
    // old box and blocks init).
    const obx = (hubStore as unknown as { outbox: { enqueue: (r: unknown) => Promise<unknown> } }).outbox;
    await obx.enqueue({
      commandId: "cmd_g34_3_p", clientRequestId: "local_g34_3_p", instanceId: I,
      prompt: "g34 3 P", createdAt: Date.now(),
    });
    await pump(200);
    expect(send).not.toHaveBeenCalled();
    const loginP = h.login("bootstrap", "any-secret", "dev");
    await pump(200); // enter runAuthTransition; credential request parked
    // The old box is disposed and the transition is open.
    expect(h.authTransitionOpenForTest).toBe(true);

    // ensureOutbox must BLOCK during the transition — it cannot resolve to an
    // installed box while the credential body is pending.
    let ensureSettled = false;
    const ensureP = h.ensureOutbox().then((r) => { ensureSettled = true; return r; });
    await pump(1_000);
    expect(ensureSettled).toBe(false);

    // Old lifetime must not POST during the window even if its timer fires.
    await vi.advanceTimersByTimeAsync(120_000);
    await pump(500);
    expect(send).not.toHaveBeenCalled();

    // Body settles successfully: transition releases, ensure resolves with a
    // FRESH outbox that delivers the pending row once.
    releaseLogin();
    await loginP;
    await pump(1_000);
    await expect(ensureP).resolves.toBe("ok");
    expect(h.outbox).not.toBeNull();
    expect(h.outbox!.lifetime).not.toBe(oldLife);
    await h.flushAllOutbox();
    await pump(2_000);
    expect(send).toHaveBeenCalledTimes(1);

    // ---- Failed body: the blocker still releases and a box initializes ----
    let failLogin: () => void = () => undefined;
    vi.spyOn(api, "login").mockImplementation(
      () => new Promise((_resolve, reject) => {
        failLogin = () => reject(new Error("login body failed"));
      }),
    );
    const failedLogin = h.login("bootstrap", "bad", "dev2");
    await pump(200);
    expect(h.authTransitionOpenForTest).toBe(true);
    let ensure2 = false;
    const p2 = h.ensureOutbox().then((r) => { ensure2 = true; return r; });
    await pump(500);
    expect(ensure2).toBe(false); // blocked during the failed transition
    failLogin();
    await expect(failedLogin).rejects.toThrow("login body failed");
    await expect(p2).resolves.toBeDefined(); // blocker released; init can run
    expect(ensure2).toBe(true);
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE34 item 4: a replacement login that REUSES the existing connection still initializes a current outbox and flushes a pending row automatically (no second send/reload)", async () => {
  vi.useFakeTimers();
  vi.setSystemTime(1_150_000);
  const I = "ins_g34_4";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: fifoWebLocks().locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    sessionStorage.setItem("remuda-outbox-owner", "owner_g34_4");
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(api, "instanceGet").mockResolvedValue({ id: I, journalId: JOURNAL } as never);
    vi.spyOn(api, "eventsSubscribe").mockResolvedValue(g26Subscription() as never);
    const send = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_i: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => ({
        relatedCommandIds: [],
        command: { commandId, id: commandId, state: "accepted", revision: "1", dispatch: "native-acknowledged", resolution: "clear" },
      })) as unknown as Api["instanceSend"],
    );
    vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
    vi.spyOn(api, "instanceList").mockResolvedValue({ items: [] } as never);
    vi.spyOn(api, "hostList").mockResolvedValue({ items: [] } as never);
    vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
    vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as never);
    vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as never);
    await bootLive(api, hubStore);
    await hubStore.follow(I);
    hubStore.setConnectionStateForTest("live");
    const h = hubStore as unknown as {
      login: (k: "bootstrap", s: string, d: string) => Promise<void>;
      flushAllOutbox: () => Promise<void>;
      connection: unknown;
      outbox: { lifetime: string } | null;
    };
    // The connection machine exists and is reused by a replacement login.
    expect(h.connection).not.toBeNull();
    const oldLife = h.outbox!.lifetime;

    // Pending row, enqueued live but held back from auto-flush by going
    // offline right before the login transition.
    hubStore.setConnectionStateForTest("offline");
    const obx = (hubStore as unknown as { outbox: { enqueue: (r: unknown) => Promise<unknown> } }).outbox;
    await obx.enqueue({
      commandId: "cmd_g34_4_p", clientRequestId: "local_g34_4_p", instanceId: I,
      prompt: "g34 4 P", createdAt: Date.now(),
    });
    await pump(200);
    expect(send).not.toHaveBeenCalled();

    // Replacement login resolves immediately (new cookie), reusing the
    // connection. The delayed-body blocker is not needed: runAuthTransition
    // disposes, login returns, bootstrap sees this.connection and (before the
    // fix) returned without an outbox. After the fix it initializes the
    // current box and the post-boot live state flushes the pending row.
    vi.spyOn(api, "login").mockResolvedValue({ deviceId: "dev_g34_4_new", token: "tok_new", name: "new" });
    await h.login("bootstrap", "secret", "dev");
    await pump(3_000);
    // No manual second send or flush: the replacement login's init serviced
    // the row automatically.
    expect(h.outbox).not.toBeNull();
    expect(h.outbox!.lifetime).not.toBe(oldLife);
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[4]).toBe("cmd_g34_4_p");
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});
