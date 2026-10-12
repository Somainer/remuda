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
      outbox: { refreshDurable: () => Promise<unknown>; pendingClaimWakeups: () => string[] };
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
    expect(internal.outbox.pendingClaimWakeups()).toEqual([cmd]);
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

it("GATE17 item 3a: a stale foreign timer reconciled away after the row is restored and claimed-held never POSTs before the held-retry deadline", async () => {
  // A armed foreign-expiry T for B; B's delivery FAILED (row back to pending,
  // lease cleared) and B closed. At T-500 an A flush claims and POSTs; the Hub
  // answers held (retry at ~T+1500). The stale T timer used to fire at T and
  // re-POST the held row before its own retry clock. The snapshot at the
  // T-500 flush must cancel the obsolete timer.
  vi.useFakeTimers();
  const owner = "owner_g17_i3a";
  const foreign = "owner_g17_i3a_crashed_b";
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
          state: "queued",
          dispatch: "not-dispatched",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"]);
    await bootLive(api, hubStore);
    const internal = hubStore as unknown as {
      ensureOutbox: () => Promise<boolean>;
      flushAllOutbox: () => Promise<void>;
      leaseWakeupTimer: Map<string, unknown>;
    };
    expect(await internal.ensureOutbox()).toBe(true);
    await internal.flushAllOutbox();

    const cmd = "cmd_g17_i3a";
    const T = Date.now() + 10_000;
    // B holds a live foreign lease; A arms T.
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmd,
          clientRequestId: "local_g17_i3a",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "held race",
          createdAt: Date.now() - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: foreign, until: T },
        } satisfies OutboxRecord,
      ]),
    );
    await internal.flushAllOutbox();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);
    expect(send).not.toHaveBeenCalled();

    // B's failed delivery restores pending and clears its lease; B closes.
    await vi.advanceTimersByTimeAsync(1_000);
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmd,
          clientRequestId: "local_g17_i3a",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "held race",
          createdAt: Date.now() - 2_000,
          attempts: 1,
          state: "pending",
        } satisfies OutboxRecord,
      ]),
    );

    // T-500: A claims the pending row; the Hub holds it (retry at T+1500).
    await vi.advanceTimersByTimeAsync(8_500);
    await internal.flushAllOutbox();
    for (let i = 0; i < 20; i += 1) await Promise.resolve();
    expect(send).toHaveBeenCalledTimes(1);
    // Snapshot reconciliation cancelled the obsolete T timer.
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(false);

    // The stale T fires nothing: explicit no-POST-before-the-held-deadline.
    await vi.advanceTimersByTimeAsync(500);
    for (let i = 0; i < 20; i += 1) await Promise.resolve();
    expect(send).toHaveBeenCalledTimes(1);

    // The held row's OWN 2 s retry clock delivers the second POST.
    await vi.advanceTimersByTimeAsync(1_500);
    for (let i = 0; i < 30 && send.mock.calls.length < 2; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(send).toHaveBeenCalledTimes(2);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE17 item 3b: a stale lease timer revalidates the durable row on fire and never flushes a row no longer foreign-leased", async () => {
  // The timer can be the first reader of a state change that happened without
  // a local flush snapshot: a cloned sibling (same persisted owner) received
  // `held` and cleared the lease while THIS document still had T armed. The
  // firing timer must re-read and prove a stealable foreign lease before
  // flushing — the old unconditional flush POSTed the held row.
  vi.useFakeTimers();
  const owner = "owner_g17_i3b";
  const foreign = "owner_g17_i3b_b";
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
    const send = vi
      .spyOn(api, "instanceSend")
      .mockImplementation((async (_i: string, _p: string, _a: unknown, _m: unknown, commandId: string) => ({
        relatedCommandIds: [],
        command: {
          commandId,
          id: commandId,
          state: "queued",
          dispatch: "not-dispatched",
          resolution: "clear",
        },
      })) as unknown as Api["instanceSend"]);
    await bootLive(api, hubStore);
    const internal = hubStore as unknown as {
      ensureOutbox: () => Promise<boolean>;
      flushAllOutbox: () => Promise<void>;
      leaseWakeupTimer: Map<string, unknown>;
    };
    expect(await internal.ensureOutbox()).toBe(true);
    await internal.flushAllOutbox();

    const cmd = "cmd_g17_i3b";
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmd,
          clientRequestId: "local_g17_i3b",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "stale fire",
          createdAt: Date.now() - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: foreign, until: Date.now() + 5_000 },
        } satisfies OutboxRecord,
      ]),
    );
    await internal.flushAllOutbox();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);

    // At T-1 s a cloned sibling persists the held answer (no lease). THIS
    // document performs no refresh — the stale timer is the next reader.
    await vi.advanceTimersByTimeAsync(4_000);
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmd,
          clientRequestId: "local_g17_i3b",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "stale fire",
          createdAt: Date.now() - 5_000,
          attempts: 1,
          state: "held",
          gotResponse: true,
        } satisfies OutboxRecord,
      ]),
    );

    // T fires: revalidation finds no foreign lease -> no flush, timer gone.
    await vi.advanceTimersByTimeAsync(1_000);
    for (let i = 0; i < 20; i += 1) await Promise.resolve();
    expect(send).not.toHaveBeenCalled();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(false);
    // Nothing this timer owns re-POSTs the row later either.
    await vi.advanceTimersByTimeAsync(60_000);
    expect(send).not.toHaveBeenCalled();
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE18 item 1: a fired foreign-lease timer delivers the lease-less pending row the dead owner left — its only trigger — while a held row never rides along", async () => {
  // A armed foreign-expiry T for B's lease. B's POST fails: B restores the
  // row to PENDING with the lease cleared and arms only an in-memory retry,
  // which B's pagehide then clears. B is closed; A is an otherwise IDLE
  // focused tab — at T its timer is the row's only trigger. The gate-17 item-3
  // revalidation predicate recognised only an inflight FOREIGN lease, so the
  // lease-less pending row it read at T produced no flush and the command was
  // never POSTed. A HELD row on the same instance must NOT ride the triggered
  // flush (its own held-retry clock owns that POST).
  vi.useFakeTimers();
  const owner = "owner_g18_i1";
  const foreign = "owner_g18_i1_crashed_b";
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
      leaseWakeupTimer: Map<string, unknown>;
    };
    expect(await internal.ensureOutbox()).toBe(true);
    await internal.flushAllOutbox();
    expect(send).not.toHaveBeenCalled();

    const cmdP = "cmd_g18_i1_pending";
    const cmdH = "cmd_g18_i1_held";
    const T = Date.now() + 10_000;
    // B holds a live foreign lease; A arms T.
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmdP,
          clientRequestId: "local_g18_i1_p",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "pending after crash",
          createdAt: Date.now() - 2_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: foreign, until: T },
        } satisfies OutboxRecord,
      ]),
    );
    await internal.flushAllOutbox();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);
    expect(send).not.toHaveBeenCalled();

    // B's failed POST restores P to pending (lease cleared); an EARLIER row of
    // the same instance was answered held. B closes; its in-memory retry dies
    // with it. A performs no flush — the stale timer is the next reader.
    await vi.advanceTimersByTimeAsync(2_000);
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmdH,
          clientRequestId: "local_g18_i1_h",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "held earlier",
          createdAt: Date.now() - 5_000,
          attempts: 1,
          state: "held",
          gotResponse: true,
        } satisfies OutboxRecord,
        {
          commandId: cmdP,
          clientRequestId: "local_g18_i1_p",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "pending after crash",
          createdAt: Date.now() - 2_000,
          attempts: 1,
          state: "pending",
          lastError: "network down",
        } satisfies OutboxRecord,
      ]),
    );

    // T fires on an idle tab: P is delivered exactly once; H never rides.
    await vi.advanceTimersByTimeAsync(8_000);
    for (let i = 0; i < 30 && send.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[4]).toBe(cmdP);
    const stored = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored.find((r) => r.commandId === cmdP)?.state).toBe("sent");
    expect(stored.find((r) => r.commandId === cmdH)?.state).toBe("held");

    // No later double POST of P and no timer-driven POST of the held row.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(send).toHaveBeenCalledTimes(1);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE18 item 2: a fired timer flushes EVERY already-stealable instance — background-aligned wakeups must not cancel the other instance's only timer", async () => {
  // Two foreign leases share one deadline T and BOTH timers are armed (the
  // browser aligns background wakeups, so callbacks queued for the same tick
  // run in arming order with each one's read continuation landing before the
  // next timer callback). I1's callback runs first: its own row revalidated to
  // HELD (a cloned sibling persisted the held answer without a local flush, as
  // gate-17 item 3b) — so the firing instance itself must NOT POST — while I2
  // is ALREADY overdue with its callback still queued. The gate-17 reconciler
  // skipped overdue leases in the re-arm map and then CANCELLED I2's timer; the
  // due check scoped to I1 found nothing deliverable, so I2 stayed inflight
  // forever. The fire path must deliver every stealable instance (holding the
  // firing instance's held row back).
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
      leaseWakeupTimer: Map<string, unknown>;
      leaseWakeupUntil: Map<string, number>;
      leaseWakeupFired: (instanceId: string, expectedUntil: number) => Promise<void>;
    };
    expect(await internal.ensureOutbox()).toBe(true);
    await internal.flushAllOutbox();

    const cmd1 = "cmd_g18_i2_held";
    const cmd2 = "cmd_g18_i2_overdue";
    const T = Date.now() + 10_000;
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmd1,
          clientRequestId: "local_g18_i2_1",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "becomes held",
          createdAt: Date.now() - 2_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: foreign1, until: T },
        } satisfies OutboxRecord,
        {
          commandId: cmd2,
          clientRequestId: "local_g18_i2_2",
          instanceId: INS2,
          journalId: J2,
          prompt: "overdue on the second instance",
          createdAt: Date.now() - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: foreign2, until: T },
        } satisfies OutboxRecord,
      ]),
    );
    await internal.flushAllOutbox();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);
    expect(internal.leaseWakeupTimer.has(INS2)).toBe(true);
    expect(send).not.toHaveBeenCalled();

    // Wall time reaches T WITHOUT running the queued timers (setSystemTime
    // moves the clock only): I1's row is held by then, I2's lease is overdue.
    vi.setSystemTime(T);
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmd1,
          clientRequestId: "local_g18_i2_1",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "becomes held",
          createdAt: Date.now() - 2_000,
          attempts: 1,
          state: "held",
          gotResponse: true,
        } satisfies OutboxRecord,
        {
          commandId: cmd2,
          clientRequestId: "local_g18_i2_2",
          instanceId: INS2,
          journalId: J2,
          prompt: "overdue on the second instance",
          createdAt: Date.now() - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: foreign2, until: T },
        } satisfies OutboxRecord,
      ]),
    );

    // I1's aligned wakeup runs before I2's queued callback: invoke exactly the
    // timer-callback body for I1 (the wrapper first drops its own map entry).
    internal.leaseWakeupTimer.delete(INSTANCE);
    internal.leaseWakeupUntil.delete(INSTANCE);
    await internal.leaseWakeupFired(INSTANCE, T);
    for (let i = 0; i < 30 && send.mock.calls.length === 0; i += 1) await Promise.resolve();

    // I2 is delivered BY I1's fire even though I2's own callback never ran;
    // I1's held row is never POSTed.
    for (let i = 0; i < 30 && send.mock.calls.length === 0; i += 1) await Promise.resolve();
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[4]).toBe(cmd2);
    for (let i = 0; i < 30; i += 1) await Promise.resolve();
    const stored = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as OutboxRecord[];
    expect(stored.find((r) => r.commandId === cmd2)?.state).toBe("sent");
    expect(stored.find((r) => r.commandId === cmd1)?.state).toBe("held");

    // I2's still-queued callback later changes nothing (row already sent);
    // the held row keeps waiting on its own clock (gate-17 item 3b).
    await vi.advanceTimersByTimeAsync(120_000);
    expect(send).toHaveBeenCalledTimes(1);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE18 item 3: reconciliation on one command's timer hands ANOTHER tracked command's foreign replacement to the store scheduler — Y delivers at T2", async () => {
  // Two same-owner inactive claims (X, Y) each have an Outbox claim wake. X's
  // timer is the next reader of the durable set: by then X is TERMINAL (sent)
  // while Y has been re-claimed by a FOREIGN owner whose deadline T2 has not
  // arrived and who then crashes. Snapshot reconciliation used to cancel Y's
  // tracked wake with no handoff — the timer-discovery path handed off, but
  // the reconciliation path did not — so nothing scheduled T2 and Y stayed
  // inflight forever. The firing (terminal) command itself must not be handed
  // off twice.
  vi.useFakeTimers();
  const owner = "owner_g18_i3";
  const foreign = "owner_g18_i3_crashed_y";
  const INS_Y = "ins_g18_i3_y";
  const J_Y = "obj_g18_i3_journal_y";
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
        pendingClaimWakeups: () => string[];
      };
      leaseWakeupTimer: Map<string, unknown>;
      leaseWakeupUntil: Map<string, number>;
    };
    expect(await internal.ensureOutbox()).toBe(true);
    await internal.flushAllOutbox(); // settle the empty boot flush
    expect(send).not.toHaveBeenCalled();

    const cmdX = "cmd_g18_i3_x_terminal";
    const cmdY = "cmd_g18_i3_y_reclaimed";
    const T1 = Date.now() + 2_000;
    const T2 = Date.now() + 20_000;
    const baseRow = (commandId: string, instanceId: string, journalId: string, until: number): OutboxRecord => ({
      commandId,
      clientRequestId: `local_${commandId}`,
      instanceId,
      journalId,
      prompt: commandId,
      createdAt: Date.now() - 1_000,
      attempts: 1,
      state: "inflight",
      lease: { owner, until },
    });
    // Two crashed same-owner claims: the Outbox's own discovery tracks both.
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([baseRow(cmdX, INSTANCE, JOURNAL, T1), baseRow(cmdY, INS_Y, J_Y, T1)]),
    );
    await internal.outbox.refreshDurable();
    expect(internal.outbox.pendingClaimWakeups().sort()).toEqual([cmdX, cmdY].sort());
    expect(internal.leaseWakeupTimer.has(INS_Y)).toBe(false);

    // X delivers (terminal) elsewhere; Y is re-claimed by a foreign owner that
    // then crashes — the replacement lease is first seen by X's timer.
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          ...baseRow(cmdX, INSTANCE, JOURNAL, T1),
          state: "sent",
          gotResponse: true,
          lease: undefined,
        } satisfies OutboxRecord,
        {
          ...baseRow(cmdY, INS_Y, J_Y, T2),
          lease: { owner: foreign, until: T2 },
        } satisfies OutboxRecord,
      ]),
    );

    // T1: X stops (terminal); reconciliation hands Y's foreign T2 to the store.
    await vi.advanceTimersByTimeAsync(2_000);
    for (let i = 0; i < 20; i += 1) await Promise.resolve();
    expect(send).not.toHaveBeenCalled();
    expect(internal.outbox.pendingClaimWakeups()).toEqual([]);
    expect(internal.leaseWakeupTimer.has(INS_Y)).toBe(true);
    expect(internal.leaseWakeupUntil.get(INS_Y)).toBe(T2);

    // Nothing just before T2 …
    await vi.advanceTimersByTimeAsync(17_999);
    expect(send).not.toHaveBeenCalled();
    // … exactly one real POST for Y at the foreign deadline.
    await vi.advanceTimersByTimeAsync(1);
    for (let i = 0; i < 30 && send.mock.calls.length === 0; i += 1) {
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

it("GATE18 item 4: a clock rollback re-arms the fresh snapshot's sooner replacement lease U2 (now < U2 < U) and delivers at U2, never waiting for U", async () => {
  // The timer closed over U; the system clock rolls BACK after arming so the
  // callback observes now < U. Meanwhile the stored lease was replaced with U2
  // where now < U2 < U. The old code re-armed U from the closure and ignored
  // the snapshot: the replacement lease (its owner crashed) was unserved until
  // U. The rollback branch must arm the soonest live foreign lease in the
  // fresh snapshot.
  vi.useFakeTimers();
  const owner = "owner_g18_i4a";
  const foreign = "owner_g18_i4a_crashed_b";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: { query: async () => ({ held: [] }) } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  sessionStorage.setItem("remuda-outbox-owner", owner);
  try {
    const t0 = Date.now();
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
      leaseWakeupTimer: Map<string, unknown>;
      leaseWakeupUntil: Map<string, number>;
      leaseWakeupFired: (instanceId: string, expectedUntil: number) => Promise<void>;
    };
    expect(await internal.ensureOutbox()).toBe(true);
    await internal.flushAllOutbox();

    const cmd = "cmd_g18_i4a";
    const U = t0 + 10_000;
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmd,
          clientRequestId: "local_g18_i4a",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "rolled back",
          createdAt: t0 - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: foreign, until: U },
        } satisfies OutboxRecord,
      ]),
    );
    await internal.flushAllOutbox();
    expect(internal.leaseWakeupUntil.get(INSTANCE)).toBe(U);

    // Clock rolls back to t0+4000; the lease is replaced with U2 = t0+7000
    // (3 s of wall time away, strictly before U).
    vi.setSystemTime(t0 + 4_000);
    const U2 = t0 + 7_000;
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: cmd,
          clientRequestId: "local_g18_i4a",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "rolled back",
          createdAt: t0 - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: foreign, until: U2 },
        } satisfies OutboxRecord,
      ]),
    );
    // The timer body fires early (the wrapper drops its own entry first).
    internal.leaseWakeupTimer.delete(INSTANCE);
    internal.leaseWakeupUntil.delete(INSTANCE);
    await internal.leaseWakeupFired(INSTANCE, U);
    expect(internal.leaseWakeupUntil.get(INSTANCE)).toBe(U2);

    // Nothing before U2 …
    await vi.advanceTimersByTimeAsync(2_999);
    expect(send).not.toHaveBeenCalled();
    // … delivered at U2, 3 s sooner than the stale closed-over U.
    await vi.advanceTimersByTimeAsync(1);
    for (let i = 0; i < 30 && send.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[4]).toBe(cmd);
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE18 item 4: a clock rollback with no live foreign lease in the fresh snapshot drops the closed-over U instead of re-arming it", async () => {
  vi.useFakeTimers();
  const owner = "owner_g18_i4b";
  const foreign = "owner_g18_i4b_crashed_b";
  const originalNavigator = globalThis.navigator;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    writable: true,
    value: { locks: { query: async () => ({ held: [] }) } },
  });
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([]));
  sessionStorage.setItem("remuda-outbox-owner", owner);
  try {
    const t0 = Date.now();
    const { api, hubStore } = await fresh();
    vi.spyOn(api, "eventsRead").mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
    vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
    const send = vi.spyOn(api, "instanceSend").mockResolvedValue({
      relatedCommandIds: [],
      command: {
        commandId: "cmd_g18_i4b",
        id: "cmd_g18_i4b",
        state: "accepted",
        revision: "1",
        dispatch: "native-acknowledged",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    });
    await bootLive(api, hubStore);
    const internal = hubStore as unknown as {
      ensureOutbox: () => Promise<boolean>;
      flushAllOutbox: () => Promise<void>;
      leaseWakeupTimer: Map<string, unknown>;
      leaseWakeupUntil: Map<string, number>;
      leaseWakeupFired: (instanceId: string, expectedUntil: number) => Promise<void>;
    };
    expect(await internal.ensureOutbox()).toBe(true);
    await internal.flushAllOutbox();

    const U = t0 + 10_000;
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: "cmd_g18_i4b",
          clientRequestId: "local_g18_i4b",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "delivered elsewhere",
          createdAt: t0 - 1_000,
          attempts: 1,
          state: "inflight",
          lease: { owner: foreign, until: U },
        } satisfies OutboxRecord,
      ]),
    );
    await internal.flushAllOutbox();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);

    // Rollback; another tab delivered the row terminal meanwhile.
    vi.setSystemTime(t0 + 4_000);
    localStorage.setItem(
      OUTBOX_LS_KEY,
      JSON.stringify([
        {
          commandId: "cmd_g18_i4b",
          clientRequestId: "local_g18_i4b",
          instanceId: INSTANCE,
          journalId: JOURNAL,
          prompt: "delivered elsewhere",
          createdAt: t0 - 1_000,
          attempts: 1,
          state: "sent",
          gotResponse: true,
        } satisfies OutboxRecord,
      ]),
    );
    internal.leaseWakeupTimer.delete(INSTANCE);
    internal.leaseWakeupUntil.delete(INSTANCE);
    await internal.leaseWakeupFired(INSTANCE, U);
    // U is NOT resurrected: nothing armed, nothing POSTs even past U.
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(false);
    expect(internal.leaseWakeupUntil.has(INSTANCE)).toBe(false);
    await vi.advanceTimersByTimeAsync(20_000);
    expect(send).not.toHaveBeenCalled();
    hubStore.logout();
  } finally {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: originalNavigator });
    sessionStorage.removeItem("remuda-outbox-owner");
  }
});

