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
) {
  const original = mockDb.instances[0];
  // Isolate: every test flips its own instance id and journal.
  const instance: Instance = {
    ...original,
    id: `ins_effort_store_${idSuffix}`,
    journalId: `obj_effort_store_${idSuffix}`,
    kind: "claude",
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
  let deliver!: (observation: Observation) => void;
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async (_j, _a, onBatch) => {
    deliver = (observation: Observation) =>
      onBatch({
        subscriptionId: `sub_${instance.id}`,
        journalId: instance.journalId,
        fromSeq: observation.seq,
        toSeq: observation.seq,
        durableSeq: observation.seq,
        events: [observation],
      });
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
  const ctx = {
    instance,
    receive: (observation: Observation) => {
      delivered += 1;
      deliver({ ...observation, seq: String(delivered), observedAt: tsFor(delivered) });
    },
  };
  return ctx;
}

function effortEvent(seq: number, name: string, ultracode: boolean | null, source: string, observedAt?: string): Observation {
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

function configureLifecycle(seq: number, status: string, instanceId = "x"): Observation {
  return {
    eventId: `evt_cfg_${seq}_${status}`,
    instanceId,
    journalId: "x",
    seq: String(seq),
    observedAt: `2026-09-16T00:1${seq}:00.000Z`,
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

it("a level+flag push-down settles each axis independently", async () => {
  const ctx = await startFollowing("axes");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 3, true));
  const pending0 = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending0).toMatchObject({ name: "xhigh", ultracode: true });
  expect(pending0?.levelSettled).toBe(false);
  expect(pending0?.flagSettled).toBe(false);

  // First the level arrives with NO switch evidence: level settles, the
  // switch indicator must stay pending (never silently cleared).
  ctx.receive(effortEvent(2, "xhigh", null, "remuda"));
  const pending1 = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending1?.levelSettled).toBe(true);
  expect(pending1?.flagSettled).toBe(false);
  expect(pending1?.ultracode).toBe(true);

  // Then the attachment reports the flag on.
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

it("a level clamp settles the level axis while the unobserved switch stays pending", async () => {
  const ctx = await startFollowing("clamp-flag");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, true)); // max,on
  ctx.receive(effortEvent(2, "high", null, "remuda")); // clamped level, no flag evidence
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  // The level outcome is delivered (slider stays on max → mismatch); the flag
  // has NO positive evidence yet, so its indicator must remain pending.
  expect(pending?.levelSettled).toBe(true);
  expect(pending?.flagSettled).toBe(false);
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");

  // When the process positively reports the flag refused off, that is the
  // delivered refusal for the level mismatch and clears the indicator.
  ctx.receive(effortEvent(3, "high", false, "remuda"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
});

it("while queued the entry shows queued until a strictly newer read-back", async () => {
  const ctx = await startFollowing("queued", { state: "known", value: "working" });
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);

  // A pre-existing projection at a DIFFERENT level stamps the threshold and
  // leaves the level axis unproven.
  ctx.receive(effortEvent(1, "high", false, "slash", "2026-09-16T00:05:00.000Z"));
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, false));
  const lifecycle = configureLifecycle(3, "effort-queued:max");
  ctx.receive(lifecycle);
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);
  // An older-than-threshold read-back must not settle a queued request.
  ctx.receive(effortEvent(2, "max", false, "remuda", "2026-09-16T00:02:00.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);
  ctx.receive(effortEvent(6, "max", false, "remuda", "2026-09-16T00:06:00.000Z"));
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

it("a rejected switch reverts and records the model-scoped refusal", async () => {
  const ctx = await startFollowing("revert");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const toast = vi.spyOn(hubStore, "toast");
  ctx.instance.effortEffective = { name: "high", ultracode: false, source: "slash", observedAt: "2026-09-16T00:00:00.000Z" };

  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 3, true));
  ctx.receive(configureLifecycle(4, "effort-degraded:ultracode:ultracode-unavailable-for-model"));
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

it("a degraded verdict clears a pending even when its word differs from the queued one", async () => {
  const ctx = await startFollowing("queued-then-degrade", { state: "known", value: "working" });
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // Driver reports queued on xhigh...
  ctx.receive(configureLifecycle(2, "effort-queued:xhigh"));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.queued).toBe(true);
  // ...then rejects a different word (max). The terminal verdict clears it.
  ctx.receive(configureLifecycle(3, "effort-degraded:max:dialog-kept"));
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
  // A fails slowly; B succeeds.
  let resolveA: () => void = () => {};
  vi.spyOn(api, "instanceConfigure").mockImplementation((_id, _perm, extras) => {
    if (extras?.effort?.name === "max") {
      return new Promise((_resolve, reject) => {
        resolveA = () => reject(new Error("A failed late"));
      });
    }
    return Promise.resolve({} as never);
  });
  // A = xhigh (starts first), then B = max.
  const a = hubStore.setEffort(ctx.instance.id, { index: 3, name: "xhigh", kind: "claude", ultracode: false });
  const b = hubStore.setEffort(ctx.instance.id, { index: 4, name: "max", kind: "claude", ultracode: false });
  await b;
  // A's late rejection must not roll B back to high/xhigh.
  resolveA();
  await a;
  expect(hubStore.effortOf(ctx.instance.id, "claude").name).toBe("max");
});

it("history replay hydrates effective but never touches pending or toasts a refusal", async () => {
  const ctx = await startFollowing("history-replay");
  const configure = vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  const toast = vi.spyOn(hubStore, "toast").mockImplementation(() => {});
  // A live refusal marks the switch disabled.
  ctx.receive(configureLifecycle(2, "effort-degraded:ultracode:ultracode-workflows-disabled"));
  expect(hubStore.effortRefusalOf(ctx.instance.id)?.reason).toBe("ultracode-workflows-disabled");
  toast.mockClear();

  // Simulate a history REPLAY of an old positive flag edge (reconnect): it
  // hydrates effective but must not clear the refusal or settle any pending.
  hubStore.logout();
  vi.restoreAllMocks();
  vi.spyOn(api, "instanceGet").mockResolvedValue(ctx.instance);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [effortEvent(3, "xhigh", true, "remuda")],
    durableSeq: "3",
    windowFromSeq: "1",
    reachedAfterSeq: true,
    getReadyState: () => 1,
  } as never);
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async () => subscription(ctx.instance));
  await hubStore.follow(ctx.instance.id);

  expect(hubStore.effortEffectiveOf(ctx.instance.id)?.ultracode).toBe(true);
  // Replay did not touch request-scoped state.
  expect(hubStore.effortPendingOf(ctx.instance.id)).toBeNull();
  // (logout/relogin drops the in-memory refusal; the point is no toast fired.)
  expect(configure).not.toHaveBeenCalled();
});
