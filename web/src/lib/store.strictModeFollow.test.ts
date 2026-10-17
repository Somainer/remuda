/**
 * c-unboundresume r12/r13 — React StrictMode mount regression (gate failure
 * effort-sync.hub.spec.ts:326, D-056), driven through the REAL connection
 * machine.
 *
 * In dev <StrictMode> a SessionPage mount runs its effect setup, cleanup and
 * replay SYNCHRONOUSLY in one commit: follow(A) -> releaseRouteTarget(A) ->
 * follow(A). The r6/Y2 release retired the first mount's binding synchronously,
 * so the replay mounted a FRESH generation: its seed duplicated the original
 * mount's, the original seed was fenced and discarded, and the effective
 * follow socket attached only after the duplicated (later) seed. A configure
 * posted in that window saw its driver echo land in the late SEED as history
 * (noteEffortObservation live=false — hydrate only, never settles pending),
 * and the Hub never re-publishes a row at/below the subscribe snapshot on the
 * live socket. Pending stayed {name:"max"} and the chip never clamped to the
 * echoed "xhigh".
 *
 * r13: these regressions initialise through the PUBLIC bootstrap() with only
 * the API/outbox boundaries mocked, so the real ConnectionMachine owns the
 * mount attempt, its bind generation, certification and live state, and the
 * real JournalClient receives the live batches. The replay must preserve the
 * ORIGINAL binding generation and mount attempt (one followAttemptBegin,
 * success reported for that exact attempt/gen), reach live once, and settle
 * the clamped pending from the id-bearing live echo.
 */
import { afterEach, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import type { Observation } from "../types/observation";
import { effortAt } from "../features/session/effort";
import type { ConnectionMachine } from "./connection";
import { OUTBOX_LS_KEY } from "./outbox";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

interface Internals {
  connection: ConnectionMachine | null;
  connectionBindGen: number;
  connectionBoundTo: string | null;
  connectionBoundJournal: string | null;
}

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

type History = Awaited<ReturnType<Api["eventsRead"]>>;

const flush = async (rounds = 80) => {
  for (let i = 0; i < rounds; i += 1) await Promise.resolve();
};

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

function effortClampEcho(instance: Instance, requestId: string, seq = "2"): Observation {
  return {
    eventId: `evt_strict_clamp_${seq}`,
    instanceId: instance.id,
    journalId: instance.journalId,
    seq,
    observedAt: "2026-10-18T00:00:02.000Z",
    kind: "effort",
    source: { channel: "transcript" },
    payload: {
      kind: "effort",
      payload: {
        requested: { name: "max", ultracode: false },
        requestId,
        effective: {
          name: "xhigh",
          ultracode: false,
          source: "remuda",
          observedAt: "2026-10-18T00:00:02.000Z",
        },
        raw: "xhigh",
      },
    },
  } as unknown as Observation;
}

/**
 * Boot through public bootstrap() with the instance ALREADY in the Hub list
 * (the state a create()+landing leaves right before /s/<id> mounts): the real
 * ConnectionMachine installs and certifies unbound-live; only transport and
 * background-work boundaries are mocked. Seed/subscribe stay uncontrolled for
 * the caller to install after bootstrap.
 */
async function bootFollowable(instance: Instance) {
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
  vi.spyOn(api, "settlementSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockResolvedValue(undefined);
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [instance], nextCursor: null });

  await hubStore.bootstrap();
  expect(hubStore.connectionState).toBe("live");

  const internal = hubStore as unknown as Internals;
  const machine = internal.connection;
  expect(machine).not.toBeNull();
  return { api, hubStore, machine: machine as ConnectionMachine, internal };
}

/**
 * Install the controllable seed/subscribe boundary on the REAL api:
 *  - the mount-time read (seed 1) is held; later reads share seed 2;
 *  - eventsSubscribe captures the attach cursor and a live publish() that,
 *    like the Hub seam (c-effortread r9), delivers only events ABOVE the
 *    attach cursor (rows at/below it are snapshot/seed state, never
 *    re-published on the socket).
 */
