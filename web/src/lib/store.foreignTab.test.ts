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
