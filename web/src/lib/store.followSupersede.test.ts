import { afterEach, expect, it, vi } from "vitest";

const INSTANCE_A = "ins_follow_super_a";
const JOURNAL_A = "obj_follow_super_journal_a";
const INSTANCE_B = "ins_follow_super_b";
const JOURNAL_B = "obj_follow_super_journal_b";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;
type Hooks = NonNullable<Parameters<Api["eventsSubscribe"]>[4]>;

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (err: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

/**
 * c-reconnfu round 2 item 1: overlapping mounts must not share one follow
 * attempt. follow(A) is awaiting its journal seed when the user navigates to
 * B; A's late subscribe result must neither certify the machine live with no
 * B socket nor take B offline.
 */
afterEach(() => {
  vi.restoreAllMocks();
});

type Seed = Awaited<ReturnType<Api["eventsRead"]>>;

type DeferredSeed = {
  promise: Promise<Seed>;
  resolve: (value: Seed | PromiseLike<Seed>) => void;
  reject: (err: unknown) => void;
};

async function setup() {
  const { api, hubStore } = await fresh();
  const seeds: Record<string, DeferredSeed> = {
    [JOURNAL_A]: deferred<Seed>(),
    [JOURNAL_B]: deferred<Seed>(),
  };
  const hooks = new Map<string, Hooks>();
  const subscribed: string[] = [];

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
      { id: INSTANCE_A, journalId: JOURNAL_A, revision: "0", durableSeq: "0", lifecycle: "running" },
      { id: INSTANCE_B, journalId: JOURNAL_B, revision: "0", durableSeq: "0", lifecycle: "running" },
    ] as never,
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "eventsRead").mockImplementation((async (args?: { journalId?: string }) => {
    return seeds[args?.journalId ?? ""]?.promise;
  }) as Api["eventsRead"]);
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (journalId: string, _afterSeq: unknown, _onBatch: unknown, _onGap: unknown, h?: Hooks) => {
      subscribed.push(journalId);
      if (h) hooks.set(journalId, h);
      return {
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
      };
    }) as Api["eventsSubscribe"],
  );

  await hubStore.bootstrap();
  return {
    api,
    hubStore,
    seeds: (j: string) => (j === JOURNAL_A ? seeds[JOURNAL_A] : seeds[JOURNAL_B]),
    waitSubscribed: (j: string) => vi.waitFor(() => expect(subscribed).toContain(j)),
  };
}

const seedPage = {
  events: [],
  durableSeq: "0",
  windowFromSeq: null,
  reachedAfterSeq: true,
} as Awaited<ReturnType<Api["eventsRead"]>>;

it("a superseded mount's late success never certifies the still-pending mount live", async () => {
  const { hubStore, seeds, waitSubscribed } = await setup();

  const mountA = hubStore.follow(INSTANCE_A);
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("recovering"));

  // Navigate to B while A's seed is still pending.
  const mountB = hubStore.follow(INSTANCE_B);
  expect(hubStore.connectionState).toBe("recovering");

  // A finishes everything (seed + subscribe snapshot) after B superseded it.
  seeds(JOURNAL_A).resolve(seedPage);
  await waitSubscribed(JOURNAL_A);
  // The machine must stay recovering: A's socket certifies nothing for B.
  expect(hubStore.connectionState).toBe("recovering");

  // B's own completion certifies live.
  seeds(JOURNAL_B).resolve(seedPage);
  await Promise.all([mountA, mountB]);
  expect(hubStore.connectionState).toBe("live");

  hubStore.logout();
});

