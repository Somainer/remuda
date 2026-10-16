import { afterEach, expect, it, vi } from "vitest";
import { RestNetworkError } from "./api";
import { OUTBOX_LS_KEY } from "./outbox";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

/**
 * c-unboundresume: with NO bound follow (session list / new-session page) a
 * resume must prove Hub reachability with exactly one bounded, auth-queued
 * REST GET before the machine can publish live. The old unbound branch
 * resolved without any awaited request and swallowed the trailing refresh
 * errors, so 已连接 was published while the Hub was unreachable and the
 * reconnect backoff never owned recovery.
 */
async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

/**
 * Boot to an UNBOUND, live machine: bootstrap succeeds, zero instances, no
 * follow mounted. Returns the instanceList spy (the authenticated bounded GET
 * both bootstrap and the unbound reachability probe use) for per-test steering.
 */
async function bootUnbound(): Promise<{
  api: Api;
  hubStore: Store;
  list: ReturnType<typeof vi.spyOn>;
}> {
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
  // Determinism: these suites assert connection state, not the background
  // drain (mirrors store.offlineRestore.test.ts); resolve (not undefined) so
  // resumeConnection's flushed.catch still works.
  vi.spyOn(hubStore, "flushAllOutbox").mockResolvedValue(undefined);
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const list = vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
  await hubStore.bootstrap();
  expect(hubStore.connectionState).toBe("live");
  list.mockClear();
  return { api, hubStore, list };
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

it("unbound resume with the Hub unreachable never publishes live and hands over to the reconnect backoff", async () => {
  const { hubStore, list } = await bootUnbound();
  const states: string[] = [];
  hubStore.subscribe(() => states.push(hubStore.getSnapshot().connection));

  // Every Hub read fails at the network layer (half-open link / refused).
  list.mockRejectedValue(new RestNetworkError("connect ECONNREFUSED"));
  hubStore.resumeActive(null);

  await vi.waitFor(() => expect(hubStore.getSnapshot().connection).toBe("offline"), { timeout: 4_000 });
  // The machine must never have certified live from the swallowed failure.
  expect(states).not.toContain("live");

  // Failure published, recovery belongs to the reconnect clock: after the
  // backoff another resume attempt issues another bounded probe.
  const callsWhenOffline = list.mock.calls.length;
  await vi.waitFor(() => expect(list.mock.calls.length).toBeGreaterThan(callsWhenOffline), {
    timeout: 4_000,
  });
  // Still unreachable: never live.
  expect(hubStore.getSnapshot().connection).not.toBe("live");

  hubStore.logout();
});

it("unbound resume stays non-live while the bounded GET is stalled and goes offline once the read deadline rejects", async () => {
  const { hubStore, list } = await bootUnbound();
  const states: string[] = [];
  hubStore.subscribe(() => states.push(hubStore.getSnapshot().connection));

  // The first (resume) request stalls, exactly as a half-open forward does
  // until rest()'s internal read deadline aborts it; every later read already
  // sees the dead link. (The 10 s deadline mechanics themselves are covered by
  // api.restTimeout.test.ts; the store observes their RestNetworkError.)
  let releaseStall: (() => void) | null = null;
  let stalled = true;
  list.mockImplementation(
    () =>
      new Promise((_resolve, reject) => {
        if (!stalled) {
          reject(new RestNetworkError("REST timeout after 10000 ms"));
          return;
        }
        releaseStall = () => {
          stalled = false;
          reject(new RestNetworkError("REST timeout after 10000 ms"));
        };
      }),
  );

  hubStore.resumeActive(null);
  await vi.waitFor(() => expect(hubStore.getSnapshot().connection).toBe("recovering"));
  // While the bounded request has not answered, the machine is mid-recovery —
  // never a false live.
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(hubStore.getSnapshot().connection).toBe("recovering");
  expect(states).not.toContain("live");

  releaseStall!();
  await vi.waitFor(() => expect(hubStore.getSnapshot().connection).toBe("offline"), { timeout: 4_000 });
  expect(states).not.toContain("live");

  hubStore.logout();
});

it("unbound resume publishes live only after exactly one successful bounded probe", async () => {
  const { hubStore, list } = await bootUnbound();

  // Gate the single probe behind an unresolved promise.
  let releaseProbe: (() => void) | null = null;
  list.mockImplementation(
    () =>
      new Promise((resolve) => {
        releaseProbe = () => resolve({ items: [], nextCursor: null });
      }),
  );

  hubStore.resumeActive(null);
  await vi.waitFor(() => expect(hubStore.getSnapshot().connection).toBe("recovering"));
  // The single probe acquires the S3 auth queue (a couple of microtask hops),
  // then is in flight exactly once and may not certify live unanswered.
  await vi.waitFor(() => expect(list).toHaveBeenCalledTimes(1));
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(hubStore.getSnapshot().connection).toBe("recovering");

  releaseProbe!();
  await vi.waitFor(() => expect(hubStore.getSnapshot().connection).toBe("live"), { timeout: 4_000 });
  // Exactly one reachability probe gates live; no second, parallel auth read.
  expect(list).toHaveBeenCalledTimes(1);

  hubStore.logout();
});

it("r6 item 2: a held reopen subscription resolved after supersession opens/books nothing, and a stale resync rejection never marks readonly-stale", async () => {
  const { api, hubStore } = await bootUnbound();
  const A = "ins_resync_a";
  const JA = "obj_resync_a_journal";
  const B = "ins_resync_b";
  const JB = "obj_resync_b_journal";

  // Both instances cached so follow() binds immediately; A mounts once at
  // setup, then a recovery reopen for A is steered.
  const rec = (id: string, j: string) =>
    ({ id, journalId: j, revision: "0", durableSeq: "0", lifecycle: "running" } as never);
  vi.spyOn(api, "instanceGet").mockImplementation(async (id: string) =>
    id === A ? rec(A, JA) : rec(B, JB));
  const list = vi.mocked(api.instanceList);
  list.mockResolvedValue({ items: [rec(A, JA), rec(B, JB)], nextCursor: null });
  // Seed reads answer immediately. A's reopen resync (the next JA read after
  // the seed) is HELD and later rejected to simulate a half-open link after
  // navigation; every later read answers.
  const emptyPage = {
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  };
  let readCalls = 0;
  let rejectHeldResync: ((e: unknown) => void) | null = null;
  vi.spyOn(api, "eventsRead").mockImplementation(
    ((args: { journalId: string }) => {
      readCalls += 1;
      if (readCalls === 2 && args.journalId === JA && !rejectHeldResync) {
        return new Promise((_resolve, reject) => {
          rejectHeldResync = (err) => reject(err);
        });
      }
      return Promise.resolve(emptyPage);
    }) as Api["eventsRead"],
  );

  // Subscribe #1 (initial A mount) resolves live immediately. Subscribe #2
  // (A's held reopen) is held until the test releases it.
  let subCalls = 0;
  let releaseHeldSub: (() => void) | null = null;
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    ((((journalId: string) => {
      subCalls += 1;
      if (subCalls === 2 && journalId === JA) {
        return new Promise((resolve) => {
          releaseHeldSub = () =>
            resolve({
              subscriptionId: "sub_a_reopen",
              journalId: JA,
              durableSeq: "0",
              windowFromSeq: null,
              reachedAfterSeq: true,
              getReadyState: () => 1,
              snapshot: {
                projectionVersion: "v1",
                projectionEpoch: "epoch_a_reopen",
                asOfSeq: "0",
                instance: {} as never,
                runs: [],
                commands: [],
                pendingInteractions: [],
                nodes: [],
                history: { earliestRetainedSeq: "0", complete: true },
              },
            });
        });
      }
      return Promise.resolve({
        subscriptionId: `sub_${subCalls}_${journalId}`,
        journalId,
        durableSeq: "0",
        windowFromSeq: null,
        reachedAfterSeq: true,
        getReadyState: () => 1,
        snapshot: {
          projectionVersion: "v1",
          projectionEpoch: `epoch_${subCalls}_${journalId}`,
          asOfSeq: "0",
          instance: {} as never,
          runs: [],
          commands: [],
          pendingInteractions: [],
          nodes: [],
          history: { earliestRetainedSeq: "0", complete: true },
        },
      });
    }) as unknown) as Api["eventsSubscribe"]),
  );

  const flush = async (rounds = 60) => {
    for (let i = 0; i < rounds; i += 1) await Promise.resolve();
  };

  // Initial A mount (live socket).
  await hubStore.follow(A);
  await flush();
  expect(hubStore.getSnapshot().connection).toBe("live");
  const internal = hubStore as unknown as {
    setFollowLiveForTest: (o: boolean, f: boolean, j?: string) => void;
    journals: Map<string, { status: string }>;
    connectionBoundTo: string | null;
    connectionBoundJournal: string | null;
  };
  internal.setFollowLiveForTest(true, true, JA);
  const journalStatus = () => internal.journals.get(JA)?.status;
  expect(journalStatus()).toBe("live");

  // A recovery reopen with a HELD subscription (socket not yet open).
  const reopen = (
    hubStore as unknown as { reopenFollow: (id: string) => Promise<void> }
  ).reopenFollow(A);
  void reopen.catch(() => undefined);
  await vi.waitFor(() => expect(subCalls).toBe(2));
  // Mark the held socket dead before it opens (the reopen must not certify a
  // dead socket); opening re-registers readiness from the live response.
  internal.setFollowLiveForTest(false, false, JA);

  // The subscription resolves WHILE A IS STILL THE OWNER: the socket opens
  // legitimately and the resync read (#3) starts — held.
  releaseHeldSub!();
  await flush();
  expect(internal.connectionBoundTo).toBe(A);
  await vi.waitFor(() => expect(rejectHeldResync).not.toBeNull());

  // Ownership changes DURING the resync: away to B, then back to A (the
  // reopened socket is adopted live).
  await hubStore.follow(B);
  await flush();
  expect(internal.connectionBoundTo).toBe(B);
  await hubStore.follow(A);
  await flush();
  expect(internal.connectionBoundTo).toBe(A);
  internal.setFollowLiveForTest(true, true, JA);
  expect(journalStatus()).toBe("live");

  // The old resync read now rejects: its ownership token is stale (new A
  // generation), so it must be a silent stale attempt — NO readonly-stale on
  // the current live A and no machine disturbance.
  rejectHeldResync!(new RestNetworkError("resync 502"));
  await flush();
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(journalStatus()).not.toBe("readonly-stale");
  expect(hubStore.getSnapshot().connection).toBe("live");

  hubStore.logout();
});

