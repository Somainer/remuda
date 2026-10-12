/**
 * §9.1 effort store behavior:
 * - a terminal-side /effort (effort observation, no pending configure) moves
 *   the slider to the observed stop WITHOUT posting a configure;
 * - our pending push-down clears pending on a newer effective projection;
 * - queued lifecycle shows the queued entry;
 * - a rejected push-down reverts the slider and exposes its refusal (D-056);
 * - §9.1 read-back provenance, per axis (D-056 level + ultracode flag);
 * - c-perffu/c-effortui: per-request threshold so a replaced request's stale
 *   projection cannot clear the newer request.
 */
import { afterEach, expect, it, vi } from "vitest";
import type { Observation } from "../types/observation";
import type { Instance } from "../types/instance";
import { api } from "./api";
import { mockDb } from "./mock";
import { hubStore } from "./store";
import { effortAt } from "../features/session/effort";

afterEach(() => {
  hubStore.logout();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

function subscription(instance: Instance) {
  return {
    subscriptionId: `sub_${instance.id}`,
    journalId: instance.journalId,
    durableSeq: "1",
    windowFromSeq: "1",
    windowAfterSeq: "",
    reachedAfterSeq: true,
    getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_effort_store",
      asOfSeq: "1",
      instance,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { complete: true, earliestRetainedSeq: "1" },
    },
    history: [],
  };
}

async function startFollowing(
  idSuffix: string,
  activity: Instance["activity"] = { state: "known", value: "idle" },
  driver: Instance["driver"] = "claude-print",
) {
  // Pin Date (only Date — real timers keep running) before every fixture
  // timestamp so a client request's send time (pending.at, the r12 item 3
  // legacy lower bound when no queued edge exists) precedes its read-backs.
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z"), toFake: ["Date"] });
  const original = mockDb.instances[0];
  // Isolate: every test flips its own instance id and journal.
  const instance: Instance = {
    ...original,
    id: `ins_effort_store_${idSuffix}`,
    journalId: `obj_effort_store_${idSuffix}`,
    kind: "claude",
    driver,
    effortName: null,
    effortIndex: null,
    effortUltracode: null,
    effortEffective: null,
    activity,
  };
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "1",
    windowFromSeq: "1",
    reachedAfterSeq: true,
  } as never);
  let deliverBatch!: (observations: Observation[]) => void;
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async (_j, _a, onBatch) => {
    const send = (observations: Observation[]) =>
      onBatch({
        subscriptionId: `sub_${instance.id}`,
        journalId: instance.journalId,
        fromSeq: observations[0]!.seq,
        toSeq: observations[observations.length - 1]!.seq,
        durableSeq: observations[observations.length - 1]!.seq,
        events: observations,
      });
    deliverBatch = (observations: Observation[]) => send(observations);
    return subscription(instance);
  });
  await hubStore.follow(instance.id);
  // The follow snapshot is at seq 1; live events start at 2. Assign every
  // delivered event a contiguous envelope seq regardless of the content seq
  // the test baked into its payload timestamps. The payload keeps its own
  // observedAt (what the effort reducer compares); the envelope gets a
  // matching monotonic timestamp so the journal never sees a gap.
  let delivered = 1;
  const tsFor = (n: number) => `2026-10-06T00:0${n}:00.000Z`;
  const stamp = (observation: Observation) => {
    delivered += 1;
    // Keep an explicit envelope observedAt (effort/lifecycle payloads share
    // one host clock in production); only synthesize one when absent.
    const observedAt = observation.observedAt ?? tsFor(delivered);
    return { ...observation, seq: String(delivered), observedAt };
  };
  const ctx = {
    instance,
    receive: (observation: Observation) => deliverBatch([stamp(observation)]),
    // r16 item 4: deliver several journal events in ONE follow/catch-up batch.
    // The JournalClient flushes them in ascending seq order; the store must
    // fold activity and effort lifecycles in that same order.
    receiveBatch: (observations: Observation[]) => deliverBatch(observations.map(stamp)),
  };
  return ctx;
}

function effortEvent(
  seq: number,
  name: string,
  ultracode: boolean | null,
  source: string,
  observedAt?: string,
  extra?: { requested?: { name?: string; ultracode?: boolean } | null; requestId?: string | null },
): Observation {
  return {
    eventId: `evt_eff_${seq}_${name}_${ultracode}`,
    instanceId: "x",
    journalId: "x",
    seq: String(seq),
    observedAt: `2026-09-16T00:0${seq}:00.000Z`,
    kind: "effort",
    source: { channel: "transcript" },
    payload: {
      kind: "effort",
      payload: {
        ...(extra
          ? {
              ...(extra.requested !== undefined ? { requested: extra.requested } : {}),
              ...(extra.requestId !== undefined ? { requestId: extra.requestId } : {}),
            }
          : {}),
        effective: {
          name,
          ultracode,
          source,
          observedAt: observedAt ?? `2026-09-16T00:0${seq}:00.000Z`,
        },
        raw: name,
      },
    },
  } as unknown as Observation;
}

/** A configure lifecycle carrying the driver-echoed per-request id (r10). */
function configureLifecycleFor(
  seq: number,
  status: string,
  requestId: string,
  instanceId = "x",
  observedAt?: string,
): Observation {
  const event = configureLifecycle(seq, status, instanceId, observedAt);
  const payload = event.payload as Record<string, unknown>;
  payload.relatedIds = { effortRequestId: requestId };
  return event;
}

function configureLifecycle(seq: number, status: string, instanceId = "x", observedAt?: string): Observation {
  return {
    eventId: `evt_cfg_${seq}_${status}`,
    instanceId,
    journalId: "x",
    seq: String(seq),
    observedAt: observedAt ?? `2026-09-16T00:1${seq}:00.000Z`,
    kind: "lifecycle",
    source: { channel: "runtime" },
    payload: {
      type: "native",
      nativeName: "instance.configure",
      status: { state: "known", value: status },
    },
  } as unknown as Observation;
}

it("a terminal-side level switch moves the slider without configure", async () => {
  const ctx = await startFollowing("terminal");
  const configure = vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  ctx.receive(effortEvent(2, "xhigh", false, "slash"));
  const selected = hubStore.effortOf(ctx.instance.id, "claude");
  expect(selected.name).toBe("xhigh");
  expect(configure).not.toHaveBeenCalled();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("xhigh");
});

it("a terminal-side ultracode switch parks the slider on xhigh with the flag", async () => {
  const ctx = await startFollowing("ultra");
  ctx.receive(effortEvent(2, "xhigh", true, "slash"));
  const selected = hubStore.effortOf(ctx.instance.id, "claude");
  expect(selected).toMatchObject({ name: "xhigh", index: 3, ultracode: true });
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.ultracode).toBe(true);
});

it("our pending level push-down clears when the read-back lands", async () => {
  const ctx = await startFollowing("pending");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 3, false));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.name).toBe("xhigh");
  ctx.receive(effortEvent(2, "xhigh", false, "remuda"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("xhigh");
});

