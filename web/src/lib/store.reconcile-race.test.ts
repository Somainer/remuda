import { afterEach, expect, it, vi } from "vitest";
import type { CommandResult } from "../types/command";
import type { Observation } from "../types/observation";
import { OUTBOX_LS_KEY } from "./outbox";

const INSTANCE = "ins_reconcile_race";
const JOURNAL = "obj_reconcile_race_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

function commandResult(commandId: string, state: CommandResult["command"]["state"]): CommandResult {
  return {
    relatedCommandIds: [],
    command: {
      commandId,
      id: commandId,
      revision: "1",
      createdAt: "2026-09-15T00:00:00.000Z",
      updatedAt: "2026-09-15T00:00:00.000Z",
      actor: { principalId: "prn_1", type: "human", deviceId: "dev_1", instanceId: INSTANCE },
      origin: "ui",
      operation: "instance.send",
      target: { hostId: "hst_1", instanceId: INSTANCE, runId: null },
      payloadDigest: "sha256:00",
      state,
      dispatch: "intent-durable",
      resolution: "clear",
    },
  };
}

/** Fresh store/api module pair per test — the store is a process singleton. */
async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

/**
 * Mount a followed instance and hand back the live-batch injector the follow
 * subscription exposes (same harness shape as store.localBubble twin test).
 */
async function mountFollow(api: Api, hubStore: Store) {
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "1", durableSeq: "0" } as Awaited<
      ReturnType<Api["instanceList"]>
    >["items"][number]],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as Awaited<ReturnType<Api["interactionList"]>>);
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_reconcile_race",
    journalId: JOURNAL,
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
        getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_reconcile_race",
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
  const onBatch = subscribe.mock.calls[0]?.[2] as
    | ((batch: Record<string, unknown>) => void)
    | undefined;
  if (!onBatch) throw new Error("follow did not expose onBatch");
  return onBatch;
}

function screenBatch(seq: string, text: string): Record<string, unknown> {
  return {
    subscriptionId: "sub_reconcile_race",
    journalId: JOURNAL,
    fromSeq: seq,
    toSeq: seq,
    durableSeq: seq,
    events: [
      {
        kind: "raw_tty",
        eventId: `evt_screen_${seq}`,
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq,
        payload: { text },
      } as unknown as Observation,
    ],
  };
}

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

it("a list poll that began before create completes never drops the created instance", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "interactionList").mockResolvedValue([] as Awaited<ReturnType<Api["interactionList"]>>);
  // Response queue per fetch START: the first list fetch (the stale poll)
  // stays pending until released, and resolves WITHOUT the new instance —
  // exactly an older server snapshot landing after the create.
  type ListPage = Awaited<ReturnType<Api["instanceList"]>>;
  let releaseOlderPoll: (items: ListPage["items"]) => void = () => {};
  const olderPoll = new Promise<ListPage>((resolve) => {
    releaseOlderPoll = (items) => resolve({ items, nextCursor: null });
  });
  vi.spyOn(api, "instanceList")
    .mockReturnValueOnce(olderPoll)
    .mockResolvedValue({ items: [], nextCursor: null });
  const instance = {
    id: INSTANCE,
    journalId: JOURNAL,
    hostId: "hst_1",
    kind: "claude",
    driver: "claude-pty",
    lifecycle: "running",
  } as Awaited<ReturnType<Api["instanceCreate"]>>["instance"];
  vi.spyOn(api, "instanceCreate").mockResolvedValue({
    instance,
    command: commandResult("cmd_create", "accepted").command,
  });

  // 1. A poll starts BEFORE the create and hangs in flight.
  const olderRefresh = hubStore.refresh();
  // 2. The create lands and optimistically inserts the row (its own post-create
  //    refresh returns an immediate empty list while the older poll is pending).
  const created = await hubStore.create({
    hostId: "hst_1",
    kind: "claude",
    driver: "claude-pty",
    model: "e2e/auto",
    permissionMode: "default",
    prompt: "p",
  });
  expect(created.id).toBe(INSTANCE);

  // 3. The stale, pre-create poll now resolves with an older list that omits
  //    the instance. The optimistic row must survive — SessionPage mounts on
  //    this exact moment and used to render 会话不存在.
  releaseOlderPoll([]);
  await olderRefresh;
  expect(hubStore.getSnapshot().instances.some((i) => i.id === INSTANCE)).toBe(true);

  // 4. Once every older request has settled and a genuinely post-create poll
  //    speaks, its list is authoritative again (the real server now lists the
  //    instance; an empty post-create response removes a row that is gone).
  await hubStore.refresh();
  expect(hubStore.getSnapshot().instances.some((i) => i.id === INSTANCE)).toBe(false);
});

