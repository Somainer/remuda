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