it("a superseded mount's failure while the new mount is still pending leaves it recovering", async () => {
  // c-reconnfu round 3 item 7: the old ordering finished B BEFORE rejecting
  // A, so the gen guard never had to protect a still-pending B. The honest
  // race is: A fails WHILE B's seed is still in flight — B must stay
  // recovering (A's failure is dropped), and only B's own later completion
  // drives the machine to live.
  const { hubStore, seeds, waitSubscribed } = await setup();

  const mountA = hubStore.follow(INSTANCE_A);
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("recovering"));
  const mountB = hubStore.follow(INSTANCE_B);
  expect(hubStore.connectionState).toBe("recovering");

  // A fails while B is STILL pending: B must not be taken offline — it stays
  // recovering under its own attempt.
  seeds(JOURNAL_A).reject(new Error("JOURNAL_A_SEED_FAILED"));
  await expect(mountA).rejects.toThrow("JOURNAL_A_SEED_FAILED");
  expect(hubStore.connectionState).toBe("recovering");
  // A never opened a socket (its seed rejected first); B's is still pending.
  expect(hubStore.connectionState).not.toBe("offline");

  // Only B's own outcome drives the machine: B completes and certifies live.
  seeds(JOURNAL_B).resolve(seedPage);
  await waitSubscribed(JOURNAL_B);
  await mountB;
  expect(hubStore.connectionState).toBe("live");

  hubStore.logout();
});

it("a superseded mount's late failure after B is already live never takes it offline", async () => {
  const { hubStore, seeds, waitSubscribed } = await setup();

  const mountA = hubStore.follow(INSTANCE_A);
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("recovering"));
  const mountB = hubStore.follow(INSTANCE_B);

  // B succeeds and certifies live.
  seeds(JOURNAL_B).resolve(seedPage);
  await waitSubscribed(JOURNAL_B);
  expect(hubStore.connectionState).toBe("live");

  // A fails late; its rejection still surfaces to its own caller, but B stays
  // live (no offline, no reconnect backoff).
  seeds(JOURNAL_A).reject(new Error("JOURNAL_A_SEED_FAILED"));
  await expect(mountA).rejects.toThrow("JOURNAL_A_SEED_FAILED");
  await mountB;
  expect(hubStore.connectionState).toBe("live");

  hubStore.logout();
});

it("rebinding to an already-mounted live session certifies it and retires the prior attempt", async () => {
  const { hubStore, seeds, waitSubscribed } = await setup();

  // B fully mounts first (OPEN socket + snapshot frame).
  const seedB = hubStore.follow(INSTANCE_B);
  seeds(JOURNAL_B).resolve(seedPage);
  await seedB;
  await waitSubscribed(JOURNAL_B);
  expect(hubStore.connectionState).toBe("live");

  // Navigate to A (seed hangs) — the machine goes recovering for A.
  const mountA = hubStore.follow(INSTANCE_A);
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("recovering"));

  // Navigate BACK to B: its mounted socket is still OPEN+fresh — B certifies
  // live immediately and A's in-flight attempt is retired.
  const mountBAgain = hubStore.follow(INSTANCE_B);
  await mountBAgain;
  expect(hubStore.connectionState).toBe("live");

  // A's late failure cannot take B offline.
  seeds(JOURNAL_A).reject(new Error("JOURNAL_A_SEED_FAILED"));
  await expect(mountA).rejects.toThrow("JOURNAL_A_SEED_FAILED");
  expect(hubStore.connectionState).toBe("live");

  hubStore.logout();
});

/**
 * c-reconnfu round 3 item 2: the seed-lost handoff must be SCOPED to the
 * captured (journal, binding generation, attempt). Rapid A → B → A → B issues a
 * DEFERRED DUPLICATE seed per journal (the second follow() of a journal whose
 * first seed is still pending starts a second eventsRead). When the duplicate A seed
 * resolves after B's stale first mount already opened a live B socket, the
 * journals.has(A) early return used to hand off unconditionally:
 * followRebindLive() certified live and retired the CURRENT (B) attempt +
 * watchdog — so the duplicate B seed's later rejection was dropped by the
 * machine and the connection stayed live.
 */