it("a failed catch-up settles the per-session journal status (not a latched 重连) and a retry restores live", async () => {
  const { api, hubStore } = await fresh();
  const onBatch = await mountFollow(api, hubStore);
  expect(onBatch).toBeTypeOf("function");
  // In the machine-less unit harness the link starts recovering; the
  // per-session journal status is live after follow and is what this test
  // asserts (global connection is owned solely by the connection machine).
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("live");

  // The bounded journal read fails (HTTP 502 / network drop): no events.
  vi.mocked(api.eventsRead).mockRejectedValue(new Error("HTTP 502"));
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => undefined);

  await hubStore.catchup(INSTANCE);

  // The per-session status settles at the truthful readonly-stale state
  // (JournalBanner shows 只读 + 重试). The global connection is owned solely
  // by the connection machine and is NOT written by journal onStatus.
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("readonly-stale");
  expect(toast).toHaveBeenCalled();

  // The existing retry action (the banner 重试 button calls catchup) with a
  // healthy read restores live on both layers.
  vi.mocked(api.eventsRead).mockResolvedValue({
    events: [],
    durableSeq: "1",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  await hubStore.catchup(INSTANCE);
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("live");
});

it("a slow failed catch-up cannot downgrade a newer successful recovery (resume generation)", async () => {
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);

  // Catch-up A's read is held open; catch-up B starts after it and succeeds
  // with an empty resync (cursor does not advance). A's late 502 must not
  // touch status.
  let releaseA: (v: never) => void = () => {};
  const aRead = new Promise<never>((_resolve, reject) => {
    releaseA = () => reject(new Error("HTTP 502 A"));
  });
  vi.spyOn(hubStore, "toast").mockImplementation(() => undefined);
  vi.mocked(api.eventsRead)
    .mockReturnValueOnce(aRead)
    .mockResolvedValueOnce({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });

  void hubStore.catchup(INSTANCE);
  await hubStore.catchup(INSTANCE); // B succeeds → live
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("live");

  releaseA(undefined as never);
  await new Promise((r) => setTimeout(r, 10));
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("live");
});

it("a failed catch-up is cleared automatically when the follow socket delivers a contiguous batch", async () => {
  const { api, hubStore } = await fresh();
  const onBatch = await mountFollow(api, hubStore);
  onBatch(screenBatch("1", "JOURNAL-1"));
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("live");

  vi.mocked(api.eventsRead).mockRejectedValue(new Error("HTTP 502"));
  vi.spyOn(hubStore, "toast").mockImplementation(() => undefined);
  await hubStore.catchup(INSTANCE);
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("readonly-stale");

  // No manual retry: the still-open follow socket delivers the next turn's
  // contiguous frame. applyBatch flushes it and the journal returns to live
  // (the global connection indicator is owned solely by the connection
  // machine; this test asserts the per-session status).
  onBatch(screenBatch("2", "JOURNAL-2"));
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("live");
});