it("a level push-down and a flag push-down each settle their own axis", async () => {
  const ctx = await startFollowing("axes");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  // Gesture 1: tier drag to xhigh (decoupled: the plain level word leaves the
  // orthogonal flag untouched, so this request moves ONLY the level).
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 3, false));
  const pending0 = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending0).toMatchObject({ name: "xhigh", axis: "tier" });
  expect(pending0?.levelSettled).toBe(false);
  expect(pending0?.flagSettled).toBe(true);

  // The level read-back settles the tier request.
  ctx.receive(effortEvent(2, "xhigh", null, "remuda"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();

  // Gesture 2: flip the orthogonal switch on at xhigh — a flag-ONLY request;
  // the level axis starts settled (it is not moving).
  await hubStore.setUltracode(ctx.instance.id, true);
  const pendingFlag = hubStore.effortPendingOf(ctx.instance.id);
  expect(pendingFlag).toMatchObject({ name: "xhigh", ultracode: true, axis: "flag" });
  expect(pendingFlag?.levelSettled).toBe(true);
  expect(pendingFlag?.flagSettled).toBe(false);

  // The attachment reports the flag on: the flag request settles.
  ctx.receive(effortEvent(3, "xhigh", true, "remuda"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("a rapid level→switch flip reads the freshest requested tier (D-056 race)", async () => {
  const ctx = await startFollowing("rapid-flag");
  const posts: { name?: string; ultracode?: boolean }[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  // Tier drag to xhigh...
  const level = hubStore.setEffort(ctx.instance.id, effortAt("claude", 3, false));
  // ...and flip the switch on BEFORE the level POST resolves.
  await hubStore.setUltracode(ctx.instance.id, true);
  await level;

  // The switch configure carries the FRESH xhigh tier, not the stale high.
  const flagPost = posts.find((p) => p.ultracode === true);
  expect(flagPost?.name).toBe("xhigh");
  // The optimistic selection is xhigh + on.
  expect(hubStore.effortOf(ctx.instance.id, "claude")).toMatchObject({
    name: "xhigh",
    ultracode: true,
  });
});

it("a cold switch flip states the flag axis and predicts the decoupled toggle word (r9 item 6)", async () => {
  const ctx = await startFollowing("cold-axis");
  // No local state, no read-back, null durable record (startFollowing clears
  // effortName/effortUltracode): a true cold start on a 2.1.289 session.
  const posts: { name?: string; ultracode?: boolean; axis?: string }[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  await hubStore.setUltracode(ctx.instance.id, true);

  // The configure carries the EXPLICIT flag axis at the current displayed tier.
  const flagPost = posts.find((p) => p.ultracode === true);
  expect(flagPost?.axis).toBe("flag");
  expect(flagPost?.name).toBe("high");

  // The pending indicator predicts the exact orthogonal toggle word even with
  // no proven selection, so the queued/applied matcher can settle it (it never
  // sticks on a stale "切换中" from a guessed level word).
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending?.commandWord).toBe("ultracode on");
});

it("a clamped level named by THIS request's echo settles the tier request; an unnamed clamp does not", async () => {
  const ctx = await startFollowing("clamp-flag");
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  // Decoupled tier request to max: only the level moves, the flag is left to a
  // separate toggle (its axis starts settled).
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false)); // max
  const id = posts[0]?.requestId!;
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending?.levelSettled).toBe(false);

  // The org cap clamps max to high. In PRODUCTION the verdict edge carries
  // the driver's requested echo naming the request's id (r10): the clamp is
  // this request's delivered outcome (rendered as a mismatch) and settles.
  ctx.receive(
    effortEvent(2, "high", null, "remuda", undefined, {
      requested: { name: "max", ultracode: false },
      requestId: id,
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");

  // An ID-LESS clamped echo (older driver) can no longer authorize the
  // opposite level: it must leave a fresh max request pending (r11 item 4).
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false)); // max again
  ctx.receive(
    effortEvent(3, "high", null, "remuda", undefined, {
      requested: { name: "max", ultracode: false },
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
});

it("a clamped level with NO echo and a non-matching value never settles a request", async () => {
  const ctx = await startFollowing("clamp-flag-stale");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false)); // max
  // A high projection with nothing tying it to the max request could be a
  // terminal-side switch to high: it must not settle the max pending.
  ctx.receive(effortEvent(2, "high", null, "slash"));
  const stillPending = hubStore.effortPendingOf(ctx.instance.id);
  expect(stillPending?.levelSettled).toBe(false);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();

  // The genuine max read-back then settles it.
  ctx.receive(effortEvent(3, "max", false, "remuda"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("while queued the entry shows queued until a strictly newer read-back", async () => {
  const ctx = await startFollowing("queued", { state: "known", value: "working" });
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  // A pre-existing projection at a DIFFERENT level stamps the threshold and
  // leaves the level axis unproven.
  ctx.receive(effortEvent(1, "high", false, "slash", "2026-09-16T00:05:00.000Z"));
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false));
  // The queued edge is THIS request's own threshold (after the 00:05
  // projection, before the 00:06 read-back).
  const lifecycle = configureLifecycle(3, "effort-queued:max", "x", "2026-09-16T00:05:30.000Z");
  ctx.receive(lifecycle);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);
  // An older-than-threshold read-back must not settle a queued request.
  ctx.receive(effortEvent(2, "max", false, "remuda", "2026-09-16T00:02:00.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);
  ctx.receive(effortEvent(6, "max", false, "remuda", "2026-09-16T00:06:00.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r9 item 8: a delayed OFF lifecycle after a newer ON request leaves B's pending untouched", async () => {
  const ctx = await startFollowing("off-after-on", { state: "known", value: "idle" });
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  // A flips the switch OFF at the current high tier...
  await hubStore.setUltracode(ctx.instance.id, false);
  const idA = posts[0]?.requestId!;
  const pendingA = hubStore.effortPendingOf(ctx.instance.id);
  expect(pendingA).toMatchObject({ name: "high", ultracode: false });
  // ...then B flips it ON before A's lifecycle lands.
  await hubStore.setUltracode(ctx.instance.id, true);
  const idB = posts[1]?.requestId!;
  expect(hubStore.effortPendingOf(ctx.instance.id)).toMatchObject({ name: "high", ultracode: true });

  // A's delayed OFF (both the space command form and the legacy separator
  // form) must not clear B's pending/threshold nor select B off. The id-less
  // edges simulate an older driver; an id-bearing A edge names A explicitly.
  for (const word of ["ultracode off", "ultracode:off"]) {
    ctx.receive(configureLifecycle(9, `effort-applied:${word}`));
    const pending = hubStore.effortPendingOf(ctx.instance.id);
    expect(pending, `B's ON pending survives a delayed ${word} applied`).toMatchObject({
      name: "high",
      ultracode: true,
    });
  }
  ctx.receive(configureLifecycleFor(9, "effort-applied:ultracode off", idA));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toMatchObject({
    name: "high",
    ultracode: true,
  });

  // A delayed OFF QUEUED edge must not stamp B's queued time or flip it queued.
  const beforeQueued = hubStore.effortPendingOf(ctx.instance.id)?.queuedObservedAt;
  ctx.receive(configureLifecycle(10, "effort-queued:ultracode off", "x", "2026-10-08T12:09:00.000Z"));
  const after = hubStore.effortPendingOf(ctx.instance.id);
  expect(after?.ultracode).toBe(true);
  expect(after?.queuedObservedAt).toBe(beforeQueued);

  // B's own ON applied clears it.
  ctx.receive(configureLifecycleFor(11, "effort-applied:ultracode on", idB));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("a stale queued lifecycle for a replaced request is dropped (A→B race)", async () => {
  const ctx = await startFollowing("ab-queued", { state: "known", value: "idle" });
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 2, false)); // A: high
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false)); // B: max
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(false);
  // A late queued lifecycle for A names a different axis — it must not
  // overwrite B's pending indicator nor flip it to queued.
  ctx.receive(configureLifecycle(4, "effort-queued:high"));
  const after = hubStore.effortPendingOf(ctx.instance.id);
  expect(after?.name).toBe("max");
  expect(after?.queued).toBe(false);
});

it("item 3: request A's read-back never settles B — queued threshold and the requested echo", async () => {
  const ctx = await startFollowing("ab-readback", { state: "known", value: "working" });
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  // A switches to xhigh while the agent works.
  await hubStore.setEffort(ctx.instance.id, { index: 3, name: "xhigh", kind: "claude", ultracode: false });
  ctx.receive(configureLifecycle(2, "effort-queued:xhigh", "x", "2026-10-08T12:00:01.000Z"));
  // B replaces A before the idle point.
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  ctx.receive(configureLifecycle(3, "effort-queued:max", "x", "2026-10-08T12:00:03.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toMatchObject({ name: "max", queued: true });

  // A's verdict lands at 12:00:02 — BEFORE B's own queued edge — and echoes
  // A's axes. It updates the projection but must not settle B's indicator.
  ctx.receive({
    eventId: "evt_a_readback",
    instanceId: "x",
    journalId: "x",
    seq: "4",
    observedAt: "2026-10-08T12:00:02.000Z",
    kind: "effort",
    source: { channel: "transcript" },
    payload: {
      kind: "effort",
      payload: {
        requested: { name: "xhigh", ultracode: false },
        effective: {
          name: "xhigh",
          ultracode: false,
          source: "remuda",
          observedAt: "2026-10-08T12:00:02.000Z",
        },
        raw: "xhigh",
      },
    },
  } as unknown as Observation);
  const pendingAfterA = hubStore.effortPendingOf(ctx.instance.id);
  expect(pendingAfterA?.name, "B keeps its own indicator through A's read-back").toBe("max");
  expect(pendingAfterA?.levelSettled).not.toBe(true);

  // B's own verdict (echoes B's axes, newer than B's queued edge) settles.
  ctx.receive({
    eventId: "evt_b_readback",
    instanceId: "x",
    journalId: "x",
    seq: "5",
    observedAt: "2026-10-08T12:00:04.000Z",
    kind: "effort",
    source: { channel: "transcript" },
    payload: {
      kind: "effort",
      payload: {
        requested: { name: "max", ultracode: false },
        effective: {
          name: "max",
          ultracode: false,
          source: "remuda",
          observedAt: "2026-10-08T12:00:04.000Z",
        },
        raw: "max",
      },
    },
  } as unknown as Observation);
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r4 item 3: a decoupled tier drag with the flag on is matched by the plain level lifecycle word", async () => {
  // On decoupled builds the driver types a plain `/effort max` for a
  // {max,true} TIER move (the ortho flag rides along), so the queued/applied/
  // degraded lifecycle carries the LEVEL word even though the pending's flag
  // is on. The pending must be matched and cleared — 切换中 must not stick.
  const ctx = await startFollowing("decoupled-drag-flag-on", { state: "known", value: "working" });
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  const row: Instance = {
    ...ctx.instance,
    durableSeq: "99",
    effortEffective: {
      name: "high",
      ultracode: true,
      source: "remuda",
      observedAt: "2026-10-08T12:00:00.000Z",
    },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [row] } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  hubStore["emit"]({ instances: [row] });
  await hubStore.refresh();

  // Drag to max with the flag still on → the driver types plain /effort max.
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: true });
  const id = posts[0]?.requestId!;
  expect(hubStore.effortPendingOf(ctx.instance.id)?.commandWord).toBe("max");

  // QUEUED with the LEVEL word despite flag=true (the r10+ driver echoes the
  // configure request id).
  ctx.receive(configureLifecycleFor(2, "effort-queued:max", id, "x", "2026-10-08T12:01:01.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toMatchObject({
    name: "max",
    ultracode: true,
    queued: true,
  });

  // A flag REFUSAL word ("ultracode") for the same request also clears the
  // pending (the configure carried the flag even though the typed word was
  // the plain level).
  ctx.receive(configureLifecycleFor(3, "effort-degraded:max:dialog-kept", id, "x", "2026-10-08T12:01:05.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("a decoupled queued `ultracode on` edge names the flag request and keeps the tier", async () => {
  // The driver's decoupled command word is "ultracode on" (a space); the
  // queued/applied lifecycle must parse as the FLAG axis, not an unknown tier.
  const ctx = await startFollowing("queued-flag-word", { state: "known", value: "working" });
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  await hubStore.setEffort(ctx.instance.id, { index: 3, name: "xhigh", kind: "claude", ultracode: false });
  await hubStore.setUltracode(ctx.instance.id, true);
  const id = posts[1]?.requestId!;
  ctx.receive(configureLifecycleFor(2, "effort-queued:ultracode on", id, "x", "2026-10-08T12:02:01.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toMatchObject({
    name: "xhigh",
    ultracode: true,
    queued: true,
  });
  ctx.receive(configureLifecycleFor(3, "effort-applied:ultracode on", id, "x", "2026-10-08T12:02:05.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("item 3: A's degraded LEVEL word never rolls back B's in-flight ultracode request", async () => {
  const ctx = await startFollowing("ab-degrade-flag", { state: "known", value: "working" });
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const toast = vi.spyOn(hubStore, "toast");

  // A: plain xhigh while working.
  await hubStore.setEffort(ctx.instance.id, { index: 3, name: "xhigh", kind: "claude", ultracode: false });
  ctx.receive(configureLifecycle(2, "effort-queued:xhigh", "x", "2026-10-08T12:01:01.000Z"));
  // B: flip the switch on at xhigh before A's verdict arrives.
  await hubStore.setUltracode(ctx.instance.id, true);
  ctx.receive(configureLifecycle(3, "effort-queued:ultracode on", "x", "2026-10-08T12:01:03.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toMatchObject({ name: "xhigh", ultracode: true });

  // A's late level degradation names xhigh — the old flag-ignoring matcher
  // treated it as B's verdict and cleared/reverted B. It must be dropped:
  // a level word cannot name a flag-on request.
  ctx.receive(configureLifecycle(4, "effort-degraded:xhigh:dialog-kept", "x", "2026-10-08T12:01:04.000Z"));
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending, "B's 切换中 indicator survives A's degraded verdict").not.toBeNull();
  expect(pending).toMatchObject({ name: "xhigh", ultracode: true });
  expect(hubStore.effortOf(ctx.instance.id, "claude")).toMatchObject({ name: "xhigh", ultracode: true });
  expect(toast, "no refusal toast for a verdict that belongs to A").not.toHaveBeenCalled();
});

it("a rejected switch reverts and records the model-scoped refusal", async () => {
  const ctx = await startFollowing("revert");
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  const toast = vi.spyOn(hubStore, "toast");
  ctx.instance.effortEffective = { name: "high", ultracode: false, source: "slash", observedAt: "2026-09-16T00:00:00.000Z" };

  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 3, true));
  ctx.receive(configureLifecycleFor(4, "effort-degraded:ultracode:ultracode-unavailable-for-model", posts[0]?.requestId!));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortOf(ctx.instance.id, "claude")).toMatchObject({ name: "high", ultracode: false });
  const refusal = hubStore.effortRefusalOf(ctx.instance.id);
  expect(refusal?.reason).toBe("ultracode-unavailable-for-model");
  expect(refusal?.scope).toBe("model");
  expect(toast).toHaveBeenCalled();
});

it("a workflows-disabled refusal is process-scoped and clears on a positive on read", async () => {
  const ctx = await startFollowing("wfrefuse");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 3, true));
  ctx.receive(configureLifecycle(4, "effort-degraded:ultracode:ultracode-workflows-disabled"));
  expect(hubStore.effortRefusalOf(ctx.instance.id)?.scope).toBe("process");

  // A later successful read-back clears it.
  ctx.receive(effortEvent(5, "xhigh", true, "remuda"));
  expect(hubStore.effortRefusalOf(ctx.instance.id)).toBeNull();
});

it("an id-less degraded verdict whose word differs from the queued one can never clear the pending (c-effortread r12 item 2)", async () => {
  const ctx = await startFollowing("queued-then-degrade", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // Driver reports queued on xhigh (a configure posted outside this UI, so
  // the pending is synthesized from the id-less lifecycle)...
  ctx.receive(configureLifecycle(2, "effort-queued:xhigh", ctx.instance.id, "2026-10-12T02:00:00.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);
  const queuedId = hubStore.effortPendingOf(ctx.instance.id)?.requestId!;
  // ...then an id-less rejection of a DIFFERENT word (max). The word alone
  // cannot tell a stale verdict from this request's, so it neither clears nor
  // rolls back; the request awaits its bounded reconciliation.
  ctx.receive(configureLifecycle(3, "effort-degraded:max:dialog-kept", ctx.instance.id, "2026-10-12T02:01:00.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(queuedId);
  // A fresh read-back matching the queued request is the one id-less path
  // that settles it.
  ctx.receive(effortEvent(4, "xhigh", true, "remuda", "2026-10-12T02:02:00.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("an unsolicited ultracode refusal disables the switch with no client pending", async () => {
  const ctx = await startFollowing("unsolicited-refuse");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // No setEffort — the refusal arrives for a configure posted outside the UI.
  ctx.receive(configureLifecycle(2, "effort-degraded:ultracode:ultracode-workflows-disabled"));
  const refusal = hubStore.effortRefusalOf(ctx.instance.id);
  expect(refusal?.scope).toBe("process");
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("the model-scoped refusal expires when the model changes", async () => {
  const ctx = await startFollowing("model-expiry");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // The refused model is the current (optimistic) one via a public model set.
  await hubStore.setModel(ctx.instance.id, "claude-sonnet-4-6");
  ctx.instance.effortEffective = { name: "high", ultracode: false, source: "slash", observedAt: "2026-09-16T00:00:00.000Z" };
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 3, true));
  ctx.receive(configureLifecycle(4, "effort-degraded:ultracode:ultracode-unavailable-for-model"));
  const refusal = hubStore.effortRefusalOf(ctx.instance.id);
  expect(refusal?.reason).toBeTruthy();
  expect(refusal?.modelId).toBe("claude-sonnet-4-6");
  // A /model accept moves the current model: the bound refusal is lifted.
  await hubStore.setModel(ctx.instance.id, "claude-opus-5");
  expect(hubStore.effortRefusalOf(ctx.instance.id)).toBeNull();
});

it("r9 item 7: a null poll never lowers the withdrawal watermark; a stale level stays withdrawn", async () => {
  const ctx = await startFollowing("null-poll-watermark");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);

  // T1: the durable record projects high via a poll.
  const at = (ms: string | null) =>
    ({ ...ctx.instance, effortEffective: ms == null ? null : { name: "high", ultracode: false, source: "remuda", observedAt: ms } }) as Instance;
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [at("2026-10-08T00:01:00.000Z")] } as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("high");

  // T2: a live read-back-unavailable edge withdraws the projection.
  ctx.receive({
    eventId: "evt_eff_null_poll_withdrawn",
    instanceId: "x",
    journalId: "x",
    seq: "3",
    kind: "effort",
    observedAt: "2026-10-08T00:02:00.000Z",
    source: { channel: "transcript" },
    payload: {
      effective: {
        name: null,
        ultracode: null,
        source: "remuda",
        observedAt: "2026-10-08T00:02:00.000Z",
        readbackAvailable: false,
      },
      raw: null,
    },
  } as unknown as Observation);
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();

  // Null poll: it must record NULL (not the old T1 view as the watermark).
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [at(null)] } as never);
  await hubStore.refresh();

  // A stale T1.5 level view arrives both via a delayed poll AND replayed
  // history; each must stay withdrawn (watermark is still T2).
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [at("2026-10-08T00:01:30.000Z")] } as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();
  ctx.receive(effortEvent(4, "high", false, "slash", "2026-10-08T00:01:45.000Z"));
  expect(
    hubStore.effortEffectiveOf(ctx.instance.id),
    "a replayed T1.5 history level stays withdrawn below the T2 watermark",
  ).toBeNull();
});

it("a poll projection settles the push-down without a live event", async () => {
  const ctx = await startFollowing("poll-settle");
  const projected: Instance = {
    ...ctx.instance,
    effortEffective: { name: "max", ultracode: false, source: "remuda", observedAt: "2026-09-21T00:00:01.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [projected] } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
});

it("the level mismatch still leaves the slider on the requested max (clamp)", async () => {
  const ctx = await startFollowing("poll-clamp");
  const clamped: Instance = {
    ...ctx.instance,
    effortEffective: { name: "xhigh", ultracode: null, source: "remuda", observedAt: "2026-09-21T00:00:05.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [clamped] } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false));
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");
});

it("an older poll projection cannot overwrite the newer effective", async () => {
  const ctx = await startFollowing("monotonic");
  const newerAt = "2026-09-21T00:00:09.000Z";
  ctx.receive(effortEvent(2, "max", false, "slash", newerAt));
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        effortEffective: { name: "xhigh", ultracode: null, source: "remuda", observedAt: "2026-09-21T00:00:01.000Z" },
      },
    ],
  } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
});

it("c-effortui: a poll between request A and request B cannot clear B's pending", async () => {
  // The replaced-request provenance bug: setEffort stamps each request with
  // the freshest effective observedAt. Request A settles at t002; request B
  // is made right after, and a poll that returns A's read-back (t002) must
  // not settle B — only a strictly newer projection may.
  const ctx = await startFollowing("ab-threshold");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  // Request A (high) settles via its own live edge.
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 2, false));
  ctx.receive(effortEvent(2, "high", false, "remuda", "2026-09-16T00:02:00.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.observedAt).toBe("2026-09-16T00:02:00.000Z");

  // A poll now returns A's read-back (same timestamp) while request B is in
  // flight. B's threshold is t002; an equal-or-older projection proves nothing.
  const stale: Instance = {
    ...ctx.instance,
    effortEffective: { name: "high", ultracode: false, source: "remuda", observedAt: "2026-09-16T00:02:00.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [stale] } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);

  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false)); // B: max
  expect(hubStore.effortPendingOf(ctx.instance.id)?.name).toBe("max");

  // B's own newer read-back settles it.
  const settled: Instance = {
    ...ctx.instance,
    effortEffective: { name: "max", ultracode: false, source: "remuda", observedAt: "2026-09-16T00:03:00.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [settled] } as never);
  await hubStore.refresh();
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
});

it("a failed configure rolls back only the still-current request and never rejects", async () => {
  const ctx = await startFollowing("configure-fail");
  vi.spyOn(api, "instanceConfigure").mockRejectedValue(new Error("network down"));
  const unhandled: unknown[] = [];
  const onUnhandled = (e: PromiseRejectionEvent) => unhandled.push(e.reason);
  window.addEventListener("unhandledrejection", onUnhandled);

  // No read-back exists; optimistic xhigh must revert away.
  const p = hubStore.setEffort(ctx.instance.id, { index: 3, name: "xhigh", kind: "claude", ultracode: false });
  await expect(p).resolves.toBeUndefined(); // no rejection escapes
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).not.toBe("xhigh");

  window.removeEventListener("unhandledrejection", onUnhandled);
  expect(unhandled).toHaveLength(0);
});

it("a failed configure for a replaced request does not clobber the newer one", async () => {
  const ctx = await startFollowing("configure-fail-replaced");
  const toast = vi.spyOn(hubStore, "toast");
  // A fails slowly; B succeeds.
  let resolveA: () => void = () => {};
  vi.spyOn(api, "instanceConfigure").mockImplementation((_id, _perm, extras) => {
    if (extras?.effort?.name === "xhigh") {
      // A (xhigh) hangs until we reject it late.
      return new Promise((_resolve, reject) => {
        resolveA = () => reject(new Error("A failed late"));
      });
    }
    // B (max) and anything else resolve immediately.
    return Promise.resolve({} as never);
  });
  // A = xhigh (starts first), then B = max.
  const a = hubStore.setEffort(ctx.instance.id, { index: 3, name: "xhigh", kind: "claude", ultracode: false });
  const b = hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  await b;
  // B's configure returned; its indicator is still pending the read-back
  // (no verdict in this test). A's late rejection must not delete it.
  expect(hubStore.effortPendingOf(ctx.instance.id)?.name).toBe("max");
  resolveA();
  await a;
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");
  // Item 8: B's 切换中 indicator survives A's HTTP failure.
  expect(hubStore.effortPendingOf(ctx.instance.id)?.name).toBe("max");
  // The failure is still surfaced, but distinguished from a live failure.
  expect(toast).toHaveBeenCalledWith(expect.stringContaining("上一次"));
});

it("a replaced request's late HTTP SUCCESS does not overwrite the newer choice", async () => {
  const ctx = await startFollowing("configure-success-replaced");
  // A hangs; B resolves immediately; A resolves successfully only later.
  let resolveA: () => void = () => {};
  vi.spyOn(api, "instanceConfigure").mockImplementation((_id, _perm, extras) => {
    if (extras?.effort?.name === "xhigh") {
      return new Promise((resolve) => {
        resolveA = () => resolve({} as never);
      });
    }
    return Promise.resolve({} as never);
  });
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [] } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);

  const a = hubStore.setEffort(ctx.instance.id, { index: 3, name: "xhigh", kind: "claude", ultracode: false });
  const b = hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  await b;
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");

  // A's late 200 carries xhigh; its post-success optimistic emit is nonce-stale
  // and must not replace B's max.
  resolveA();
  await a;
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");
});

it("a null baseline with no queued stamp: only a matching value settles the cold request", async () => {
  const ctx = await startFollowing("null-threshold");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // No effective state exists yet (baseline null, queued stamp not yet seen).
  // Cold flag-ON request at the default high tier.
  await hubStore.setUltracode(ctx.instance.id, true);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.baselineObservedAt).toBeNull();
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queuedObservedAt).toBeNull();

  // A terminal-side observation reporting the flag OFF is someone/something
  // else's state, not our request — even with no time stamps it must settle
  // nothing (matching tier alone never confirms the flag, r10 item 2).
  ctx.receive(effortEvent(2, "high", false, "slash"));
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending).not.toBeNull();
  expect(pending?.flagSettled).toBe(false);

  // A matching positive ON read-back settles the flag.
  ctx.receive(effortEvent(3, "high", true, "remuda"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});


it("a degraded lifecycle for replaced request A never reverts/clears request B", async () => {
  const ctx = await startFollowing("degraded-after-replace");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});
  // A = xhigh, replaced immediately by B = max before either settles.
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 3, false));
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.name).toBe("max");
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");

  // A's level-degraded verdict (names xhigh) lands while B is in flight.
  ctx.receive(configureLifecycle(3, "effort-degraded:xhigh:no-readback-within-window", ctx.instance.id));

  // B survives intact: still optimistic max, still pending, no stale toast.
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");
  expect(hubStore.effortPendingOf(ctx.instance.id)?.name).toBe("max");
  expect(hubStore.effortRefusalOf(ctx.instance.id)).toBeNull();
  expect(toast).not.toHaveBeenCalled();

  // B's own read-back then settles normally.
  ctx.receive(effortEvent(4, "max", false, "remuda"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");
});

it("history replay hydrates effective but never records a refusal, pending or toast", async () => {
  // A fresh follow whose SEEDED HISTORY contains both an effort observation
  // (hydrate) and a degraded ultracode lifecycle (must be ignored on replay).
  const instance: Instance = {
    ...mockDb.instances[0],
    id: "ins_effort_store_replay",
    journalId: "obj_effort_store_replay",
    kind: "claude",
    effortName: null,
    effortIndex: null,
    effortUltracode: null,
  };
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  const configure = vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});
  const seedEvents = [
    effortEvent(2, "xhigh", true, "remuda"),
    {
      eventId: "evt_cfg_replay", instanceId: "x", journalId: "x", seq: "3",
      kind: "lifecycle", observedAt: "2026-09-16T00:10:00Z",
      source: { channel: "runtime" },
      payload: {
        type: "native", nativeName: "instance.configure",
        nativeId: { state: "known", value: "not-applicable" },
        status: { state: "known", value: "effort-degraded:ultracode:ultracode-workflows-disabled" },
        relatedIds: {},
      },
    } as unknown as Observation,
  ];
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: seedEvents,
    durableSeq: "3",
    windowFromSeq: "1",
    reachedAfterSeq: true,
    getReadyState: () => 1,
  } as never);
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async () => {
    return {
      subscriptionId: "sub_replay",
      journalId: instance.journalId,
      durableSeq: "3",
      windowFromSeq: "1",
      reachedAfterSeq: true,
      getReadyState: () => 1,
      snapshot: {
        projectionVersion: "v1", projectionEpoch: "e", asOfSeq: "3", instance,
        runs: [], commands: [], pendingInteractions: [], nodes: [],
        history: { earliestRetainedSeq: "1", complete: true },
      },
    };
  });

  await hubStore.follow(instance.id);

  // The observation hydrated effective...
  expect(hubStore.effortEffectiveOf(instance.id)?.ultracode).toBe(true);
  // ...but the REPLAYED degraded lifecycle recorded no refusal, no pending,
  // fired no toast, and triggered no configure.
  expect(hubStore.effortRefusalOf(instance.id)).toBeNull();
  expect(hubStore.effortPendingOf(instance.id)).toBeNull();
  expect(toast).not.toHaveBeenCalled();
  expect(configure).not.toHaveBeenCalled();
});

const modelRefuse = "effort-degraded:ultracode:ultracode-unavailable-for-model";
const flagOn = (seq: number, name: string): Observation =>
  ({
    eventId: `evt_on_${seq}`, instanceId: "x", journalId: "x", seq: String(seq),
    kind: "effort", observedAt: `2026-09-17T00:0${seq}:00Z`,
    source: { channel: "transcript" },
    payload: {
      kind: "effort",
      payload: { effective: { name, ultracode: true, source: "remuda", observedAt: `2026-09-17T00:0${seq}:00Z` }, raw: name },
    },
  } as unknown as Observation);

it("success on model B does not erase model A's refusal", async () => {
  const ctx = await startFollowing("refusal-model-a");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // Establish model A first so the refusal binds to its id.
  await hubStore.setModel(ctx.instance.id, "model-a");
  // Refusal on model A.
  ctx.receive(configureLifecycle(2, modelRefuse, ctx.instance.id));
  expect(hubStore.effortRefusalOf(ctx.instance.id)?.scope).toBe("model");
  expect(hubStore.effortRefusalOf(ctx.instance.id)?.modelId).toBe("model-a");
  // A positive flag read-back is gated by model id; simulate a switch to a
  // DIFFERENT model, then positive evidence for the new model.
  await hubStore.setModel(ctx.instance.id, "model-b");
  ctx.receive(flagOn(3, "xhigh"));
  // On B the switch is enabled (the model-A refusal does not block B)…
  expect(hubStore.effortRefusalOf(ctx.instance.id)).toBeNull();
  // …but B's positive evidence must NOT erase A's stored refusal: the raw
  // model-scoped record survives.
  const raw = (
    hubStore as unknown as { state: { effortRefusal: Record<string, { scope: string; modelId: string | null }> } }
  ).state.effortRefusal[ctx.instance.id];
  expect(raw).toMatchObject({ scope: "model", modelId: "model-a" });
  // Switching back to A re-arms the block without any new degraded event.
  await hubStore.setModel(ctx.instance.id, "model-a");
  expect(hubStore.effortRefusalOf(ctx.instance.id)?.modelId).toBe("model-a");
});

it("a refusal recorded with a null model id never auto-clears", async () => {
  const ctx = await startFollowing("refusal-null-model");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // Seed a null-modelId refusal directly.
  hubStore["emit"]({
    effortRefusal: {
      [ctx.instance.id]: { reason: "ultracode-unavailable-for-model", scope: "model", modelId: null, at: "2026-09-17T00:00:00Z" },
    },
  });
  // A positive flag read-back must not clear it (model unbound).
  ctx.receive(flagOn(2, "xhigh"));
  expect(hubStore.effortRefusalOf(ctx.instance.id)).not.toBeNull();
});

it("item 4a: a polled positive flag for another model never clears the model-scoped refusal", async () => {
  const ctx = await startFollowing("refusal-poll-scope");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // Refusal is recorded for model A.
  await hubStore.setModel(ctx.instance.id, "model-a");
  ctx.receive(configureLifecycle(2, modelRefuse, ctx.instance.id));
  const rawRefusal = () =>
    (hubStore as unknown as { state: { effortRefusal: Record<string, { scope: string; modelId: string | null }> } })
      .state.effortRefusal[ctx.instance.id];
  expect(rawRefusal()).toMatchObject({ scope: "model", modelId: "model-a" });

  // Move to model B, then a POLL carries positive ultracode evidence on the
  // Hub record. The model-A refusal must survive (the old poll path cleared
  // any refusal on any positive flag).
  const rowB: Instance = {
    ...ctx.instance,
    durableSeq: "50",
    model: "model-b",
    effortEffective: { name: "xhigh", ultracode: true, source: "remuda", observedAt: "2026-10-08T12:00:00.000Z" },
  };
  const list = vi.spyOn(api, "instanceList").mockResolvedValue({ items: [rowB] } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  await hubStore.setModel(ctx.instance.id, "model-b");
  await hubStore.refresh();
  expect(rawRefusal(), "model A refusal survives B's positive poll").toMatchObject({
    scope: "model",
    modelId: "model-a",
  });

  // Positive evidence while the current model IS the bound one clears it.
  await hubStore.setModel(ctx.instance.id, "model-a");
  list.mockResolvedValue({
    items: [
      {
        ...rowB,
        durableSeq: "51",
        model: "model-a",
        effortEffective: { name: "xhigh", ultracode: true, source: "remuda", observedAt: "2026-10-08T12:01:00.000Z" },
      },
    ],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortRefusalOf(ctx.instance.id)).toBeNull();
});

it("item 4b: a refusal before model state binds to the launch model and clears on its positive flag", async () => {
  const ctx = await startFollowing("refusal-early-binding");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // No modelEffective, no picker selection yet — only the launch spec names a
  // model. Make sure the store sees it (as on a fresh follow).
  hubStore["emit"]({
    instances: [{ ...ctx.instance, durableSeq: "99", model: "launch-model-x" }],
  });

  // The refusal arrives before any model catalog/effective projection.
  ctx.receive(configureLifecycle(2, modelRefuse, ctx.instance.id));
  const refusal = hubStore.effortRefusalOf(ctx.instance.id);
  expect(refusal?.scope).toBe("model");
  expect(refusal?.modelId, "bound to the launch model, never null").toBe("launch-model-x");

  // Positive switch evidence for that same model clears it (the null-bound
  // refusal used to block every later model and never auto-cleared).
  ctx.receive(flagOn(3, "xhigh"));
  expect(hubStore.effortRefusalOf(ctx.instance.id)).toBeNull();
});

it("r2 item 1: the version gate prefers the snapshot version, then the host's pinned CLI, else unknown", () => {
  const base: Instance = {
    ...mockDb.instances[0],
    id: "ins_version_gate",
    hostId: "host-version-gate",
    kind: "claude",
  };
  const host = {
    id: "host-version-gate",
    label: "version host",
    state: "online",
    cli: [{ kind: "claude", version: "2.1.268", installed: true }],
  } as never;
  const baseNoSnapshot: Instance = {
    ...base,
    capabilities: { ...base.capabilities, binaryVersion: "" },
  };
  hubStore["emit"]({
    hosts: [...hubStore.getSnapshot().hosts, host],
    instances: [baseNoSnapshot],
  });

  // 1. No snapshot version → pinned host CLI wins (2.1.268 = coupled).
  expect(hubStore.effortVersionGate(base.id)).toBe("coupled");

  // 2. A reported snapshot version outranks the host pin (2.1.289 decoupled).
  hubStore["emit"]({
    instances: [{ ...baseNoSnapshot, capabilities: { ...base.capabilities, binaryVersion: "2.1.289" } }],
  });
  expect(hubStore.effortVersionGate(base.id)).toBe("decoupled");

  // 3. No host pin and no snapshot → unknown (switch locks, never fabricated).
  const orphan: Instance = { ...base, id: "ins_version_orphan", hostId: "host-does-not-exist" };
  hubStore["emit"]({
    instances: [{ ...orphan, capabilities: { ...orphan.capabilities, binaryVersion: "" } }],
  });
  expect(hubStore.effortVersionGate(orphan.id)).toBe("unknown");

  // 4. An old pinned binary → legacy.
  const legacyHost = {
    id: "host-legacy-claude",
    label: "legacy host",
    state: "online",
    cli: [{ kind: "claude", version: "2.1.150", installed: true }],
  } as never;
  const legacyInstance: Instance = {
    ...base,
    id: "ins_version_legacy",
    hostId: "host-legacy-claude",
    capabilities: { ...base.capabilities, binaryVersion: "" },
  };
  hubStore["emit"]({
    hosts: [...hubStore.getSnapshot().hosts, legacyHost],
    instances: [legacyInstance],
  });
  expect(hubStore.effortVersionGate(legacyInstance.id)).toBe("legacy");
});

/** Pin the followed test instance's reported Claude binary version (gate).
 *  Also serves the pinned row from polls, so the post-configure refresh does
 *  not drop the followed instance and regress the gate to unknown. */
function pinGate(ctx: { instance: Instance }, version: string) {
  const row: Instance = {
    ...ctx.instance,
    durableSeq: "99",
    capabilities: { ...ctx.instance.capabilities, binaryVersion: version },
  };
  hubStore["emit"]({ instances: [row] });
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [row] } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
}

it("item 9: create seeds the orthogonal ultracode flag from the spec", async () => {
  const ctx = await startFollowing("create-ultra");
  const baseInstance = {
    ...ctx.instance,
    id: "ins_create_ultra",
    journalId: "obj_create_ultra",
    effortName: "xhigh",
    effortIndex: 3,
    effortUltracode: true,
  };
  vi.spyOn(api, "instanceCreate").mockResolvedValue({ instance: baseInstance } as never);

  await hubStore.create({
    kind: "claude",
    effortName: "xhigh",
    effortIndex: 3,
    effortUltracode: true,
  } as never);

  // The fresh page shows the switch ON before any read-back; a later slider
  // drag starts from {xhigh,true}, never from {xhigh,false}.
  const seeded = hubStore.effortOf(baseInstance.id, "claude");
  expect(seeded).toMatchObject({ name: "xhigh", index: 3, ultracode: true });

  // And a flag-OFF launch seeds false explicitly.
  vi.spyOn(api, "instanceCreate").mockResolvedValue({
    instance: { ...baseInstance, id: "ins_create_off", effortName: "high", effortIndex: 2, effortUltracode: false },
  } as never);
  await hubStore.create({
    kind: "claude",
    effortName: "high",
    effortIndex: 2,
    effortUltracode: false,
  } as never);
  expect(hubStore.effortOf("ins_create_off", "claude")).toMatchObject({
    name: "high",
    ultracode: false,
  });
});

it("r5 item 3: the ultracode lock keys on driver shell-pty, including PROMOTED shells on 2.1.289", async () => {
  // r4 keyed on isGenericPty(), which returns false for promoted shells
  // (mode==="promoted") and for shell-pty with a structured tier — leaving the
  // switch enabled on a carrier that refuses every flag-on configure. The lock
  // must follow the DRIVER.
  const host = {
    id: "host-shell-pty",
    label: "shell pty",
    state: "online" as const,
    cli: [{ kind: "claude", version: "2.1.289", installed: true }],
  } as never;
  const cases: Array<{ label: string; mode: Instance["mode"]; signalTier: string }> = [
    // Generic promoted shell (no structured signal).
    { label: "generic", mode: "native", signalTier: "none" },
    // r5 regression: a PROMOTED shell — isGenericPty() returns false here.
    { label: "promoted", mode: "promoted", signalTier: "hook" },
    // r5 regression: shell-pty with a structured tier — also false.
    { label: "structured-tier", mode: "native", signalTier: "hook" },
  ];
  for (const { label, mode, signalTier } of cases) {
    const id = `ins_shell_pty_${label}`;
    const shell: Instance = {
      ...mockDb.instances[0],
      id,
      hostId: "host-shell-pty",
      kind: "claude",
      driver: "shell-pty",
      mode,
      nativeRef: { ...mockDb.instances[0].nativeRef, signalTier: signalTier as never },
      capabilities: { ...mockDb.instances[0].capabilities, binaryVersion: "2.1.289" } as never,
    };
    hubStore["emit"]({ hosts: [host], instances: [shell] });
    expect(hubStore.effortVersionGate(id), `${label} shell-pty locks`).toBe("unknown");
    const gated = (hubStore as unknown as {
      gateLiveEffort: (id: string, e: { name: string; index: number; kind: string; ultracode: boolean }) => unknown;
    }).gateLiveEffort(id, { name: "xhigh", index: 3, kind: "claude", ultracode: true });
    expect(gated, `${label} flag-on fails closed`).toMatchObject({ ultracode: false });
  }

  // claude-pty with 2.1.289 stays decoupled (the lock is carrier-specific).
  const herdr: Instance = {
    ...mockDb.instances[0],
    id: "ins_herdr_claude_pty",
    hostId: "host-shell-pty",
    kind: "claude",
    driver: "claude-pty",
    capabilities: { ...mockDb.instances[0].capabilities, binaryVersion: "2.1.289" } as never,
  };
  hubStore["emit"]({ instances: [herdr] });
  expect(hubStore.effortVersionGate(herdr.id)).toBe("decoupled");
});

it("r5 item 2: the command-word predictor falls back to the durable record on reload/other device", async () => {
  // A decoupled session pinned {high,false}, opened with NO live effective
  // projection and NO optimistic selection (reload on another device / created
  // outside this tab): flipping the switch on must predict "ultracode on" —
  // matching the driver (which falls back observed ?? requested ?? launch).
  // The r4 predictor only read effective/optimistic state and predicted
  // "high", so queued/applied/degraded ("ultracode on|off") never matched.
  const ctx = await startFollowing("predictor-durable-fallback");
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [] } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const row: Instance = {
    ...ctx.instance,
    driver: "claude-pty",
    capabilities: { ...ctx.instance.capabilities, binaryVersion: "2.1.289" },
    effortName: "high",
    effortIndex: 2,
    effortUltracode: false,
  };
  hubStore["emit"]({ hosts: [], instances: [row] });
  // Ensure no live/optimistic state shadows the durable record.
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();
  // Flip the switch on (wire {high,ultracode:true}); the predictor must use
  // the durable record fallback and predict "ultracode on".
  await hubStore.setUltracode(ctx.instance.id, true);
  const queuedWord = (() => {
    const p = (hubStore as unknown as {
      state: { effortPending: Record<string, { commandWord?: string }> };
    }).state.effortPending[ctx.instance.id];
    return p?.commandWord;
  })();
  expect(queuedWord).toBe("ultracode on");
  // Lifecycle matching accepts the space form (r5 item 2 separator fix).
  // Applied (not degraded) so the applied-word match path settles it.
  ctx.receive(configureLifecycleFor(2, "effort-applied:ultracode on", posts[0]?.requestId!, "xhigh"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  hubStore.logout();
});

it("r4 item 4: a coupled drag to max survives a reload with persisted {xhigh,true}", async () => {
  const ctx = await startFollowing("coupled-reloaded-drag");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // Pinned 2.1.277 (coupled); the DURABLE record is {xhigh,true} — the
  // post-reload/other-device state, with NO optimistic state.effort entry.
  await pinGate(ctx, "2.1.277");
  const recorded: Instance = {
    ...ctx.instance,
    durableSeq: "100",
    capabilities: { ...ctx.instance.capabilities, binaryVersion: "2.1.277" },
    effortName: "xhigh",
    effortIndex: 3,
    effortUltracode: true,
    effortEffective: {
      name: "xhigh",
      ultracode: true,
      source: "remuda",
      observedAt: "2026-10-08T12:00:00.000Z",
    },
  };
  vi.mocked(api.instanceList).mockResolvedValue({ items: [recorded] } as never);
  hubStore["emit"]({ instances: [recorded] });
  await hubStore.refresh();
  // The optimistic-only map is empty after reload (record drives effortOf).
  expect(hubStore["state"].effort[ctx.instance.id]).toBeUndefined();
  expect(hubStore.effortOf(ctx.instance.id, "claude")).toMatchObject({
    name: "xhigh",
    ultracode: true,
  });

  // First drag to max must NOT be re-linked to xhigh: coupled flag exists only
  // at xhigh, so it posts {max,false} exactly once and the slider stays on max.
  let posted: { name?: string; ultracode?: boolean } = {};
  vi.mocked(api.instanceConfigure).mockImplementation(
    (_id: string, _perm: string, extras?: { effort?: { name?: string; ultracode?: boolean } }) => {
      posted = extras?.effort ?? {};
      return Promise.resolve({} as never);
    },
  );
  // PRE gate", hubStore.effortVersionGate(ctx.instance.id), "effortOf", JSON.stringify(hubStore.effortOf(ctx.instance.id,"claude")), "inst", hubStore["state"].instances.find(i=>i.id===ctx.instance.id)?.effortName, hubStore["state"].instances.find(i=>i.id===ctx.instance.id)?.effortUltracode);
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: true });
  expect(posted).toMatchObject({ name: "max", ultracode: false });
  expect(hubStore.effortOf(ctx.instance.id, "claude")).toMatchObject({ name: "max" });
});

it("item 2 coupled: turning the switch on moves the slider to xhigh and posts {xhigh,on}", async () => {
  const ctx = await startFollowing("coupled-on");
  await pinGate(ctx, "2.1.277");
  const posts: { name?: string; ultracode?: boolean; index?: number }[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  // Start at max, switch off.
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  posts.length = 0;
  // Flip the switch on — the slider must jump to xhigh.
  await hubStore.setUltracode(ctx.instance.id, true);
  expect(posts).toHaveLength(1);
  expect(posts[0]).toMatchObject({ name: "xhigh", ultracode: true });
  expect(hubStore.effortOf(ctx.instance.id, "claude")).toMatchObject({
    name: "xhigh",
    index: 3,
    ultracode: true,
  });
});

it("item 2 coupled: sliding away from xhigh turns the switch off and never posts {non-xhigh,on}", async () => {
  const ctx = await startFollowing("coupled-slide-away");
  await pinGate(ctx, "2.1.277");
  const posts: { name?: string; ultracode?: boolean }[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  // Switch on first (slider parks on xhigh).
  await hubStore.setUltracode(ctx.instance.id, true);
  expect(hubStore.effortOf(ctx.instance.id, "claude")).toMatchObject({ name: "xhigh", ultracode: true });
  posts.length = 0;
  // Drag the slider from xhigh down to high — exactly what the slider emits
  // (the current flag rides along).
  await hubStore.setEffort(ctx.instance.id, { index: 2, name: "high", kind: "claude", ultracode: true });
  expect(posts).toHaveLength(1);
  expect(posts[0]).toMatchObject({ name: "high", ultracode: false });
  expect(posts.some((p) => p.ultracode === true && p.name !== "xhigh")).toBe(false);
  expect(hubStore.effortOf(ctx.instance.id, "claude")).toMatchObject({ name: "high", ultracode: false });
});

it("item 2 coupled: a defensive {non-xhigh,on} drag from xhigh is clamped before the post", async () => {
  const ctx = await startFollowing("coupled-clamp");
  await pinGate(ctx, "2.1.277");
  const posts: { name?: string; ultracode?: boolean }[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  // The switch is on at xhigh (the coupled parked state).
  await hubStore.setUltracode(ctx.instance.id, true);
  posts.length = 0;
  // A slider drag emits the CURRENT flag with the new tier — {max,true} must
  // never be posted on a coupled session.
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: true });
  expect(posts).toHaveLength(1);
  expect(posts[0]).toMatchObject({ name: "max", ultracode: false });
});

it("item 2 decoupled: a flip keeps the tier and posts the flag; a drag carries the flag", async () => {
  const ctx = await startFollowing("decoupled-flag");
  await pinGate(ctx, "2.1.289");
  const posts: { name?: string; ultracode?: boolean }[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  // Flag on at high: the slider must NOT move to xhigh.
  await hubStore.setEffort(ctx.instance.id, { index: 2, name: "high", kind: "claude", ultracode: false });
  await hubStore.setUltracode(ctx.instance.id, true);
  const onPost = posts.at(-1)!;
  expect(onPost).toMatchObject({ name: "high", ultracode: true });
  expect(hubStore.effortOf(ctx.instance.id, "claude")).toMatchObject({ name: "high", ultracode: true });
  // Drag to max with the flag on: the boolean rides along unchanged.
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: true });
  expect(posts.at(-1)).toMatchObject({ name: "max", ultracode: true });
  expect(hubStore.effortOf(ctx.instance.id, "claude")).toMatchObject({ name: "max", ultracode: true });
  // Flag off is a {max,false} flag-only post.
  await hubStore.setUltracode(ctx.instance.id, false);
  expect(posts.at(-1)).toMatchObject({ name: "max", ultracode: false });
});

it("item 2 legacy/unknown: the boolean fails closed even if a client posts it", async () => {
  for (const version of ["2.1.180", ""]) {
    const ctx = await startFollowing(version ? "legacy-flag" : "unknown-flag");
    await pinGate(ctx, version);
    const posts: { name?: string; ultracode?: boolean }[] = [];
    vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
      posts.push(extras?.effort ?? {});
      return {} as never;
    });
    await hubStore.setUltracode(ctx.instance.id, true);
    expect(posts.at(-1)?.ultracode).toBe(false);
    expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(false);
    hubStore.logout();
    vi.restoreAllMocks();
  }
});