function installJournalBoundary(
  api: Api,
  instance: Instance,
  seeds: { first: { promise: Promise<History> }; later: { promise: Promise<History> } },
) {
  let reads = 0;
  vi.spyOn(api, "eventsRead").mockImplementation(() => {
    reads += 1;
    return reads === 1 ? seeds.first.promise : seeds.later.promise;
  });
  let attachSeq = "";
  let publish: (observations: Observation[]) => void = () => undefined;
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (_journalId, afterSeq, onBatch) => {
      attachSeq = String(afterSeq);
      publish = (observations) => {
        const fresh = observations.filter((o) => Number(o.seq) > Number(afterSeq));
        if (!fresh.length) return;
        onBatch({
          subscriptionId: "sub_strict_follow",
          journalId: instance.journalId,
          fromSeq: fresh[0]!.seq,
          toSeq: fresh[fresh.length - 1]!.seq,
          durableSeq: fresh[fresh.length - 1]!.seq,
          events: fresh,
        });
      };
      return {
        subscriptionId: "sub_strict_follow",
        journalId: instance.journalId,
        durableSeq: String(afterSeq),
        windowFromSeq: "1",
        reachedAfterSeq: true,
        getReadyState: () => 1,
        snapshot: {
          projectionVersion: "v1",
          projectionEpoch: "epoch_strict_follow",
          asOfSeq: String(afterSeq),
          instance,
          runs: [],
          commands: [],
          pendingInteractions: [],
          nodes: [],
          history: { earliestRetainedSeq: "1", complete: true },
        },
      };
    }) as Api["eventsSubscribe"],
  );
  return {
    readCount: () => reads,
    attachSeq: () => attachSeq,
    subscribe,
    publish: (observations: Observation[]) => publish(observations),
  };
}

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

it("StrictMode follow/release/follow of the same target keeps the ORIGINAL binding and mount attempt, reaches live, and settles the clamped pending live", async () => {
  vi.useFakeTimers({ now: new Date("2026-10-18T00:00:00.000Z"), toFake: ["Date"] });
  const original = (await import("./mock")).mockDb.instances[0];
  const instance: Instance = {
    ...original,
    id: "ins_strict_follow",
    journalId: "obj_strict_follow",
    kind: "claude",
    driver: "claude-print",
    effortName: null,
    effortIndex: null,
    effortUltracode: null,
    effortEffective: null,
    activity: { state: "known", value: "idle" },
  };
  const { api, hubStore, machine, internal } = await bootFollowable(instance);
  const genAtBoot = internal.connectionBindGen;

  // Observe the REAL machine's mount-attempt ownership (call-through).
  const begin = vi.spyOn(machine, "followAttemptBegin");
  const end = vi.spyOn(machine, "followAttemptEnd");

  const seed1 = deferred<History>();
  const seed2 = deferred<History>();
  const journal = installJournalBoundary(api, instance, { first: seed1, later: seed2 });

  let configureRequestId = "";
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    configureRequestId = (extras?.effort as { requestId?: string } | undefined)?.requestId ?? "";
    return {} as never;
  });

  // React StrictMode mount: setup -> cleanup -> replay, synchronously in one
  // commit — no await between the three calls.
  const first = hubStore.follow(instance.id);
  void first.catch(() => undefined);
  hubStore.releaseRouteTarget(instance.id);
  const replay = hubStore.follow(instance.id);
  void replay.catch(() => undefined);

  // The replay RE-ADOPTED the original mount: exactly one attempt begin, the
  // binding generation never bumped past the first follow's, and the target
  // stayed bound the whole commit.
  expect(begin).toHaveBeenCalledTimes(1);
  const mountGen = begin.mock.calls[0]![0]!;
  const mountAttemptId = begin.mock.results[0]!.value as number;
  expect(mountGen).toBe(internal.connectionBindGen);
  expect(internal.connectionBindGen).toBe(genAtBoot + 1);
  expect(internal.connectionBoundTo).toBe(instance.id);
  expect(internal.connectionBoundJournal).toBe(instance.journalId);
  expect(journal.readCount()).toBe(1);

  // The mount-time seed completes EMPTY before the user configures anything;
  // the real machine certifies the mount live and reports success for the
  // ORIGINAL attempt/gen (not a replay mount's replacement attempt).
  await vi.waitFor(() => expect(journal.readCount()).toBe(1));
  seed1.resolve({ events: [], durableSeq: "1", windowFromSeq: "1", reachedAfterSeq: true });
  await flush();
  await vi.waitFor(() => expect(hubStore.getSnapshot().connection).toBe("live"));
  expect(end).toHaveBeenCalledWith(true, mountAttemptId, mountGen);
  expect(journal.subscribe).toHaveBeenCalledTimes(1);
  expect(Number(journal.attachSeq())).toBeLessThan(2);

  // The user drags the tier to max; the optimistic pending waits for THIS
  // request's id-bearing echo.
  await hubStore.setEffort(instance.id, effortAt("claude", 4, false), "tier");
  expect(configureRequestId).not.toBe("");
  expect(hubStore.effortPendingOf(instance.id)).toMatchObject({
    name: "max",
    levelSettled: false,
  });

  // The driver projects the clamped xhigh verdict AFTER the socket attached:
  // it is live bus state and a duplicate replayed seed never happens.
  seed2.resolve({
    events: [effortClampEcho(instance, configureRequestId)],
    durableSeq: "2",
    windowFromSeq: "1",
    reachedAfterSeq: true,
  });
  await flush();
  journal.publish([effortClampEcho(instance, configureRequestId)]);
  await flush();

  // The pending settles from the live id-bearing echo and the chip clamps.
  expect(hubStore.effortPendingOf(instance.id)).toBeNull();
  expect(hubStore.effortEffectiveOf(instance.id)?.name).toBe("xhigh");
  expect(hubStore.connectionState).toBe("live");
  // Still exactly one seed/subscribe/attempt: the replay mounted nothing new.
  expect(journal.readCount()).toBe(1);
  expect(journal.subscribe).toHaveBeenCalledTimes(1);
  expect(begin).toHaveBeenCalledTimes(1);

  hubStore.logout();
});

