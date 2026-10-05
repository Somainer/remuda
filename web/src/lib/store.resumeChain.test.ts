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
 * c-reconnfu gate-flake regression: a queued row's post-delivery housekeeping
 * (resumeAfterReconnect + a /screen read) is chained on the per-instance
 * reconcile chain, and the connection resume's follow reopen serialises on
 * that SAME chain. When the flush was awaited FIRST, a /screen read stalled by
 * a saturated Node (gate load: REST timeout, NODE_BUSY backoff) parked the
 * socket reopen for tens of seconds AFTER the row had committed — the link sat
 * in recovering and the journal banner never cleared. The resume must reopen
 * the follow independently of the flush tail; the subscribe snapshot alone
 * certifies the link.
 */
afterEach(() => {
  vi.restoreAllMocks();
});

it("resume reopens the follow socket without waiting on the flush's post-delivery screen read", async () => {
  const { api, hubStore } = await fresh();
  let networkUp = true;
  // The Node's screen RPC is saturated for the whole test.
  const screenParked = deferred<{ lines: string[] }>();
  vi.spyOn(api, "screenRead").mockReturnValue(screenParked.promise);

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
  const eventsRead = vi
    .spyOn(api, "eventsRead")
    .mockImplementation(() =>
      networkUp
        ? Promise.resolve({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true })
        : Promise.reject(new Error("NETWORK_DOWN")),
    ) as unknown as Api["eventsRead"];

  // First subscribe is the live mount; the resume must open a SECOND one.
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

  vi.spyOn(api, "instanceSend").mockImplementation(
    (async (
      _iid: string,
      _prompt: string,
      _attachments: unknown[],
      _mode: string,
      commandId: string,
    ) => {
      if (!networkUp) throw new Error("NETWORK_DOWN");
      return {
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
      };
    }) as unknown as Api["instanceSend"],
  );
  // A lost-POST reconciliation while red must fail (row stays queued).
  vi.spyOn(api, "instanceCommandStatus").mockRejectedValue(new Error("NETWORK_DOWN"));

  await hubStore.bootstrap();
  await hubStore.follow(INSTANCE);
  await vi.waitFor(() => expect(subscribe).toHaveBeenCalledTimes(1));
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"));

  // The socket dies (no frames in flight) and every Hub call fails: machine
  // goes offline and its reconnect retries stay red.
  networkUp = false;
  socketReady = 3;
  firstOnClose!();
  await vi.waitFor(() => expect(["offline", "recovering"]).toContain(hubStore.connectionState));
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("offline"), { timeout: 5_000 });

  // A message queued while the link is down durably parks.
  await hubStore.send(INSTANCE, "recover behind a stalled screen read");

  // Hub is reachable again (REST + follow green); the NODE's screen read is
  // still parked for the entire test. A foreground resume kicks recovery.
  networkUp = true;
  socketReady = 1;
  hubStore.resumeActive(INSTANCE);

  // The follow reopen certifies the link without ever waiting on the flush
  // tail's chained /screen read (which never settles here). Pre-fix the reopen
  // was queued behind it, so the socket stayed at one subscription and the
  // machine never returned to live.
  await vi.waitFor(() => expect(subscribe).toHaveBeenCalledTimes(2), { timeout: 5_000 });
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"), { timeout: 5_000 });
  expect(eventsRead).toHaveBeenCalled();

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
