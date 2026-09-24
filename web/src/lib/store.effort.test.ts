/**
 * §9.1 effort-sync-2 store behavior:
 * - a terminal-side `/effort` (effort observation, no pending push-down) moves
 *   the slider to the observed stop WITHOUT posting instance.configure (no
 *   ping-pong);
 * - a read-back that settles OUR pending push-down clears pending but leaves
 *   the slider where the user put it;
 * - a rejected push-down (effort-degraded lifecycle) clears pending and
 *   reverts the slider to the last observed level.
 */
import { afterEach, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import type { Observation } from "../types/observation";
import { api } from "./api";
import { mockDb } from "./mock";
import { hubStore } from "./store";

type History = Awaited<ReturnType<typeof api.eventsRead>>;

function subscription(instance: Instance) {
  return {
    subscriptionId: `sub_${instance.id}`,
    journalId: instance.journalId,
    durableSeq: "1",
    windowFromSeq: "1",
    reachedAfterSeq: true,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_effort_store",
      asOfSeq: "1",
      instance,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "1", complete: true },
    },
  };
}

async function startFollowing(
  idSuffix: string,
  activity: Instance["activity"] = { state: "known", value: "idle" },
) {
  const original = mockDb.instances[0];
  const instance: Instance = {
    ...original,
    id: `ins_effort_store_${idSuffix}`,
    journalId: `obj_effort_store_${idSuffix}`,
    kind: "claude",
    effortName: null,
    effortIndex: null,
    effortUltracode: null,
    activity,
  };
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "1",
    windowFromSeq: "1",
    reachedAfterSeq: true,
  } as unknown as History);
  let deliver!: Parameters<typeof api.eventsSubscribe>[2];
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async (_j, _a, onBatch) => {
    deliver = onBatch;
    return subscription(instance);
  });
  await hubStore.follow(instance.id);
  return {
    instance,
    receive: (observation: Observation) =>
      deliver({
        subscriptionId: `sub_${instance.id}`,
        journalId: instance.journalId,
        fromSeq: observation.seq,
        toSeq: observation.seq,
        durableSeq: observation.seq,
        events: [observation],
      }),
  };
}

function effortEvent(seq: number, name: string, ultracode: boolean | null, source: string): Observation {
  return {
    eventId: `evt_eff_${seq}_${name}`,
    instanceId: "x",
    journalId: "x",
    seq: String(seq),
    kind: "effort",
    observedAt: `2026-09-16T00:0${seq}:00Z`,
    source: { channel: "transcript" },
    payload: {
      effective: { name, ultracode, source, observedAt: `2026-09-16T00:0${seq}:00Z` },
      raw: name,
    },
  } as unknown as Observation;
}

function configureLifecycle(seq: number, status: string, instanceId: string): Observation {
  return {
    eventId: `evt_cfg_${seq}`,
    instanceId,
    journalId: "x",
    seq: String(seq),
    kind: "lifecycle",
    observedAt: `2026-09-16T00:1${seq}:00Z`,
    source: { channel: "runtime" },
    payload: {
      type: "native",
      nativeName: "instance.configure",
      nativeId: { state: "not-applicable" },
      status: { state: "known", value: status },
      relatedIds: {},
    },
  } as unknown as Observation;
}

afterEach(() => {
  hubStore.logout();
  vi.restoreAllMocks();
});

it("a terminal-side switch moves the slider without posting configure (no ping-pong)", async () => {
  const ctx = await startFollowing("terminal");
  const configure = vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  // First observed level (e.g. baseline assistant record).
  ctx.receive(effortEvent(2, "high", null, "launch"));
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("high");
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("high");

  // Human types /effort xhigh in the terminal.
  ctx.receive(effortEvent(3, "xhigh", false, "slash"));
  const selected = hubStore.effortOf(ctx.instance.id, "claude");
  expect(selected.name).toBe("xhigh");
  expect(selected.ultracode).toBeFalsy();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.source).toBe("slash");
  // The store moved the slider purely from the observation — no configure.
  expect(configure).not.toHaveBeenCalled();
});

it("an ultracode read-back from the terminal parks the slider on the ultracode stop", async () => {
  const ctx = await startFollowing("ultra");
  ctx.receive(effortEvent(2, "xhigh", true, "slash"));
  const selected = hubStore.effortOf(ctx.instance.id, "claude");
  expect(selected.name).toBe("xhigh");
  expect(selected.ultracode).toBe(true);
  expect(selected.index).toBe(3);
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("xhigh");
});

it("our pending push-down clears when the effective read-back lands", async () => {
  const ctx = await startFollowing("pending");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // User moves the slider to xhigh.
  await hubStore.setEffort(ctx.instance.id, {
    index: 3,
    name: "xhigh",
    kind: "claude",
    ultracode: false,
  });
  expect(hubStore.effortPendingOf(ctx.instance.id)?.word).toBe("xhigh");
  ctx.receive(effortEvent(2, "xhigh", false, "remuda"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("xhigh");
});

it("a queued lifecycle shows the pending entry; a degraded one reverts the slider", async () => {
  const ctx = await startFollowing("revert");
  const configure = vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});

  // Baseline high.
  ctx.receive(effortEvent(2, "high", null, "launch"));
  // While working, the user picks max; the driver journals queued.
  await hubStore.setEffort(ctx.instance.id, {
    index: 4,
    name: "max",
    kind: "claude",
    ultracode: false,
  });
  ctx.receive(configureLifecycle(3, "effort-queued:max", ctx.instance.id));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);

  // Claude refuses (Esc on the dialog): the chip reverts to high and the
  // pending entry clears with a toast.
  ctx.receive(configureLifecycle(4, "effort-degraded:max:dialog-kept", ctx.instance.id));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("high");
  expect(toast).toHaveBeenCalled();
  // configure was called once (the user click); the revert did not call it.
  expect(configure).toHaveBeenCalledTimes(1);
});

