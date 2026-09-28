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

it("a superseded mount's late failure never takes the newly bound session offline", async () => {
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