it("an RPC screen committed first is not rolled back by catch-up re-deriving the same journal screen", async () => {
  // Reviewer trigger: journal has the DONE screen at seq 1; the parallel
  // tty.screen read resolves first with the new working buffer; catch-up then
  // delivers a NON-screen event at seq 2. The re-derived seq-1 screen must not
  // overwrite the newer RPC buffer (which would restore the DONE badge).
  const { api, hubStore } = await fresh();
  const onBatch = await mountFollow(api, hubStore);

  onBatch(screenBatch("1", "DONE old task"));
  expect(hubStore.getSnapshot().screens[INSTANCE]?.done).toBe(true);

  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: ["working on new task"] });
  await hubStore.refreshScreen(INSTANCE);
  const afterRpc = hubStore.getSnapshot().screens[INSTANCE];
  expect(afterRpc?.lines).toEqual(["working on new task"]);
  expect(afterRpc?.done).toBe(false);
  expect(afterRpc?.journalSeq).toBe("1");

  // Catch-up delivers a regular assistant message at seq 2 — the latest
  // SCREEN observation is still seq 1, and that equal basis must not commit.
  onBatch({
    subscriptionId: "sub_reconcile_race",
    journalId: JOURNAL,
    fromSeq: "2",
    toSeq: "2",
    durableSeq: "2",
    events: [
      {
        kind: "message",
        eventId: "evt_msg_2",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "2",
        completeness: "structured",
        payload: { role: "assistant", text: "still working" },
      } as unknown as Observation,
    ],
  });

  const screen = hubStore.getSnapshot().screens[INSTANCE];
  expect(screen?.lines).toEqual(["working on new task"]);
  expect(screen?.done).toBe(false);

  // A genuinely newer journal screen (seq 3) wins again.
  onBatch(screenBatch("3", "DONE new task"));
  const later = hubStore.getSnapshot().screens[INSTANCE];
  expect(later?.lines.join(" ")).toContain("DONE new task");
  expect(later?.journalSeq).toBe("3");
});

it("an unexpected 500 screen read is reported by both layers, never replaced with a fallback", async () => {
  const { HubHttpError } = await import("./httpError");
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  onBatchScreen(api, hubStore, "1", "JOURNAL-BEFORE");
  const error = new HubHttpError(500, "INTERNAL", "boom", []);
  vi.spyOn(api, "screenRead").mockRejectedValue(error);

  // Direct call: the error propagates; the committed screen is untouched and
  // no fallback silently masquerades as current content.
  await expect(hubStore.refreshScreen(INSTANCE)).rejects.toBe(error);
  expect(hubStore.getSnapshot().screens[INSTANCE]?.lines.join(" ")).toContain("JOURNAL-BEFORE");

  // Scheduler layer (the 2.5 s list poll fan-out): unexpected failures are
  // reported as an advisory, unlike NODE_BUSY which only backs off.
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => undefined);
  hubStore.refreshScreens([INSTANCE]);
  await vi.waitFor(() => expect(toast).toHaveBeenCalled());

  // A 4xx (offline host / no PTY for this carrier) is converted by the api
  // layer into an expected empty read, so refreshScreen falls back to the
  // journal screen without any error.
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  await expect(hubStore.refreshScreen(INSTANCE)).resolves.toBeUndefined();
});

function onBatchScreen(api: Api, hubStore: Store, seq: string, text: string) {
  const subscribe = vi.mocked(api.eventsSubscribe);
  const onBatch = subscribe.mock.calls[0]?.[2] as (batch: Record<string, unknown>) => void;
  onBatch(screenBatch(seq, text));
  return hubStore.getSnapshot().screens[INSTANCE];
}

