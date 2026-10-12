import { afterEach, expect, it, vi } from "vitest";
import type { ConnectionMachine } from "./connection";
import { REST_PROBE_MS } from "./connection";
import { OUTBOX_LS_KEY, type OutboxRecord } from "./outbox";

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
    expect(await internal.ensureOutbox()).toBe(true);
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
    expect(await internal.ensureOutbox()).toBe(true);
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

it("GATE22 item 4 (rebuilds gate-21 item-1a): two real stores — B blocked past 250 ms, A's wake-driven flush loses its suppressed IX read with the IY turn queued and recovery consumed covered; after release only the scoped epilogue re-arms R; real flows deliver every row", async () => {
  // Rebuild of the gate-21 item-1a regression per RULES. The old outbox.test
  // version drove bare Outboxes by hand (manual beginDeliveryPass /
  // endDeliveryPasses, a hand-held lock, a vi.fn handoff) and stopped with A's
  // row still PENDING. Here TWO real HubStores share one localStorage backend
  // behind the FIFO Web Locks shim and every transition is a real delivery
  // flow. The harness gates only WHEN flushes START (timing, never storage):
  // the two SEND flushes and every wake flush but the first park; the FIRST
  // wake flush (R's own registered deliver wake) runs for real and is the
  // flush under test:
  //  - B's real POST for S (IY) blocks inside the instance lock;
  //  - A enqueues R (IX); R's wake fires (T does not exist yet, so the fire
  //    reconciles no other row) and its guarded flush starts ONLY after T is
  //    enqueued: snapshot [R,T pending, S live-inflight by B], pass covers
  //    IX+IY; the suppressed in-lock read on IX aborts while S is unsent
  //    (fault at A's real storage adapter) and the IY turn QUEUES behind B —
  //    flush and scoped suppression stay alive. T's own wake fires LATER,
  //    while IX is already covered, so its reconcile cannot cross-arm R (the
  //    discovery branch skips covered instances), and that wake flush parks;
  //  - fake time advances 400 ms while B is blocked: the bounded recovery
  //    read is consumed under the live suppression, arms nothing, chain ends;
  //  - the user REALLY retracts queued T while the IY turn is queued; after B
  //    is released the queued turn finds T gone and posts NOTHING, so the
  //    flush ends without another pass. With the gate-21 epilogue REMOVED
  //    nothing re-reads: R strands; real code's scoped epilogue arms R's one
  //    bounded wake;
  //  - B's own drain claims R and fails it at the API boundary (POST + status
  //    GET), restores R lease-less pending, and closes between passes;
  //  - A's REAL epilogue-registered timer is the only remaining trigger:
  //    same-command delivery of R. Final calls exactly B:[S, R-failed]
  //    A:[R]; S and R durably sent, T deleted by the real retract.
  vi.useFakeTimers();
  vi.setSystemTime(900_000);
  const ownerA = "owner_g22_i4_a";
  const ownerB = "owner_g22_i4_b";
  const IX = "ins_g22_i4_x";
  const IY = "ins_g22_i4_y";
  const originalNavigator = globalThis.navigator;
  const shim = fifoWebLocks();
  const realQuery = shim.locks.query;
  shim.locks.query = async () => {
    throw new Error("locks.query unavailable during load");
  };
  Object.defineProperty(globalThis, "navigator", { configurable: true, writable: true, value: { locks: shim.locks } });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  const lockName = (iid: string) => `remuda-outbox-${iid}`;
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
    vi.spyOn(apiB, "instanceCommandStatus").mockRejectedValue(new Error("status network down"));
    const sendB = vi.spyOn(apiB, "instanceSend");
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
    const sendA = vi.spyOn(apiA, "instanceSend");
    await bootLive(apiA, a);
    await (a as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    await (b as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
    vi.spyOn(Math, "random").mockReturnValue(0.999);

    // Flush-start gates: send-triggered flushes park; the FIRST wake-triggered
    // flush (R's own wake) runs on its own gate; every later flush parks until
    // the scenario is over.
    let resolveFirstWakeFlush: (() => void) | null = null;
    const firstWakeFlushGate = new Promise<void>((resolve) => {
      resolveFirstWakeFlush = resolve;
    });
    let resolveRecoveryFlush: (() => void) | null = null;
    const recoveryFlushGate = new Promise<void>((resolve) => {
      resolveRecoveryFlush = resolve;
    });
    let resolveParkedFlushes: (() => void) | null = null;
    const parkedFlushesGate = new Promise<void>((resolve) => {
      resolveParkedFlushes = resolve;
    });
    const aInternals = a as unknown as { flushAllOutbox: () => Promise<void> };
    const realFlushA = aInternals.flushAllOutbox.bind(aInternals);
    let wakeFlushes = 0;
    aInternals.flushAllOutbox = (() => {
      const fromWake = new Error("st").stack?.includes("fireLeaseWake");
      if (fromWake) {
        wakeFlushes += 1;
        // Ordinal choreography: #1 = R's first wake (the flush under test);
        // T's own wake is cancelled by the real retract, so #2 is R's
        // scoped-epilogue recovery wake; anything later parks to the end.
        if (wakeFlushes === 1) return firstWakeFlushGate.then(() => realFlushA());
        if (wakeFlushes === 2) return recoveryFlushGate.then(() => realFlushA());
      }
      return parkedFlushesGate.then(() => realFlushA());
    }) as typeof aInternals.flushAllOutbox;

    // Suppressed in-lock reads on IX abort while B's S is unsent; fire reads
    // (no suppression) and reads once S sent / outside IX pass untouched.
    // Park epilogue/recovery reads (anything not under a delivery lock and
    // not the flush's own snapshot read) between T's enqueue and the moment
    // flush 1's scoped cover is established — otherwise T's post-commit
    // epilogue reconciles BEFORE the flush suppresses and would cross-arm a
    // fresh R wake, exactly the redundant-trigger path the gate-21 hand test
    // excluded by construction.
    let parkEpilogueReads = false;
    let resolveEpilogueReads: (() => void) | null = null;
    let epilogueReadsGate: Promise<void> | null = null;
    const aBoxInternals = a as unknown as {
      outbox: {
        storage: { all: () => Promise<OutboxRecord[]> };
        deliverySuppressed: Map<string, number>;
      };
    };
    const realAllA = aBoxInternals.outbox.storage.all.bind(aBoxInternals.outbox.storage);
    let suppressedFaults = 0;
    aBoxInternals.outbox.storage.all = vi.fn((() => {
      const syncStack = new Error("st").stack ?? "";
      return (async () => {
        const held = (await shim.locks.query()).held.map((h) => h.name);
        // Park only post-commit / bounded-recovery reads (they enter via
        // attemptReadRecovery): the flush snapshot, the in-lock deliverer
        // read and the timer-fire read all proceed on their own timing.
        if (parkEpilogueReads && syncStack.includes("attemptReadRecovery") && epilogueReadsGate) {
          await epilogueReadsGate;
        }
        const sState = (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[]).find(
          (r) => r.prompt === "gate22 4 S",
        )?.state;
        if (held.includes(lockName(IX)) && sState !== "sent" && aBoxInternals.outbox.deliverySuppressed.has(IX)) {
          suppressedFaults += 1;
          throw new Error("g22-i4 suppressed in-lock read abort while sibling blocked");
        }
        return await realAllA();
      })();
    }) as typeof aBoxInternals.outbox.storage.all);

    let releaseB: (() => void) | null = null;
    const bGate = new Promise<void>((resolve) => {
      releaseB = resolve;
    });
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
    sendB.mockImplementation(
      (async (iid: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => {
        if (iid === IY && sendB.mock.calls.length === 1) {
          await bGate; // S: blocked holding IY
          return accepted(commandId ?? "cmd_x");
        }
        throw new Error("g22-i4 B boundary POST failure");
      }) as unknown as Api["instanceSend"],
    );
    sendA.mockImplementation(
      (async (_iid: string, _p: string, _x?: unknown, _m?: string, commandId?: string) =>
        accepted(commandId ?? "cmd_x")) as unknown as Api["instanceSend"],
    );

    // 1) B sends S: real claim, POST pending inside IY.
    void b.send(IY, "gate22 4 S");
    for (let i = 0; i < 2_000 && sendB.mock.calls.length === 0; i += 1) await Promise.resolve();
    const sId = sendB.mock.calls[0]?.[4] as string;
    expect(sId).toBeTruthy();

    // 2) A enqueues R; its send flush parks; R's deliver wake is registered.
    void a.send(IX, "gate22 4 R");
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    const rowOf = (prompt: string) => storedRows().find((r) => r.prompt === prompt);
    for (let i = 0; i < 2_000 && !rowOf("gate22 4 R"); i += 1) await Promise.resolve();
    const rId = rowOf("gate22 4 R")!.commandId;
    await pump(500);

    // 3) R's wake fires while T does not exist: its flush parks at the
    //    one-shot gate (nothing else is reconciled at that fire).
    await vi.advanceTimersByTimeAsync(1);
    for (let i = 0; i < 2_000 && wakeFlushes < 1; i += 1) await Promise.resolve();
    expect(wakeFlushes).toBe(1);

    // 4) T is enqueued NOW with epilogue reads parked; its send flush parks.
    //    Release R's wake flush: its own snapshot read passes the park (it is
    //    the flush's read), covers IX+IY, the IX read faults and the IY turn
    //    queues — THEN T's parked epilogue is released into a covered world,
    //    where its reconcile cannot cross-arm R.
    parkEpilogueReads = true;
    epilogueReadsGate = new Promise<void>((resolve) => {
      resolveEpilogueReads = resolve;
    });
    void a.send(IY, "gate22 4 T");
    for (let i = 0; i < 2_000 && !rowOf("gate22 4 T"); i += 1) await Promise.resolve();
    resolveFirstWakeFlush!();
    for (let i = 0; i < 5_000 && suppressedFaults < 1; i += 1) await Promise.resolve();
    expect(suppressedFaults).toBeGreaterThanOrEqual(1);
    resolveEpilogueReads!();
    parkEpilogueReads = false;
    await pump(3_000);
    const tBubble = a.getSnapshot().bubbles.find((x) => x.text === "gate22 4 T")!;
    expect(sendA).not.toHaveBeenCalled();
    expect(sendB).toHaveBeenCalledTimes(1);

    // 5) Past the bounded recovery delays while B is blocked, with T's own
    //    deliver wake still registered (fake time parked, no timer fires): the
    //    recovery reads are consumed under suppression and arm nothing.
    await vi.advanceTimersByTimeAsync(1_000);
    await pump(3_000);
    expect(sendB).toHaveBeenCalledTimes(1);
    expect(sendA).not.toHaveBeenCalled();
    expect((a as unknown as { outbox: { pendingLeaseWakeups: () => string[] } }).outbox.pendingLeaseWakeups()).not.toContain(rId);

    // 6) The user REALLY retracts queued T while the IY turn is queued: its
    //    registered wake is cancelled BEFORE it can fire, so no later fire
    //    reconciles R past the flush's end.
    await a.retract(tBubble.clientRequestId);
    expect(rowOf("gate22 4 T")).toBeUndefined();
    await pump(2_000);
    expect(sendA).not.toHaveBeenCalled();

    // 7) Release B: S settles; A's queued turn comes back empty and the flush
    //    ends (scoped epilogue is the ONLY remaining R cover); B's drain
    //    claims R and fails it at the boundary. Close B between passes.
    releaseB!();
    for (let i = 0; i < 10_000 && sendB.mock.calls.length < 2; i += 1) await Promise.resolve();
    for (let i = 0; i < 10_000 && rowOf("gate22 4 R")?.state !== "pending"; i += 1) await Promise.resolve();
    b.logout();
    await pump(1_000);
    expect(sendB.mock.calls.map((c) => c[4])).toEqual([sId, rId]);
    expect(sendA).not.toHaveBeenCalled();
    expect(rowOf("gate22 4 S")?.state).toBe("sent");
    expect(rowOf("gate22 4 R")?.state).toBe("pending");
    expect(rowOf("gate22 4 R")?.lease).toBeUndefined();

    // 8) A's REAL scoped-epilogue timer is the only remaining trigger for R
    //    (deliver wake or, if the epilogue read raced B's drain claim, the one
    //    30 s lease-deadline wake — allow for the longer arm). Let its wake
    //    flush (the third wake flush) run for real.
    resolveRecoveryFlush!();
    for (let i = 0; i < 400 && sendA.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(100);
    }
    expect(sendA.mock.calls.map((c) => c[4])).toEqual([rId]);
    expect(rowOf("gate22 4 R")?.state).toBe("sent");

    // Release every parked flush (nothing left); exact combined calls.
    resolveParkedFlushes!();
    await pump(3_000);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(sendB.mock.calls.map((c) => c[4])).toEqual([sId, rId]);
    expect(sendA.mock.calls.map((c) => c[4])).toEqual([rId]);
    a.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE22 item 5 (rebuilds gate-21 item-2): B really finishes X AND restores Y to pending before A's tracked X deadline, then closes — advancing only to X's deadline, A delivers Y before Y's original 30 s lease deadline", async () => {
  // Rebuild of the gate-21 item-2 regression per RULES. The older version
  // advanced all the way to Y's own 30 s lease deadline, so it never proved
  // that the tracked X deadline was sufficient. Here TWO real HubStores share
  // one localStorage backend behind the FIFO Web Locks shim; the initial
  // inflight claims are the only seeded state and every later transition is a
  // real delivery flow.
  //
  // A boots FIRST with X (instance IX, B lease 10 s) and Y (instance IY, B
  // lease 30 s): it tracks the two deadline wakes and posts nothing. B boots
  // second with locks.query FREE, so its load REALLY reclaims the two
  // same-owner fresh leases to pending; its boot flush then sends X clear and
  // fails Y at the API boundary (POST rejects AND status GET rejects), which
  // restores Y lease-less pending — all near t0, BEFORE A's tracked X
  // deadline. B closes with its pinned jittered retry cancelled.
  //
  // Advancing fake time ONLY to X's deadline: A's real X fire is the trigger
  // — its authoritative read sees X sent and Y PENDING; the central
  // reconciliation arms Y's one 0 ms wake, Y's real fire hands off to A's
  // guarded flush and A delivers Y under the same id, ~20 s before Y's
  // original 30 s lease deadline. Combined API calls exactly B:[X, Y] A:[Y];
  // no second POST and no manual flush anywhere.
  vi.useFakeTimers();
  vi.setSystemTime(1_000_000);
  const ownerA = "owner_g22_i5_a";
  const ownerB = "owner_g22_i5_b";
  const IX = "ins_g22_i5_x";
  const IY = "ins_g22_i5_y";
  const X = "cmd_g22_i5_x";
  const Y = "cmd_g22_i5_y";
  const shim = fifoWebLocks();
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: shim.locks },
  });
  localStorage.setItem(
    OUTBOX_LS_KEY,
    JSON.stringify([
      {
        commandId: X,
        clientRequestId: `local_${X}`,
        instanceId: IX,
        journalId: JOURNAL,
        prompt: "gate22 5 x",
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
        prompt: "gate22 5 y",
        createdAt: Date.now() - 1_000,
        attempts: 1,
        state: "inflight",
        lease: { owner: ownerB, until: Date.now() + 30_000 },
      } satisfies OutboxRecord,
    ]),
  );
  const pump = async (n = 2_000) => {
    for (let i = 0; i < n; i += 1) await Promise.resolve();
  };
  try {
    // A boots first with the foreign live leases: tracks both deadlines.
    sessionStorage.setItem("remuda-outbox-owner", ownerA);
    const { api: apiA, hubStore: a } = await fresh();
    vi.spyOn(apiA, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(apiA, "screenRead").mockResolvedValue({ lines: [] });
    vi.spyOn(apiA, "instanceCommandStatus").mockRejectedValue(new Error("a status unused"));
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
    const aOutbox = (a as unknown as { outbox: { pendingLeaseWakeups: () => string[] } }).outbox;
    await pump(1_000);
    expect(sendA).not.toHaveBeenCalled();
    expect(aOutbox.pendingLeaseWakeups().sort()).toEqual([X, Y].sort());

    // B boots: locks.query is FREE, its load reclaims both own fresh leases
    // (real reclaimInflight) and its boot flush runs both rows for real.
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
      (async (_iid: string, _p: string, _x?: unknown, _m?: string, commandId?: string) => {
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
        throw new Error("gate22 5 boundary POST failure");
      }) as unknown as Api["instanceSend"],
    );
    // Pin the store-retry jitter high and close B before any retry can fire.
    vi.spyOn(Math, "random").mockReturnValue(0.999);
    const storedRows = () => JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    const row = (id: string) => storedRows().find((r) => r.commandId === id);
    await bootLive(apiB, b);
    // B's real flow sends X, then claims Y, POSTs and fails the status GET,
    // and restores Y lease-less pending — wait for the durable restore, THEN
    // close B so its pinned jittered retry is cancelled.
    for (let i = 0; i < 2_000; i += 1) {
      if (sendB.mock.calls.length === 2 && row(Y)?.state === "pending") break;
      await Promise.resolve();
    }
    b.logout();
    expect(sendB.mock.calls.map((c) => c[4])).toEqual([X, Y]);

    // Both transitions committed well before the X deadline (fake time still
    // at the boot instant): X sent, Y lease-less pending.
    expect(row(X)?.state).toBe("sent");
    expect(row(Y)?.state).toBe("pending");
    expect(row(Y)?.lease).toBeUndefined();
    expect(sendA).not.toHaveBeenCalled();

    // Advance ONLY to the tracked X deadline (plus microtasks), nowhere near
    // Y's original 30 s lease deadline.
    await vi.advanceTimersByTimeAsync(10_000);
    for (let i = 0; i < 200 && sendA.mock.calls.length === 0; i += 1) {
      await Promise.resolve();
      await vi.advanceTimersByTimeAsync(1);
    }
    expect(sendA).toHaveBeenCalledTimes(1);
    expect(sendA.mock.calls[0]?.[4]).toBe(Y);
    expect(row(Y)?.state).toBe("sent");
    // The whole recovery finished by ~10 s: Y never waited for its 30 s
    // deadline and X was never posted by A.
    expect(Date.now()).toBeLessThan(1_000_000 + 20_000);
    expect(sendA.mock.calls.some((c) => c[4] === X)).toBe(false);

    // Far past every deadline: exactly the one A POST and B's two calls.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(sendA).toHaveBeenCalledTimes(1);
    expect(sendB).toHaveBeenCalledTimes(2);
    a.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});
