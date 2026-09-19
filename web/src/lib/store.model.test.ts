/**
 * §9.1 model-sync store behavior:
 * - the launch snapshot carries the discovered catalog (gateway models) and
 *   current effective model;
 * - a terminal-side `/model` (model observation, no pending push-down) moves
 *   the picker selection WITHOUT posting instance.configure (no ping-pong);
 * - a read-back that settles OUR pending push-down clears pending and reports
 *   the resolved id (an alias can resolve to a concrete gateway id);
 * - a rejected push-down (model-degraded) clears pending, reverts, toasts.
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
      projectionEpoch: "epoch_model_store",
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
    id: `ins_model_store_${idSuffix}`,
    journalId: `obj_model_store_${idSuffix}`,
    kind: "claude",
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

function modelEvent(
  seq: number,
  id: string,
  source: string,
  catalog?: { models: string[]; source: "gateway-discovery" | "settings" | "builtin" },
): Observation {
  return {
    eventId: `evt_mdl_${seq}_${id}`.replace(/[^a-zA-Z0-9_]/g, "_"),
    instanceId: "x",
    journalId: "x",
    seq: String(seq),
    kind: "model",
    observedAt: `2026-09-16T00:0${seq}:00Z`,
    source: { channel: "transcript" },
    payload: {
      // Launch and Remuda-switch edges name the requested id; a hand-typed
      // terminal /model (source "slash") does not.
      requested: source === "slash" ? undefined : id,
      effective: { id, source, observedAt: `2026-09-16T00:0${seq}:00Z` },
      raw: id,
      ...(catalog
        ? { catalog: { ...catalog, observedAt: `2026-09-16T00:0${seq}:00Z` } }
        : {}),
    },
  } as unknown as Observation;
}

/** A catalog-only refresh edge (scoped cache landed post-promotion): no
 *  requested id, effective model unchanged. */
function catalogRefreshEvent(
  seq: number,
  id: string,
  catalog: { models: string[]; source: "gateway-discovery" | "settings" | "builtin" },
): Observation {
  return {
    eventId: `evt_mdlcat_${seq}`,
    instanceId: "x",
    journalId: "x",
    seq: String(seq),
    kind: "model",
    observedAt: `2026-09-16T00:0${seq}:00Z`,
    source: { channel: "transcript" },
    payload: {
      effective: { id, source: "launch", observedAt: `2026-09-16T00:0${seq}:00Z` },
      catalog: { ...catalog, observedAt: `2026-09-16T00:0${seq}:00Z` },
    },
  } as unknown as Observation;
}