it("a list response in flight across logout is dropped; the request seq stays monotonic", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "interactionList").mockResolvedValue([] as Awaited<ReturnType<Api["interactionList"]>>);
  vi.spyOn(api, "deviceRevoke").mockResolvedValue(undefined as never);
  type ListPage = Awaited<ReturnType<Api["instanceList"]>>;
  let releaseFirst: (items: ListPage["items"]) => void = () => {};
  const firstPoll = new Promise<ListPage>((resolve) => {
    releaseFirst = (items) => resolve({ items, nextCursor: null });
  });
  let releaseSecond: (items: ListPage["items"]) => void = () => {};
  const secondPoll = new Promise<ListPage>((resolve) => {
    releaseSecond = (items) => resolve({ items, nextCursor: null });
  });
  const oldRow = { id: "ins_OLD_SESSION", journalId: "j_old", revision: "1", durableSeq: "0" } as ListPage["items"][number];
  vi.spyOn(api, "instanceList")
    .mockReturnValueOnce(firstPoll) // pre-logout poll
    .mockReturnValueOnce(secondPoll) // post-logout poll, begun pre-create
    .mockResolvedValue({ items: [], nextCursor: null });
  const instance = {
    id: INSTANCE,
    journalId: JOURNAL,
    hostId: "hst_1",
    kind: "claude",
    driver: "claude-pty",
    lifecycle: "running",
  } as Awaited<ReturnType<Api["instanceCreate"]>>["instance"];
  vi.spyOn(api, "instanceCreate").mockResolvedValue({
    instance,
    command: commandResult("cmd_create2", "accepted").command,
  });

  // 1. Poll starts before logout and is still awaiting when the session ends.
  const staleRefresh = hubStore.refresh();
  hubStore.logout();
  expect(hubStore.getSnapshot().instances).toEqual([]);
  // 2. Its old-session body resolves: the epoch guard drops it wholesale.
  releaseFirst([oldRow]);
  await staleRefresh;
  expect(hubStore.getSnapshot().instances).toEqual([]);

  // 3. A poll started in the new (post-logout) epoch is still in flight when
  //    a session is created — pinning must work with the monotonic (un-reset)
  //    request seq.
  const pendingRefresh = hubStore.refresh();
  const created = await hubStore.create({
    hostId: "hst_1",
    kind: "claude",
    driver: "claude-pty",
    model: "e2e/auto",
    permissionMode: "default",
    prompt: "p",
  });
  expect(created.id).toBe(INSTANCE);
  releaseSecond([]);
  await pendingRefresh;
  // The response that began before the create cannot drop the optimistic row.
  expect(hubStore.getSnapshot().instances.some((i) => i.id === INSTANCE)).toBe(true);
  expect(hubStore.getSnapshot().instances.some((i) => i.id === "ins_OLD_SESSION")).toBe(false);
  // 4. A settled post-create response is authoritative again.
  await hubStore.refresh();
  expect(hubStore.getSnapshot().instances.some((i) => i.id === INSTANCE)).toBe(false);
});

it("a tty.screen read in flight before a newer journal frame cannot overwrite it", async () => {
  const { api, hubStore } = await fresh();
  const onBatch = await mountFollow(api, hubStore);

  // A journal-derived screen already exists (seq 1).
  onBatch(screenBatch("1", "JOURNAL-1"));
  let releaseRead: (value: { lines: string[] }) => void = () => {};
  const slowRead = new Promise<{ lines: string[] }>((resolve) => {
    releaseRead = resolve;
  });
  vi.spyOn(api, "screenRead").mockReturnValueOnce(slowRead);

  // Start the RPC read, then land a newer journal screen while it flies.
  const read = hubStore.refreshScreen(INSTANCE);
  onBatch(screenBatch("2", "JOURNAL-2"));
  releaseRead({ lines: ["RPC-OLD-BUFFER"] });
  await read;

  const screen = hubStore.getSnapshot().screens[INSTANCE];
  expect(screen?.lines.join(" ")).toContain("JOURNAL-2");
  expect(screen?.lines.join(" ")).not.toContain("RPC-OLD-BUFFER");
  expect(screen?.journalSeq).toBe("2");
});

it("a tty.screen read newer than the committed journal frame still commits", async () => {
  const { api, hubStore } = await fresh();
  const onBatch = await mountFollow(api, hubStore);
  onBatch(screenBatch("1", "JOURNAL-1"));
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: ["RPC-LIVE-BUFFER"] });

  await hubStore.refreshScreen(INSTANCE);

  const screen = hubStore.getSnapshot().screens[INSTANCE];
  expect(screen?.lines).toContain("RPC-LIVE-BUFFER");
  expect(screen?.journalSeq).toBe("1");
});

it("a stale tty.screen read cannot supersede a newer read that already committed", async () => {
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);

  let releaseOld: (value: { lines: string[] }) => void = () => {};
  const oldReadPromise = new Promise<{ lines: string[] }>((resolve) => {
    releaseOld = resolve;
  });
  vi.spyOn(api, "screenRead")
    .mockReturnValueOnce(oldReadPromise)
    .mockResolvedValueOnce({ lines: ["NEWER-READ"] });

  const old = hubStore.refreshScreen(INSTANCE);
  await hubStore.refreshScreen(INSTANCE);
  expect(hubStore.getSnapshot().screens[INSTANCE]?.lines).toEqual(["NEWER-READ"]);

  releaseOld({ lines: ["STALE-READ"] });
  await old;
  expect(hubStore.getSnapshot().screens[INSTANCE]?.lines).toEqual(["NEWER-READ"]);
});

