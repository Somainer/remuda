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

it("a failed background catch-up restores the connection latch and surfaces the error", async () => {
  const { api, hubStore } = await fresh();
  const onBatch = await mountFollow(api, hubStore);
  expect(onBatch).toBeTypeOf("function");
  expect(hubStore.getSnapshot().connection).toBe("live");

  // Every journal read now fails: resumeAfterReconnect cannot complete.
  vi.mocked(api.eventsRead).mockRejectedValue(new Error("HTTP 502"));
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => undefined);

  await hubStore.catchup(INSTANCE);

  // The old awaited code latched the UI at 「重连中」; it must recover.
  expect(hubStore.getSnapshot().connection).toBe("live");
  expect(toast).toHaveBeenCalled();
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
