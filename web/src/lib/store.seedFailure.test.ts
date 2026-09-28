import { afterEach, expect, it, vi } from "vitest";

const INSTANCE = "ins_seed_fail";
const JOURNAL = "obj_seed_fail_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (err: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

/**
 * c-reconnfu item 2: the bootstrap REST calls succeeded (machine starts
 * live), then the FIRST journal-seed read on mount fails. The session must
 * not sit "live" with no reconnect timer: the mount is recovering while the
 * seed is pending, failure propagates offline, and the reconnect loop retries
 * the exact mount.
 */
afterEach(() => {
  vi.restoreAllMocks();
});

it("a journal-seed failure after bootstrap goes recovering -> offline -> reconnect retry", async () => {
  const { api, hubStore } = await fresh();
  const seed = deferred<Awaited<ReturnType<Api["eventsRead"]>>>();

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
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "0", durableSeq: "0", lifecycle: "running" } as never],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const read = vi.spyOn(api, "eventsRead").mockReturnValue(seed.promise);
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });

  await hubStore.bootstrap();
  expect(hubStore.connectionState).toBe("live");

  // Mount starts: before the seed settles the active follow is recovering.
  const mount = hubStore.follow(INSTANCE);
  await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
  expect(hubStore.connectionState).toBe("recovering");

  // Seed failure: honest offline, and the follow promise rejects to the page.
  seed.reject(new Error("JOURNAL_SEED_FAILED"));
  await expect(mount).rejects.toThrow("JOURNAL_SEED_FAILED");
  expect(hubStore.connectionState).toBe("offline");

  // The bounded reconnect loop retries the mount (a second seed read).
  await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2), { timeout: 5_000 });

  hubStore.logout();
});
