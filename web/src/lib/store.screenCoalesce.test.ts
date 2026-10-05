import { afterEach, expect, it, vi } from "vitest";
import { OUTBOX_LS_KEY } from "./outbox";

const INSTANCE = "ins_screen_coalesce";
const JOURNAL = "obj_screen_coalesce_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

type ScreenResult = { lines: string[] };
type ScreenDeferred = {
  promise: Promise<ScreenResult>;
  resolve: (value: ScreenResult | PromiseLike<ScreenResult>) => void;
  reject: (err: unknown) => void;
};

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
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  } as Awaited<ReturnType<Api["eventsRead"]>>);
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
  return { api, hubStore, subscribe, ScreenNodeBusyError };
}

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

it("coalesces post-delivery refreshes: one active read + one follow-up after many sends", async () => {
  const { api, hubStore } = await mountLiveStore();

  // Start ONE screen read through the real list-scheduler path and park it.
  const gates: ScreenDeferred[] = [];
  let active = 0;
  let peak = 0;
  const screenRead = vi.spyOn(api, "screenRead").mockImplementation(
    (id) => {
      active += 1;
      peak = Math.max(peak, active);
      expect(id).toBe(INSTANCE);
      const gate = deferred<{ lines: string[] }>();
      gates.push(gate);
      return gate.promise.finally(() => {
        active -= 1;
      });
    },
  );
  hubStore.refreshScreens([INSTANCE]);
  await vi.waitFor(() => expect(screenRead).toHaveBeenCalledTimes(1));
  expect(active).toBe(1);

  // Complete many sends while /screen is still parked. Each delivery's
  // post-delivery kick must collapse onto the single coalesced demand: no
  // second RPC starts, and at most one read is ever active for the instance.
  const SENDS = 10;
  for (let i = 0; i < SENDS; i += 1) {
    await hubStore.send(INSTANCE, `message ${i}`);
  }
  await vi.waitFor(() => expect(api.instanceSend).toHaveBeenCalledTimes(SENDS), { timeout: 5_000 });
  // Let every delivery's post-delivery journal settle/kick run.
  await new Promise((r) => setTimeout(r, 30));
  expect(screenRead).toHaveBeenCalledTimes(1);
  expect(peak).toBe(1);
  expect(active).toBe(1);

  // Releasing the parked read yields exactly ONE follow-up read (the coalesced
  // demand), not one per send.
  gates[0]!.resolve({ lines: ["screen"] });
  await vi.waitFor(() => expect(screenRead).toHaveBeenCalledTimes(2));
  await new Promise((r) => setTimeout(r, 50));
  expect(screenRead).toHaveBeenCalledTimes(2);
  expect(peak).toBe(1);

  gates[1]!.resolve({ lines: ["screen"] });
  await vi.waitFor(() => expect(active).toBe(0));
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