it("a read-back-unavailable edge clears the projection but keeps the pending switch (D-056 (4))", async () => {
  const ctx = await startFollowing("withdrawn");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // A verified resume projected high+ultracode.
  ctx.receive(effortEvent(2, "high", true, "launch"));
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("high");

  // Remuda arms a switch to max: pending indicator on.
  await hubStore.setEffort(ctx.instance.id, {
    index: 4,
    name: "max",
    kind: "claude",
    ultracode: false,
  });
  expect(hubStore.effortPendingOf(ctx.instance.id)?.name).toBe("max");

  // The transcript is replaced: the driver withdraws read-back (name/flag
  // null, readbackAvailable false).
  const withdrawn = {
    eventId: "evt_eff_withdrawn",
    instanceId: "x",
    journalId: "x",
    seq: "3",
    kind: "effort",
    observedAt: "2026-10-08T12:05:00Z",
    source: { channel: "transcript" },
    payload: {
      effective: {
        name: null,
        ultracode: null,
        source: "unknown",
        observedAt: "2026-10-08T12:05:00Z",
        readbackAvailable: false,
      },
      raw: null,
    },
  } as unknown as Observation;
  ctx.receive(withdrawn);

  // Projected state is withdrawn -> the chip renders ? ...
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();
  // ... but the pending switch is neither settled nor rejected.
  expect(hubStore.effortPendingOf(ctx.instance.id)?.name).toBe("max");
});