it("a poll-projected read-back settles a pending push-down without a live event (c-effortflake)", async () => {
  // The live follow frame was gapped/coalesced under load, but the driver
  // still projected the read-back onto the durable Hub record before acking.
  // The configure-ack refresh folds that projection and must settle pending;
  // otherwise the chip stays on 切换中 forever even though the Hub knows the
  // new level.
  const ctx = await startFollowing("poll-settle");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const projected: Instance = {
    ...ctx.instance,
    effortEffective: {
      name: "max",
      ultracode: false,
      source: "remuda",
      observedAt: "2026-09-21T00:00:01.000Z",
    },
  };
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [projected] } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);

  await hubStore.setEffort(ctx.instance.id, {
    index: 4,
    name: "max",
    kind: "claude",
    ultracode: false,
  });

  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
});

it("a queued push-down survives a poll returning the unchanged level, then settles on the newer one", async () => {
  const working = { state: "known", value: "working" } as const;
  const ctx = await startFollowing("poll-queued", working);
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  // Baseline read-back the session already had before the push-down.
  ctx.receive(effortEvent(2, "xhigh", false, "remuda"));
  const baselineAt = "2026-09-16T00:02:00Z";

  const list = vi.spyOn(api, "instanceList");
  list.mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        activity: working,
        effortEffective: { name: "xhigh", ultracode: false, source: "remuda", observedAt: baselineAt },
      },
    ],
  } as never);

  await hubStore.setEffort(ctx.instance.id, {
    index: 4,
    name: "max",
    kind: "claude",
    ultracode: false,
  });
  // The configure-ack poll still shows the OLD level (the turn has not ended):
  // the queued push-down must remain, not be cleared by an equal observedAt.
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);

  // The turn ends and the new level projects; a later poll settles it.
  list.mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        effortEffective: { name: "xhigh", ultracode: false, source: "remuda", observedAt: "2026-09-16T00:03:00Z" },
      },
    ],
  } as never);
  await hubStore.refresh();
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("a poll-settled clamp still leaves the slider on the requested stop when its live frame lands (c-effortflake)", async () => {
  // The live frame for our max push-down is lost; the configure-ack poll
  // settles pending from the durable record — which shows the NATIVE CLAMP
  // (effective xhigh). When that same edge later arrives on the live socket
  // it must still read as OUR push-down: the slider stays on max so the
  // 请求 max → 实际 xhigh mismatch survives, instead of folding the slider to
  // the observed (terminal-switch) level.
  const ctx = await startFollowing("poll-clamp");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  const clampedAt = "2026-09-21T00:00:05.000Z";
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        effortEffective: { name: "xhigh", ultracode: null, source: "remuda", observedAt: clampedAt },
      },
    ],
  } as never);

  await hubStore.setEffort(ctx.instance.id, {
    index: 4,
    name: "max",
    kind: "claude",
    ultracode: false,
  });
  // The poll settled pending; effective is the clamped level but the slider
  // the user moved is still on max.
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("xhigh");
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");

  // The same clamped edge arrives on the live socket (gapped frame caught up).
  ctx.receive({
    ...effortEvent(2, "xhigh", null, "remuda"),
    observedAt: clampedAt,
    payload: {
      effective: { name: "xhigh", ultracode: null, source: "remuda", observedAt: clampedAt },
      raw: "xhigh",
    },
  } as unknown as Observation);
  // Still our push-down: the slider is NOT folded to the clamped level.
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");
});

it("an older poll projection cannot overwrite a newer live effective or move the slider", async () => {
  // Monotonic guard: a stale/slower durable record (e.g. a poll answering out
  // of order after the live socket already advanced) must never roll the
  // effective level back, and the projection path never writes the slider.
  const ctx = await startFollowing("monotonic");
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);

  // A terminal-side switch arrives live: effective max, slider folds to max.
  const newerAt = "2026-09-21T01:00:09.000Z";
  ctx.receive({
    ...effortEvent(2, "max", false, "slash"),
    observedAt: newerAt,
    payload: {
      effective: { name: "max", ultracode: false, source: "slash", observedAt: newerAt },
      raw: "max",
    },
  } as unknown as Observation);
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");

  // A poll then returns a durable projection carrying an OLDER effective level.
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      {
        ...ctx.instance,
        effortEffective: {
          name: "xhigh",
          ultracode: null,
          source: "remuda",
          observedAt: "2026-09-21T01:00:01.000Z",
        },
      },
    ],
  } as never);
  await hubStore.refresh();

  // Neither the effective value nor the slider may regress to the older level.
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.name).toBe("max");
  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.observedAt).toBe(newerAt);
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");
});
