import { afterEach, expect, it, vi } from "vitest";
import { OUTBOX_LS_KEY } from "./outbox";

const A = "ins_owed_a";
const JA = "obj_owed_a_journal";
const B = "ins_owed_b";
const JB = "obj_owed_b_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  const promise = new Promise<T>((res) => (resolve = res));
  return { promise, resolve };
}

type Internals = {
  reconcileOwed: Set<string>;
  flushAllOutbox: () => Promise<void>;
  resumeConnection: () => Promise<void>;
  outbox: { get: (id: string) => { state: string } | undefined; pendingFor: (id: string) => { commandId: string }[] };
  chainReconcile: (i: string, j: () => Promise<unknown>) => Promise<unknown>;
};

async function mountTwo() {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  const api = apiModule.api as Api;
  const hubStore = storeModule.hubStore as Store;

  vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null } as never);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as never);
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as never);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      { id: A, journalId: JA, revision: "0", durableSeq: "0", lifecycle: "running" },
      { id: B, journalId: JB, revision: "0", durableSeq: "0", lifecycle: "running" },
    ] as never,
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const eventsRead = vi
    .spyOn(api, "eventsRead")
    .mockResolvedValue({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    } as Awaited<ReturnType<Api["eventsRead"]>>);
  let ready = 1;
  let onClose: (() => void) | undefined;
  const subscribe = vi
    .spyOn(api, "eventsSubscribe")
    .mockImplementation(
      (async (_jid, _after, _onBatch, _onGap, hooks) => {
        if (hooks?.onClose && !onClose) onClose = hooks.onClose;
        const n = subscribe.mock.calls.length;
        return {
          subscriptionId: `sub_${n}`,
          journalId: n >= 2 ? JB : JA,
          durableSeq: "0",
          windowFromSeq: null,
          reachedAfterSeq: true,
          getReadyState: () => ready,
          snapshot: {
            projectionVersion: "v1",
            projectionEpoch: `epoch_${n}`,
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
    (async (_iid: string, _prompt: string, _att: unknown[], _mode: string | undefined, commandId: string) => ({
      relatedCommandIds: [],
      command: {
        commandId,
        id: commandId,
        revision: "1",
        createdAt: "2026-10-06T00:00:00.000Z",
        updatedAt: "2026-10-06T00:00:00.000Z",
        state: "sent",
        resolution: "unknown",
      },
    })) as unknown as Api["instanceSend"],
  );
  await hubStore.bootstrap();
  return {
    api,
    hubStore,
    subscribe,
    eventsRead,
    closeA: () => onClose?.(),
    setReady: (r: number) => {
      ready = r;
    },
  };
}

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

/**
 * c-reconnfu fix-5 item 1: A delivers while A's follow owns recovery (drain
 * records an owed catch-up, skips its own REST read). Opening B's live socket
 * REBINDS the machine away from A and cancels A's reconnect timer; A must
 * still get ONE coalesced REST fallback so its accepted row settles.
 */
it("a delivered row whose follow owner is rebound to another session still gets a REST fallback", async () => {
  const { api, hubStore, closeA, eventsRead } = await mountTwo();
  const internals = hubStore as unknown as Internals;

  await hubStore.follow(A);
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"));

  // Pin A's reconnect as PENDING and never reopening. In the real regression
  // A's reconnect timer is canceled by followRebindLive when B comes up
  // (before A reopens); an instant mock socket would otherwise race the drain
  // and flip the machine back to live on its own. With resume held the machine
  // stays offline/recovering and BOUND to A while A's journal chain stays free
  // for the flush.
  internals.resumeConnection = () => new Promise<void>(() => {});

  closeA();
  await vi.waitFor(() =>
    expect(["offline", "recovering"]).toContain(hubStore.connectionState),
  );

  // send() persists even offline; an explicit flush delivers it. The drain
  // job runs on A's free chain, sees A's bound follow own recovery, records
  // the debt, and skips its own REST read.
  await hubStore.send(A, "owed rebind");
  void internals.flushAllOutbox();
  await vi.waitFor(() => expect(api.instanceSend).toHaveBeenCalledTimes(1), { timeout: 2_000 });
  await vi.waitFor(() => expect(internals.reconcileOwed.has(A)).toBe(true), { timeout: 3_000 });
  const cid = ((api.instanceSend as unknown as { mock: { calls: unknown[][] } }).mock.calls[0]?.[4] as string) ?? "";
  expect(cid).toBeTruthy();

  // The owed REST fallback for A returns A's delivered user event. It must be
  // CONTIGUOUS from A's applied cursor (0): JournalClient.flush only emits a
  // gap-free prefix, so a seq-9 event would buffer forever and never fold.
  const aFallback = vi.fn();
  eventsRead.mockImplementation(
    ((req: { journalId: string }) => {
      if (req.journalId === JA) aFallback();
      return Promise.resolve(
        req.journalId === JA
          ? {
              events: [
                {
                  kind: "message",
                  eventId: `${JA}/1`,
                  journalId: JA,
                  instanceId: A,
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
                    blocks: [{ type: "text", text: "owed rebind" }],
                    targetBlock: null,
                    parentToolCallId: null,
                    nativeOrigin: { state: "known", value: "user" },
                    commandId: cid,
                  },
                },
              ],
              durableSeq: "1",
              windowFromSeq: null,
              reachedAfterSeq: true,
            }
          : { events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true },
      );
    }) as never,
  );

  // Open B live → bind A→B → the abandoned A instance gets its coalesced REST
  // fallback now, independent of the machine now bound to B.
  await hubStore.follow(B);
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"), { timeout: 3_000 });
  await vi.waitFor(() => expect(aFallback).toHaveBeenCalled(), { timeout: 5_000 });
  await vi.waitFor(() => expect(internals.reconcileOwed.has(A)).toBe(false), { timeout: 5_000 });
  await vi.waitFor(() => expect(internals.outbox.get(cid)?.state).toBe("done"), { timeout: 5_000 });

  hubStore.logout();
});

/**
 * c-reconnfu fix-5 item 2 / gate 6 item 3 (deterministic): ownership is
 * decided when the queued drain job RUNS. A send drains while live, its
 * post-delivery job waits behind an earlier slow catch-up J0, and the link
 * flips live→offline meanwhile; the executing job reads the CURRENT offline
 * state (record debt, skip its own REST) rather than a stale live decision.
 *
 * Determinism: the machine's resume is pinned to a never-resolving promise
 * BEFORE the socket closes, so no reconnect reopen can race the state back to
 * live (the old ~1/10 flake). There are no wall-clock waits: completion is
 * awaited via a sentinel job enqueued on the instance's own reconcile chain,
 * which runs strictly after the drain's tail job.
 */
it("decides follow ownership when the drain job runs, not when it was enqueued", async () => {
  const { api, hubStore, closeA } = await mountTwo();
  const internals = hubStore as unknown as Internals;
  await hubStore.follow(A);
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"));

  // Occupy A's journal chain with an earlier slow job J0; the send's
  // post-delivery tail job queues behind it.
  const j0 = deferred<void>();
  void internals.chainReconcile(A, () => j0.promise);

  await hubStore.send(A, "flip while queued");
  await vi.waitFor(() => expect(api.instanceSend).toHaveBeenCalledTimes(1));

  // Pin recovery FIRST, then close A: the machine settles offline and its
  // reconnect attempt hangs forever, so the bound state cannot flip back.
  internals.resumeConnection = () => new Promise<void>(() => {});
  closeA();
  await vi.waitFor(() =>
    expect(["offline", "recovering"]).toContain(hubStore.connectionState),
  );

  // Prove the drain would not itself run a REST resume even though the send
  // was enqueued while live.
  const { JournalClient } = await import("./journal");
  const resumeSpy = vi
    .spyOn(JournalClient.prototype, "resumeAfterReconnect")
    .mockImplementation((async () => {}) as never);

  // Release J0, then await a sentinel on A's chain: it runs strictly AFTER
  // the drain tail job whose decision this test asserts.
  j0.resolve();
  await internals.chainReconcile(A, async () => {});
  expect(internals.reconcileOwed.has(A)).toBe(true);
  expect(resumeSpy).not.toHaveBeenCalled();

  hubStore.logout();
});

