import { afterEach, expect, it, vi } from "vitest";
import type { OwnerGlance } from "../types/generated";
import type { Id } from "../types/wire";

const id = (value: string) => value as Id;

const GLANCE: OwnerGlance = {
  generatedAt: "2026-10-30T10:09:00.000Z",
  timezoneOffsetMinutes: -new Date().getTimezoneOffset(),
  counts: { needYou: 2, doneUnseen: 1, working: 3, doneToday: 4 },
  doneUnseenIds: ["ins_done"],
  waiting: [
    { instanceId: "ins_wait", kind: "approval", headline: "approval: run it?", title: "waiting" },
  ],
  working: [
    { instanceId: "ins_work", kind: "working", headline: "pushing the fix", phrase: "pushing the fix", title: "working" },
    { instanceId: "ins_bare", kind: "working", headline: "bare title", title: "bare title" },
  ],
  more: { waiting: 0, working: 0 },
};

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

/**
 * home-glance-d060: the store owns the ONE owner-glance read the home shares.
 * These pin the client half of home.md §5.3 — the tz offset the client must
 * send, the in-flight coalescing, and the row index the ledger headline reads.
 */
afterEach(() => {
  vi.restoreAllMocks();
});

it("sends the local-day offset and stores the projection", async () => {
  const { api, hubStore } = await fresh();
  const ownerGlance = vi.spyOn(api, "ownerGlance").mockResolvedValue(GLANCE);
  vi.spyOn(api, "eventsRead").mockRejectedValue(new Error("no journal reads on the home poll"));

  await hubStore.refreshGlance();

  expect(ownerGlance).toHaveBeenCalledTimes(1);
  expect(ownerGlance).toHaveBeenCalledWith(-new Date().getTimezoneOffset());
  expect(hubStore.getSnapshot().glance).toEqual(GLANCE);
  expect(hubStore.glanceHeadlineOf(id("ins_work"))).toBe("pushing the fix");
  // r6 item 2: the row sentence's phrase — absent when the Hub had none, even
  // though the headline then falls back to the title.
  expect(hubStore.glancePhraseOf(id("ins_work"))).toBe("pushing the fix");
  expect(hubStore.glanceHeadlineOf(id("ins_bare"))).toBe("bare title");
  expect(hubStore.glancePhraseOf(id("ins_bare"))).toBeUndefined();
  expect(hubStore.glanceRowOf(id("ins_wait"))?.kind).toBe("approval");
  // The glance read is the only request this path makes: no per-row journal.
  expect(api.eventsRead).not.toHaveBeenCalled();
});

it("coalesces overlapping reads into one request", async () => {
  const { api, hubStore } = await fresh();
  let release!: (value: OwnerGlance) => void;
  const ownerGlance = vi.spyOn(api, "ownerGlance").mockImplementation(
    () => new Promise<OwnerGlance>((resolve) => {
      release = resolve;
    }),
  );

  const first = hubStore.refreshGlance();
  const second = hubStore.refreshGlance();
  expect(second).toBe(first);
  expect(ownerGlance).toHaveBeenCalledTimes(1);
  release(GLANCE);
  await Promise.all([first, second]);
  expect(hubStore.getSnapshot().glance).toEqual(GLANCE);
});

it("keeps the last glance but marks it failed, and clears the mark on recovery", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "ownerGlance").mockResolvedValueOnce(GLANCE);
  await hubStore.refreshGlance();
  expect(hubStore.getSnapshot().glanceAt).toBeTypeOf("number");
  expect(hubStore.getSnapshot().glanceFailed).toBe(false);

  vi.spyOn(api, "ownerGlance").mockRejectedValueOnce(new Error("offline"));
  await hubStore.refreshGlance();
  const failed = hubStore.getSnapshot();
  expect(failed.glance).toEqual(GLANCE);
  // Frozen numbers are NOT fresh: the row greys and names the last good read.
  expect(failed.glanceFailed).toBe(true);
  expect(failed.glanceAt).toBeTypeOf("number");

  vi.spyOn(api, "ownerGlance").mockResolvedValueOnce(GLANCE);
  await hubStore.refreshGlance();
  expect(hubStore.getSnapshot().glanceFailed).toBe(false);
});

it("forgets the previous identity's glance when the identity changes", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "ownerGlance").mockResolvedValue(GLANCE);
  await hubStore.refreshGlance();
  expect(hubStore.getSnapshot().glance).toEqual(GLANCE);

  // The one retirement helper every external identity change passes through
  // (logout, peer tab, 401) — and the re-login emit beside it in `login`.
  hubStore.applyAuthIdentityChangeForTest();

  const after = hubStore.getSnapshot();
  expect(after.glance).toBeNull();
  expect(after.glanceAt).toBeNull();
  expect(after.glanceFailed).toBe(false);
  expect(hubStore.glanceRowOf(id("ins_work"))).toBeUndefined();
});

it("a re-login under a different device clears the glance (the login path)", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null } as never);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as never);
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as never);
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);
  vi.spyOn(api, "ownerGlance").mockResolvedValue(GLANCE);
  vi.spyOn(api, "login").mockResolvedValue({ deviceId: "dev_owner_a", token: "t", name: "a" } as never);
  await hubStore.login("bootstrap", "code-a", "a");
  await hubStore.refreshGlance();
  expect(hubStore.getSnapshot().glance).toEqual(GLANCE);

  vi.spyOn(api, "login").mockResolvedValue({ deviceId: "dev_owner_b", token: "t2", name: "b" } as never);
  await hubStore.login("bootstrap", "code-b", "b");
  expect(hubStore.getSnapshot().glance).toBeNull();
});

it("a read in flight across a retirement cannot repaint the retired identity", async () => {
  const { api, hubStore } = await fresh();
  let release!: (value: OwnerGlance) => void;
  vi.spyOn(api, "ownerGlance").mockImplementation(
    () =>
      new Promise<OwnerGlance>((resolve) => {
        release = resolve;
      }),
  );
  const pending = hubStore.refreshGlance();
  // A peer tab logs out mid-read: extEpoch moves, bootGen does not.
  hubStore.applyAuthIdentityChangeForTest();
  release(GLANCE);
  await pending;
  expect(hubStore.getSnapshot().glance).toBeNull();
  expect(hubStore.getSnapshot().glanceFailed).toBe(false);
});
