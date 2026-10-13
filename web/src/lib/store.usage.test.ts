/**
 * context-usage-1 store behavior: the Hub-computed usage rollup arrives on
 * polled instance rows, but `mergeInstanceSnapshots` deliberately keeps the
 * local instance while its follow-bumped `durableSeq` is ahead. Hydration
 * must therefore fold the rollup into a separate map so fresh TPM windows
 * are never hidden by the stale retained instance object.
 */
import { afterEach, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import {
  USAGE_MISSING_CHANNELS,
  contextHeadline,
  type UsageRollup,
} from "../features/session/contextUsage";
import { api } from "./api";
import { mockDb } from "./mock";
import { hubStore } from "./store";

type History = Awaited<ReturnType<typeof api.eventsRead>>;

const rollup1: UsageRollup = {
  contextUsedTokens: 34_290,
  contextWindowTokens: 200_000,
  contextPct: 17,
  contextPctApproximate: false,
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

/** The Hub's authoritative Unavailable rollup (legacy/rolled-back shape). */
const rollupUnavailable: UsageRollup = {
  unavailable: true,
  contextUsedTokens: null,
  contextWindowTokens: null,
  contextPct: null,
  contextPctApproximate: false,
  sessionInputTokens: null,
  sessionOutputTokens: null,
  cacheReadTokens: null,
  cacheCreationTokens: null,
  turns: 0,
  tpmIn60s: null,
  tpmOut60s: null,
  tpmIn5m: null,
  tpmOut5m: null,
  lastTurnAt: null,
};

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

it("r17 item 4: an authoritative Unavailable rollup replaces cached positive usage through a refresh, then a recovered positive rollup replaces it back", async () => {
  const base: Instance = mockDb.instances[0];
  const instance: Instance = { ...base, id: "ins_usage_unavail", journalId: "obj_usage_unavail" };
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "5",
    windowFromSeq: "1",
    reachedAfterSeq: true,
        getReadyState: () => 1,
  } as unknown as History);
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async () => ({
    subscriptionId: "sub_usage_unavail",
    journalId: instance.journalId,
    durableSeq: "5",
    windowFromSeq: "1",
    reachedAfterSeq: true,
        getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_usage_unavail",
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
  vi.spyOn(api, "interactionList").mockResolvedValue([]);

  const listWith = (usageRollup: UsageRollup | null) =>
    vi.spyOn(api, "instanceList").mockResolvedValue({
      items: [{ ...instance, durableSeq: "1", usageRollup }],
      nextCursor: null,
    } as Awaited<ReturnType<typeof api.instanceList>>);

  // Positive usage cached first; the chip headline shows real counters.
  listWith(rollup1);
  await hubStore.refresh();
  expect(hubStore.usageRollupOf(instance.id)).toEqual(rollup1);
  expect(contextHeadline(rollup1).text).toBe("34.3k/200.0k (17%)");
  expect(contextHeadline(rollup1).missing).toBeNull();

  // A later store refresh carries the authoritative Unavailable state with
  // turns 0. The turn-count gate used to keep the stale positive rollup, so
  // the chip never showed Unknown.
  listWith(rollupUnavailable);
  await hubStore.refresh();
  const folded = hubStore.usageRollupOf(instance.id);
  expect(folded?.unavailable).toBe(true);
  expect(folded?.turns).toBe(0);
  // The chip/headline render Unknown, never the cached 17% or a guess.
  const headline = contextHeadline(folded as UsageRollup);
  expect(headline.text).toBe("—/— (—%)");
  expect(headline.missing).toBe(USAGE_MISSING_CHANNELS.unavailable);

  // A repeated Unavailable response with identical content is not a churn.
  const before = hubStore.usageRollupOf(instance.id);
  await hubStore.refresh();
  expect(hubStore.usageRollupOf(instance.id)).toBe(before);

  // The reverse transition (migration/recovery, usage projects again) also
  // wins regardless of turn comparisons.
  listWith(rollup2);
  await hubStore.refresh();
  expect(hubStore.usageRollupOf(instance.id)).toEqual(rollup2);
  expect(contextHeadline(rollup2).text).toBe("34.3k/200.0k (18%)");
});

it("r18 item 4: an older deferred refresh cannot overwrite a newer availability state in either direction", async () => {
  const base: Instance = mockDb.instances[0];
  const instance: Instance = {
    ...base,
    id: "ins_usage_order",
    journalId: "obj_usage_order",
  };
  vi.spyOn(api, "interactionList").mockResolvedValue([]);

  type Deferred<T> = { promise: Promise<T>; resolve: (value: T) => void };
  const defer = <T>(): Deferred<T> => {
    let resolve!: (value: T) => void;
    const promise = new Promise<T>((r) => {
      resolve = r;
    });
    return { promise, resolve };
  };

  const runDirection = async (
    older: UsageRollup,
    newer: UsageRollup,
    olderName: string,
    newerName: string,
  ) => {
    const olderList = defer<Awaited<ReturnType<typeof api.instanceList>>>();
    const newerList = defer<Awaited<ReturnType<typeof api.instanceList>>>();
    const olderInteractions = defer<Awaited<ReturnType<typeof api.interactionList>>>();
    const newerInteractions = defer<Awaited<ReturnType<typeof api.interactionList>>>();

    const listSpy = vi
      .spyOn(api, "instanceList")
      .mockImplementationOnce(() => olderList.promise)
      .mockImplementationOnce(() => newerList.promise);
    const interactionsSpy = vi
      .spyOn(api, "interactionList")
      .mockImplementationOnce(() => olderInteractions.promise)
      .mockImplementationOnce(() => newerInteractions.promise);

    // Older read starts first and stays pending; newer read starts and lands.
    const olderRefresh = hubStore.refresh();
    const newerRefresh = hubStore.refresh();
    newerInteractions.resolve([]);
    newerList.resolve({
      items: [{ ...instance, durableSeq: "1", usageRollup: newer }],
      nextCursor: null,
    } as Awaited<ReturnType<typeof api.instanceList>>);
    await newerRefresh;
    expect(hubStore.usageRollupOf(instance.id)).toEqual(newer);

    // The older read then resolves out of order and must be dropped wholesale.
    olderInteractions.resolve([]);
    olderList.resolve({
      items: [{ ...instance, durableSeq: "1", usageRollup: older }],
      nextCursor: null,
    } as Awaited<ReturnType<typeof api.instanceList>>);
    await olderRefresh;
    expect(
      hubStore.usageRollupOf(instance.id),
      `${olderName} (the stale read) never overwrites the newer ${newerName}`,
    ).toEqual(newer);
    listSpy.mockRestore();
    interactionsSpy.mockRestore();
  };

  // Direction 1: a stale Unavailable page arrives after a newer positive one.
  await runDirection(rollupUnavailable, rollup2, "Unavailable", "positive");
  // Direction 2: a stale positive page arrives after a newer Unavailable one.
  await runDirection(rollup2, rollupUnavailable, "positive", "Unavailable");
});

it("r2 item 4: setUsageRollupForTest is inert without the e2e seam marker", () => {
  const instance: Instance = mockDb.instances[0];
  // Production default: no marker on window.
  (window as unknown as { __remudaE2E?: boolean }).__remudaE2E = undefined;
  const before = hubStore.usageRollupOf(instance.id);
  hubStore.setUsageRollupForTest(instance.id, rollup1);
  expect(hubStore.usageRollupOf(instance.id)).toBe(before);

  // With the explicit e2e marker the seam injects the rollup.
  window.__remudaE2E = true;
  hubStore.setUsageRollupForTest(instance.id, rollup1);
  expect(hubStore.usageRollupOf(instance.id)).toEqual(rollup1);
  delete window.__remudaE2E;
});