async function setupPerCallSeeds() {
  const { api, hubStore } = await fresh();
  const queues: Record<string, DeferredSeed[]> = {
    [JOURNAL_A]: [],
    [JOURNAL_B]: [],
  };
  const hooks = new Map<string, Hooks>();
  const subscribed: string[] = [];
  const nextSeed = (j: string): DeferredSeed => {
    const d = deferred<Seed>();
    queues[j].push(d);
    return d;
  };

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
      { id: INSTANCE_A, journalId: JOURNAL_A, revision: "0", durableSeq: "0", lifecycle: "running" },
      { id: INSTANCE_B, journalId: JOURNAL_B, revision: "0", durableSeq: "0", lifecycle: "running" },
    ] as never,
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "eventsRead").mockImplementation((async (args?: { journalId?: string }) => {
    const j = args?.journalId ?? "";
    const d = queues[j] ? nextSeed(j) : deferred<Seed>();
    return d.promise;
  }) as Api["eventsRead"]);
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (journalId: string, _afterSeq: unknown, _onBatch: unknown, _onGap: unknown, h?: Hooks) => {
      subscribed.push(journalId);
      if (h) hooks.set(journalId, h);
      return {
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
      };
    }) as Api["eventsSubscribe"],
  );

  await hubStore.bootstrap();
  return {
    api,
    hubStore,
    hooks,
    seedCall: (j: string, i: number) => {
      const d = queues[j]?.[i];
      if (!d) throw new Error(`seed ${j}[${i}] not queued (have ${queues[j]?.length ?? 0})`);
      return d;
    },
    waitQueued: (j: string, n: number) =>
      vi.waitFor(() => expect(queues[j]?.length).toBeGreaterThanOrEqual(n)),
    waitSubscribed: (j: string) => vi.waitFor(() => expect(subscribed).toContain(j)),
  };
}

it("an obsolete duplicate seed's scoped handoff never retires the live mount's attempt (B's late failure drives offline)", async () => {
  const { hubStore, hooks, seedCall, waitQueued, waitSubscribed } = await setupPerCallSeeds();

  // Rapid A → B → A → B: every follow queues its own seed read.
  const mountA1 = hubStore.follow(INSTANCE_A);
  await waitQueued(JOURNAL_A, 1);
  const mountB1 = hubStore.follow(INSTANCE_B);
  await waitQueued(JOURNAL_B, 1);
  const mountA2 = hubStore.follow(INSTANCE_A);
  await waitQueued(JOURNAL_A, 2);
  const mountB2 = hubStore.follow(INSTANCE_B);
  await waitQueued(JOURNAL_B, 2);

  // A's first seed mounts A (its stale attempt end is dropped by gen).
  seedCall(JOURNAL_A, 0).resolve(seedPage);
  await waitSubscribed(JOURNAL_A);
  // B's first seed mounts B with a live socket: its snapshot frame certifies the
  // CURRENT B binding (same journal as B's in-flight second attempt).
  seedCall(JOURNAL_B, 0).resolve(seedPage);
  await waitSubscribed(JOURNAL_B);
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"));

  // The deferred DUPLICATE A seed now resolves and hits the
  // journals.has(A) early return. A scoped handoff must do nothing (A is an
  // obsolete binding); B's second attempt/watchdog stay armed.
  seedCall(JOURNAL_A, 1).resolve(seedPage);
  await mountA1.catch(() => undefined);
  await mountA2.catch(() => undefined);
  await new Promise((r) => setTimeout(r, 10));

  // B's duplicate seed (the CURRENT mount's catch-up) rejects. Under the
  // gate-7 policy a failed resume READ must not tear down B's already
  // frame-certified follow socket: the link stays live, healing via live
  // frames under the frame watchdog (a rejected REST catch-up is not a dead
  // link). A's obsolete handoff still must not have retired B's attempt.
  seedCall(JOURNAL_B, 1).reject(new Error("JOURNAL_B_DUP_SEED_FAILED"));
  await expect(mountB2).rejects.toThrow("JOURNAL_B_DUP_SEED_FAILED");
  await expect(mountB1).resolves.toBeUndefined();
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("live"));

  // A genuine socket drop on the CURRENT mount is still authoritative: the
  // follow's own close drives the machine offline.
  hooks.get(JOURNAL_B)?.onClose?.();
  await vi.waitFor(() => expect(hubStore.connectionState).toBe("offline"));

  hubStore.logout();
});