it("r6 item 1: leaving a failed pending route releases it; the list certifies via the unbound proof and A retries stop", async () => {
  const { api, hubStore, list } = await bootUnbound();
  const A = "ins_dead_route";

  // Every list/probe GET answers; A's lookup ALWAYS fails (deleted/uncached).
  list.mockResolvedValue({ items: [], nextCursor: null });
  let lookupCalls = 0;
  vi.spyOn(api, "instanceGet").mockImplementation(() => {
    lookupCalls += 1;
    return Promise.reject(new RestNetworkError("404 lookup failed"));
  });

  vi.useFakeTimers();
  const flush = async (rounds = 40) => {
    for (let i = 0; i < rounds; i += 1) await vi.advanceTimersByTimeAsync(0);
  };

  // Navigate to the uncached session through the REAL route entry; its lookup
  // fails and its recovery starts owning the machine.
  const routeFollow = hubStore.follow(A);
  void routeFollow.catch(() => undefined);
  await vi.waitFor(() => expect(lookupCalls).toBeGreaterThanOrEqual(1));
  await flush();
  expect(hubStore.getSnapshot().connection).not.toBe("live");

  // Return to the list: SessionPage unmount releases the route, and the next
  // foreground resume happens off /s/<id>.
  hubStore.releaseRouteTarget(A);
  hubStore.resumeActive(null);
  await flush();
  for (let i = 0; i < 60 && hubStore.getSnapshot().connection !== "live"; i += 1) {
    await vi.advanceTimersByTimeAsync(250);
  }
  expect(hubStore.getSnapshot().connection).toBe("live");
  // Writes are accepted on the list page (not offline/recovering).
  expect(hubStore.canInterrupt).toBe(true);

  // Across several later probe/watchdog cycles the departed route must never
  // be retried; the list's unbound probes are list GETs, not A lookups.
  const lookupsAtLive = lookupCalls;
  for (let i = 0; i < 45; i += 1) await vi.advanceTimersByTimeAsync(1_000);
  expect(lookupCalls).toBe(lookupsAtLive);
  expect(hubStore.getSnapshot().connection).toBe("live");

  vi.useRealTimers();
  hubStore.logout();
});