it("an unseen screen known only in the REST seed still orders the live RPC buffer", async () => {
  // Carried review case: the follow REST seed carries a screen at seq 1 but
  // the socket never re-delivers it, so screens[id] is absent when the RPC
  // runs. The RPC basis must still come from the observed events (seq 1), and
  // a later non-screen event at seq 2 must not roll the RPC buffer back.
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  // Seed directly with a seq-1 screen; no live onBatch ever delivers it.
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [
      {
        kind: "raw_tty",
        eventId: "evt_seed_screen",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "1",
        payload: { text: "DONE old task", nativeName: "screen", status: "done?" },
      } as unknown as Observation,
    ],
    durableSeq: "1",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_seed",
    journalId: JOURNAL,
    durableSeq: "1",
    windowFromSeq: null,
    reachedAfterSeq: true,
        getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "seed-epoch",
      asOfSeq: "1",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "1", complete: true },
    },
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as Awaited<ReturnType<Api["interactionList"]>>);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      { id: INSTANCE, journalId: JOURNAL, durableSeq: "0" } as Awaited<
        ReturnType<Api["instanceList"]>
      >["items"][number],
    ],
    nextCursor: null,
  });

  await hubStore.refresh();
  await hubStore.follow(INSTANCE);
  // The REST seed is in events, but follow() never wrote it to screens.
  expect(hubStore.getSnapshot().screens[INSTANCE]).toBeUndefined();

  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: ["working on new task"] });
  await hubStore.refreshScreen(INSTANCE);
  const afterRpc = hubStore.getSnapshot().screens[INSTANCE];
  expect(afterRpc?.lines).toEqual(["working on new task"]);
  expect(afterRpc?.done).toBe(false);
  expect(afterRpc?.journalSeq).toBe("1");

  // A non-screen live event at seq 2 re-derives seq 1; it must not win.
  const subscribe = vi.mocked(api.eventsSubscribe);
  const onBatch = subscribe.mock.calls[0]?.[2] as (b: Record<string, unknown>) => void;
  onBatch({
    subscriptionId: "sub_seed",
    journalId: JOURNAL,
    fromSeq: "2",
    toSeq: "2",
    durableSeq: "2",
    events: [
      {
        kind: "lifecycle",
        eventId: "evt_native_status_2",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "2",
        payload: { type: "native", nativeName: "agent_status", status: "working" },
      } as unknown as Observation,
    ],
  });
  const final = hubStore.getSnapshot().screens[INSTANCE];
  expect(final?.lines).toEqual(["working on new task"]);
  expect(final?.done).toBe(false);
});

it("cancel is refused offline and sends nothing (canInterrupt gate)", async () => {
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  const cancel = vi.spyOn(api, "instanceCancel");
  // Simulate the connection machine going offline.
  hubStore.setConnectionStateForTest("offline");
  expect(hubStore.canInterrupt).toBe(false);
  await hubStore.cancel(INSTANCE);
  expect(cancel).not.toHaveBeenCalled();
  hubStore.setConnectionStateForTest("live");
  expect(hubStore.canInterrupt).toBe(true);
  // A quiet-but-linked (stale) session can still be interrupted; only offline
  // and recovering refuse (round-2 item 12).
  hubStore.setConnectionStateForTest("stale");
  expect(hubStore.canInterrupt).toBe(true);
  cancel.mockClear();
  cancel.mockResolvedValue(commandResult("cmd_cancel2", "settled"));
  await hubStore.cancel(INSTANCE);
  expect(cancel).toHaveBeenCalledTimes(1);
  hubStore.setConnectionStateForTest("recovering");
  expect(hubStore.canInterrupt).toBe(false);
  hubStore.setConnectionStateForTest("live");
  cancel.mockResolvedValue(commandResult("cmd_cancel", "settled"));
  await hubStore.cancel(INSTANCE);
  expect(cancel).toHaveBeenCalledTimes(2);
});