it("r6 item 6: a poll with no Hub effort projection never erases a live-only edge", async () => {
  const ctx = await startFollowing("poll-null-never-projected");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  // The live follow socket projects a level BEFORE the Hub record exists.
  ctx.receive(effortEvent(2, "high", false, "slash"));
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("high");
  // An unrelated lifecycle write advances updatedAt while the Hub still has
  // no effortEffective on this instance.
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ ...ctx.instance, effortEffective: undefined, updatedAt: "2026-10-08T13:00:00Z" }],
  } as never);
  await hubStore.refresh();
  expect(
    hubStore.effortEffectiveOf(ctx.instance.id)?.name,
    "a null the Hub never projected must not render the chip ?"
  ).toBe("high");
  expect(
    hubStore.effortReadbackWithdrawnOf(ctx.instance.id),
    "never projected: not a withdrawal"
  ).toBe(false);
});

it("r6 item 6: a Hub projection becoming null on a later poll withdraws the live edge", async () => {
  const ctx = await startFollowing("poll-hub-null-withdraws");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const list = vi.spyOn(api, "instanceList");
  // Poll A: the Hub has projected max.
  list.mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        effortEffective: { name: "max", ultracode: false, source: "remuda", observedAt: "2026-10-08T12:00:00Z" },
      },
    ],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
  // Poll B: the Hub applied the withdrawal edge (record null).
  list.mockResolvedValue({
    items: [{ ...ctx.instance, effortEffective: undefined, updatedAt: "2026-10-08T13:00:00Z" }],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();
  expect(
    hubStore.effortReadbackWithdrawnOf(ctx.instance.id),
    "a level→null Hub record marks the explicit withdrawal"
  ).toBe(true);
  // Poll C: still null must not throw/restore.
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();
  // A later projection clears the marker again.
  list.mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        effortEffective: { name: "high", ultracode: false, source: "remuda", observedAt: "2026-10-08T14:00:00Z" },
      },
    ],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("high");
  expect(hubStore.effortReadbackWithdrawnOf(ctx.instance.id)).toBe(false);
});

it("r6 item 6: a no-fraction updatedAt must not outrank a fractional observedAt", async () => {
  // SQLite writes second precision ("…:00Z", no fraction); the live edge
  // carries milliseconds ("…:00.100Z"). A STRING compare puts 'Z' > '.',
  // so the older second-precision value looked NEWER and replaced the
  // fresher projection. Only parsed instants must decide ordering.
  const ctx = await startFollowing("poll-same-second");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const list = vi.spyOn(api, "instanceList");
  list.mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        effortEffective: { name: "high", ultracode: false, source: "remuda", observedAt: "2026-10-08T12:00:00.100Z" },
      },
    ],
  } as never);
  await hubStore.refresh();
  // Second precision, same wall second = strictly OLDER (.000 vs .100).
  list.mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        effortEffective: { name: "medium", ultracode: false, source: "remuda", observedAt: "2026-10-08T12:00:00Z" },
      },
    ],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("high");
});

it("r8 item 1: an older level edge after a withdrawal (Load earlier) cannot restore the projection", async () => {
  const ctx = await startFollowing("withdrawn-sticky-older-edge");
  // Level at 12:00, withdrawal at 12:05 (deletes the projection).
  ctx.receive(effortEvent(2, "high", false, "remuda", "2026-10-08T12:00:00.000Z"));
  const withdrawn: Observation = {
    eventId: "evt_eff_wd_1",
    instanceId: "x",
    journalId: "x",
    seq: "3",
    kind: "effort",
    observedAt: "2026-10-08T12:05:00Z",
    source: { channel: "transcript" },
    payload: {
      effective: {
        name: null,
        ultracode: null,
        source: "unknown",
        observedAt: "2026-10-08T12:05:00Z",
        readbackAvailable: false,
      },
      raw: null,
    },
  } as unknown as Observation;
  ctx.receive(withdrawn);
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortReadbackWithdrawnOf(ctx.instance.id)).toBe(true);
  expect(hubStore.effortWithdrawnAtOf(ctx.instance.id)).toBe("2026-10-08T12:05:00Z");

  // "Load earlier" replays an OLDER medium edge (12:03) as history.
  const older = effortEvent(4, "medium", false, "slash", "2026-10-08T12:03:00.000Z");
  hubStore["noteEffortObservation"](ctx.instance.id, older, false);
  expect(
    hubStore.effortEffectiveOf(ctx.instance.id),
    "the older edge must not re-project a withdrawn level"
  ).toBeNull();
  expect(hubStore.effortReadbackWithdrawnOf(ctx.instance.id)).toBe(true);

  // A same-timestamp edge is also rejected (<= watermark).
  const sameTime = effortEvent(5, "high", false, "slash", "2026-10-08T12:05:00.000Z");
  hubStore["noteEffortObservation"](ctx.instance.id, sameTime, false);
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();

  // A strictly NEWER valid edge restores the projection and clears the marker.
  const newer = effortEvent(6, "max", false, "remuda", "2026-10-08T12:06:00.000Z");
  hubStore["noteEffortObservation"](ctx.instance.id, newer, true);
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
  expect(hubStore.effortReadbackWithdrawnOf(ctx.instance.id)).toBe(false);
});

it("r8 item 1: a withdrawal sets the watermark even with no projection held (reload seed)", async () => {
  // Reload case: the seeded history contains the withdrawal but the
  // projection was already absent (no earlier level in the live map). The
  // marker must still be set so an older poll edge cannot repopulate it.
  const ctx = await startFollowing("withdrawn-seed-reload");
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();
  const withdrawn: Observation = {
    eventId: "evt_eff_wd_seed",
    instanceId: "x",
    journalId: "x",
    seq: "2",
    kind: "effort",
    observedAt: "2026-10-08T13:00:00Z",
    source: { channel: "transcript" },
    payload: {
      effective: {
        name: null,
        ultracode: null,
        source: "unknown",
        observedAt: "2026-10-08T13:00:00Z",
        readbackAvailable: false,
      },
      raw: null,
    },
  } as unknown as Observation;
  ctx.receive(withdrawn);
  expect(hubStore.effortReadbackWithdrawnOf(ctx.instance.id)).toBe(true);
  // An older poll record cannot clear the withdrawal.
  const older: Instance = {
    ...ctx.instance,
    effortEffective: { name: "high", ultracode: false, source: "remuda", observedAt: "2026-10-08T12:59:00.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [older] } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortReadbackWithdrawnOf(ctx.instance.id)).toBe(true);
  // A newer poll record restores.
  const newer: Instance = {
    ...older,
    effortEffective: { name: "max", ultracode: false, source: "remuda", observedAt: "2026-10-08T13:01:00.000Z" },
  };
  vi.mocked(api.instanceList).mockResolvedValue({ items: [newer] } as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
  expect(hubStore.effortReadbackWithdrawnOf(ctx.instance.id)).toBe(false);
});

it("r6 item 8a: an out-of-order withdrawal (older observedAt) never deletes the newer edge", async () => {
  const ctx = await startFollowing("withdrawn-out-of-order");
  // Newer valid edge M (contiguous after the follow cursor).
  ctx.receive(effortEvent(2, "high", false, "slash"));
  ctx.receive(effortEvent(3, "max", false, "slash"));
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
  // An older withdrawal frame arriving late (Load earlier).
  const staleWithdrawn = {
    eventId: "evt_eff_withdrawn_old",
    instanceId: "x",
    journalId: "x",
    seq: "4",
    kind: "effort",
    observedAt: "2026-09-16T00:01:30Z",
    source: { channel: "transcript" },
    payload: {
      effective: {
        name: null,
        ultracode: null,
        source: "unknown",
        observedAt: "2026-09-16T00:01:30Z",
        readbackAvailable: false,
      },
      raw: null,
    },
  } as unknown as Observation;
  ctx.receive(staleWithdrawn);
  expect(
    hubStore.effortEffectiveOf(ctx.instance.id)?.name,
    "the newer max projection survives the older withdrawal"
  ).toBe("max");
  expect(
    hubStore.effortReadbackWithdrawnOf(ctx.instance.id),
    "an out-of-order withdrawal must not set the marker"
  ).toBe(false);
  // A newer withdrawal still wins.
  const freshWithdrawn = {
    ...staleWithdrawn,
    eventId: "evt_eff_withdrawn_new",
    seq: "5",
    observedAt: "2026-10-08T13:00:00Z",
    payload: {
      effective: {
        name: null,
        ultracode: null,
        source: "unknown",
        observedAt: "2026-10-08T13:00:00Z",
        readbackAvailable: false,
      },
      raw: null,
    },
  } as unknown as Observation;
  ctx.receive(freshWithdrawn);
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortReadbackWithdrawnOf(ctx.instance.id)).toBe(true);
});

it("an identical poll still settles queued effort left by a historical onPrepend replay (c-perffu r2)", async () => {
  // End state of an ascending load-earlier (onPrepend) replay: first the
  // newer remuda effective edge folds (with no push-down in flight yet, it
  // lands as a terminal-side switch), THEN the configure row journaled after
  // it is replayed as queued — so pending exists while the effective map
  // already carries the newer read-back. The next poll returns that SAME
  // record; pending settlement must run independently of effective-record
  // equality (r1 had nested it inside the effective fold gate).
  const ctx = await startFollowing("r2-replay");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);

  ctx.receive(effortEvent(2, "high", null, "launch"));
  const newerAt = "2026-09-22T00:03:00Z";
  ctx.receive({
    ...effortEvent(3, "xhigh", false, "remuda"),
    observedAt: newerAt,
    payload: {
      effective: { name: "xhigh", ultracode: false, source: "remuda", observedAt: newerAt },
      raw: "xhigh",
    },
  } as unknown as Observation);
  // The queued configure row, journaled AFTER the effective edge, replayed
  // last: pending re-appears over the already-newer effective.
  ctx.receive(configureLifecycle(4, "effort-queued:max", ctx.instance.id));
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.observedAt).toBe(newerAt);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);

  // An IDENTICAL durable projection (fresh object, equal content/observedAt)
  // showing XHIGH while the queued request asked for MAX: r10 identity — a
  // non-matching value cannot confirm the request, so the queued indicator
  // survives even though the projection is newer.
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        effortEffective: { name: "xhigh", ultracode: false, source: "remuda", observedAt: newerAt },
      },
    ],
  } as never);
  await hubStore.refresh();

  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.observedAt).toBe(newerAt);

  // A later poll carrying the matching MAX level (strictly newer than the
  // pre-request baseline) settles independently of effective-record equality.
  const maxAt = "2026-09-22T00:04:00Z";
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        effortEffective: { name: "max", ultracode: false, source: "remuda", observedAt: maxAt },
      },
    ],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
});

