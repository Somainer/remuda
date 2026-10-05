import { afterEach, expect, it, vi } from "vitest";
import { OUTBOX_LS_KEY } from "./outbox";

const INSTANCE = "ins_screen_coalesce";
const JOURNAL = "obj_screen_coalesce_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (err: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/**
 * c-reconnfu fix-3: post-delivery screen refreshes used to append one chained
 * screen job per drained send, so a slow /screen accumulated unbounded pending
 * screen work. They now share the list scheduler's single-flight guard
 * (screenPending) and NODE_BUSY back-off, with at most ONE coalesced follow-up
 * demand per instance. These tests drive the real scheduler + real durable
 * delivery paths.
 */
async function fresh(): Promise<{
  api: Api;
  hubStore: Store;
  ScreenNodeBusyError: typeof import("./api").ScreenNodeBusyError;
}> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return {
    api: apiModule.api,
    hubStore: storeModule.hubStore,
    ScreenNodeBusyError: apiModule.ScreenNodeBusyError,
  };
}

async function mountLiveStore(): Promise<{
  api: Api;
  hubStore: Store;
  subscribe: ReturnType<typeof vi.spyOn>;
  eventsRead: ReturnType<typeof vi.fn>;
  ScreenNodeBusyError: typeof import("./api").ScreenNodeBusyError;
}> {
  const { api, hubStore, ScreenNodeBusyError } = await fresh();
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
    items: [
      {
        id: INSTANCE,
        journalId: JOURNAL,
        revision: "0",
        durableSeq: "0",
        lifecycle: "running",
      } as never,
    ],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const eventsRead = vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>) as unknown as ReturnType<typeof vi.fn>;
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async () => ({
      subscriptionId: `sub_coalesce_${subscribe.mock.calls.length + 1}`,
      journalId: JOURNAL,
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
      getReadyState: () => 1,
      snapshot: {
        projectionVersion: "v1",
        projectionEpoch: "epoch_coalesce",
        asOfSeq: "0",
        instance: {} as never,
        runs: [],
        commands: [],
        pendingInteractions: [],
        nodes: [],
        history: { earliestRetainedSeq: "0", complete: true },
      },
    })) as Api["eventsSubscribe"],
  );
  vi.spyOn(api, "instanceSend").mockImplementation(
    (async (
      _iid: string,
      _prompt: string,
      _attachments: unknown[],
      _mode: string,
      commandId: string,
    ) => ({
      relatedCommandIds: [],
      command: {
        commandId,
        id: commandId,
        revision: "1",
        createdAt: "2026-10-05T00:00:00.000Z",
        updatedAt: "2026-10-05T00:00:00.000Z",
        state: "completed",
        resolution: "completed",
      },
    })) as unknown as Api["instanceSend"],
  );

  await hubStore.bootstrap();
  await hubStore.follow(INSTANCE);
  await vi.waitFor(() => expect(subscribe).toHaveBeenCalledTimes(1));
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"));
  return { api, hubStore, subscribe, eventsRead, ScreenNodeBusyError };
}

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

/** Private scheduler surface the test asserts against (runtime fields). */
type SchedulerInternals = {
  kickScreenRefresh: (id: string) => void;
  outbox: { pendingFor: (id: string) => unknown[] };
  screenQueue: string[];
  screenCoalesced: Set<string>;
};