it("GATE17 item 2: the Outbox wake is what FIRST discovers the foreign replacement — it hands T2 to the store, which POSTs exactly once at T2", async () => {
  // A's tracked SAME-OWNER wake (a cloned sibling's inactive claim, T1) fires
  // and only THEN discovers B's FOREIGN replacement lease (T2, B crashed).
  // Gate 16 made the Outbox wake hand off, but nothing scheduled T2: the store
  // arms foreign expiry only from flush/unload rearm, and its Outbox
  // subscription merely updates the pending count. The handoff must carry the
  // authoritative row+deadline to a store scheduling hook.
  vi.useFakeTimers();
  const owner = "owner_g17_i2";
  const foreign = "owner_g17_i2_crashed_b";
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
        pendingClaimWakeups: () => string[];
      };
      leaseWakeupTimer: Map<string, unknown>;
      leaseWakeupUntil: Map<string, number>;
    };
    expect(await internal.ensureOutbox()).toBe(true);
    await internal.flushAllOutbox(); // settle the empty boot flush
    expect(send).not.toHaveBeenCalled();

    const T1 = Date.now() + 2_000;
    const T2 = Date.now() + 20_000;
    const cmd = "cmd_g17_i2_handoff";
    const rowWith = (leaseOwner: string, until: number): OutboxRecord => ({
      commandId: cmd,
      clientRequestId: "local_g17_i2",
      instanceId: INSTANCE,
      journalId: JOURNAL,
      prompt: "handoff",
      createdAt: Date.now() - 1_000,
      attempts: 1,
      state: "inflight",
      lease: { owner: leaseOwner, until },
    });
    // A cloned sibling (same persisted owner) claimed after load and stalled:
    // the Outbox's own wake tracks T1. The store has NO foreign timer yet.
    localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([rowWith(owner, T1)]));
    await internal.outbox.refreshDurable();
    expect(internal.outbox.pendingClaimWakeups()).toEqual([cmd]);
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(false);

    // B's FOREIGN replacement lease (expiry T2) lands WITHOUT any store
    // refresh: the Outbox timer is the first thing to discover it.
    localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([rowWith(foreign, T2)]));

    // T1 fires: handoff -> the store arms T2; nothing POSTs.
    await vi.advanceTimersByTimeAsync(2_000);
    for (let i = 0; i < 10; i += 1) await Promise.resolve();
    expect(send).not.toHaveBeenCalled();
    expect(internal.outbox.pendingClaimWakeups()).toEqual([]);
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);
    expect(internal.leaseWakeupUntil.get(INSTANCE)).toBe(T2);
    // The cache carries the authoritative foreign row, not A's stale lease.
    const cached = (internal.outbox as unknown as { get: (id: string) => OutboxRecord | undefined }).get(cmd);
    expect(cached?.lease?.owner).toBe(foreign);

    // Still nothing just before T2 …
    await vi.advanceTimersByTimeAsync(17_999);
    expect(send).not.toHaveBeenCalled();
    // … exactly one real POST at the foreign deadline (B crashed).
    await vi.advanceTimersByTimeAsync(1);
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

