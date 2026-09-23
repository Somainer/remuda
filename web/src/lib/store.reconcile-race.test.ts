import { afterEach, expect, it, vi } from "vitest";
import type { CommandResult } from "../types/command";
import type { Observation } from "../types/observation";

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
  expect(hubStore.getSnapshot().connection).toBe("live");
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("live");

  // The bounded journal read fails (HTTP 502 / network drop): no events.
  vi.mocked(api.eventsRead).mockRejectedValue(new Error("HTTP 502"));
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => undefined);

  await hubStore.catchup(INSTANCE);

  // The per-session status SessionPage actually renders must not stay at
  // reconnecting: the client settles at the truthful readonly-stale state
  // (JournalBanner shows 只读 + 重试), and the global indicator follows.
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("readonly-stale");
  expect(hubStore.getSnapshot().connection).toBe("reconnecting");
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
  expect(hubStore.getSnapshot().connection).toBe("live");
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
  expect(hubStore.getSnapshot().connection).toBe("reconnecting");

  // No manual retry: the still-open follow socket delivers the next turn's
  // contiguous frame. applyBatch flushes it and the client returns to live,
  // clearing both the per-session banner and the global indicator.
  onBatch(screenBatch("2", "JOURNAL-2"));
  expect(hubStore.getSnapshot().journalStatus[INSTANCE]).toBe("live");
  expect(hubStore.getSnapshot().connection).toBe("live");
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
