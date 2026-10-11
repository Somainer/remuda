import { afterEach, expect, it, vi } from "vitest";
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