it("create is refused while disconnected (D-055 item 14)", async () => {
  const { api, hubStore } = await fresh();
  const create = vi.spyOn(api, "instanceCreate");
  hubStore.setConnectionStateForTest("offline");
  await expect(hubStore.create({} as never)).rejects.toThrow(/disconnected/);
  expect(create).not.toHaveBeenCalled();
  hubStore.setConnectionStateForTest("recovering");
  await expect(hubStore.create({} as never)).rejects.toThrow(/disconnected/);
  expect(create).not.toHaveBeenCalled();
});

it("a held row with repeated text is not settled by an older journal message (HIGH-2)", async () => {
  const { api, hubStore } = await fresh();
  const onBatch = await mountFollow(api, hubStore);
  // History already contains a user message "continue" (seq 1).
  onBatch({
    subscriptionId: "sub",
    journalId: JOURNAL,
    fromSeq: "1",
    toSeq: "1",
    durableSeq: "1",
    events: [
      {
        kind: "message",
        eventId: "evt_continue_1",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "1",
        payload: {
          nodeId: "u1",
          messageId: "u1",
          role: "user",
          phase: "input",
          revision: "1",
          baseRevision: null,
          operation: "replace",
          status: "complete",
          blocks: [{ type: "text", text: "continue" }],
          targetBlock: null,
          parentToolCallId: null,
          nativeOrigin: { state: "known", value: "user" },
          commandId: "cmd_old_continue",
        },
      } as unknown as Observation,
    ],
  });
  // Hold a NEW unsent "continue" while a turn is working (no commandId yet).
  const id = hubStore.hold(INSTANCE, "continue", "turn");
  // Any later journal event re-runs bubble settlement.
  onBatch({
    subscriptionId: "sub",
    journalId: JOURNAL,
    fromSeq: "2",
    toSeq: "2",
    durableSeq: "2",
    events: [
      {
        kind: "lifecycle",
        eventId: "evt_lc_2",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "2",
        payload: { type: "native", nativeName: "agent_status", status: "working" },
      } as unknown as Observation,
    ],
  });
  const bubble = hubStore.getSnapshot().bubbles.find((b) => b.clientRequestId === id)!;
  // Text matching was removed: the new unsent held row is still queued/held.
  expect(bubble.state).toBe("queued");
  expect(bubble.held).toBe(true);
  expect(bubble.commandId).toBeNull();
});

it("a steer while offline degrades to an ordinary queued turn", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  const send = vi.spyOn(api, "instanceSend");
  const id = hubStore.hold(INSTANCE, "offline steer", "turn");
  hubStore.setConnectionStateForTest("offline");
  const landed = await hubStore.steerHeld(INSTANCE, id);
  // NOT an authoritative acceptance: nothing was interrupted offline, so the
  // Composer gets false and never raises 已打断; the row is merely queued.
  expect(landed).toBe(false);
  expect(send).not.toHaveBeenCalled();
  const bubble = hubStore.getSnapshot().bubbles.find((b) => b.clientRequestId === id);
  expect(bubble?.promptMode).toBe("new-turn");
});

it("a queued 2xx that was forwarded is not re-POSTed by a later flush", async () => {
  // Regression from hub-live steer ordering: the fake Node answers a plain
  // {ok:true}, so the Hub row is queued + transport-written + reconciling. A
  // later outbox flush must NOT send it again (the journal settles it).
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, durableSeq: "0" } as Awaited<ReturnType<Api["instanceList"]>>["items"][number]],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as Awaited<ReturnType<Api["interactionList"]>>);

  const queuedForwarded = commandResult("cmd_acked_once", "accepted");
  // The fake non-durable ack: queued + forwarded (transport-written).
  queuedForwarded.command.state = "queued";
  queuedForwarded.command.dispatch = "transport-written";
  queuedForwarded.command.resolution = "reconciling";
  const send = vi.spyOn(api, "instanceSend").mockImplementation(
    async (_i, _p, _a, _m, clientCommandId) => ({
      relatedCommandIds: [],
      command: {
        ...queuedForwarded.command,
        commandId: clientCommandId ?? "cmd_acked_once",
        id: clientCommandId ?? "cmd_acked_once",
      },
    }),
  );

  await hubStore.send(INSTANCE, "a prompt the node acked loosely");
  await vi.waitFor(() => expect(send).toHaveBeenCalledTimes(1));
  const wireId = send.mock.calls[0]?.[4];
  expect(wireId?.startsWith("cmd_")).toBe(true);
  // A second flush (e.g. a turn-end edge) must not re-POST it.
  await hubStore["flushAllOutbox"]();
  await new Promise((r) => setTimeout(r, 20));
  expect(send).toHaveBeenCalledTimes(1);
  expect(hubStore.getSnapshot().bubbles[0]?.commandId).toBe(wireId);
});

