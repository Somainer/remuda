import { afterEach, expect, it, vi } from "vitest";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

/**
 * c-perffu r12: the hand-written guard inside refresh()'s unchanged-poll
 * skip-emit — `!this.state.interactionsHydrated` — exists for the offline
 * reload: bootstrap's interaction-list fetch failed, so a later SUCCESSFUL
 * poll must still emit (even with identical empty lists) to settle the
 * arrival baseline. Without that emit interactionsHydrated would stay false
 * and the first later pending question would be taken as the baseline (badge
 * only, no toast). The second identical poll must then stay silent (r8
 * quiet-poll identity skip).
 */
async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

afterEach(() => {
  vi.restoreAllMocks();
});

it("a successful poll after a failed bootstrap interaction-list settles hydration once, then polls stay silent", async () => {
  const { api, hubStore } = await fresh();

  // Bootstrap reaches the fetch fan-out with a device session on disk (the
  // offline-reload branch), but interactionList rejects: Promise.all fails,
  // bootstrap emits ready/authed WITHOUT interactionsHydrated.
  vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({
    items: [],
    nextCursor: null,
  } as Awaited<ReturnType<Api["hostList"]>>);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as Awaited<
    ReturnType<Api["deviceList"]>
  >);
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as Awaited<
    ReturnType<Api["passkeyList"]>
  >);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [],
    nextCursor: null,
  } as Awaited<ReturnType<Api["instanceList"]>>);
  // Only the BOOTSTRAP read fails; every later poll is a successful empty page.
  vi.spyOn(api, "interactionList")
    .mockRejectedValueOnce(new Error("offline"))
    .mockResolvedValue([] as Awaited<ReturnType<Api["interactionList"]>>);

  await hubStore.bootstrap();

  const before = hubStore.getSnapshot();
  expect(before.authed).toBe(true);
  expect(
    before.interactionsHydrated,
    "a failed bootstrap interaction-list leaves the baseline unsettled",
  ).toBe(false);

  let emits = 0;
  const unsubscribe = hubStore.subscribe(() => {
    emits += 1;
  });

  // First SUCCESSFUL poll after the failed bootstrap: the empty page is a
  // valid success and must emit despite unchanged identities, flipping the
  // hydration flag.
  await hubStore.refresh();
  expect(
    hubStore.getSnapshot().interactionsHydrated,
    "the first successful poll settles the baseline",
  ).toBe(true);
  expect(emits).toBeGreaterThanOrEqual(1);

  // Second identical poll: no row identity changes and the baseline is
  // settled, so the quiet-poll skip suppresses the entire render wave.
  const beforeSecond = emits;
  await hubStore.refresh();
  expect(emits, "an unchanged settled poll emits nothing").toBe(beforeSecond);

  unsubscribe();
});