it("r5 item 1: a held unbound list GET cannot certify a session entered through the real follow() route; its lookup failure runs its own recovery", async () => {
  const { api, hubStore, list } = await bootUnbound();
  const TARGET = "ins_route_entry";
  const JOURNAL = "obj_route_entry_journal";

  // First list call (the unbound resume proof) stays held; later list calls
  // (settlement / stale probes) answer immediately.
  let releaseOld: (() => void) | null = null;
  let firstHeld = false;
  list.mockImplementation(
    () =>
      new Promise((resolve) => {
        if (firstHeld) {
          resolve({ items: [], nextCursor: null });
          return;
        }
        firstHeld = true;
        releaseOld = () => resolve({ items: [], nextCursor: null });
      }),
  );
  // The route-entry lookup fails once (its initial follow lookup) and resolves
  // on the recovery retry.
  let lookupCalls = 0;
  let rejectLookup: ((e: unknown) => void) | null = null;
  vi.spyOn(api, "instanceGet").mockImplementation(() => {
    lookupCalls += 1;
    if (lookupCalls === 1) {
      return new Promise((_resolve, reject) => {
        rejectLookup = (err) => reject(err);
      });
    }
    return Promise.resolve({
      id: TARGET,
      journalId: JOURNAL,
      revision: "0",
      durableSeq: "0",
      lifecycle: "running",
    } as Awaited<ReturnType<Api["instanceGet"]>>);
  });
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async () => ({
      subscriptionId: "sub_route_entry",
      journalId: JOURNAL,
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
      getReadyState: () => 1,
      snapshot: {
        projectionVersion: "v1",
        projectionEpoch: "epoch_route_entry",
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

  // Unbound list resume, its GET held.
  hubStore.resumeActive(null);
  await vi.waitFor(() => expect(list).toHaveBeenCalledTimes(1));

  // REAL route entry (SessionPage effect calls hubStore.follow(id)) while the
  // old list GET is still parked and the target's lookup is pending.
  vi.useFakeTimers();
  const flush = async (rounds = 40) => {
    for (let i = 0; i < rounds; i += 1) await vi.advanceTimersByTimeAsync(0);
  };
  const routeFollow = hubStore.follow(TARGET);
  void routeFollow.catch(() => undefined);
  await vi.waitFor(() => expect(lookupCalls).toBe(1));
  await flush();

  // The old list GET answers: the target was chosen synchronously at follow
  // entry, so its generation superseded the unbound attempt — no live.
  releaseOld!();
  await flush();
  for (let i = 0; i < 20 && hubStore.getSnapshot().connection === "recovering"; i += 1) {
    await vi.advanceTimersByTimeAsync(50);
  }
  expect(hubStore.getSnapshot().connection).not.toBe("live");
  expect(subscribe).not.toHaveBeenCalled();

  // The lookup failure is THIS target's recovery failure: the machine settles
  // via probe and the reconnect path retries the lookup (2nd call), then a
  // real seed + subscribe certifies live.
  rejectLookup!(new RestNetworkError("lookup ECONNRESET"));
  await expect(routeFollow).rejects.toBeTruthy();
  await flush();
  for (let i = 0; i < 120 && subscribe.mock.calls.length === 0; i += 1) {
    await vi.advanceTimersByTimeAsync(1_000);
  }
  await flush();
  expect(lookupCalls).toBeGreaterThan(1);
  expect(subscribe).toHaveBeenCalledTimes(1);
  expect(hubStore.getSnapshot().connection).toBe("live");

  vi.useRealTimers();
  hubStore.logout();
});

it("r5 item 2: a superseded pending lookup cannot reclaim the binding — A resolves after B is chosen, B mounts", async () => {
  const { api, hubStore } = await bootUnbound();
  const A = "ins_superseded_a";
  const JA = "obj_superseded_a_journal";
  const B = "ins_superseded_b";
  const JB = "obj_superseded_b_journal";

  // Lookup calls 1 (A) and 2 (B) are held independently; releasing an id
  // resolves that id's held lookup AND every later lookup for the same id
  // (reopenFollow → follow re-fetches the uncached record on mount).
  const records: Record<string, { id: string; journalId: string }> = {
    [A]: { id: A, journalId: JA },
    [B]: { id: B, journalId: JB },
  };
  const released = new Set<string>();
  const waiters = new Map<string, Array<() => void>>();
  vi.spyOn(api, "instanceGet").mockImplementation(
    (id: string) =>
      new Promise((resolve) => {
        const answer = () =>
          resolve({
            ...records[id],
            revision: "0",
            durableSeq: "0",
            lifecycle: "running",
          } as Awaited<ReturnType<Api["instanceGet"]>>);
        if (released.has(id)) {
          answer();
          return;
        }
        const list = waiters.get(id) ?? [];
        list.push(answer);
        waiters.set(id, list);
      }),
  );
  const release = (id: string) => {
    released.add(id);
    for (const answer of waiters.get(id) ?? []) answer();
    waiters.delete(id);
  };
  const seedCalls: string[] = [];
  vi.spyOn(api, "eventsRead").mockImplementation(((args: { journalId: string }) => {
    seedCalls.push(args.journalId);
    return Promise.resolve({
      events: [],
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
    });
  }) as Api["eventsRead"]);
  const subscribeJournals: string[] = [];
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    ((((journalId: string) => {
      subscribeJournals.push(journalId);
      return Promise.resolve({
        subscriptionId: `sub_${journalId}`,
        journalId,
        durableSeq: "0",
        windowFromSeq: null,
        reachedAfterSeq: true,
        getReadyState: () => 1,
        snapshot: {
          projectionVersion: "v1",
          projectionEpoch: `epoch_${journalId}`,
          asOfSeq: "0",
          instance: {} as never,
          runs: [],
          commands: [],
          pendingInteractions: [],
          nodes: [],
          history: { earliestRetainedSeq: "0", complete: true },
        },
      });
    }) as unknown) as Api["eventsSubscribe"]),
  );
  const internal = hubStore as unknown as {
    connectionBoundTo: string | null;
    connectionBoundJournal: string | null;
  };
  const flush = async (rounds = 60) => {
    for (let i = 0; i < rounds; i += 1) await Promise.resolve();
  };

  // A chosen first, its lookup held; B chosen next (new owner), held too.
  hubStore.resumeActive(A);
  await vi.waitFor(() => expect(waiters.has(A)).toBe(true));
  hubStore.resumeActive(B);
  await vi.waitFor(() => expect(waiters.has(B)).toBe(true));
  await flush();
  expect(internal.connectionBoundTo).toBe(B);

  // A's stale lookup resolves and its queued reopen/seed work runs as far as
  // it can: it must NOT rebind, seed, or subscribe.
  release(A);
  await flush();
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(internal.connectionBoundTo).toBe(B);
  expect(internal.connectionBoundJournal).toBeNull();
  expect(hubStore.getSnapshot().connection).not.toBe("live");
  expect(seedCalls).not.toContain(JA);
  expect(subscribeJournals).not.toContain(JA);

  // B's lookup resolves: only B seeds/subscribes and certifies live.
  release(B);
  await vi.waitFor(() => expect(subscribeJournals).toContain(JB), { timeout: 4_000 });
  await flush();
  expect(internal.connectionBoundTo).toBe(B);
  expect(internal.connectionBoundJournal).toBe(JB);
  expect(seedCalls).toEqual([JB]);
  expect(subscribeJournals).toEqual([JB]);
  expect(hubStore.getSnapshot().connection).toBe("live");

  hubStore.logout();
});

it("r4 item 2: a held unbound list request cannot certify a NEWLY pending session; live waits for its seed+subscribe", async () => {
  const { api, hubStore, list } = await bootUnbound();
  const PENDING = "ins_pending_midresume";
  const JOURNAL = "obj_pending_midresume_journal";

  // The FIRST list call (the list resume's reachability proof) stays held;
  // every later list call (settlement / stale probes) answers immediately.
  let releaseOld: (() => void) | null = null;
  let firstHeld = false;
  list.mockImplementation(
    () =>
      new Promise((resolve) => {
        if (firstHeld) {
          resolve({ items: [], nextCursor: null });
          return;
        }
        firstHeld = true;
        releaseOld = () => resolve({ items: [], nextCursor: null });
      }),
  );

  // The pending session's lookup fails once (its initial follow lookup also
  // failed — the page only has a target id) and resolves on the next
  // recovery attempt.
  let lookupCalls = 0;
  vi.spyOn(api, "instanceGet").mockImplementation(() => {
    lookupCalls += 1;
    if (lookupCalls < 2) return Promise.reject(new RestNetworkError("lookup ECONNRESET"));
    return Promise.resolve({
      id: PENDING,
      journalId: JOURNAL,
      revision: "0",
      durableSeq: "0",
      lifecycle: "running",
    } as Awaited<ReturnType<Api["instanceGet"]>>);
  });
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async () => ({
      subscriptionId: "sub_pending_midresume",
      journalId: JOURNAL,
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
      getReadyState: () => 1,
      snapshot: {
        projectionVersion: "v1",
        projectionEpoch: "epoch_pending_midresume",
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

  vi.useFakeTimers();
  const flush = async (rounds = 40) => {
    for (let i = 0; i < rounds; i += 1) await vi.advanceTimersByTimeAsync(0);
  };

  // 1) Foreground unbound resume on the list; hold its reachability GET.
  hubStore.resumeActive(null);
  await flush();
  expect(hubStore.getSnapshot().connection).toBe("recovering");
  expect(list).toHaveBeenCalledTimes(1);

  // 2) Navigate to the uncached session (its lookup fails) and come back to
  // the foreground: Shell calls resumeActive(id). Entering the pending target
  // must be an ownership transition that supersedes the held unbound attempt.
  hubStore.resumeActive(PENDING);
  await flush();

  // 3) The OLD list request answers successfully. With the old code it
  // certified live for the pending session (no generation change → the resume
  // coalesced and its success published live before any seed/subscribe).
  const stateAtSubscribe: string[] = [];
  const rawSubscribe = subscribe.getMockImplementation();
  subscribe.mockImplementation(((...args: unknown[]) => {
    stateAtSubscribe.push(hubStore.getSnapshot().connection);
    return (rawSubscribe as (...a: unknown[]) => unknown)(...args);
  }) as Api["eventsSubscribe"]);
  releaseOld!();
  await flush();
  for (let i = 0; i < 30 && hubStore.getSnapshot().connection === "recovering"; i += 1) {
    await vi.advanceTimersByTimeAsync(100);
  }
  expect(hubStore.getSnapshot().connection).not.toBe("live");
  expect(stateAtSubscribe).toHaveLength(0);

  // 4) Recovery continues quietly: the stale probe proves REST reachable, the
  // next reopen retries the pending lookup — which now mounts for real. Live
  // is published only once seed + subscribe completed (not when it starts).
  for (let i = 0; i < 60 && stateAtSubscribe.length === 0; i += 1) {
    await vi.advanceTimersByTimeAsync(1_000);
  }
  await flush();
  expect(stateAtSubscribe).toHaveLength(1);
  expect(stateAtSubscribe[0]).not.toBe("live");
  expect(hubStore.getSnapshot().connection).toBe("live");

  vi.useRealTimers();
  hubStore.logout();
});

it("r3 item 1: a pending session target is never certified live by successful list probes; follow recovery must seed+subscribe first", async () => {
  const { api, hubStore, list } = await bootUnbound();
  const PENDING = "ins_pending_target";
  const JOURNAL = "obj_pending_target_journal";

  // Every LIST probe answers (REST reachable), but the pending session's own
  // lookup fails twice and only the third recovery attempt resolves it.
  list.mockResolvedValue({ items: [], nextCursor: null });
  const instanceGet = vi.spyOn(api, "instanceGet");
  instanceGet.mockReset();
  let lookupCalls = 0;
  instanceGet.mockImplementation(() => {
    lookupCalls += 1;
    if (lookupCalls < 3) return Promise.reject(new RestNetworkError("lookup ECONNRESET"));
    return Promise.resolve({
      id: PENDING,
      journalId: JOURNAL,
      revision: "0",
      durableSeq: "0",
      lifecycle: "running",
    } as Awaited<ReturnType<Api["instanceGet"]>>);
  });
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async () => ({
      subscriptionId: "sub_pending_target",
      journalId: JOURNAL,
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
      getReadyState: () => 1,
      snapshot: {
        projectionVersion: "v1",
        projectionEpoch: "epoch_pending_target",
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

  // Machine timers run on fake time (15 s stale probe, 30 s episode deadline).
  vi.useFakeTimers();
  const flush = async (rounds = 30) => {
    for (let i = 0; i < rounds; i += 1) await vi.advanceTimersByTimeAsync(0);
  };
  const tickUntil = async (pred: () => boolean) => {
    for (let i = 0; i < 60; i += 1) {
      if (pred()) return true;
      await vi.advanceTimersByTimeAsync(1_000);
    }
    return pred();
  };
  const states: string[] = [];
  hubStore.subscribe(() => states.push(hubStore.getSnapshot().connection));

  // A session page whose initial follow lookup failed: foreground resume
  // records a PENDING target (boundTo=id, boundJournal=null).
  hubStore.resumeActive(PENDING);
  await flush();
  // The resume lookup failed and the settlement list probe succeeded ->
  // quiet stale, NOT live.
  expect(lookupCalls).toBe(1);
  expect(hubStore.getSnapshot().connection).toBe("stale");
  expect(states).not.toContain("live");

  // Timer-driven recovery cycles: the stale probes keep answering ok. A
  // pending target must run follow recovery (instanceGet retried); it must
  // NEVER take the unbound probe's direct-live certification.
  expect(await tickUntil(() => lookupCalls >= 2)).toBe(true);
  await flush();
  expect(hubStore.getSnapshot().connection).not.toBe("live");
  expect(states).not.toContain("live");
  expect(subscribe).not.toHaveBeenCalled();
  const statesBeforeMount = [...states];

  // The third lookup resolves: a real seed + subscribe completes the mount,
  // and only THEN may the machine publish live.
  expect(await tickUntil(() => subscribe.mock.calls.length >= 1)).toBe(true);
  await flush();
  expect(hubStore.getSnapshot().connection).toBe("live");
  expect(statesBeforeMount).not.toContain("live");
  expect(lookupCalls).toBeGreaterThanOrEqual(3);

  vi.useRealTimers();
  hubStore.logout();
});

it("r2 item 1: a held unbound probe cannot certify the first follow before its seed; the seed's failure runs bound recovery", async () => {
  const { api, hubStore, list } = await bootUnbound();
  const states: string[] = [];
  hubStore.subscribe(() => states.push(hubStore.getSnapshot().connection));

  // Foreground unbound resume on the list page; hold its single GET.
  let releaseProbe: (() => void) | null = null;
  list.mockImplementation(
    () =>
      new Promise((resolve) => {
        releaseProbe = () => resolve({ items: [], nextCursor: null });
      }),
  );
  hubStore.resumeActive(null);
  await vi.waitFor(() => expect(list).toHaveBeenCalledTimes(1));
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(hubStore.getSnapshot().connection).toBe("recovering");

  // Navigate into the FIRST session while that probe is parked. The mount's
  // journal seed is held as well.
  const INSTANCE = "ins_unbound_bind";
  const JOURNAL = "obj_unbound_bind_journal";
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
    revision: "0",
    durableSeq: "0",
    lifecycle: "running",
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  let seedCalls = 0;
  let releaseSeed: ((err: unknown) => void) | null = null;
  vi.spyOn(api, "eventsRead").mockImplementation(
    () =>
      new Promise((_resolve, reject) => {
        seedCalls += 1;
        if (seedCalls === 1) releaseSeed = (err) => reject(err);
        else reject(new RestNetworkError("connect ECONNREFUSED"));
      }),
  );
  // The seed is held, so a subscribe must never be attempted.
  vi.spyOn(api, "eventsSubscribe").mockImplementation(() => {
    throw new Error("subscribe must not open while the seed is held");
  });

  const mount = hubStore.follow(INSTANCE);
  void mount.catch(() => undefined);
  await vi.waitFor(() => expect(seedCalls).toBe(1));

  // The list probe answers FIRST. The old null->journal binding kept the same
  // generation, so the follow reused the unbound attempt: the probe consumed
  // it and published live before the follow seeded/opened.
  releaseProbe!();
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(hubStore.getSnapshot().connection).not.toBe("live");
  expect(states).not.toContain("live");

  // The seed then fails. Its failure MUST reach the mount's OWN attempt:
  // settle probe fails -> offline with the reconnect clock armed, and the
  // clock re-attempts the bound mount seed (bound-follow recovery runs).
  releaseSeed!(new RestNetworkError("seed ECONNRESET"));
  await vi.waitFor(() => expect(hubStore.getSnapshot().connection).toBe("offline"), { timeout: 4_000 });
  expect(states).not.toContain("live");
  const seedCallsAtOffline = seedCalls;
  await vi.waitFor(() => expect(seedCalls).toBeGreaterThan(seedCallsAtOffline), { timeout: 4_000 });
  expect(hubStore.getSnapshot().connection).not.toBe("live");

  hubStore.logout();
});

it("unbound resume probe answering 401 takes the logout/retirement path, never live", async () => {
  const { hubStore, list } = await bootUnbound();
  // Imported AFTER resetModules so it is the SAME class realm the re-imported
  // store checks with instanceof inside isUnauthorized.
  const { HubHttpError } = await import("./httpError");
  const states: string[] = [];
  hubStore.subscribe(() => states.push(hubStore.getSnapshot().connection));

  list.mockRejectedValue(new HubHttpError(401, "unauthorized", "Unauthorized"));
  hubStore.resumeActive(null);

  await vi.waitFor(() => expect(hubStore.getSnapshot().authed).toBe(false), { timeout: 4_000 });
  const snapshot = hubStore.getSnapshot();
  expect(snapshot.authed).toBe(false);
  expect(snapshot.session).toBeNull();
  expect(snapshot.connection).toBe("offline");
  expect(states).not.toContain("live");
  // The old identity's machine was disposed by the external retirement.
  const internal = hubStore as unknown as { connection: unknown };
  expect(internal.connection).toBeNull();
});