function configureLifecycle(seq: number, status: string, instanceId: string): Observation {
  return {
    eventId: `evt_mdlcfg_${seq}`,
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

it("the launch snapshot records the discovered catalog and current model", async () => {
  const ctx = await startFollowing("catalog");
  ctx.receive(
    modelEvent(2, "ark/seed-evolving[1m]", "launch", {
      models: ["ark/seed-evolving[1m]", "model_hub/es1_orange_o50", "model_hub/es1_orange_o48"],
      source: "gateway-discovery",
    }),
  );
  const catalog = hubStore.modelCatalogOf(ctx.instance.id);
  expect(catalog?.source).toBe("gateway-discovery");
  expect(catalog?.models).toContain("model_hub/es1_orange_o50");
  // The picker list includes discovered ids and the current model.
  expect(hubStore.modelListOf(ctx.instance.id)).toContain("model_hub/es1_orange_o50");
  expect(hubStore.modelOf(ctx.instance.id, "claude")).toBe("ark/seed-evolving[1m]");
});

it("a terminal-side /model moves the picker without posting configure", async () => {
  const ctx = await startFollowing("terminal");
  const configure = vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  ctx.receive(modelEvent(2, "model_hub/base", "launch"));
  // Human types /model in the terminal and picks a gateway id.
  ctx.receive(modelEvent(3, "model_hub/es1_orange_o50", "slash"));
  expect(hubStore.modelEffectiveOf(ctx.instance.id)?.id).toBe("model_hub/es1_orange_o50");
  expect(hubStore.modelOf(ctx.instance.id, "claude")).toBe("model_hub/es1_orange_o50");
  expect(configure).not.toHaveBeenCalled();
});

it("our pending push-down clears on read-back and reports the resolved id", async () => {
  const ctx = await startFollowing("pending");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  ctx.receive(modelEvent(2, "model_hub/es1_orange_o48[1m]", "launch"));
  await hubStore.setModel(ctx.instance.id, "sonnet");
  expect(hubStore.modelPendingOf(ctx.instance.id)?.id).toBe("sonnet");
  // sonnet resolves through the pinned env to the concrete gateway id.
  ctx.receive(modelEvent(3, "model_hub/es1_orange_o48[1m]", "remuda"));
  expect(hubStore.modelPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.modelEffectiveOf(ctx.instance.id)?.id).toBe("model_hub/es1_orange_o48[1m]");
});

it("a not-found lifecycle clears pending, reverts, and toasts", async () => {
  const ctx = await startFollowing("notfound");
  const configure = vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});
  ctx.receive(modelEvent(2, "model_hub/base", "launch"));
  await hubStore.setModel(ctx.instance.id, "bogus-xyz-123");
  ctx.receive(configureLifecycle(3, "model-degraded:bogus-xyz-123:not-found", ctx.instance.id));
  expect(hubStore.modelPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.modelOf(ctx.instance.id, "claude")).toBe("model_hub/base");
  expect(toast).toHaveBeenCalled();
  expect(configure).toHaveBeenCalledTimes(1);
});

it("a rejected instance.configure clears modelPending, reverts and toasts the reason", async () => {
  const ctx = await startFollowing("rejected");
  vi.spyOn(api, "instanceConfigure").mockRejectedValue(new Error("HOST_OFFLINE node down"));
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});
  ctx.receive(modelEvent(2, "model_hub/base", "launch"));
  // Must not throw: the store is the failure mouth (SessionPage does not void
  // the promise, so a rethrow would be an unhandled rejection).
  await expect(hubStore.setModel(ctx.instance.id, "sonnet")).resolves.toBeUndefined();
  expect(hubStore.modelPendingOf(ctx.instance.id)).toBeNull();
  // Optimistic selection reverted to the last observed effective id.
  expect(hubStore.modelOf(ctx.instance.id, "claude")).toBe("model_hub/base");
  expect(toast).toHaveBeenCalledTimes(1);
  expect(toast.mock.calls[0][0]).toContain("HOST_OFFLINE node down");
});

it("a catalog-only refresh edge never settles an in-flight switch", async () => {
  const ctx = await startFollowing("catalogrefresh");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  ctx.receive(
    modelEvent(2, "model_hub/base", "launch", {
      models: ["model_hub/base"],
      source: "gateway-discovery",
    }),
  );
  await hubStore.setModel(ctx.instance.id, "sonnet");
  expect(hubStore.modelPendingOf(ctx.instance.id)?.id).toBe("sonnet");
  // The scoped cache lands a beat later: catalog updates, pending survives.
  ctx.receive(
    catalogRefreshEvent(3, "model_hub/base", {
      models: ["model_hub/base", "sonnet", "model_hub/o50"],
      source: "gateway-discovery",
    }),
  );
  expect(hubStore.modelPendingOf(ctx.instance.id)?.id).toBe("sonnet");
  expect(hubStore.modelListOf(ctx.instance.id)).toContain("model_hub/o50");
  // The optimistic selection was not moved back to the stale effective id.
  expect(hubStore.modelOf(ctx.instance.id, "claude")).toBe("sonnet");
  // The real verdict then settles normally.
  ctx.receive(modelEvent(4, "model_hub/o48", "remuda"));
  expect(hubStore.modelPendingOf(ctx.instance.id)).toBeNull();
});