it("offline enqueue then online flush POSTs each commandId exactly once (fake API)", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "0", durableSeq: "0", lifecycle: "running" } as never],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const postIds: string[] = [];
  vi.spyOn(api, "instanceSend").mockImplementation(
    ((_iid: string, _p: string, _r?: unknown[], _m?: string, commandId?: string) => {
      const id = commandId!;
      postIds.push(id);
      return Promise.resolve(commandResult(id, "accepted"));
    }) as Api["instanceSend"],
  );
  await hubStore.refresh();

  // Offline: two sends enqueue, zero POSTs.
  hubStore.setConnectionStateForTest("offline");
  await hubStore.send(INSTANCE, "off one");
  await hubStore.send(INSTANCE, "off two");
  expect(api.instanceSend).not.toHaveBeenCalled();
  const queuedIds = hubStore
    .getSnapshot()
    .bubbles.filter((b) => b.instanceId === INSTANCE && b.text.startsWith("off "))
    .map((b) => b.commandId!)
    .filter(Boolean);
  expect(queuedIds).toHaveLength(2);

  // Back online: one flush delivers both, same ids, once each.
  hubStore.setConnectionStateForTest("live");
  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  expect(postIds.sort()).toEqual([...queuedIds].sort());
  expect(postIds).toHaveLength(2);
});

it("pagehide then persisted pageshow re-enables the outbox and flushes (BFCache, item 6)", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "0", durableSeq: "0", lifecycle: "running" } as never],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const send = vi.spyOn(api, "instanceSend").mockResolvedValue(commandResult("cmd_bf", "accepted"));
  await hubStore.refresh();

  hubStore.setConnectionStateForTest("live");
  await hubStore.send(INSTANCE, "bfcache msg");
  // Simulate the first POST being retriable so a row is deliverable, then
  // pagehide (app backgrounded).
  expect((hubStore as unknown as { pageIsUnloadingForTest: boolean }).pageIsUnloadingForTest).toBe(false);
  await (hubStore as unknown as { pageShowForTest: (p: boolean) => Promise<void> }).pageShowForTest(true);
  // pagehide set the flag, persisted pageshow cleared it and flushed.
  expect((hubStore as unknown as { pageIsUnloadingForTest: boolean }).pageIsUnloadingForTest).toBe(false);
  await vi.waitFor(() => expect(send).toHaveBeenCalled());
});

