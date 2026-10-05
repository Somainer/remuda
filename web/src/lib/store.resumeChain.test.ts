import { afterEach, expect, it, vi } from "vitest";

const INSTANCE = "ins_resume_chain";
const JOURNAL = "obj_resume_chain_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

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
 * c-reconnfu gate-flake regression: a Node-bound /screen read stalled by a
 * saturated Node (gate load: REST timeout, NODE_BUSY back-off) must not park
 * the follow reopen. The screen read is started THROUGH THE REAL STORE PATH
 * (the list scheduler → refreshScreen({chained})), confirmed in flight, and
 * left unresolved; only then is the follow socket closed and recovery kicked.
 * The second subscription AND the live state MUST both arrive while the screen
 * RPC is still parked — the screen result is never released first.
 *
 * With the old shared chain (the screen read chained on chainReconcile, which
 * reopenFollow also joins), the reopen job queues behind the parked read and
 * the second subscription is never opened, so this test fails there.
 */
afterEach(() => {
  vi.restoreAllMocks();
});

it("recovery reopens the follow and certifies live while a real screen read is parked", async () => {
  const { api, hubStore } = await fresh();
  let networkUp = true;
  // The Node's screen RPC is saturated for the whole recovery.
  const screenParked = deferred<{ lines: string[] }>();
  const screenRead = vi.spyOn(api, "screenRead").mockReturnValue(screenParked.promise);

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
  vi.spyOn(api, "eventsRead").mockImplementation(
    () =>
      networkUp
        ? Promise.resolve({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true })
        : Promise.reject(new Error("NETWORK_DOWN")),
  ) as unknown as Api["eventsRead"];

  // First subscribe is the live mount; the recovery must open a SECOND one.
  let firstOnClose: (() => void) | undefined;
  let socketReady = 1;
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (_journalId, _afterSeq, _onBatch, _onGap, hooks) => {
      if (!networkUp) throw new Error("FOLLOW_CONNECT_FAILED");
      if (!firstOnClose && hooks?.onClose) firstOnClose = hooks.onClose;
      return {
        subscriptionId: `sub_${subscribe.mock.calls.length}`,
        journalId: JOURNAL,
        durableSeq: "0",
        windowFromSeq: null,
        reachedAfterSeq: true,
        getReadyState: () => socketReady,
        snapshot: {
          projectionVersion: "v1",
          projectionEpoch: `epoch_${subscribe.mock.calls.length}`,
          asOfSeq: "0",
          instance: {} as never,
          runs: [],
          commands: [],
          pendingInteractions: [],
          nodes: [],
          history: { earliestRetainedSeq: "0", complete: true },
        },
      };
    }) as Api["eventsSubscribe"],
  );

  await hubStore.bootstrap();
  await hubStore.follow(INSTANCE);
  await vi.waitFor(() => expect(subscribe).toHaveBeenCalledTimes(1));
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"));

  // Start a screen read through the REAL scheduler path and wait until the
  // Node RPC is actually in flight — then leave it unresolved for the rest of
  // the recovery.
  hubStore.refreshScreens([INSTANCE]);
  await vi.waitFor(() => expect(screenRead).toHaveBeenCalledTimes(1));

  // The follow socket dies and every Hub call fails: the machine goes offline
  // and its reconnect retries stay red.
  networkUp = false;
  socketReady = 3;
  firstOnClose!();
  await vi.waitFor(() => expect(["offline", "recovering"]).toContain(hubStore.connectionState));
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("offline"), { timeout: 5_000 });

  // Hub is reachable again (REST + follow green); the NODE's screen read is
  // STILL parked. A foreground resume kicks recovery.
  networkUp = true;
  socketReady = 1;
  hubStore.resumeActive(INSTANCE);

  // The second subscription must open and the link must certify live while the
  // screen RPC is parked — the screen result is released only afterwards. With
  // the old shared chain the reopen queued behind the parked read, so no
  // second subscription ever happened and this wait failed.
  await vi.waitFor(() => expect(subscribe).toHaveBeenCalledTimes(2), { timeout: 5_000 });
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"), { timeout: 5_000 });
  expect(screenRead).toHaveBeenCalledTimes(1);

  hubStore.logout();
  screenParked.resolve({ lines: [] });
});

/**
 * The split keeps the original ordering guarantee: a poll-driven /screen read
 * must still wait behind journal reconciliation already queued for the
 * instance (bea03e40 — an RPC read must not overtake a fresher catch-up), even
 * though the reverse (journal/link work waiting behind /screen) is gone.
 */
it("a queued screen read waits behind the journal resync, which never waits behind the screen read", async () => {
  const { api, hubStore } = await fresh();

  // Gate the resume's catch-up read; screen reads report when they start.
  let releaseJournalRead: (() => void) | null = null;
  const screenStarted: string[] = [];
  vi.spyOn(api, "screenRead").mockImplementation(
    (id) =>
      new Promise((resolve) => {
        screenStarted.push(id);
        resolve({ lines: [] });
      }),
  );

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
  // Initial seed reads resolve; only a read after `gateJournalRead` is armed
  // parks (the resume's resumeAfterReconnect catch-up).
  let gateJournalRead = false;
  vi.spyOn(api, "eventsRead").mockImplementation(
    () =>
      new Promise((resolve) => {
        const answer = { events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true };
        if (gateJournalRead && !releaseJournalRead) {
          releaseJournalRead = () => resolve(answer);
        } else {
          resolve(answer);
        }
      }),
  ) as unknown as Api["eventsRead"];
  // One live-probe value the store reads on every liveness check; flipped to
  // CLOSED (no close callback, like an iOS-silenced socket) so a foreground
  // resume really reopens, then back to OPEN for the reopened socket.
  let socketReady = 1;
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async () => ({
      subscriptionId: `sub_order_${subscribe.mock.calls.length}`,
      journalId: JOURNAL,
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
      getReadyState: () => socketReady,
      snapshot: {
        projectionVersion: "v1",
        projectionEpoch: `epoch_order_${subscribe.mock.calls.length}`,
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

  // The socket is silently CLOSED (no close event). Arm the catch-up gate,
  // then start a resume whose reopen job opens a fresh socket and parks on the
  // resumeAfterReconnect catch-up read.
  socketReady = 3;
  gateJournalRead = true;
  hubStore.resumeActive(INSTANCE);
  socketReady = 1;
  await vi.waitFor(() => expect(subscribe).toHaveBeenCalledTimes(2));
  await vi.waitFor(() => expect(releaseJournalRead).not.toBeNull());

  // The poll fan-out enqueues a screen read while the journal job is parked:
  // it must NOT start until the journal catch-up is released.
  hubStore.refreshScreens([INSTANCE]);
  await new Promise((r) => setTimeout(r, 30));
  expect(screenStarted).not.toContain(INSTANCE);

  releaseJournalRead!();
  await vi.waitFor(() => expect(screenStarted).toContain(INSTANCE));
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"));

  hubStore.logout();
});
