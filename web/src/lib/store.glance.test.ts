import { afterEach, expect, it, vi } from "vitest";
import type { OwnerGlance } from "../types/generated";
import type { Id } from "../types/wire";

const id = (value: string) => value as Id;

const GLANCE: OwnerGlance = {
  generatedAt: "2026-10-30T10:09:00.000Z",
  timezoneOffsetMinutes: -new Date().getTimezoneOffset(),
  counts: { needYou: 2, doneUnseen: 1, working: 3, doneToday: 4 },
  waiting: [
    { instanceId: "ins_wait", kind: "approval", headline: "approval: run it?", title: "waiting" },
  ],
  working: [
    { instanceId: "ins_work", kind: "working", headline: "pushing the fix", title: "working" },
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

it("keeps the last glance when a refresh fails", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "ownerGlance").mockResolvedValueOnce(GLANCE);
  await hubStore.refreshGlance();
  vi.spyOn(api, "ownerGlance").mockRejectedValueOnce(new Error("offline"));
  await hubStore.refreshGlance();
  expect(hubStore.getSnapshot().glance).toEqual(GLANCE);
});