it("a load-earlier replay hydrates observations but never queues; a live queued switch survives it and settles on a current poll (c-perffu r4-2 merge)", async () => {
  // Merge of c-perffu r4-2 onto the c-effortread r8 replay rule: history rows
  // (onPrepend) hydrate the effective map but are NEVER treated as live
  // frames — a historical queued configure in the older page cannot surface a
  // 切换中 indicator. This still drives the REAL load-earlier path (floor
  // descriptor, beforeSeq paging, monotonic effective guard) and then proves
  // poll settlement (stale t00:05 below the request; an equal-instant t00:14
  // must NOT settle — r11 item 4 requires a STRICTLY newer read-back — and a
  // later t00:15 with the matching level settles) for a genuinely live queued
  // switch.
  const id = "ins_effort_prepend";
  const journalId = "obj_effort_prepend";
  const instance: Instance = {
    ...mockDb.instances[0],
    id,
    journalId,
    kind: "claude",
    effortName: null,
    effortIndex: null,
    effortUltracode: null,
    activity: { state: "known", value: "idle" },
  } as Instance;
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  let deliverLive!: (observation: Observation) => void;
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async (_j, _a, onBatch) => {
    deliverLive = (observation) =>
      onBatch({
        subscriptionId: `sub_${id}`,
        journalId,
        fromSeq: observation.seq,
        toSeq: observation.seq,
        durableSeq: observation.seq,
        events: [observation],
      });
    return subscription(instance);
  });

  const seedEvents = [
    // The bounded tail window: only the newer remuda read-back. No queued row.
    { ...effortEvent(5, "xhigh", false, "remuda") },
  ];
  const olderEvents = [
    // The older launch level (monotonic guard must not roll effective back)…
    { ...effortEvent(1, "high", null, "launch") },
    // …and a historical queued configure row. Under the r8 replay rule this is
    // HISTORY and must NOT queue an indicator.
    configureLifecycle(4, "effort-queued:max", id),
  ];
  const olderRequests: unknown[] = [];

  vi.spyOn(api, "eventsRead").mockImplementation(async (args) => {
    if (args && "beforeSeq" in args && args.beforeSeq !== undefined) {
      olderRequests.push(args);
      return {
        events: olderEvents,
        durableSeq: "5",
        windowFromSeq: "1",
        reachedAfterSeq: true,
        getReadyState: () => 1,
      } as never;
    }
    return {
      events: seedEvents,
      durableSeq: "5",
      // Loaded floor is seq 5, strictly above the older rows.
      windowFromSeq: "5",
      reachedAfterSeq: true,
      getReadyState: () => 1,
    } as never;
  });

  await hubStore.follow(id);
  expect(hubStore.effortEffectiveOf(id)?.observedAt).toBe("2026-09-16T00:05:00.000Z");
  expect(hubStore.effortPendingOf(id)).toBeNull();

  // Real load-earlier: floor 5 -> beforeSeq must be "4". loadEarlier returns
  // the bounded-read descriptor {prepended,end,floor} (UO-6a load-earlier).
  const result = await hubStore.loadEarlier(id);
  expect(result).not.toBeNull();
  expect(result).toMatchObject({ prepended: true, end: true, floor: "1" });
  expect(result!.floor).toBe("1");
  expect(olderRequests).toHaveLength(1);
  expect((olderRequests[0] as { beforeSeq?: string }).beforeSeq).toBe("4");

  // The older launch edge never rolls effective back, and the REPLAYED queued
  // lifecycle is ignored: no pending indicator appears from history.
  expect(hubStore.effortEffectiveOf(id)?.name).toBe("xhigh");
  expect(hubStore.effortEffectiveOf(id)?.observedAt).toBe("2026-09-16T00:05:00.000Z");
  expect(hubStore.effortPendingOf(id)).toBeNull();

  // The genuinely in-flight request arrives LIVE and queues.
  deliverLive(configureLifecycle(6, "effort-queued:max", id, "2026-09-16T00:14:00.000Z"));
  expect(hubStore.effortPendingOf(id)?.queued).toBe(true);

  // A stale poll carrying a read-back OLDER than the queued request
  // (00:05 < 00:14) must NOT clear the switch.
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      {
        ...instance,
        effortEffective: {
          name: "xhigh",
          ultracode: false,
          source: "remuda",
          observedAt: "2026-09-16T00:05:00Z",
        },
      },
    ],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortPendingOf(id)?.queued).toBe(true);

  // A read-back at the SAME parsed instant as the queued request (the durable
  // record truncates milliseconds) could be the state the previous command
  // left behind: r11 item 4 requires STRICTLY newer, so it stays pending.
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      {
        ...instance,
        effortEffective: {
          name: "max",
          ultracode: false,
          source: "remuda",
          observedAt: "2026-09-16T00:14:00Z",
        },
      },
    ],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortPendingOf(id)).not.toBeNull();

  // A STRICTLY later read-back naming the requested level settles it.
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      {
        ...instance,
        effortEffective: {
          name: "max",
          ultracode: false,
          source: "remuda",
          observedAt: "2026-09-16T00:15:00Z",
        },
      },
    ],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortPendingOf(id)).toBeNull();
});


