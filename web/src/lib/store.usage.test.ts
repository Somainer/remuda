/**
 * context-usage-1 store behavior: the Hub-computed usage rollup arrives on
 * polled instance rows, but `mergeInstanceSnapshots` deliberately keeps the
 * local instance while its follow-bumped `durableSeq` is ahead. Hydration
 * must therefore fold the rollup into a separate map so fresh TPM windows
 * are never hidden by the stale retained instance object.
 */
import { afterEach, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import type { UsageRollup } from "../features/session/contextUsage";
import { api } from "./api";
import { mockDb } from "./mock";
import { hubStore } from "./store";

type History = Awaited<ReturnType<typeof api.eventsRead>>;

const rollup1: UsageRollup = {
  contextUsedTokens: 34_290,
  contextWindowTokens: 200_000,
  contextPct: 17,
  sessionInputTokens: 4_794,
  sessionOutputTokens: 260,
  cacheReadTokens: 29_496,
  cacheCreationTokens: 0,
  turns: 1,
  tpmIn60s: 4_794,
  tpmOut60s: 260,
  tpmIn5m: 959,
  tpmOut5m: 52,
  lastTurnAt: "2026-09-17T00:00:00.000Z",
};

const rollup2: UsageRollup = { ...rollup1, turns: 2, contextPct: 18, sessionOutputTokens: 445 };

afterEach(() => {
  hubStore.logout();
  vi.restoreAllMocks();
});

it("hydrates the polled rollup even when the local instance wins the seq merge", async () => {
  const base: Instance = mockDb.instances[0];
  // Local instance is five journal events ahead of the list row.
  const instance: Instance = { ...base, id: "ins_usage_store", journalId: "obj_usage_store" };
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "5",
    windowFromSeq: "1",
    reachedAfterSeq: true,
        getReadyState: () => 1,
  } as unknown as History);
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async () => ({
    subscriptionId: "sub_usage_store",
    journalId: instance.journalId,
    durableSeq: "5",
    windowFromSeq: "1",
    reachedAfterSeq: true,
        getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_usage",
      asOfSeq: "5",
      instance: { ...instance, durableSeq: "5" },
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "1", complete: true },
    },
  }));
  await hubStore.follow(instance.id);
  expect(hubStore.usageRollupOf(instance.id)).toBeNull();

  // A polled row LOWER durableSeq (merge keeps local) but WITH a rollup.
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const listSpy = vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ ...instance, durableSeq: "1", usageRollup: rollup1 }],
    nextCursor: null,
  } as Awaited<ReturnType<typeof api.instanceList>>);
  await hubStore.refresh();
  expect(hubStore.usageRollupOf(instance.id)).toEqual(rollup1);

  // A later poll with the same/fewer turns must not regress a fresher map.
  listSpy.mockResolvedValue({
    items: [{ ...instance, durableSeq: "1", usageRollup: rollup1 }],
    nextCursor: null,
  } as Awaited<ReturnType<typeof api.instanceList>>);
  await hubStore.refresh();
  expect(hubStore.usageRollupOf(instance.id)).toEqual(rollup1);

  // A newer turn count wins.
  listSpy.mockResolvedValue({
    items: [{ ...instance, durableSeq: "1", usageRollup: rollup2 }],
    nextCursor: null,
  } as Awaited<ReturnType<typeof api.instanceList>>);
  await hubStore.refresh();
  expect(hubStore.usageRollupOf(instance.id)).toEqual(rollup2);
});