it("coalesces N fully-drained delivery refreshes into one follow-up reading the latest delivery", async () => {
  const { api, hubStore, eventsRead } = await mountLiveStore();
  const internals = hubStore as unknown as SchedulerInternals;

  const SENDS = 5;
  const prompts = Array.from({ length: SENDS }, (_, i) => `ordered message ${i}`);
  // The newest command the Node screen would render; advanced per send.
  let latestPrompt = "";

  // The FIRST read (the list fan-out's read) stays parked. Every LATER read
  // self-settles immediately and echoes the most recently delivered command —
  // no gate is left unreleased after the parked one.
  const firstGate = deferred<{ lines: string[] }>();
  let active = 0;
  let peak = 0;
  let readCount = 0;
  const screenRead = vi.spyOn(api, "screenRead").mockImplementation((id) => {
    expect(id).toBe(INSTANCE);
    readCount += 1;
    active += 1;
    peak = Math.max(peak, active);
    if (readCount === 1) {
      return firstGate.promise.finally(() => {
        active -= 1;
      });
    }
    return Promise.resolve({ lines: ["screen", latestPrompt] }).finally(() => {
      active -= 1;
    });
  });

  // Each post-delivery refresh REQUEST goes through kickScreenRefresh.
  const kick = vi.spyOn(internals, "kickScreenRefresh");

  hubStore.refreshScreens([INSTANCE]);
  await vi.waitFor(() => expect(screenRead).toHaveBeenCalledTimes(1));
  expect(active).toBe(1);
  expect(internals.screenQueue.length).toBe(0);

  // Drive the sends STRICTLY SERIALLY: the next send starts only after the
  // previous one has fully drained — the POST settled, the outbox empty, and
  // the post-delivery journal reconciliation plus its refresh REQUEST done.
  for (let i = 0; i < SENDS; i += 1) {
    const expectedSends = i + 1;
    const expectedKicks = kick.mock.calls.length + 1;
    const expectedReads = eventsRead.mock.calls.length + 1;
    latestPrompt = prompts[i]!;
    await hubStore.send(INSTANCE, prompts[i]!);

    await vi.waitFor(() => expect(api.instanceSend).toHaveBeenCalledTimes(expectedSends));
    await vi.waitFor(() => expect(internals.outbox.pendingFor(INSTANCE).length).toBe(0));
    // The drain's post-delivery refresh REQUEST fired, exactly once…
    await vi.waitFor(() => expect(kick.mock.calls.length).toBe(expectedKicks));
    // …after its journal-chain reconciliation (one resume read per drain).
    await vi.waitFor(() => expect(eventsRead.mock.calls.length).toBe(expectedReads));

    // …but while the first read is parked the demand is COALESCED: still one
    // active read, no queue buildup, no second RPC despite the fresh request.
    expect(screenRead).toHaveBeenCalledTimes(1);
    expect(peak).toBe(1);
    expect(active).toBe(1);
    expect(internals.screenQueue.length).toBe(0);
  }
  // Several (one per drain) refresh requests occurred, all collapsed onto the
  // single active read with exactly ONE coalesced follow-up pending.
  expect(kick.mock.calls.length).toBe(SENDS);
  const coalescedWhileParked = internals.screenCoalesced.size;
  expect(internals.screenQueue.length).toBe(0);

  // Release the parked read. It commits a stale buffer; the single coalesced
  // follow-up then reads the CURRENT (latest) delivery.
  firstGate.resolve({ lines: ["screen", "STALE-initial"] });
  // Decisive bound: with unbounded per-delivery enqueue this waits for 2 but
  // observes 1 + SENDS reads (one queued job per delivery).
  await vi.waitFor(() => expect(screenRead).toHaveBeenCalledTimes(2));
  await vi.waitFor(() => expect(active).toBe(0));
  // The coalesced pending demand really was a single flag while parked.
  expect(coalescedWhileParked).toBe(1);
  // No third read ever: the coalesced demand was exactly one follow-up.
  await new Promise((r) => setTimeout(r, 50));
  expect(screenRead).toHaveBeenCalledTimes(2);
  expect(peak).toBe(1);
  expect(internals.screenQueue.length).toBe(0);
  expect(internals.screenCoalesced.size).toBe(0);

  const committed = (hubStore as unknown as {
    state: { screens: Record<string, { lines: string[] }> };
  }).state.screens[INSTANCE]?.lines ?? [];
  expect(committed.join("\n")).toContain(prompts[SENDS - 1]!);
  expect(committed).not.toContain("STALE-initial");

  hubStore.logout();
});

it("a NODE_BUSY parked read's coalesced follow-up rides the shared back-off timer", async () => {
  const { api, hubStore, ScreenNodeBusyError } = await mountLiveStore();

  const firstGate = deferred<{ lines: string[] }>();
  const screenRead = vi.spyOn(api, "screenRead").mockImplementationOnce(() => firstGate.promise);
  hubStore.refreshScreens([INSTANCE]);
  await vi.waitFor(() => expect(screenRead).toHaveBeenCalledTimes(1));

  // Sends land while the read is parked → one coalesced demand.
  for (let i = 0; i < 3; i += 1) await hubStore.send(INSTANCE, `busy ${i}`);
  await vi.waitFor(() => expect(api.instanceSend).toHaveBeenCalledTimes(3));
  await new Promise((r) => setTimeout(r, 30));
  expect(screenRead).toHaveBeenCalledTimes(1);

  // The parked read answers NODE_BUSY: the coalesced follow-up must NOT hammer
  // the Node immediately — it rides the 80 ms back-off timer's re-arm.
  screenRead.mockResolvedValue({ lines: ["recovered"] });
  firstGate.reject(new ScreenNodeBusyError(80));
  await new Promise((r) => setTimeout(r, 30));
  expect(screenRead).toHaveBeenCalledTimes(1);
  await vi.waitFor(() => expect(screenRead).toHaveBeenCalledTimes(2));
  await new Promise((r) => setTimeout(r, 50));
  expect(screenRead).toHaveBeenCalledTimes(2);

  hubStore.logout();
});