it("GATE18 item 6 (real store): an offline-retired attempt's delayed catch-up rejection never crosses a binding onto adopted live B — real resume/subscribe, zero settlement probes", async () => {
  // Real-store counterpart of the gate-17 item-4 machine unit test (which
  // injects resume/probe/liveness and calls noteBinding/followRebindLive
  // directly). Here the resume is the store's real reopenFollow ->
  // openFollowSocket path, the socket death and browser offline go through the
  // real machine dispatch/window listener, and the binding change is a real
  // mounted-session follow. A's catch-up rejects AFTER B was adopted live; B
  // must never demote and no settlement probe may run for either journal.
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
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    windowFromSeq: "1",
    durableSeq: "0",
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>);
  let rejectA!: (err: Error) => void;
  let aSubscribeCalls = 0;
  let bSubscribeCalls = 0;
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (journalId: string) => {
      if (journalId === JOURNAL) {
        aSubscribeCalls += 1;
        // A's SECOND subscribe (the real reconnect resume) stays pending until
        // the test rejects it after the binding moved to B.
        if (aSubscribeCalls === 2) {
          await new Promise<never>((_resolve, reject) => {
            rejectA = reject;
          });
        }
      } else {
        bSubscribeCalls += 1;
      }
      return {
        subscriptionId: `sub_g18i6_${journalId}` as never,
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

  // A's link dies; a real online-driven resume starts and hangs on A's
  // reopen subscribe; the follow socket then closes mid-attempt (latched); a
  // real browser offline event retires the attempt with the offline flag.
  store.setFollowLiveForTest(false, false, JOURNAL);
  store.connection.dispatch({ type: "online" });
  for (let i = 0; i < 20 && aSubscribeCalls < 2; i += 1) await Promise.resolve();
  expect(aSubscribeCalls).toBe(2);
  expect(store.connection.state).toBe("recovering");
  store.connection.dispatch({ type: "close" });
  window.dispatchEvent(new Event("offline"));
  expect(store.connection.state).toBe("offline");

  // Navigate to the MOUNTED B: real bind + adoption certifies B live.
  await hubStore.follow(INSTANCE_B);
  expect(store.connectionBoundJournal).toBe(JOURNAL_B);
  expect(store.connection.state).toBe("live");

  // A's delayed catch-up rejects across the binding.
  rejectA(new Error("A catch-up lost the race"));
  for (let i = 0; i < 30; i += 1) await Promise.resolve();
  expect(store.connection.state).toBe("live");

  // The stale completion was dropped BEFORE any settlement: no REST probe for
  // either journal, and no subscribe was reopened for anyone.
  const probes = () =>
    vi.mocked(api.eventsRead).mock.calls.filter((c) => c[0]?.limit === 1).length;
  expect(probes()).toBe(0);
  expect(aSubscribeCalls).toBe(2);
  expect(bSubscribeCalls).toBe(1);

  // Still live with nothing reopened well past the rejection (the frame
  // watchdog's own 15 s silent-socket cycle is intentionally not crossed).
  await vi.advanceTimersByTimeAsync(10_000);
  for (let i = 0; i < 20; i += 1) await Promise.resolve();
  expect(store.connection.state).toBe("live");
  expect(probes()).toBe(0);
  expect(aSubscribeCalls).toBe(2);
  expect(bSubscribeCalls).toBe(1);
  vi.useRealTimers();
  hubStore.logout();
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
