import { afterEach, expect, it, vi } from "vitest";

const INSTANCE = "ins_follow_liveness";
const JOURNAL = "obj_follow_liveness_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

/**
 * ROUND5-3 regression: foreground/online liveness must read the socket's REAL
 * readyState at check time, not a value sampled once when the subscribe
 * promise resolved. The phone case: iOS silently expires the socket while the
 * tab is suspended — readyState becomes CLOSED (3) with NO close callback.
 */
afterEach(() => {
  vi.restoreAllMocks();
});

it("a foreground resume sees a silently-closed socket (no close event) and reopens", async () => {
  const { api, hubStore } = await fresh();

  // Mutable real readyState: starts OPEN, later goes CLOSED WITHOUT onClose.
  let socketReadyState = 1;

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
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async () => ({
      subscriptionId: "sub_follow_liveness",
      journalId: JOURNAL,
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
      // The probe reads the socket's CURRENT readyState on every call.
      getReadyState: () => socketReadyState,
      snapshot: {
        projectionVersion: "v1",
        projectionEpoch: "epoch_follow_liveness",
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

  await hubStore.bootstrap();
  await hubStore.follow(INSTANCE);
  await vi.waitFor(() => expect(subscribe).toHaveBeenCalledTimes(1));

  // Foreground resume with an OPEN, freshly-framed socket: trust the cached
  // live, do NOT reopen (one subscribe only).
  hubStore.resumeActive(INSTANCE);
  await new Promise((r) => setTimeout(r, 10));
  expect(subscribe).toHaveBeenCalledTimes(1);

  // The socket silently dies while suspended: real readyState is now CLOSED
  // but no close callback ever fires.
  socketReadyState = 3;

  // Foreground resume MUST observe the dead socket and reopen+catch up.
  hubStore.resumeActive(INSTANCE);
  await vi.waitFor(() => expect(subscribe).toHaveBeenCalledTimes(2), { timeout: 5_000 });
});

it("a connectivity change IS emitted even when every other field is equal (c-perffu r7 candidate-3)", async () => {
  const { api, hubStore } = await fresh();
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
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async () => ({
      subscriptionId: "sub_conn",
      journalId: JOURNAL,
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
        getReadyState: () => 1,
      snapshot: {
        projectionVersion: "v1",
        projectionEpoch: "epoch_conn",
        asOfSeq: "0",
        instance: {} as never,
        runs: [],
        commands: [],
        pendingInteractions: [],
        nodes: [],
        history: { earliestRetainedSeq: "1", complete: true },
      },
    })),
  );
  const connected = {
    id: INSTANCE,
    journalId: JOURNAL,
    revision: "0",
    durableSeq: "0",
    lifecycle: "running",
    connectivity: "connected",
  } as unknown as Awaited<ReturnType<Api["instanceList"]>>["items"][number];
  const list = vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [connected],
    nextCursor: null,
  });
  await hubStore.refresh();

  // Server marks the host offline: connectivity flips, all else equal.
  list.mockResolvedValue({
    items: [{ ...connected, connectivity: "disconnected" }],
    nextCursor: null,
  });
  await hubStore.refresh();
  expect(hubStore.getSnapshot().instances[0]?.connectivity).toBe("disconnected");

  // And recovery back to connected emits too.
  list.mockResolvedValue({ items: [connected], nextCursor: null });
  await hubStore.refresh();
  expect(hubStore.getSnapshot().instances[0]?.connectivity).toBe("connected");
});