it("a StrictMode replay after a GENUINE departure joins the fresh remount's attempt, never the stale parked mount", async () => {
  const original = (await import("./mock")).mockDb.instances[0];
  const instance: Instance = {
    ...original,
    id: "ins_strict_remount",
    journalId: "obj_strict_remount",
    kind: "claude",
    driver: "claude-print",
    effortName: null,
    effortIndex: null,
    effortUltracode: null,
    effortEffective: null,
    activity: { state: "known", value: "idle" },
  };
  const { api, hubStore, machine, internal } = await bootFollowable(instance);

  const begin = vi.spyOn(machine, "followAttemptBegin");
  const end = vi.spyOn(machine, "followAttemptEnd");

  const seed1 = deferred<History>();
  const seed2 = deferred<History>();
  const journal = installJournalBoundary(api, instance, { first: seed1, later: seed2 });

  // First mount; its seed is parked, then the user GENUINELY leaves (cleanup
  // with no same-commit replay): the deferred departure releases ownership and
  // the unbound proof certifies the list.
  const first = hubStore.follow(instance.id);
  void first.catch(() => undefined);
  await vi.waitFor(() => expect(journal.readCount()).toBe(1));
  const firstGen = internal.connectionBindGen;
  hubStore.releaseRouteTarget(instance.id);
  await flush();
  await vi.waitFor(() => {
    expect(internal.connectionBoundTo).toBeNull();
    expect(hubStore.getSnapshot().connection).toBe("live");
  });

  // A later remount in a NEW commit, itself StrictMode-invoked as setup ->
  // cleanup -> replay. The replay must join the FRESH mount (new generation),
  // never the stale mount parked under the departed binding.
  const remount = hubStore.follow(instance.id);
  void remount.catch(() => undefined);
  hubStore.releaseRouteTarget(instance.id);
  const replay = hubStore.follow(instance.id);
  void replay.catch(() => undefined);

  await vi.waitFor(() => expect(journal.readCount()).toBe(2));
  expect(begin).toHaveBeenCalledTimes(2);
  const freshGen = begin.mock.calls[1]![0]!;
  const freshAttemptId = begin.mock.results[1]!.value as number;
  expect(freshGen).toBe(internal.connectionBindGen);
  expect(freshGen).toBeGreaterThan(firstGen);

  // The stale mount's held seed resolves: fenced by the departure, it mounts
  // nothing and certifies nothing.
  seed1.resolve({ events: [], durableSeq: "1", windowFromSeq: "1", reachedAfterSeq: true });
  await flush();
  expect(journal.subscribe).not.toHaveBeenCalled();
  expect(end).not.toHaveBeenCalledWith(expect.anything(), expect.anything(), firstGen);

  // The fresh remount's seed resolves and the REAL machine certifies THAT
  // attempt live; the replay added no third attempt and the socket attached
  // before the echo.
  seed2.resolve({ events: [], durableSeq: "1", windowFromSeq: "1", reachedAfterSeq: true });
  await flush();
  await vi.waitFor(() => expect(hubStore.getSnapshot().connection).toBe("live"));
  expect(end).toHaveBeenCalledWith(true, freshAttemptId, freshGen);
  expect(journal.subscribe).toHaveBeenCalledTimes(1);
  expect(begin).toHaveBeenCalledTimes(2);
  expect(Number(journal.attachSeq())).toBeLessThan(2);

  // Live delivery after the remount reaches the journal over the fresh socket.
  journal.publish([effortClampEcho(instance, "effreq_fresh_remount")]);
  await flush();
  expect(hubStore.effortEffectiveOf(instance.id)?.name).toBe("xhigh");

  await Promise.allSettled([first, remount, replay]);
  hubStore.logout();
});
