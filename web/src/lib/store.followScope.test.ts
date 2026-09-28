import { afterEach, expect, it, vi } from "vitest";

const INSTANCE_A = "ins_follow_scope_a";
const JOURNAL_A = "obj_follow_scope_journal_a";
const INSTANCE_B = "ins_follow_scope_b";
const JOURNAL_B = "obj_follow_scope_journal_b";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;
type Hooks = NonNullable<Parameters<Api["eventsSubscribe"]>[4]>;

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

/**
 * c-reconnfu item 1: navigating A → B leaves A's follow socket open (its
 * events still hydrate the session list), but the global connection state is
 * owned by the CURRENTLY BOUND session only. A's frames must not certify B's
 * dead link live, and A's close must not take B offline.
 */
afterEach(() => {
  vi.restoreAllMocks();
});

async function mountTwoSessions() {
  const { api, hubStore } = await fresh();
  const hooks = new Map<string, Hooks>();
  const readyState = new Map<string, number>([
    [JOURNAL_A, 1],
    [JOURNAL_B, 1],
  ]);

  vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null } as Awaited<
    ReturnType<Api["hostList"]>
  >);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["deviceList"]>>);
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as Awaited<
    ReturnType<Api["passkeyList"]>
  >);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      { id: INSTANCE_A, journalId: JOURNAL_A, revision: "0", durableSeq: "0", lifecycle: "running" },
      { id: INSTANCE_B, journalId: JOURNAL_B, revision: "0", durableSeq: "0", lifecycle: "running" },
    ] as never,
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (journalId: string, _afterSeq: unknown, _onBatch: unknown, _onGap: unknown, h?: Hooks) => {
      if (h) hooks.set(journalId, h);
      return {
        subscriptionId: `sub_${journalId}`,
        journalId,
        durableSeq: "0",
        windowFromSeq: null,
        reachedAfterSeq: true,
        getReadyState: () => readyState.get(journalId) ?? 3,
        snapshot: {
          projectionVersion: "v1",
          projectionEpoch: `epoch_${journalId}`,
          asOfSeq: "0",
          instance: {} as never,
          runs: [],
          commands: [],
          pendingInteractions: [],
          nodes: [],
          history: { earliestRetainedSeq: "0", complete: true },
        },
      };
    }) as Api["eventsSubscribe"],
  );

  await hubStore.bootstrap();
  await hubStore.follow(INSTANCE_A);
  await hubStore.follow(INSTANCE_B);
  return { api, hubStore, hooks, readyState };
}

it("an inactive session's streaming frames do not mark the bound session's dead socket live", async () => {
  const { hubStore, hooks } = await mountTwoSessions();
  const hooksA = hooks.get(JOURNAL_A)!;
  const hooksB = hooks.get(JOURNAL_B)!;

  // B is bound and live; A keeps streaming in the background.
  hooksB.onFrame?.();
  expect(hubStore.connectionState).toBe("live");
  hooksA.onFrame?.();
  hooksA.onFrame?.();
  expect(hubStore.connectionState).toBe("live");

  // B's socket dies. The machine must go offline and STAY offline even though
  // A's still-open socket keeps framing.
  hooksB.onClose?.();
  expect(hubStore.connectionState).toBe("offline");
  hooksA.onFrame?.();
  hooksA.onFrame?.();
  hooksA.onFrame?.();
  expect(hubStore.connectionState).toBe("offline");

  hubStore.logout();
});

it("an inactive session's socket close does not take the bound live session offline", async () => {
  const { hubStore, hooks, readyState } = await mountTwoSessions();
  const hooksA = hooks.get(JOURNAL_A)!;
  const hooksB = hooks.get(JOURNAL_B)!;

  // B is bound and freshly framed; A (backgrounded) closes.
  hooksB.onFrame?.();
  readyState.set(JOURNAL_A, 3);
  hooksA.onClose?.();
  expect(hubStore.connectionState).toBe("live");

  hubStore.logout();
});
