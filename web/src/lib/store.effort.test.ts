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
      // Keep an explicit envelope observedAt (effort/lifecycle payloads share
      // one host clock in production); only synthesize one when absent.
      const observedAt = observation.observedAt ?? tsFor(delivered);
      deliver({ ...observation, seq: String(delivered), observedAt });
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

it("a decoupled queued `ultracode on` edge names the flag request and keeps the tier", async () => {
  // The driver's decoupled command word is "ultracode on" (a space); the
  // queued/applied lifecycle must parse as the FLAG axis, not an unknown tier.
  const ctx = await startFollowing("queued-flag-word", { state: "known", value: "working" });
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  await hubStore.setEffort(ctx.instance.id, { index: 3, name: "xhigh", kind: "claude", ultracode: false });
  await hubStore.setUltracode(ctx.instance.id, true);
  ctx.receive(configureLifecycle(2, "effort-queued:ultracode on", "x", "2026-10-08T12:02:01.000Z"));
  expect(hubStore.effortPendingOf(ctx.instance.id)).toMatchObject({
    name: "xhigh",
    ultracode: true,
    queued: true,
  });
  ctx.receive(configureLifecycle(3, "effort-applied:ultracode on", "x", "2026-10-08T12:02:05.000Z"));
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

it("a null threshold is not 'newer': a terminal projection for another level never settles the request", async () => {
  const ctx = await startFollowing("null-threshold");
  vi.spyOn(api, "instanceConfigure").mockResolvedValue({} as never);
  // No effective state exists yet (threshold null). Request max+ultracode on.
  await hubStore.setEffort(ctx.instance.id, effortAt("claude", 4, true));
  expect(hubStore.effortPendingOf(ctx.instance.id)?.thresholdObservedAt).toBeNull();

  // A terminal-side /effort observation (NOT a remuda configure read-back)
  // names a DIFFERENT level with the flag off — someone/something else's
  // state, not our request. With a null threshold it must settle NOTHING.
  ctx.receive(effortEvent(2, "high", false, "slash"));
  const pending = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending).not.toBeNull();
  expect(pending?.levelSettled).toBe(false);
  expect(pending?.flagSettled).toBe(false);

  // A terminal observation that DOES name our requested level settles the
  // level axis but still not the flag (it reports off, we asked on).
  ctx.receive(effortEvent(3, "max", false, "slash"));
  const pending2 = hubStore.effortPendingOf(ctx.instance.id);
  expect(pending2?.levelSettled).toBe(true);
  expect(pending2?.flagSettled).toBe(false);

  // Our own remuda read-back then settles the flag (delivered off = refusal
  // of the flag) and clears the indicator.
  ctx.receive(effortEvent(4, "max", false, "remuda"));
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
