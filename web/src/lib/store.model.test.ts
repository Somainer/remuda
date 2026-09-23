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

async function startFollowing(idSuffix: string, launchModel?: string) {
  const original = mockDb.instances[0];
  const base: Instance = {
    ...original,
    id: `ins_model_store_${idSuffix}`,
    journalId: `obj_model_store_${idSuffix}`,
    kind: "claude",
  };
  // `undefined` launchModel means a journal-discovered row with no durable
  // model; an explicit value sets it; null explicitly removes the field.
  const instance: Instance =
    launchModel === undefined
      ? (() => {
            const { model: _omit, ...rest } = base;
            return rest as Instance;
          })()
      : launchModel === null
        ? (() => {
            const { model: _omit, ...rest } = base;
            return { ...rest, model: null } as Instance;
          })()
        : { ...base, model: launchModel };
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
  requestedOverride?: string,
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
      // terminal /model (source "slash") does not. A Remuda configure that
      // resolved to a different concrete id stamps the configured alias.
      requested: requestedOverride ?? (source === "slash" ? undefined : id),
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

it("the requested half tracks the launch spec, then a deliberate /model switch", async () => {
  // Regression (2026-09-23 acceptance rounds 4/5):
  //  - a live read-back folded into the picker used to be the "requested"
  //    half, so requested == running and the pair vanished;
  //  - then the half was pinned to the launch spec forever, so a user's own
  //    `/model` switch showed as a divergence. A deliberate switch IS a
  //    request: the observed slash id becomes the requested half.
  const launchModel = "passthrough/ark/seed-evolving";
  const ctx = await startFollowing("requested", launchModel);
  // Launch read-back: the gateway strips its routing prefix.
  ctx.receive(modelEvent(2, "ark/seed-evolving", "launch"));
  expect(hubStore.modelOf(ctx.instance.id, "claude")).toBe("ark/seed-evolving");
  expect(hubStore.modelEffectiveOf(ctx.instance.id)?.id).toBe("ark/seed-evolving");
  // The pair's requested half keeps the durable launch spec.
  expect(hubStore.modelRequestedOf(ctx.instance.id)).toBe(launchModel);

  // A later terminal /model is a deliberate request to the new id: requested
  // moves to C and equals running, so the pair collapses to one id.
  ctx.receive(modelEvent(3, "model_hub/es1_orange_o50", "slash"));
  expect(hubStore.modelOf(ctx.instance.id, "claude")).toBe("model_hub/es1_orange_o50");
  expect(hubStore.modelEffectiveOf(ctx.instance.id)?.id).toBe("model_hub/es1_orange_o50");
  expect(hubStore.modelRequestedOf(ctx.instance.id)).toBe("model_hub/es1_orange_o50");
});

it("a journal-discovered instance with no durable model invents no request", async () => {
  // Node-discovered instance: the Hub row has no model; only an effective id
  // exists. No fabricated "opus" — requested is null, displays show one id.
  const ctx = await startFollowing("discovered", undefined);
  expect(ctx.instance.model).toBeUndefined();
  ctx.receive(modelEvent(2, "sonnet", "unknown"));
  expect(hubStore.modelEffectiveOf(ctx.instance.id)?.id).toBe("sonnet");
  expect(hubStore.modelRequestedOf(ctx.instance.id)).toBeNull();
});

it("a settled remuda configure requests the configured id even when it resolved", async () => {
  const ctx = await startFollowing("configure", "ark/seed-evolving");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // The launch read-back lands first (the live journal always carries it
  // before any switch edge; delivering the switch edge alone would gap the
  // client's watermark).
  ctx.receive(modelEvent(2, "ark/seed-evolving", "launch"));
  await hubStore.setModel(ctx.instance.id, "e2e/fast");
  // The verdict edge stamps requested=e2e/fast, effective=e2e/plain (alias
  // resolved to a different concrete id); requested stays the configured id.
  ctx.receive(modelEvent(3, "e2e/plain", "remuda", undefined, "e2e/fast"));
  expect(hubStore.modelPendingOf(ctx.instance.id)).toBeNull();
  expect(hubStore.modelRequestedOf(ctx.instance.id)).toBe("e2e/fast");
  expect(hubStore.modelEffectiveOf(ctx.instance.id)?.id).toBe("e2e/plain");
});

it("an unknown read-back does not change the requested half", async () => {
  const ctx = await startFollowing("unknown-edge", "model_hub/A");
  ctx.receive(modelEvent(2, "model_hub/B", "launch"));
  expect(hubStore.modelRequestedOf(ctx.instance.id)).toBe("model_hub/A");
  // Unattributed assistant-model change: running moves, the request is still
  // the launch spec (the Hub roster clears its own stale pair; the web just
  // keeps the durable request until a deliberate switch supersedes it).
  ctx.receive(modelEvent(3, "model_hub/C", "unknown"));
  expect(hubStore.modelEffectiveOf(ctx.instance.id)?.id).toBe("model_hub/C");
  expect(hubStore.modelRequestedOf(ctx.instance.id)).toBe("model_hub/A");
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
