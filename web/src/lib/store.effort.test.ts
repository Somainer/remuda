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
    floorSeq: "1",
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

async function startFollowing(idSuffix: string) {
  const original = mockDb.instances[0];
  const instance: Instance = {
    ...original,
    id: `ins_effort_store_${idSuffix}`,
    journalId: `obj_effort_store_${idSuffix}`,
    kind: "claude",
    effortName: null,
    effortIndex: null,
    effortUltracode: null,
  };
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "1",
    floorSeq: "1",
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