it("a stale effort poll older than the queued configure leaves the queued switch pending (c-perffu r7-3)", async () => {
  // Same fixture shape, isolated: baseline read-back t1, queued configure
  // journaled at t2, then a poll returns an effective at t1.5 (newer than the
  // baseline, older than the request) — pending must survive.
  const ctx = await startFollowing("poll-queued-stale", { state: "known", value: "idle" });
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const t1 = "2026-09-16T00:10:00Z";
  const t15 = "2026-09-16T00:10:30Z";
  const t2 = "2026-09-16T00:11:00Z";
  const list = vi.spyOn(api, "instanceList");
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  // Baseline effective t1.
  list.mockResolvedValue({
    items: [{ ...ctx.instance, effortEffective: { name: "high", ultracode: null, source: "launch", observedAt: t1 } }],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.observedAt).toBe(t1);

  // User queues a max switch: baseline t1.
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });

  // Live delivery: the queued configure is journaled at t2 (the r10+ driver
  // echoes the configure request id).
  const queued = {
    ...configureLifecycleFor(2, "effort-queued:max", posts[0]?.requestId!, ctx.instance.id),
    observedAt: t2,
  };
  ctx.receive(queued);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);

  // A stale poll: t1.5 is newer than baseline t1 but older than the request t2.
  list.mockResolvedValue({
    items: [{ ...ctx.instance, effortEffective: { name: "high", ultracode: null, source: "launch", observedAt: t15 } }],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);

  // An equal-instant read-back (t2, same parsed second as the queued edge)
  // stays pending under r11 item 4's strictly-newer bound.
  list.mockResolvedValue({
    items: [{ ...ctx.instance, effortEffective: { name: "max", ultracode: false, source: "remuda", observedAt: t2 } }],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);

  // A STRICTLY later id-less read-back no longer settles either (r17 item 1):
  // the queued edge carried THIS request's id, so the driver proved itself
  // id-capable and an id-less durable projection may be an unowned command's
  // verdict. The effective view updates but B waits for its own verdict.
  const t230 = "2026-09-16T00:11:30Z";
  list.mockResolvedValue({
    items: [{ ...ctx.instance, effortEffective: { name: "max", ultracode: false, source: "remuda", observedAt: t230 } }],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.observedAt).toBe(t230);

  // B's own id-bearing applied verdict settles it.
  ctx.receive(configureLifecycleFor(3, "effort-applied:max", posts[0]?.requestId!, ctx.instance.id));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

// ─────────────────────── c-effortread r10 items 1–3 ───────────────────────

it("r10 item 1: an unchanged older poll cannot settle a queued ON (baseline and queued times stay separate)", async () => {
  const ctx = await startFollowing("r10-item1", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const list = vi.spyOn(api, "instanceList");
  const posts: { requestId?: string }[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  // Pre-request baseline: high, flag OFF at T0.
  const t0 = "2026-10-01T00:00:00.000Z";
  list.mockResolvedValue({
    items: [{ ...ctx.instance, effortEffective: { name: "high", ultracode: false, source: "remuda", observedAt: t0 } }],
  } as never);
  await hubStore.refresh();

  // User queues the orthogonal switch ON while the agent is working.
  const post = hubStore.setUltracode(ctx.instance.id, true);
  const requestId = posts[0]?.requestId;
  expect(requestId).toBeTruthy();

  // THIS request's queued lifecycle (id echoed by the driver) lands at T1,
  // strictly after the baseline.
  const t1 = "2026-10-01T00:01:00.000Z";
  ctx.receive(configureLifecycleFor(2, "effort-queued:ultracode on", requestId!, ctx.instance.id, t1));
  const pendingAtT1 = hubStore.effortPendingOf(ctx.instance.id);
  expect(pendingAtT1?.queued).toBe(true);
  expect(pendingAtT1?.queuedObservedAt).toBe(t1);
  expect(pendingAtT1?.baselineObservedAt).toBe(t0);
  await post;

  // A stale poll carrying the UNCHANGED pre-request state ({high,false}@T0):
  // matching tier must never identify the ON request; the indicator stays.
  list.mockResolvedValue({
    items: [{ ...ctx.instance, effortEffective: { name: "high", ultracode: false, source: "remuda", observedAt: t0 } }],
  } as never);
  await hubStore.refresh();
  const stillQueued = hubStore.effortPendingOf(ctx.instance.id);
  expect(stillQueued).not.toBeNull();
  expect(stillQueued?.queued).toBe(true);
  expect(stillQueued?.requestId).toBe(requestId);

  // A genuine post-request ON read-back at T2 updates the effective view but,
  // under r17 item 1, does NOT settle: the queued edge proved the driver is
  // id-capable for THIS request, so an id-less poll projection could be an
  // unowned command's verdict. B stays pending for its own id verdict.
  const t2 = "2026-10-01T00:02:00.000Z";
  list.mockResolvedValue({
    items: [{ ...ctx.instance, effortEffective: { name: "high", ultracode: true, source: "remuda", observedAt: t2 } }],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.ultracode).toBe(true);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(requestId);

  // B's own id-bearing applied verdict settles.
  ctx.receive(configureLifecycleFor(3, "effort-applied:ultracode on", requestId!, ctx.instance.id));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r10 item 2: request A OFF's executing read-back (echo id A) vetoes queued request B ON", async () => {
  const ctx = await startFollowing("r10-item2", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  // Observed baseline: high with the flag ON.
  ctx.receive(effortEvent(2, "high", true, "slash", "2026-10-02T00:00:00.000Z"));

  // A flips OFF, then before it runs B flips ON again.
  const a = hubStore.setUltracode(ctx.instance.id, false);
  const idA = posts[0]?.requestId;
  const b = hubStore.setUltracode(ctx.instance.id, true);
  const idB = posts[1]?.requestId;
  expect(idA).toBeTruthy();
  expect(idB).toBeTruthy();
  expect(idA).not.toBe(idB);
  await Promise.all([a, b]);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idB);

  // A's queued/applied edges (id A) arrive late: dropped, B untouched.
  ctx.receive(configureLifecycleFor(3, "effort-queued:ultracode off", idA!, ctx.instance.id, "2026-10-02T00:01:00.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idB);
  ctx.receive(configureLifecycleFor(4, "effort-applied:ultracode off", idA!, ctx.instance.id, "2026-10-02T00:01:05.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idB);

  // A EXECUTES: its read-back echoes request id A with the flag now OFF.
  // Matching tier ("high") must never confirm B's flag operation.
  ctx.receive(
    effortEvent(5, "high", false, "remuda", "2026-10-02T00:01:10.000Z", {
      requested: { name: "high", ultracode: false },
      requestId: idA,
    }),
  );
  const pendingAfterA = hubStore.effortPendingOf(ctx.instance.id);
  expect(pendingAfterA?.requestId).toBe(idB);
  expect(pendingAfterA?.flagSettled).toBe(false);
  expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(true);

  // B's own ON read-back settles.
  ctx.receive(
    effortEvent(6, "high", true, "remuda", "2026-10-02T00:02:00.000Z", {
      requested: { name: "high", ultracode: true },
      requestId: idB,
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r10 item 3: request A's timeout/degraded verdict never deletes or rolls back newer request B", async () => {
  const ctx = await startFollowing("r10-item3", { state: "known", value: "working" });
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  ctx.receive(effortEvent(2, "high", true, "slash", "2026-10-03T00:00:00.000Z"));
  const a = hubStore.setUltracode(ctx.instance.id, false);
  const idA = posts[0]?.requestId!;
  await a;
  await hubStore.setUltracode(ctx.instance.id, true);
  const idB = posts[1]?.requestId!;
  expect(idA).not.toBe(idB);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idB);
  expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(true);

  // A times out with no read-back: its degraded verdict carries id A.
  ctx.receive(
    configureLifecycleFor(3, "effort-degraded:ultracode off:no-readback-within-window", idA, ctx.instance.id, "2026-10-03T00:01:00.000Z"),
  );
  const bSurvives = hubStore.effortPendingOf(ctx.instance.id);
  expect(bSurvives?.requestId).toBe(idB);
  expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(true);
  expect(toast).not.toHaveBeenCalled();

  // A dialog-kept cancellation of A likewise never touches B.
  ctx.receive(
    configureLifecycleFor(4, "effort-degraded:ultracode off:dialog-kept", idA, ctx.instance.id, "2026-10-03T00:01:05.000Z"),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idB);
  expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(true);
  expect(toast).not.toHaveBeenCalled();

  // B's own timeout DOES end B (exact id binding) and rolls its selection
  // back to the observed OFF state A left behind.
  ctx.receive(
    effortEvent(5, "high", false, "remuda", "2026-10-03T00:01:10.000Z"),
  );
  ctx.receive(
    configureLifecycleFor(6, "effort-degraded:ultracode on:no-readback-within-window", idB, ctx.instance.id, "2026-10-03T00:02:00.000Z"),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(false);
  expect(toast).toHaveBeenCalledTimes(1);
});

it("r10 item 3: a genuine model/workflow refusal for A is recorded but never deletes B", async () => {
  const ctx = await startFollowing("r10-item3-refusal", { state: "known", value: "working" });
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  await hubStore.setUltracode(ctx.instance.id, false);
  const idA = posts[0]?.requestId!;
  await hubStore.setUltracode(ctx.instance.id, true);
  const idB = posts[1]?.requestId!;
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idB);

  // A's model refusal is process/model environment evidence: recorded (the
  // switch is disabled for this environment) but B's optimistic selection and
  // indicator are NOT rolled back by A's verdict.
  ctx.receive(
    configureLifecycleFor(3, "effort-degraded:ultracode off:ultracode-unavailable-for-model", idA, ctx.instance.id, "2026-10-04T00:01:00.000Z"),
  );
  expect(hubStore.effortRefusalOf(ctx.instance.id)).not.toBeNull();
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idB);
  expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(true);
  void toast;
});

it("r11 item 2: A ON executes, B OFF queues onto the same observation — A's ON echo and an unchanged baseline poll keep B pending", async () => {
  const ctx = await startFollowing("r11-item2-on-off");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const list = vi.spyOn(api, "instanceList");
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  // Observed baseline: high with the flag OFF (B's own target).
  const t0 = "2026-10-05T00:00:00.000Z";
  ctx.receive(effortEvent(2, "high", false, "slash", t0));

  // A flips ON and executes; before it reads back B flips OFF again.
  const a = hubStore.setUltracode(ctx.instance.id, true);
  const idA = posts[0]?.requestId!;
  const b = hubStore.setUltracode(ctx.instance.id, false);
  const idB = posts[1]?.requestId!;
  await Promise.all([a, b]);
  const pendingB = hubStore.effortPendingOf(ctx.instance.id);
  expect(pendingB?.requestId).toBe(idB);
  expect(pendingB?.levelSettled).toBe(true); // the flag axis never moves the level
  expect(pendingB?.flagSettled).toBe(false); // moved axis starts unsettled even though baseline matches

  // A EXECUTES: its ON verdict echoes A's id. It must return B unchanged.
  ctx.receive(
    effortEvent(3, "high", true, "remuda", "2026-10-05T00:01:00.000Z", {
      requested: { name: "high", ultracode: true },
      requestId: idA,
    }),
  );
  const afterA = hubStore.effortPendingOf(ctx.instance.id);
  expect(afterA?.requestId).toBe(idB);
  expect(afterA?.flagSettled).toBe(false);

  // An unchanged baseline poll ({high,false}@T0 — exactly B's target and the
  // old observation) must not clear B either: no id, not strictly newer.
  list.mockResolvedValue({
    items: [{ ...ctx.instance, effortEffective: { name: "high", ultracode: false, source: "remuda", observedAt: t0 } }],
  } as never);
  await hubStore.refresh();
  const afterPoll = hubStore.effortPendingOf(ctx.instance.id);
  expect(afterPoll?.requestId).toBe(idB);
  expect(afterPoll?.flagSettled).toBe(false);

  // B's own OFF read-back settles.
  ctx.receive(
    effortEvent(4, "high", false, "remuda", "2026-10-05T00:02:00.000Z", {
      requested: { name: "high", ultracode: false },
      requestId: idB,
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r11 item 2 tier analogue: A max executes, B high queues — A's max echo keeps B's tier request pending", async () => {
  const ctx = await startFollowing("r11-item2-tier");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  ctx.receive(effortEvent(2, "high", false, "slash", "2026-10-06T00:00:00.000Z"));

  // A drags to max, then B drags back to high before A reads back.
  const a = hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  const idA = posts[0]?.requestId!;
  const bst = hubStore.setEffort(ctx.instance.id, { index: 2, name: "high", kind: "claude", ultracode: false });
  const idB = posts[1]?.requestId!;
  await Promise.all([a, bst]);
  const pendingB = hubStore.effortPendingOf(ctx.instance.id);
  expect(pendingB?.requestId).toBe(idB);
  expect(pendingB?.axis).toBe("tier");
  expect(pendingB?.levelSettled).toBe(false); // moved tier starts unsettled though baseline IS high
  expect(pendingB?.flagSettled).toBe(true); // untouched flag axis

  // A's max verdict (id A) can never settle B's high request.
  ctx.receive(
    effortEvent(3, "max", false, "remuda", "2026-10-06T00:01:00.000Z", {
      requested: { name: "max", ultracode: false },
      requestId: idA,
    }),
  );
  const afterA = hubStore.effortPendingOf(ctx.instance.id);
  expect(afterA?.requestId).toBe(idB);
  expect(afterA?.levelSettled).toBe(false);

  // B's own high verdict settles.
  ctx.receive(
    effortEvent(4, "high", false, "remuda", "2026-10-06T00:02:00.000Z", {
      requested: { name: "high", ultracode: false },
      requestId: idB,
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r11 item 2: an accepted NO-OP switch still settles from its own request-id completion echo", async () => {
  const ctx = await startFollowing("r11-item2-noop");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  // Flag already OFF; the OFF configure is a no-op at the native level.
  ctx.receive(effortEvent(2, "high", false, "slash", "2026-10-07T00:00:00.000Z"));
  const posted = hubStore.setUltracode(ctx.instance.id, false);
  const idB = posts[0]?.requestId!;
  await posted;
  // The moved axis starts unsettled even though the value already matches.
  expect(hubStore.effortPendingOf(ctx.instance.id)?.flagSettled).toBe(false);

  // The driver's accepted no-op verdict changes no projection but carries the
  // request-specific completion evidence (its own id): that settles the axis.
  ctx.receive(
    effortEvent(3, "high", false, "remuda", "2026-10-07T00:01:00.000Z", {
      requested: { name: "high", ultracode: false },
      requestId: idB,
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r11 item 2: a wire-synthesized queued no-op starts its moved axis unsettled", async () => {
  const ctx = await startFollowing("r11-item2-wire-queued", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  // Observed flag OFF; a configure posted OUTSIDE this UI queues an OFF (no-op).
  ctx.receive(effortEvent(2, "high", false, "slash", "2026-10-08T00:00:00.000Z"));
  ctx.receive(
    configureLifecycleFor(3, "effort-queued:ultracode off", "wire-noop", ctx.instance.id, "2026-10-08T00:01:00.000Z"),
  );
  const queued = hubStore.effortPendingOf(ctx.instance.id);
  expect(queued?.requestId).toBe("wire-noop");
  expect(queued?.flagSettled).toBe(false); // moved axis must not pre-settle on the matching baseline

  // Its own accepted OFF completion settles it.
  ctx.receive(
    effortEvent(4, "high", false, "remuda", "2026-10-08T00:02:00.000Z", {
      requested: { name: "high", ultracode: false },
      requestId: "wire-noop",
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r11 item 3: an id-less older-driver A OFF timeout while B ON is queued keeps B and never rolls back", async () => {
  const ctx = await startFollowing("r11-item3-iddless", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  ctx.receive(effortEvent(2, "high", true, "slash", "2026-10-09T00:00:00.000Z"));

  // A flips OFF (older driver will journal NO request id), then B flips ON.
  const a = hubStore.setUltracode(ctx.instance.id, false);
  await a;
  const idB = (await (async () => {
    await hubStore.setUltracode(ctx.instance.id, true);
    return posts[1]?.requestId!;
  })());
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idB);
  expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(true);

  // A times out with an ID-LESS degraded verdict (both the read-back-window
  // and the dialog-kept words). Command-word matching alone used to delete the
  // newer client request B and roll its selection back.
  ctx.receive(
    configureLifecycle(3, "effort-degraded:ultracode off:no-readback-within-window", ctx.instance.id, "2026-10-09T00:01:00.000Z"),
  );
  let b = hubStore.effortPendingOf(ctx.instance.id);
  expect(b, "B survives A's id-less no-readback verdict").not.toBeNull();
  expect(b?.requestId).toBe(idB);
  expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(true);
  expect(toast, "an id-less verdict that cannot name A raises no toast").not.toHaveBeenCalled();

  ctx.receive(
    configureLifecycle(4, "effort-degraded:ultracode off:dialog-kept", ctx.instance.id, "2026-10-09T00:01:05.000Z"),
  );
  b = hubStore.effortPendingOf(ctx.instance.id);
  expect(b, "B survives A's id-less dialog-kept verdict").not.toBeNull();
  expect(b?.requestId).toBe(idB);
  expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(true);
  expect(toast).not.toHaveBeenCalled();

  // B's own id-bearing completion settles normally.
  ctx.receive(configureLifecycleFor(5, "effort-applied:ultracode on", idB, ctx.instance.id, "2026-10-09T00:02:00.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r11 item 3: an id-less genuine model/workflow refusal is recorded as environment evidence but never deletes B", async () => {
  const ctx = await startFollowing("r11-item3-envrefuse", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  await hubStore.setUltracode(ctx.instance.id, false);
  await hubStore.setUltracode(ctx.instance.id, true);
  const idB = posts[1]?.requestId!;

  // An id-less workflows-disabled refusal (older driver, possibly A's
  // environment) is genuine environment evidence: the switch is disabled, but
  // the newer client request B is neither deleted nor rolled back.
  ctx.receive(
    configureLifecycle(3, "effort-degraded:ultracode off:ultracode-workflows-disabled", ctx.instance.id, "2026-10-10T00:01:00.000Z"),
  );
  expect(hubStore.effortRefusalOf(ctx.instance.id)).not.toBeNull();
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idB);
  expect(hubStore.effortOf(ctx.instance.id, "claude").ultracode).toBe(true);
});

it("r12 item 2: id-less applied/degraded lifecycles never clear an observer tab's lifecycle pending", async () => {
  const ctx = await startFollowing("r12-item2-observer", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  // Another client's configure, observed from THIS tab only over the wire:
  // baseline flag ON, then an ID-LESS queued edge synthesizes a lifecycle
  // pending for an OFF switch.
  ctx.receive(effortEvent(2, "high", true, "slash", "2026-10-12T00:00:00.000Z"));
  ctx.receive(
    configureLifecycle(3, "effort-queued:ultracode off", ctx.instance.id, "2026-10-12T00:01:00.000Z"),
  );
  const queued = hubStore.effortPendingOf(ctx.instance.id);
  expect(queued?.source).toBe("lifecycle");
  expect(queued?.ultracode).toBe(false);
  const queuedId = queued?.requestId!;

  // An id-less APPLIED edge with the same word cannot name the request: it
  // may be a replaced request A's stale verdict. It must not clear B's view;
  // legacy success arrives only through the strict read-back fallback.
  ctx.receive(
    configureLifecycle(4, "effort-applied:ultracode off", ctx.instance.id, "2026-10-12T00:02:00.000Z"),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId, "id-less applied keeps B pending").toBe(queuedId);

  // Id-less timeout and dialog-kept verdicts likewise neither delete nor roll
  // back, and they raise no toast (the request is still in flight).
  ctx.receive(
    configureLifecycle(5, "effort-degraded:ultracode off:no-readback-within-window", ctx.instance.id, "2026-10-12T00:03:00.000Z"),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId, "id-less timeout keeps B pending").toBe(queuedId);
  expect(toast, "an id-less timeout that cannot name the request raises no toast").not.toHaveBeenCalled();

  ctx.receive(
    configureLifecycle(6, "effort-degraded:ultracode off:dialog-kept", ctx.instance.id, "2026-10-12T00:04:00.000Z"),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId, "id-less dialog-kept keeps B pending").toBe(queuedId);
  expect(toast).not.toHaveBeenCalled();

  // The strict legacy read-back fallback (fresh echo, past both bounds, value
  // matches the moved flag) is still the ONE id-less path that settles.
  ctx.receive(
    effortEvent(7, "high", false, "remuda", "2026-10-12T00:05:00.000Z", {
      requested: { ultracode: false },
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id), "the strict read-back fallback settles B").toBeNull();
});

it("r12 item 2: an id-less genuine refusal is recorded but never clears an observer tab's lifecycle pending", async () => {
  const ctx = await startFollowing("r12-item2-envrefuse", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);

  ctx.receive(effortEvent(2, "high", true, "slash", "2026-10-12T01:00:00.000Z"));
  ctx.receive(
    configureLifecycle(3, "effort-queued:ultracode off", ctx.instance.id, "2026-10-12T01:01:00.000Z"),
  );
  const queuedId = hubStore.effortPendingOf(ctx.instance.id)?.requestId!;

  // Id-less environment evidence: the refusal IS recorded (switch disabled),
  // but the lifecycle pending it cannot name survives.
  ctx.receive(
    configureLifecycle(4, "effort-degraded:ultracode off:ultracode-workflows-disabled", ctx.instance.id, "2026-10-12T01:02:00.000Z"),
  );
  expect(hubStore.effortRefusalOf(ctx.instance.id)).not.toBeNull();
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(queuedId);
});

it("r11 item 4: an older driver's clamped max echo (id-less) never settles queued C=max past the timestamp and effective-value bounds", async () => {
  const ctx = await startFollowing("r11-item4-clamp", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  // Observed low.
  ctx.receive(effortEvent(2, "low", false, "slash", "2026-10-11T00:00:00.000Z"));

  // A drags to max and executes; before it reads back the user picks low (B)
  // and then max again (C). The older driver (no request ids) has clamped A
  // to high; its verdict is outstanding.
  const aPost = hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  await hubStore.setEffort(ctx.instance.id, { index: 2, name: "low", kind: "claude", ultracode: false });
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  const idC = posts[2]?.requestId!;
  await aPost;
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idC);

  // C's own queued edge at t2.
  ctx.receive(configureLifecycleFor(3, "effort-queued:max", idC, ctx.instance.id, "2026-10-11T00:02:00.000Z"));

  // A's delayed verdict: an ID-LESS max echo but the EFFECTIVE value is high
  // (clamped) at t1, BEFORE C queued. It must not settle C even though its
  // requested word names max.
  ctx.receive(
    effortEvent(4, "high", false, "remuda", "2026-10-11T00:01:00.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  const c = hubStore.effortPendingOf(ctx.instance.id);
  expect(c, "the clamped id-less echo never authorizes C").not.toBeNull();
  expect(c?.requestId).toBe(idC);
  expect(c?.levelSettled).toBe(false);

  // An id-less max echo at a time EQUAL to the queued instant still cannot
  // settle (must be strictly newer), even with the matching value.
  ctx.receive(
    effortEvent(5, "max", false, "remuda", "2026-10-11T00:02:00.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idC);

  // C's own id-bearing read-back settles it.
  ctx.receive(
    effortEvent(6, "max", false, "remuda", "2026-10-11T00:03:00.000Z", {
      requested: { name: "max", ultracode: false },
      requestId: idC,
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r12 item 3: an id-less queued edge is C's lower bound — A's stale max read-back cannot settle C", async () => {
  const ctx = await startFollowing("r12-item3-queued-bound", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  // Baseline low BEFORE any request (clock is pinned at 00:00:00).
  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));

  // A drags to max and executes; before it reads back the user picks low (B)
  // and then max (C). The driver is older: every queued edge is ID-LESS.
  const aPost = hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  await hubStore.setEffort(ctx.instance.id, { index: 2, name: "low", kind: "claude", ultracode: false });
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  await aPost;
  const idC = posts[2]?.requestId!;
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idC);

  // B's id-less queued edge is word-mismatched and dropped; C itself
  // replaced B, so r13 item 1 makes C's own same-word id-less queued edge
  // ambiguous (A was also max): it must NOT bind — C falls back to its own
  // client send time as the legacy lower bound.
  ctx.receive(configureLifecycle(3, "effort-queued:low", ctx.instance.id, "2026-09-16T00:00:10.000Z"));
  ctx.receive(configureLifecycle(4, "effort-queued:max", ctx.instance.id, "2026-09-16T00:00:30.000Z"));
  expect(
    hubStore.effortPendingOf(ctx.instance.id)?.queuedObservedAt,
    "an ambiguous same-word edge after A→B→C never binds",
  ).toBeNull();

  // A's DELAYED max read-back (T0): post-baseline, value matches C, but older
  // than C's queued bound. It must stay a veto, not settle C.
  ctx.receive(
    effortEvent(5, "max", false, "remuda", "2026-09-15T23:59:59.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  const c = hubStore.effortPendingOf(ctx.instance.id);
  expect(c, "A's stale max read-back never settles C").not.toBeNull();
  expect(c?.requestId).toBe(idC);
  expect(c?.levelSettled).toBe(false);

  // A genuinely fresh read-back past C's queued bound settles.
  ctx.receive(
    effortEvent(6, "max", false, "remuda", "2026-09-16T00:00:45.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r12 item 3: with no queued edge the client send time is the legacy lower bound", async () => {
  const ctx = await startFollowing("r12-item3-send-bound", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  const aPost = hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  await hubStore.setEffort(ctx.instance.id, { index: 2, name: "low", kind: "claude", ultracode: false });
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  await aPost;

  // No queued edge at all (older driver). A read-back produced BEFORE C even
  // left the client (clock pins the send at 00:00:00) cannot settle C.
  ctx.receive(
    effortEvent(3, "max", false, "remuda", "2026-09-15T23:59:59.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id), "pre-send read-back stays pending").not.toBeNull();

  // Post-send fresh evidence settles through the strict fallback.
  ctx.receive(
    effortEvent(4, "max", false, "remuda", "2026-09-16T00:00:05.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r12 item 3: a wire pending with no parseable queued bound fails closed", async () => {
  const ctx = await startFollowing("r12-item3-fail-closed", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  // A configure posted outside this UI; the queued edge carries no id and an
  // unparseable observedAt, so neither request id nor a trustworthy time bound
  // exists. The pending must not settle on a value-matching read-back.
  ctx.receive(configureLifecycle(3, "effort-queued:max", ctx.instance.id, "not-a-timestamp"));
  const queued = hubStore.effortPendingOf(ctx.instance.id);
  expect(queued?.source).toBe("lifecycle");
  expect(queued?.queuedObservedAt).toBeNull();

  ctx.receive(
    effortEvent(4, "max", false, "remuda", "2026-09-16T00:00:30.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  expect(
    hubStore.effortPendingOf(ctx.instance.id),
    "no trustworthy lower bound: fail closed, stay pending",
  ).not.toBeNull();
});

it("r13 item 1: a delayed same-word A edge after A→B→C never lowers C's send-time bound", async () => {
  const ctx = await startFollowing("r13-item1-backlog", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  // Baseline low at T0, before A.
  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:50.000Z"));

  // A=max executes at T1 (23:59:55 client time); its id-less queued edge and
  // its max read-back are delayed in the delivery backlog.
  vi.setSystemTime(new Date("2026-09-15T23:59:55.000Z"));
  const aPost = hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  // The user changes their mind twice at T3: B=low then C=max (same word as A).
  vi.setSystemTime(new Date("2026-09-16T00:00:10.000Z"));
  await hubStore.setEffort(ctx.instance.id, { index: 2, name: "low", kind: "claude", ultracode: false });
  await hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  await aPost;
  const c = hubStore.effortPendingOf(ctx.instance.id);
  expect(c?.afterReplacement, "C replaced B").toBe(true);

  // The backlog flushes A's QUEUED edge first: same word ("max"), but its
  // stamp T1 (23:59:56) is older than C's send and C is a replacement — the
  // edge is ambiguous and must neither claim C nor stamp its bound.
  ctx.receive(configureLifecycle(3, "effort-queued:max", ctx.instance.id, "2026-09-15T23:59:56.000Z"));
  expect(
    hubStore.effortPendingOf(ctx.instance.id)?.nonce,
    "A's stale queued edge never replaces C",
  ).toBe(c?.nonce);

  // Then A's delayed max read-back at T2 (23:59:59): post-baseline and value
  // matches C, but older than C's own send time (T3 = 00:00:10). It must stay
  // a veto — before the fix the stamped T1 bound let this settle C.
  ctx.receive(
    effortEvent(4, "max", false, "remuda", "2026-09-15T23:59:59.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending, "A's stale read-back never settles C").not.toBeNull();
  expect(pending?.nonce).toBe(c?.nonce);
  expect(pending?.levelSettled).toBe(false);
  expect(
    pending?.queuedObservedAt,
    "an ambiguous word-only edge after A→B→C never lowers the send-time bound",
  ).toBeNull();

  // A genuinely fresh read-back produced AFTER C left the client settles it
  // through the strict fallback.
  ctx.receive(
    effortEvent(5, "max", false, "remuda", "2026-09-16T00:00:30.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r14 item 1: an older same-word queued edge never lowers an observer pending's established bound", async () => {
  const ctx = await startFollowing("r14-item1-monotonic", { state: "known", value: "idle" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);

  // Observer baseline low at T0; this tab only watches the wire.
  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:50.000Z"));

  // B's id-less effort-queued:max arrives at T3 and establishes the pending's
  // queued bound (T0 < T1 < T2 < T3).
  ctx.receive(configureLifecycle(3, "effort-queued:max", ctx.instance.id, "2026-09-16T00:00:30.000Z"));
  const b = hubStore.effortPendingOf(ctx.instance.id);
  expect(b?.source).toBe("lifecycle");
  expect(b?.queuedObservedAt).toBe("2026-09-16T00:00:30.000Z");

  // The backlog then flushes the REPLACED request A's delayed edge: same word
  // ("max"), an id-less observer edge matches by word, but its stamp T1 is
  // OLDER than the established bound.
  ctx.receive(configureLifecycle(4, "effort-queued:max", ctx.instance.id, "2026-09-16T00:00:10.000Z"));
  const stillB = hubStore.effortPendingOf(ctx.instance.id);
  expect(stillB?.nonce).toBe(b?.nonce);
  expect(
    stillB?.queuedObservedAt,
    "the older edge never replaces the established queued bound",
  ).toBe("2026-09-16T00:00:30.000Z");

  // A's matching max read-back at T2 is newer than the baseline (T0) and value
  // matches, but OLDER than B's queued bound (T3): it must never settle B.
  ctx.receive(
    effortEvent(5, "max", false, "remuda", "2026-09-16T00:00:20.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  const vetoed = hubStore.effortPendingOf(ctx.instance.id);
  expect(vetoed, "A's read-back between the two edges never settles B").not.toBeNull();
  expect(vetoed?.nonce).toBe(b?.nonce);
  expect(vetoed?.levelSettled).toBe(false);

  // A genuinely fresh read-back AFTER B's bound settles through the fallback.
  ctx.receive(
    effortEvent(6, "max", false, "remuda", "2026-09-16T00:00:45.000Z", {
      requested: { name: "max", ultracode: false },
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

async function advanceEffortReconcile(ms: number) {
  // Full fake timers (Date stays pinned at the startFollowing clock) so the
  // 15 s bounded reconciliation is deterministic and instant.
  await vi.advanceTimersByTimeAsync(ms);
  // Let the reconciliation's awaited refresh settle.
  await Promise.resolve();
  await Promise.resolve();
}

/**
 * A Node-validated native turn activity event (the same shape the follow
 * socket carries for a shell-pty instance). r13 item 2: the working→idle
 * transition is the ONLY authority on a queued request leaving the queue.
 */
function activityEvent(
  activity: "working" | "idle",
  instanceId = "x",
  nativeName = "Stop",
): Observation {
  return {
    eventId: `evt_act_${nativeName}_${activity}`,
    instanceId,
    journalId: "x",
    seq: "0",
    kind: "lifecycle",
    source: { channel: "runtime" },
    payload: {
      type: "native",
      nativeName,
      status: { state: "known", value: activity },
      relatedIds: { remudaActivity: activity },
    },
  } as unknown as Observation;
}

it("r13 item 2: the bound starts on working → idle evidence, so a no-op after the turn ends unconfirmed", async () => {
  const ctx = await startFollowing("r13-item2-idle", { state: "known", value: "working" }, "shell-pty");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // The id-less driver keeps projecting the unchanged baseline.
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  let listCalls = 0;
  vi.spyOn(api, "instanceList").mockImplementation(async () => {
    listCalls += 1;
    return { items: [baseline] } as never;
  });
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  const p = hubStore.setEffort(ctx.instance.id, { index: 1, name: "low", kind: "claude", ultracode: false });
  await advanceEffortReconcile(0);
  await p;
  const listsAfterPost = listCalls;
  const queued = hubStore.effortPendingOf(ctx.instance.id);
  expect(queued?.queued, "sent while a turn is running").toBe(true);
  expect(queued?.leftQueueAt).toBeNull();

  // The first bound expires WITH the turn still running: the request has not
  // left the queue, so no read-back is attempted and no outcome is surfaced.
  await advanceEffortReconcile(15_001);
  expect(listCalls, "never reconcile a request still in the queue").toBe(listsAfterPost);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);
  expect(toast).not.toHaveBeenCalled();

  // Current activity evidence ends the turn: the queued flag clears and the
  // request-owned 15 s bound starts at the idle instant.
  ctx.receive(activityEvent("idle", ctx.instance.id, "interrupted"));
  const exited = hubStore.effortPendingOf(ctx.instance.id);
  expect(exited?.queued, "the idle evidence tracks the exit from the queue").toBe(false);
  expect(typeof exited?.leftQueueAt).toBe("number");

  // Before the post-idle bound elapses: still a silent pending, no read-back.
  await advanceEffortReconcile(14_999);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  expect(toast).not.toHaveBeenCalled();

  // At the post-idle bound no fresh evidence ever arrived: the request ends
  // explicitly unconfirmed (r13 item 4 — the bound performs no Hub re-read).
  await advanceEffortReconcile(2);
  expect(listCalls, "the bound never re-reads the stored Hub record").toBe(listsAfterPost);
  expect(hubStore.effortPendingOf(ctx.instance.id), "unconfirmed ends the pending").toBeNull();
  expect(toast.mock.calls[0]?.[0] ?? "").toContain("未确认");
});

it("r14 item 2: the bound starts on working → waiting-interaction → idle, so a no-op ends unconfirmed", async () => {
  const ctx = await startFollowing("r14-item2-waiting", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // The id-less driver keeps projecting the unchanged baseline.
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  let listCalls = 0;
  vi.spyOn(api, "instanceList").mockImplementation(async () => {
    listCalls += 1;
    return { items: [baseline] } as never;
  });
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  // A full-instance entity snapshot over the follow socket (the hook-activity
  // tier that carries PermissionRequest/Elicitation and Stop activity).
  const entityActivity = (value: "waiting-interaction" | "idle"): Observation =>
    ({
      eventId: `evt_act_entity_${value}`,
      instanceId: ctx.instance.id,
      journalId: "x",
      seq: "0",
      kind: "lifecycle",
      source: { channel: "runtime" },
      observedAt: "2026-09-16T00:00:10.000Z",
      payload: {
        type: "entity",
        entityType: "instance",
        entityId: ctx.instance.id,
        entity: {
          id: ctx.instance.id,
          activity: { state: "known", value },
          activityEvidenceEventIds: [],
          nativeRef: ctx.instance.nativeRef,
          updatedAt: "2026-09-16T00:00:10.000Z",
        },
      },
    }) as unknown as Observation;

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  // Silent accepted no-op: the request targets the observed low.
  const p = hubStore.setEffort(ctx.instance.id, { index: 1, name: "low", kind: "claude", ultracode: false });
  await advanceEffortReconcile(0);
  await p;
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued, "sent while a turn is running").toBe(true);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.leftQueueAt).toBeNull();

  // The turn blocks on a human: working → waiting-interaction
  // (PermissionRequest/Elicitation). The request is still in the queue.
  ctx.receive(entityActivity("waiting-interaction"));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.leftQueueAt).toBeNull();
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);

  // The bound fires while the request has not demonstrably left the queue:
  // no reconciliation, no outcome (the 30-min max age is the backstop).
  const listsAtWaiting = listCalls;
  await advanceEffortReconcile(15_001);
  expect(listCalls, "never reconcile a request still in the queue").toBe(listsAtWaiting);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  expect(toast).not.toHaveBeenCalled();

  // Stop arrives AFTER the human answers: waiting-interaction → idle. The
  // deadline anchors here even though the previous activity was not working.
  ctx.receive(entityActivity("idle"));
  const exited = hubStore.effortPendingOf(ctx.instance.id);
  expect(exited?.queued, "the idle evidence tracks the exit from the queue").toBe(false);
  expect(typeof exited?.leftQueueAt, "the deadline starts at this idle instant").toBe("number");

  // Before the post-idle bound: still a silent pending.
  await advanceEffortReconcile(14_999);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  expect(toast).not.toHaveBeenCalled();
  // At the bound no fresh evidence arrived: explicit unconfirmed, no DB re-read.
  await advanceEffortReconcile(2);
  expect(listCalls, "the bound never re-reads the stored Hub record").toBe(listsAtWaiting);
  expect(hubStore.effortPendingOf(ctx.instance.id), "unconfirmed ends the pending").toBeNull();
  expect(toast.mock.calls[0]?.[0] ?? "").toContain("未确认");
});

it("r15 item 1: a client request created while waiting-interaction stays queued past 15 s and anchors only on idle", async () => {
  const ctx = await startFollowing("r15-item1-client-waiting", { state: "known", value: "waiting-interaction" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // The id-less driver keeps projecting the unchanged baseline.
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  let listCalls = 0;
  vi.spyOn(api, "instanceList").mockImplementation(async () => {
    listCalls += 1;
    return { items: [baseline] } as never;
  });
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  const entityActivity = (value: "idle"): Observation =>
    ({
      eventId: `evt_act_entity_${value}`,
      instanceId: ctx.instance.id,
      journalId: "x",
      seq: "0",
      kind: "lifecycle",
      source: { channel: "runtime" },
      observedAt: "2026-09-16T00:00:30.000Z",
      payload: {
        type: "entity",
        entityType: "instance",
        entityId: ctx.instance.id,
        entity: {
          id: ctx.instance.id,
          activity: { state: "known", value },
          activityEvidenceEventIds: [],
          nativeRef: ctx.instance.nativeRef,
          updatedAt: "2026-09-16T00:00:30.000Z",
        },
      },
    }) as unknown as Observation;

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  // A configure posted while a PermissionRequest/Elicitation is on screen.
  const p = hubStore.setEffort(ctx.instance.id, { index: 1, name: "low", kind: "claude", ultracode: false });
  await advanceEffortReconcile(0);
  await p;
  const sent = hubStore.effortPendingOf(ctx.instance.id);
  expect(sent?.queued, "waiting-interaction keeps the request queued").toBe(true);
  expect(sent?.leftQueueAt, "the execution deadline is not started yet").toBeNull();

  // The human takes longer than the 15 s bound to answer. The driver still
  // cannot execute (it needs Idle/Done + interactive_ready), so no deadline
  // may fire and end the request unconfirmed.
  const listsAtWaiting = listCalls;
  await advanceEffortReconcile(15_001);
  expect(listCalls, "never reconcile a request still awaiting queue exit").toBe(listsAtWaiting);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  expect(toast).not.toHaveBeenCalled();

  // Only authoritative idle eligibility starts the execution deadline.
  ctx.receive(entityActivity("idle"));
  const exited = hubStore.effortPendingOf(ctx.instance.id);
  expect(exited?.queued, "idle evidence tracks the queue exit").toBe(false);
  expect(typeof exited?.leftQueueAt).toBe("number");

  // With no fresh evidence in the post-idle window: explicit unconfirmed.
  await advanceEffortReconcile(14_999);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  await advanceEffortReconcile(2);
  expect(listCalls).toBe(listsAtWaiting);
  expect(hubStore.effortPendingOf(ctx.instance.id), "unconfirmed ends the pending").toBeNull();
  expect(toast.mock.calls[0]?.[0] ?? "").toContain("未确认");
});

it("r15 item 1: an observer pending created from a queued edge while waiting-interaction anchors only on idle", async () => {
  const ctx = await startFollowing("r15-item1-observer-waiting", { state: "known", value: "waiting-interaction" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  const entityActivity = (value: "idle"): Observation =>
    ({
      eventId: `evt_act_entity_${value}`,
      instanceId: ctx.instance.id,
      journalId: "x",
      seq: "0",
      kind: "lifecycle",
      source: { channel: "runtime" },
      observedAt: "2026-09-16T00:00:30.000Z",
      payload: {
        type: "entity",
        entityType: "instance",
        entityId: ctx.instance.id,
        entity: {
          id: ctx.instance.id,
          activity: { state: "known", value },
          activityEvidenceEventIds: [],
          nativeRef: ctx.instance.nativeRef,
          updatedAt: "2026-09-16T00:00:30.000Z",
        },
      },
    }) as unknown as Observation;

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  // Another client's configure reaches this tab only through the queued edge.
  ctx.receive(configureLifecycle(3, "effort-queued:low", ctx.instance.id, "2026-09-16T00:00:05.000Z"));
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending?.source).toBe("lifecycle");
  expect(pending?.queued, "the chip keeps 排队中 behind the interaction").toBe(true);
  expect(pending?.leftQueueAt, "the observer bound does not start while waiting").toBeNull();

  await advanceEffortReconcile(15_001);
  expect(hubStore.effortPendingOf(ctx.instance.id), "past 15 s it is still queued, not unconfirmed").not.toBeNull();
  expect(toast).not.toHaveBeenCalled();

  ctx.receive(entityActivity("idle"));
  const exited = hubStore.effortPendingOf(ctx.instance.id);
  expect(typeof exited?.leftQueueAt, "the deadline starts at authoritative idle").toBe("number");
  await advanceEffortReconcile(14_999);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  await advanceEffortReconcile(2);
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(toast.mock.calls[0]?.[0] ?? "").toContain("未确认");
});

it("r16 item 4: activity and the queued edge folded out of order in ONE batch must not idle-initialise the pending", async () => {
  // The observer tab is idle when the follow/catch-up batch arrives; the
  // batch carries, in JOURNAL ORDER, a full-instance snapshot parking the
  // session on waiting-interaction and then the driver's effort-queued:low
  // lifecycle. Folding the effort edge against the PRE-batch idle activity
  // started the 15 s deadline immediately while the driver was blocked.
  const ctx = await startFollowing("r16-item4-batch-order");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  const entityActivity = (value: "waiting-interaction" | "idle", observedAt: string): Observation =>
    ({
      eventId: `evt_act_entity_${value}_${observedAt}`,
      instanceId: ctx.instance.id,
      journalId: "x",
      seq: "0",
      kind: "lifecycle",
      source: { channel: "runtime" },
      observedAt,
      payload: {
        type: "entity",
        entityType: "instance",
        entityId: ctx.instance.id,
        entity: {
          id: ctx.instance.id,
          activity: { state: "known", value },
          activityEvidenceEventIds: [],
          nativeRef: ctx.instance.nativeRef,
          updatedAt: observedAt,
        },
      },
    }) as unknown as Observation;

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });

  // Both events arrive together, activity evidence FIRST in journal order.
  ctx.receiveBatch([
    entityActivity("waiting-interaction", "2026-09-16T00:00:05.000Z"),
    configureLifecycle(0, "effort-queued:low", ctx.instance.id, "2026-09-16T00:00:05.500Z"),
  ]);
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending?.source).toBe("lifecycle");
  expect(pending?.queued, "the chip keeps 排队中 behind the interaction").toBe(true);
  expect(pending?.leftQueueAt, "the batch's waiting-interaction evidence blocks the deadline").toBeNull();

  // Held blocked for longer than the 15 s bound: no deadline fires.
  await advanceEffortReconcile(15_001);
  expect(hubStore.effortPendingOf(ctx.instance.id), "still queued, never unconfirmed while blocked").not.toBeNull();
  expect(toast).not.toHaveBeenCalled();

  // A later batch releases the session: authoritative idle starts the FULL
  // post-idle window from this instant.
  ctx.receiveBatch([entityActivity("idle", "2026-09-16T00:00:30.000Z")]);
  const exited = hubStore.effortPendingOf(ctx.instance.id);
  expect(typeof exited?.leftQueueAt, "the full deadline window starts only on idle").toBe("number");
  await advanceEffortReconcile(14_999);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  expect(toast).not.toHaveBeenCalled();
  await advanceEffortReconcile(2);
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(toast.mock.calls[0]?.[0] ?? "").toContain("未确认");
});

it("r15 item 2: a bare coupled effort-queued:ultracode keeps xhigh/on identity and settles from a fresh xhigh/on read-back", async () => {
  const ctx = await startFollowing("r15-item2-bare-ultracode");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "high", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);

  ctx.receive(effortEvent(2, "high", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  // On a coupled build the bare lifecycle word IS the xhigh-tier-plus-flag
  // command (the driver journals exactly "/effort ultracode").
  ctx.receive(configureLifecycle(3, "effort-queued:ultracode", ctx.instance.id, "2026-09-16T00:00:05.000Z"));
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending?.source).toBe("lifecycle");
  expect(pending?.name, "the bare command targets xhigh, not the default high").toBe("xhigh");
  expect(pending?.ultracode, "it carries the flag ON as its second axis").toBe(true);

  // A fresh post-queue read-back of {xhigh, on} settles BOTH axes immediately.
  ctx.receive(effortEvent(4, "xhigh", true, "remuda", "2026-09-16T00:00:10.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id), "fresh xhigh/on read-back settles it").toBeNull();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)).toMatchObject({ name: "xhigh", ultracode: true });
});

it("r15 item 3: a provably newer queued lifecycle replaces the lifecycle-owned pending (A → B → B completes)", async () => {
  const ctx = await startFollowing("r15-item3-observer-ab");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  // Another client's configure A (low), seen by this observer tab only.
  ctx.receive(configureLifecycleFor(3, "effort-queued:low", "req-a", ctx.instance.id, "2026-09-16T00:00:05.000Z"));
  const a = hubStore.effortPendingOf(ctx.instance.id);
  expect(a).toMatchObject({ name: "low", requestId: "req-a", source: "lifecycle" });

  // Before B executed, the same client replaced it with max. The journal
  // ordering proves B newer: the observer must now track B, not A.
  ctx.receive(configureLifecycleFor(4, "effort-queued:max", "req-b", ctx.instance.id, "2026-09-16T00:00:07.000Z"));
  const b = hubStore.effortPendingOf(ctx.instance.id);
  expect(b, "B replaces the lifecycle-owned pending with its own id/target").toMatchObject({
    name: "max",
    requestId: "req-b",
    source: "lifecycle",
  });

  // A's delayed older queued edge (backlog) is rejected: B stays.
  ctx.receive(configureLifecycleFor(5, "effort-queued:low", "req-a", ctx.instance.id, "2026-09-16T00:00:06.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId, "an older replacement never wins").toBe("req-b");

  // B executes and its applied verdict completes the tracked request.
  ctx.receive(configureLifecycleFor(6, "effort-applied:max", "req-b", ctx.instance.id, "2026-09-16T00:00:10.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id), "B completing clears the pending").toBeNull();
});

it("r16 item 5: equal-millisecond queued lifecycles are ordered by journal seq (A → B → B completes)", async () => {
  const ctx = await startFollowing("r16-item5-equal-ms");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  // Protocol timestamps have millisecond resolution: A (journal seq N) and B
  // (seq N+1) carry the SAME observedAt. Timestamp-only ordering cannot tell
  // them apart and left the observer tracking A.
  const sameMs = "2026-09-16T00:00:05.000Z";
  ctx.receive(configureLifecycleFor(3, "effort-queued:low", "req-a", ctx.instance.id, sameMs));
  const aSeq = hubStore.effortPendingOf(ctx.instance.id)?.queuedSeq;
  expect(hubStore.effortPendingOf(ctx.instance.id)).toMatchObject({
    name: "low",
    requestId: "req-a",
    source: "lifecycle",
  });

  // B journals in the SAME millisecond but a strictly later journal seq.
  ctx.receive(configureLifecycleFor(4, "effort-queued:max", "req-b", ctx.instance.id, sameMs));
  const b = hubStore.effortPendingOf(ctx.instance.id);
  expect(b, "the later seq replaces A despite the equal timestamp").toMatchObject({
    name: "max",
    requestId: "req-b",
    source: "lifecycle",
  });
  expect(BigInt(b?.queuedSeq ?? "0"), "B carries its own later journal seq").toBeGreaterThan(
    BigInt(aSeq ?? "0"),
  );

  // B executes and its applied verdict completes the tracked request. The
  // strictly-timestamp older-replacement rejection is covered by the r15
  // item 3 case above (a later edge describing an older request).
  ctx.receive(configureLifecycleFor(6, "effort-applied:max", "req-b", ctx.instance.id, sameMs));
  expect(hubStore.effortPendingOf(ctx.instance.id), "B completing clears the pending").toBeNull();
});

it("r13 item 3: an observer-tab legacy pending with a silent no-op completes unconfirmed at its bound", async () => {
  const ctx = await startFollowing("r13-item3-observer-noop");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  // An id-less queued edge for a no-op (target already equals the observed
  // low) posted by ANOTHER client; this tab only observes the lifecycle.
  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  ctx.receive(configureLifecycle(3, "effort-queued:low", ctx.instance.id, "2026-09-16T00:00:05.000Z"));
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending?.source, "the observer owns no client send").toBe("lifecycle");
  expect(pending?.queued, "the driver edge proves the queue; the chip keeps 排队中").toBe(true);
  expect(typeof pending?.leftQueueAt, "idle-at-receipt anchors the bound now").toBe("number");

  // No applied id, no newer projection ever arrives: before the bound the
  // chip stays, after it the explicit unconfirmed outcome ends the pending.
  await advanceEffortReconcile(14_999);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  expect(toast).not.toHaveBeenCalled();
  await advanceEffortReconcile(2);
  expect(hubStore.effortPendingOf(ctx.instance.id), "unconfirmed ends the observer pending").toBeNull();
  expect(toast.mock.calls[0]?.[0] ?? "").toContain("未确认");
});

it("r13 item 3: an observer pending with an unusable queued timestamp fails closed, then unconfirmed", async () => {
  const ctx = await startFollowing("r13-item3-observer-badstamp");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  // The id-less queued edge carries NO parseable observedAt.
  ctx.receive(configureLifecycle(3, "effort-queued:low", ctx.instance.id, "not-a-timestamp"));
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending?.queuedObservedAt, "the unusable stamp is discarded").toBeNull();
  expect(typeof pending?.leftQueueAt, "the bound still arms").toBe("number");

  // Even a fresh value-matching id-less read-back cannot settle it: with no
  // trustworthy lower bound the strict fallback fails closed.
  ctx.receive(
    effortEvent(4, "low", false, "remuda", "2026-09-16T00:00:20.000Z", {
      requested: { name: "low", ultracode: false },
    }),
  );
  expect(
    hubStore.effortPendingOf(ctx.instance.id),
    "no lower bound: stay pending through the read-back",
  ).not.toBeNull();
  expect(toast).not.toHaveBeenCalled();

  // The armed bound ends the request explicitly instead of waiting 30 min.
  await advanceEffortReconcile(15_001);
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(toast.mock.calls[0]?.[0] ?? "").toContain("未确认");
});

it("r12 item 5: an id-less accepted no-op surfaces an explicit unconfirmed outcome at the bound", async () => {
  const ctx = await startFollowing("r12-item5-unconfirmed", { state: "known", value: "idle" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // The id-less driver never projects anything newer: every list fold returns
  // the unchanged baseline, which its mapper would have suppressed anyway.
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  // Full fake timers from the request onward (Date keeps the pinned value).
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  const p = hubStore.setEffort(ctx.instance.id, { index: 1, name: "low", kind: "claude", ultracode: false });
  await advanceEffortReconcile(0);
  await p;
  expect(hubStore.effortPendingOf(ctx.instance.id)?.name, "the no-op request starts pending").toBe("low");

  // Before the bounded reconciliation: still a silent pending.
  await advanceEffortReconcile(14_999);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  expect(toast).not.toHaveBeenCalled();

  // At the bound the fresh read-back still shows no newer evidence: surface
  // unconfirmed instead of waiting the 30-minute max age.
  await advanceEffortReconcile(2);
  expect(hubStore.effortPendingOf(ctx.instance.id), "unconfirmed ends the pending").toBeNull();
  const message = toast.mock.calls[0]?.[0] ?? "";
  expect(message, "the outcome is explicitly labelled unconfirmed").toContain("未确认");
  // No refusal evidence and the no-op targeted the observed state: the chosen
  // selection is kept rather than rolled back.
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("low");
});

it("r13 item 4: a genuine live driver projection settles the id-less request; the bound never re-reads the Hub DB", async () => {
  const ctx = await startFollowing("r13-item4-live", { state: "known", value: "idle" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // The stored Hub record stays the unchanged baseline for the WHOLE test:
  // the bound must not treat a re-read of it as fresh evidence.
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  let listCalls = 0;
  vi.spyOn(api, "instanceList").mockImplementation(async () => {
    listCalls += 1;
    return { items: [baseline] } as never;
  });
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  const p = hubStore.setEffort(ctx.instance.id, { index: 1, name: "low", kind: "claude", ultracode: false });
  await advanceEffortReconcile(0);
  await p;
  const listsAfterPost = listCalls;
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();

  // The real driver projects the post-command read-back through the LIVE
  // follow journal (the production channel), strictly newer than the send and
  // matching the requested axis. It settles through the strict fallback.
  ctx.receive(
    effortEvent(3, "low", false, "remuda", "2026-09-16T00:00:05.000Z", {
      requested: { name: "low", ultracode: false },
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id), "the live projection settles").toBeNull();

  // The bound fires later into a no-op and performs NO database re-read; a
  // settled request is never reported unconfirmed.
  await advanceEffortReconcile(20_000);
  expect(listCalls, "the bound never re-reads the stored Hub record").toBe(listsAfterPost);
  expect(toast).not.toHaveBeenCalled();
});

it("r13 item 4: with no driver query op and no live projection the bound ends unconfirmed without a re-read", async () => {
  const ctx = await startFollowing("r13-item4-noread", { state: "known", value: "idle" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  let listCalls = 0;
  vi.spyOn(api, "instanceList").mockImplementation(async () => {
    listCalls += 1;
    return { items: [baseline] } as never;
  });
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  const p = hubStore.setEffort(ctx.instance.id, { index: 1, name: "low", kind: "claude", ultracode: false });
  await advanceEffortReconcile(0);
  await p;
  const listsAfterPost = listCalls;

  await advanceEffortReconcile(15_001);
  expect(hubStore.effortPendingOf(ctx.instance.id), "explicit unconfirmed").toBeNull();
  expect(listCalls, "a stored-state re-read is never presented as fresh evidence").toBe(listsAfterPost);
  expect(toast.mock.calls[0]?.[0] ?? "").toContain("未确认");
});

it("r12 item 5: a still-queued request gets one bounded extension and is never called unconfirmed", async () => {
  const ctx = await startFollowing("r12-item5-queued", { state: "known", value: "working" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  let listCalls = 0;
  vi.spyOn(api, "instanceList").mockImplementation(async () => {
    listCalls += 1;
    return { items: [baseline] } as never;
  });
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  const p = hubStore.setEffort(ctx.instance.id, { index: 1, name: "low", kind: "claude", ultracode: false });
  await advanceEffortReconcile(0);
  await p;
  const listsAfterPost = listCalls;
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);

  // First bound while the turn is still running: extend without reconciling.
  await advanceEffortReconcile(15_001);
  expect(listCalls, "a queued request is not reconciled at the first bound").toBe(listsAfterPost);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);

  // Second bound, still queued: stop the timer but leave the pending to the
  // long-turn backstop — never an unconfirmed toast.
  await advanceEffortReconcile(15_001);
  expect(hubStore.effortPendingOf(ctx.instance.id), "the long-turn pending stays").not.toBeNull();
  expect(toast).not.toHaveBeenCalled();
});

it("r12 item 5: a replaced request A resolving late never re-arms over B's reconciliation", async () => {
  const ctx = await startFollowing("r12-item5-late-arm", { state: "known", value: "idle" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));

  // A's POST hangs; B (requested next) accepts immediately and arms its
  // reconciliation. Then A's POST resolves late and tries to arm too.
  let resolveA: () => void = () => {};
  let configureCalls = 0;
  vi.spyOn(api, "instanceConfigure").mockImplementation(async () => {
    const n = configureCalls++;
    if (n === 0) {
      const result = await new Promise<never>((resolve) => {
        resolveA = () => resolve(undefined as never);
      });
      return result;
    }
    return {} as never;
  });

  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  const selection = { index: 1, name: "low", kind: "claude", ultracode: false } as const;
  const a = hubStore.setEffort(ctx.instance.id, selection);
  await Promise.resolve();
  await Promise.resolve();
  const b = hubStore.setEffort(ctx.instance.id, { ...selection, ultracode: false });
  await b;
  const bNonce = hubStore.effortPendingOf(ctx.instance.id)?.nonce;

  // A resolves after B has armed: A's arm must NOT replace B's slot/timer.
  await advanceEffortReconcile(14_000);
  resolveA();
  await a;
  await Promise.resolve();
  await Promise.resolve();
  expect(hubStore.effortPendingOf(ctx.instance.id)?.nonce).toBe(bNonce);

  // B's own bound fires at B's deadline despite A's late configure: the
  // pending ends unconfirmed rather than being reset for another 15 s.
  await advanceEffortReconcile(1_100);
  expect(hubStore.effortPendingOf(ctx.instance.id), "B's reconciliation, not A's late arm").toBeNull();
  expect(toast).toHaveBeenCalledTimes(1);
});

it("r17 item 1: an id-less unowned ACCEPTED projection never settles an id-capable B; B's own id verdict does", async () => {
  // Mapper-to-store regression for the unowned ACCEPTED case (not only Kept):
  // an older pre-floor same-word slash's "Set effort level to max" verdict is
  // folded as an id-less terminal-side projection BETWEEN B's queued edge and
  // B's verdict. B's driver proved id-capable on the queued edge, so the
  // legacy value fallback must not settle B from that projection; B clears
  // only on its own request-id verdict.
  const ctx = await startFollowing("r17-item1-unowned-accept");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });

  // Baseline high before the request.
  ctx.receive(effortEvent(2, "high", false, "slash", "2026-10-12T00:00:00.000Z"));
  const posted = hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false));
  const idB = posts[0]?.requestId!;
  await posted;
  expect(hubStore.effortPendingOf(ctx.instance.id)?.requestId).toBe(idB);

  // The id-bearing queued edge proves the driver is id-capable for B.
  ctx.receive(
    configureLifecycleFor(3, "effort-queued:max", idB, ctx.instance.id, "2026-10-12T00:00:05.000Z"),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)?.driverEchoesId).toBe(true);

  // The unowned older command's accepted max verdict projects id-less and in
  // the value/time window: the view moves but B's pending survives.
  ctx.receive(effortEvent(4, "max", false, "slash", "2026-10-12T00:00:10.000Z"));
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending, "B stays pending through the unowned acceptance").not.toBeNull();
  expect(pending?.requestId).toBe(idB);
  expect(pending?.levelSettled, "the id-less projection settles no axis of B").toBe(false);

  // B's own id-bearing accepted verdict settles.
  ctx.receive(
    effortEvent(5, "max", false, "remuda", "2026-10-12T00:00:12.000Z", {
      requested: { name: "max", ultracode: false },
      requestId: idB,
    }),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("r17 item 1 (failed floor): B's own verdict arriving id-less under provenance loss leaves B pending until its bounded unconfirmed outcome", async () => {
  // The carrier's floor stat failed: the mapper attributes even B's OWN
  // verdict to nothing and emits it id-less. The node still journals B's
  // id-bearing queued lifecycle, so the store knows the driver is id-capable;
  // B must not settle from the id-less read-back. No own-id verdict ever
  // arrives, so the bounded reconciliation ends B explicitly (the dismissal
  // toasts against a STILL-PRESENT request, never a prematurely cleared one).
  const ctx = await startFollowing("r17-item1-failed-floor", { state: "known", value: "idle" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const posts: ({ requestId?: string })[] = [];
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _perm, extras) => {
    posts.push(extras?.effort ?? {});
    return {} as never;
  });
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "high", ultracode: false, source: "slash", observedAt: "2026-10-11T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  ctx.receive(effortEvent(2, "high", false, "slash", "2026-10-11T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-10-12T00:00:00.000Z") });
  const p = hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false));
  await advanceEffortReconcile(0);
  const idB = posts[0]?.requestId!;
  await p;

  // The independently-journaled queued edge carries B's id despite the floor
  // failure.
  ctx.receive(
    configureLifecycleFor(3, "effort-queued:max", idB, ctx.instance.id, "2026-10-12T00:00:05.000Z"),
  );
  expect(hubStore.effortPendingOf(ctx.instance.id)?.driverEchoesId).toBe(true);

  // B's own verdict, restored under provenance loss: id-less, matching, and
  // strictly inside the legacy window. The view updates; B stays pending.
  ctx.receive(effortEvent(4, "max", false, "slash", "2026-10-12T00:00:10.000Z"));
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending, "the id-less own verdict does not settle B").not.toBeNull();
  expect(pending?.requestId).toBe(idB);

  // No id verdict ever arrives: the full post-queue bound ends B unconfirmed.
  await advanceEffortReconcile(14_999);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  expect(toast).not.toHaveBeenCalled();
  await advanceEffortReconcile(2);
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(toast.mock.calls[0]?.[0] ?? "").toContain("未确认");
});

it("r17 item 2: an idle snapshot followed by a busy snapshot in ONE batch cancels the idle-armed 15 s timer", async () => {
  // One follow/catch-up batch carries, in journal order, an idle snapshot and
  // THEN a working snapshot. The per-event fold used to stamp leftQueueAt and
  // arm the 15 s bound on the intermediate idle; the later busy evidence never
  // cancelled it, so the bound toasted "unconfirmed" while the driver was
  // blocked on the new turn. The busy snapshot must withdraw the stamp and
  // cancel the timer; the full window re-anchors only at the next idle.
  const ctx = await startFollowing("r17-item2-batch-idle-busy", { state: "known", value: "waiting-interaction" });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const baseline: Instance = {
    ...ctx.instance,
    effortEffective: { name: "low", ultracode: false, source: "slash", observedAt: "2026-09-15T23:59:57.000Z" },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [baseline] } as never);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  const entityActivity = (value: "working" | "waiting-interaction" | "idle", observedAt: string): Observation =>
    ({
      eventId: `evt_act_entity_r17_${value}_${observedAt}`,
      instanceId: ctx.instance.id,
      journalId: "x",
      seq: "0",
      kind: "lifecycle",
      source: { channel: "runtime" },
      observedAt,
      payload: {
        type: "entity",
        entityType: "instance",
        entityId: ctx.instance.id,
        entity: {
          id: ctx.instance.id,
          activity: { state: "known", value },
          activityEvidenceEventIds: [],
          nativeRef: ctx.instance.nativeRef,
          updatedAt: observedAt,
        },
      },
    }) as unknown as Observation;

  ctx.receive(effortEvent(2, "low", false, "slash", "2026-09-15T23:59:57.000Z"));
  vi.useFakeTimers({ now: new Date("2026-09-16T00:00:00.000Z") });
  // The configure posts while an interaction is on screen: queued, no anchor.
  const p = hubStore.setEffort(ctx.instance.id, { index: 1, name: "low", kind: "claude", ultracode: false });
  await advanceEffortReconcile(0);
  await p;
  expect(hubStore.effortPendingOf(ctx.instance.id)?.leftQueueAt).toBeNull();

  // ONE batch: an idle snapshot stamps the exit and arms the 15 s bound, then
  // a working snapshot in the SAME batch puts the request back in the queue.
  ctx.receiveBatch([
    entityActivity("idle", "2026-09-16T00:00:05.000Z"),
    entityActivity("working", "2026-09-16T00:00:06.000Z"),
  ]);
  const requeued = hubStore.effortPendingOf(ctx.instance.id);
  expect(requeued?.queued, "the later busy evidence re-queues the request").toBe(true);
  expect(requeued?.leftQueueAt, "the idle stamp is withdrawn").toBeNull();

  // The idle-armed timer must be gone: well past 15 s while still blocked, no
  // unconfirmed toast and the pending survives.
  await advanceEffortReconcile(15_001);
  expect(hubStore.effortPendingOf(ctx.instance.id), "never unconfirmed while blocked").not.toBeNull();
  expect(toast).not.toHaveBeenCalled();

  // Authoritative idle in a LATER batch re-anchors a fresh full window.
  ctx.receiveBatch([entityActivity("idle", "2026-09-16T00:00:40.000Z")]);
  const exited = hubStore.effortPendingOf(ctx.instance.id);
  expect(exited?.queued).toBe(false);
  expect(typeof exited?.leftQueueAt).toBe("number");
  await advanceEffortReconcile(14_999);
  expect(hubStore.effortPendingOf(ctx.instance.id)).not.toBeNull();
  expect(toast).not.toHaveBeenCalled();
  await advanceEffortReconcile(2);
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(toast.mock.calls[0]?.[0] ?? "").toContain("未确认");
});
