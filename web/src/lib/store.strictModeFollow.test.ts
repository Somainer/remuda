/**
 * c-unboundresume r12 — React StrictMode mount regression (gate failure
 * effort-sync.hub.spec.ts:326, D-056).
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
 * A same-target replay within one commit must re-adopt the original mount
 * instead of rebinding: exactly one seed, an early live attach, and the echo
 * delivered after configure settles the pending as a LIVE edge.
 */
import { afterEach, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import type { Observation } from "../types/observation";
import { effortAt } from "../features/session/effort";
import { api } from "./api";
import { mockDb } from "./mock";
import { hubStore } from "./store";

type History = Awaited<ReturnType<typeof api.eventsRead>>;

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

const flush = async (rounds = 60) => {
  for (let i = 0; i < rounds; i += 1) await Promise.resolve();
};

afterEach(() => {
  hubStore.logout();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

it("StrictMode follow/release/follow of the same target keeps ONE mount: a post-seed configure echo arrives live and settles the clamped pending", async () => {
  vi.useFakeTimers({ now: new Date("2026-10-18T00:00:00.000Z"), toFake: ["Date"] });
  const original = mockDb.instances[0];
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
  // The instance is already in the list the way create()'s landing leaves it
  // before the /s/<id> route mounts, so follow() finds it cached with a journal.
  hubStore.setSlicesForTest({ instances: [instance] });

  // Seed reads are controllable. The mount-time read (seed 1) completes EMPTY
  // before any configure. Only a duplicated replay mount issues seed 2, which
  // returns AFTER the configure with the clamped echo already durable.
  const seed1 = deferred<History>();
  const seed2 = deferred<History>();
  let reads = 0;
  const eventsRead = vi.spyOn(api, "eventsRead").mockImplementation(() => {
    reads += 1;
    return reads === 1 ? seed1.promise : seed2.promise;
  });

  // Hub seam (c-effortread r9): a subscription attaches to the live bus BEFORE
  // taking its snapshot, so a row at/below the attach cursor is snapshot state
  // and is NEVER re-published on the socket. Rows above the cursor are live.
  let attachSeq = "";
  let publish!: (observations: Observation[]) => void;
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
    }) as typeof api.eventsSubscribe,
  );

  // The driver's verdict: org cap clamps the requested max to xhigh, and the
  // edge names THIS request's id (r10).
  let configureRequestId = "";
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    configureRequestId = (extras?.effort as { requestId?: string } | undefined)?.requestId ?? "";
    return {} as never;
  });
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [instance], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const clampEcho: Observation = {
    eventId: "evt_strict_clamp",
    instanceId: instance.id,
    journalId: instance.journalId,
    seq: "2",
    observedAt: "2026-10-18T00:00:02.000Z",
    kind: "effort",
    source: { channel: "transcript" },
    payload: {
      kind: "effort",
      payload: {
        requested: { name: "max", ultracode: false },
        requestId: "", // filled with the configure request id below
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

  // React StrictMode mount: setup -> cleanup -> replay, synchronously in one
  // commit — no await between the three calls.
  const first = hubStore.follow(instance.id);
  void first.catch(() => undefined);
  hubStore.releaseRouteTarget(instance.id);
  const replay = hubStore.follow(instance.id);
  void replay.catch(() => undefined);

  await vi.waitFor(() => expect(eventsRead).toHaveBeenCalled());
  // The mount-time seed completes empty BEFORE the user configures anything;
  // the follow socket attaches at seq 1.
  seed1.resolve({ events: [], durableSeq: "1", windowFromSeq: "1", reachedAfterSeq: true });
  await flush();

  // The user drags the tier to max; the optimistic pending waits for THIS
  // request's id-bearing echo.
  await hubStore.setEffort(instance.id, effortAt("claude", 4, false), "tier");
  expect(configureRequestId).not.toBe("");
  expect(hubStore.effortPendingOf(instance.id)).toMatchObject({ name: "max", levelSettled: false });
  ((clampEcho.payload as unknown as { payload: { requestId: string } }).payload).requestId = configureRequestId;

  // The driver projects the clamped xhigh verdict: it is durable by the time
  // the duplicated late seed reads, and is published on the live bus. A socket
  // attached at seq 1 receives it as a LIVE edge; a socket attaching at seq 2
  // can only see it as seed/snapshot history.
  seed2.resolve({ events: [clampEcho], durableSeq: "2", windowFromSeq: "1", reachedAfterSeq: true });
  await flush();
  publish([clampEcho]);
  await flush();

  // The pending must settle from the live id-bearing echo and the chip must
  // clamp to xhigh — never stay on the optimistic "max" for 15 s.
  expect(hubStore.effortPendingOf(instance.id)).toBeNull();
  expect(hubStore.effortEffectiveOf(instance.id)?.name).toBe("xhigh");
  // The same-commit replay re-adopted the original mount: exactly one seed and
  // one subscribe, whose attach cursor PRECEDES the configure echo (seq 2).
  expect(eventsRead).toHaveBeenCalledTimes(1);
  expect(subscribe).toHaveBeenCalledTimes(1);
  expect(Number(attachSeq)).toBeLessThan(2);
});

it("a StrictMode replay after a GENUINE departure joins the fresh remount, never the stale parked mount", async () => {
  const original = mockDb.instances[0];
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
  hubStore.setSlicesForTest({ instances: [instance] });

  // The first mount's seed stays held until after the user has genuinely left
  // the route; the remount gets its own (later) seed.
  const seed1 = deferred<History>();
  const seed2 = deferred<History>();
  let reads = 0;
  vi.spyOn(api, "eventsRead").mockImplementation(() => {
    reads += 1;
    return reads === 1 ? seed1.promise : seed2.promise;
  });
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (journalId, afterSeq) => ({
      subscriptionId: `sub_${journalId}`,
      journalId,
      durableSeq: String(afterSeq),
      windowFromSeq: "1",
      reachedAfterSeq: true,
      getReadyState: () => 1,
      snapshot: {
        projectionVersion: "v1",
        projectionEpoch: "epoch_strict_remount",
        asOfSeq: String(afterSeq),
        instance,
        runs: [],
        commands: [],
        pendingInteractions: [],
        nodes: [],
        history: { earliestRetainedSeq: "1", complete: true },
      },
    })) as typeof api.eventsSubscribe,
  );

  // Mount A; its lookup is parked, then the user genuinely leaves (cleanup
  // with no same-commit replay): the deferred departure releases ownership.
  const first = hubStore.follow(instance.id);
  void first.catch(() => undefined);
  await vi.waitFor(() => expect(reads).toBe(1));
  hubStore.releaseRouteTarget(instance.id);
  await flush();

  // A later remount in a NEW commit is itself StrictMode-invoked as setup ->
  // cleanup -> replay. The replay must join the FRESH mount, whose follow
  // entry owns the new generation — never the stale mount parked under the
  // departed binding.
  const remount = hubStore.follow(instance.id);
  void remount.catch(() => undefined);
  hubStore.releaseRouteTarget(instance.id);
  const replay = hubStore.follow(instance.id);
  void replay.catch(() => undefined);
  await vi.waitFor(() => expect(reads).toBe(2));

  // The stale mount's held seed resolves: fenced by the departure, it mounts
  // nothing.
  seed1.resolve({ events: [], durableSeq: "1", windowFromSeq: "1", reachedAfterSeq: true });
  await flush();
  expect(subscribe).not.toHaveBeenCalled();

  // The fresh remount's seed resolves and attaches exactly once.
  seed2.resolve({ events: [], durableSeq: "1", windowFromSeq: "1", reachedAfterSeq: true });
  await Promise.allSettled([first, remount, replay]);
  await flush();
  expect(subscribe).toHaveBeenCalledTimes(1);
});