it("a delayed unseen journal screen (load-earlier) never overwrites a newer list-poll RPC screen", async () => {
  // Item 7: the 2.5 s list poll starts a tty.screen read while the browser
  // only knows non-screen events through seq 3. It commits the live buffer.
  // A later load-earlier surfaces an unseen SCREEN observation at seq 1, and
  // the next live non-screen batch re-derives that old screen: the RPC basis
  // anchored on the greatest KNOWN event seq must keep the newer buffer.
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  vi.spyOn(api, "eventsRead").mockImplementation(
    async (args?: { afterSeq?: string; beforeSeq?: string }) => {
      // load-earlier window (beforeSeq set): the unseen older screen.
      if (args?.beforeSeq !== undefined) {
        return {
          events: [
            {
              kind: "raw_tty",
              eventId: "evt_unseen_screen_1",
              journalId: JOURNAL,
              instanceId: INSTANCE,
              seq: "1",
              payload: { text: "DONE unseen old task", nativeName: "screen", status: "done?" },
            } as unknown as Observation,
          ],
          durableSeq: "3",
          windowFromSeq: "1",
          reachedAfterSeq: true,
        };
      }
      // REST seed: a bounded tail of non-screen events 2..3 (no screen).
      return {
        events: [
          {
            kind: "message",
            eventId: "evt_msg_2",
            journalId: JOURNAL,
            instanceId: INSTANCE,
            seq: "2",
            completeness: "structured",
            payload: { role: "assistant", text: "working" },
          } as unknown as Observation,
          {
            kind: "lifecycle",
            eventId: "evt_life_3",
            journalId: JOURNAL,
            instanceId: INSTANCE,
            seq: "3",
            payload: { type: "native", nativeName: "agent_status", status: "working" },
          } as unknown as Observation,
        ],
        durableSeq: "3",
        windowFromSeq: "2",
        reachedAfterSeq: false,
      };
    },
  );
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_unseen",
    journalId: JOURNAL,
    durableSeq: "3",
    windowFromSeq: "2",
    reachedAfterSeq: false,
    getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_unseen",
      asOfSeq: "3",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "2", complete: false },
    },
  });
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "1", durableSeq: "3" } as Awaited<
      ReturnType<Api["instanceList"]>
    >["items"][number]],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as Awaited<ReturnType<Api["interactionList"]>>);
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: ["LIVE BUFFER from the list poll"] });
  await hubStore.refresh();
  await hubStore.follow(INSTANCE);
  expect(hubStore.getSnapshot().screens[INSTANCE]).toBeUndefined();

  // The list poll commits the current buffer. Its basis is the greatest known
  // JOURNAL seq (3), even though no screen observation has been seen.
  await hubStore.refreshScreen(INSTANCE);
  const rpc = hubStore.getSnapshot().screens[INSTANCE];
  expect(rpc?.lines).toEqual(["LIVE BUFFER from the list poll"]);
  expect(rpc?.journalSeq).toBe("3");

  // The user scrolls up: the unseen seq-1 screen joins the known events.
  expect(await hubStore.loadEarlier(INSTANCE)).toBeTruthy();

  // A fresh non-screen live batch (seq 4) re-derives the latest SCREEN (seq 1);
  // that stale screen must not roll the newer RPC buffer or its done badge.
  const onBatch = vi.mocked(api.eventsSubscribe).mock.calls[0]?.[2] as (
    batch: Record<string, unknown>,
  ) => void;
  onBatch({
    subscriptionId: "sub_unseen",
    journalId: JOURNAL,
    fromSeq: "4",
    toSeq: "4",
    durableSeq: "4",
    events: [
      {
        kind: "message",
        eventId: "evt_msg_4",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "4",
        completeness: "structured",
        payload: { role: "assistant", text: "still working" },
      } as unknown as Observation,
    ],
  });
  const screen = hubStore.getSnapshot().screens[INSTANCE];
  expect(screen?.lines).toEqual(["LIVE BUFFER from the list poll"]);
  expect(screen?.done).toBe(false);
  expect(screen?.journalSeq).toBe("3");
});

it("a cancelled beforeunload prompt unlatches outbox delivery via the scheduled setTimeout(0)", async () => {
  // Item 7: beforeunload fires even when the user is shown the browser's
  // "leave?" prompt and CANCELS — pagehide never fires, so the unload flag
  // must clear itself on the next task instead of latching delivery off.
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null } as Awaited<ReturnType<Api["hostList"]>>);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["deviceList"]>>);
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["passkeyList"]>>);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "0", durableSeq: "0", lifecycle: "running" } as never],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const send = vi.spyOn(api, "instanceSend").mockResolvedValue(commandResult("cmd_cancel_unload", "accepted"));
  await hubStore.bootstrap();
  hubStore.setConnectionStateForTest("live");

  // User triggers navigation but cancels the prompt: beforeunload fires, the
  // flag goes up synchronously, then the scheduled task clears it.
  window.dispatchEvent(new Event("beforeunload"));
  expect((hubStore as unknown as { pageIsUnloadingForTest: boolean }).pageIsUnloadingForTest).toBe(true);
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect((hubStore as unknown as { pageIsUnloadingForTest: boolean }).pageIsUnloadingForTest).toBe(false);

  // Delivery was not latched off: a queued row flushes.
  hubStore.frameForTest();
  await hubStore.send(INSTANCE, "after cancel");
  await vi.waitFor(() => expect(send).toHaveBeenCalled());
});
