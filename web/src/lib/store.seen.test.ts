import { afterEach, expect, it, vi } from "vitest";
import { api, mapInstance, type SeenView } from "./api";
import type { components } from "./api.generated";
import { hubStore } from "./store";
import { mockDb } from "./mock";
import type { Instance } from "../types/instance";
import type { Id, U64 } from "../types/wire";

afterEach(() => { hubStore.logout(); vi.restoreAllMocks(); });

const ID = "ins_seen" as Id;

function row(overrides: Partial<Instance>): Instance {
  return { ...mockDb.instances[0], id: ID, ...overrides };
}

/** Boot the store on one instance; returns the seen handler the store wired. */
async function boot(first: Instance) {
  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null });
  const list = vi.spyOn(api, "instanceList").mockResolvedValue({ items: [first], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  let onSeen: ((view: SeenView) => void) | undefined;
  vi.spyOn(api, "settlementSubscribe").mockImplementation((_onSettlement, seen) => {
    onSeen = seen;
    return () => undefined;
  });
  await hubStore.bootstrap();
  expect(onSeen).toBeDefined();
  return { list, seen: (view: SeenView) => onSeen!(view) };
}

const current = () => hubStore.getSnapshot().instances.find((instance) => instance.id === ID)!;

it("maps the Hub's seen fields onto the instance and reads an older Hub as seen", () => {
  const base: components["schemas"]["InstanceRecord"] = {
    instanceId: ID, hostId: "hst_a", kind: "claude", driver: "claude-sdk", lifecycle: "running",
    activity: "idle", connectivity: "connected", journalId: "obj_a", durableSeq: "7",
    lineageId: ID, generation: 1,
  };
  const mapped = mapInstance({ ...base, unseen: true, seenSeq: "3", turnEndSeq: "7" });
  expect(mapped.unseen).toBe(true);
  expect(mapped.seenSeq).toBe("3");
  expect(mapped.turnEndSeq).toBe("7");
  const older = mapInstance(base);
  expect(older.unseen).toBe(false);
  expect(older.seenSeq).toBeUndefined();
  expect(older.turnEndSeq).toBeUndefined();
});

it("moves seen only forward, so a poll that predates a seen report cannot bring the dot back", async () => {
  const unseen = row({ durableSeq: "5" as U64, unseen: true, turnEndSeq: "5" as U64, seenSeq: undefined });
  const { list, seen } = await boot(unseen);
  expect(current().unseen).toBe(true);

  // Another owner device saw it (the instance.seen broadcast).
  seen({ instanceId: ID, unseen: false, seenSeq: "5", turnEndSeq: "5" });
  expect(current()).toMatchObject({ unseen: false, seenSeq: "5" });

  // A poll answered from before that report still says unseen: ignored.
  await hubStore.refresh();
  expect(current()).toMatchObject({ unseen: false, seenSeq: "5" });

  // A lower seen seq changes nothing (same row identity).
  const before = current();
  seen({ instanceId: ID, unseen: true, seenSeq: "2", turnEndSeq: "5" });
  expect(current()).toBe(before);

  // A view for an instance the store does not hold is ignored.
  seen({ instanceId: "ins_other", unseen: true, turnEndSeq: "1" });
  expect(hubStore.getSnapshot().instances).toHaveLength(1);

  // A newer turn end from the poll is unseen again.
  list.mockResolvedValue({
    items: [row({ durableSeq: "8" as U64, unseen: true, turnEndSeq: "8" as U64, seenSeq: "5" as U64 })],
    nextCursor: null,
  });
  await hubStore.refresh();
  expect(current()).toMatchObject({ unseen: true, turnEndSeq: "8", seenSeq: "5" });
});

it("markSeen is optimistic on this device and rolls back what the Hub refuses", async () => {
  await boot(row({ durableSeq: "6" as U64, unseen: true, turnEndSeq: "6" as U64 }));
  let reject!: (error: Error) => void;
  const post = vi.spyOn(api, "instanceSeen").mockReturnValueOnce(
    new Promise((_, fail) => { reject = fail; }),
  );
  const refused = hubStore.markSeen(ID, { visible: true });
  // The dot is gone at once, before the Hub answers.
  expect(current()).toMatchObject({ unseen: false, seenSeq: "6" });
  expect(post).toHaveBeenLastCalledWith(ID, "6", true);
  reject(new Error("403"));
  await expect(refused).rejects.toThrow("403");
  // Refused: the optimistic seen is rolled back, so nothing local disagrees with the Hub.
  expect(current().unseen).toBe(true);
  expect(current().seenSeq).toBeUndefined();

  post.mockResolvedValueOnce({ instanceId: ID, unseen: false, seenSeq: "6", turnEndSeq: "6", seenAt: "t" });
  const view = await hubStore.markSeen(ID);
  expect(post).toHaveBeenLastCalledWith(ID, "6", undefined);
  expect(view?.unseen).toBe(false);
  expect(current()).toMatchObject({ unseen: false, seenSeq: "6" });

  // A seq past the row's journal is sent as-is (the Hub clamps) but clamped locally.
  post.mockResolvedValueOnce({ instanceId: ID, unseen: false, seenSeq: "6", turnEndSeq: "6" });
  await hubStore.markSeen(ID, { durableSeq: "40" as U64, visible: false });
  expect(post).toHaveBeenLastCalledWith(ID, "40", false);
  expect(current().seenSeq).toBe("6");

  // Unknown instance with no seq: nothing to report.
  await expect(hubStore.markSeen("ins_missing" as Id)).resolves.toBeNull();
});

it("sends a release with keepalive so it survives the page unloading", async () => {
  const fetchSpy = vi.spyOn(globalThis, "fetch").mockImplementation(async () =>
    new Response(JSON.stringify({ instanceId: ID, unseen: false }), { status: 200 }));
  await api.instanceSeen(ID, "3" as U64, false);
  const [url, init] = fetchSpy.mock.calls.at(-1)!;
  expect(String(url)).toContain(`/v1/instances/${ID}/seen`);
  expect(init).toMatchObject({ method: "POST", keepalive: true });
  expect(JSON.parse(String(init?.body))).toEqual({ durableSeq: "3", visible: false });
  await api.instanceSeen(ID, "3" as U64, true);
  expect(fetchSpy.mock.calls.at(-1)?.[1]).not.toHaveProperty("keepalive");
  await api.instanceSeen(ID, "3" as U64);
  expect(JSON.parse(String(fetchSpy.mock.calls.at(-1)?.[1]?.body))).toEqual({ durableSeq: "3" });
});

it("a visible report answered unseen re-reports once at the answer's turn end, never more", async () => {
  await boot(row({ durableSeq: "6" as U64, unseen: false, turnEndSeq: "4" as U64, seenSeq: "4" as U64 }));
  // The turn ended (seq 9) just before this report's lease: the answer still
  // reads unseen. The page is on screen, so it covers that turn end once.
  const post = vi.spyOn(api, "instanceSeen")
    .mockResolvedValueOnce({ instanceId: ID, unseen: true, seenSeq: "6", turnEndSeq: "9" })
    .mockResolvedValueOnce({ instanceId: ID, unseen: true, seenSeq: "9", turnEndSeq: "10" })
    .mockResolvedValue({ instanceId: ID, unseen: true, seenSeq: "9", turnEndSeq: "11" });
  const view = await hubStore.markSeen(ID, { visible: true });
  expect(post.mock.calls).toEqual([
    [ID, "6", true],
    [ID, "9", true],
  ]);
  // Bounded: an answer that is still unseen is left to the next renewal.
  expect(view).toMatchObject({ seenSeq: "9", turnEndSeq: "10" });
  expect(current()).toMatchObject({ seenSeq: "9", turnEndSeq: "10", unseen: true });

  // Reports that are not about the screen never re-report.
  post.mockClear();
  post.mockResolvedValueOnce({ instanceId: ID, unseen: true, seenSeq: "9", turnEndSeq: "12" });
  await hubStore.markSeen(ID);
  expect(post).toHaveBeenCalledTimes(1);

  // A failed re-report keeps the first answer and does not throw.
  post.mockClear();
  post.mockResolvedValueOnce({ instanceId: ID, unseen: true, seenSeq: "9", turnEndSeq: "13" })
    .mockRejectedValueOnce(new Error("503"));
  await expect(hubStore.markSeen(ID, { visible: true })).resolves.toMatchObject({ turnEndSeq: "13" });
  expect(post).toHaveBeenCalledTimes(2);
});

/**
 * [r4 item 3] ◉ and ⑤ read ONE glance set. A mark leaves the list and the
 * count together at the click, and a glance read that was already in flight
 * (it began before the seen write committed) cannot paint the row back; the
 * first read that began after the commit is the Hub's truth again.
 */
it("strips a marked row from the glance's ◉ list and count together, across an in-flight read", async () => {
  await boot(row({ durableSeq: "6" as U64, unseen: true, turnEndSeq: "6" as U64 }));
  const glance = (ids: string[]) => ({
    generatedAt: "2026-10-30T10:09:00.000Z",
    timezoneOffsetMinutes: 0,
    counts: { needYou: 0, doneUnseen: ids.length, working: 0, doneToday: 0 },
    doneUnseenIds: ids,
    waiting: [],
    working: [],
    more: { waiting: 0, working: 0 },
  });
  const read = vi.spyOn(api, "ownerGlance").mockResolvedValueOnce(glance([ID, "ins_other"]));
  await hubStore.refreshGlance();
  expect(hubStore.getSnapshot().glance?.counts.doneUnseen).toBe(2);

  // A glance read is in flight when the owner marks the row; it answers with
  // the pre-commit state.
  let answer!: (value: ReturnType<typeof glance>) => void;
  read.mockReturnValueOnce(new Promise((resolve) => { answer = resolve; }));
  const inFlight = hubStore.refreshGlance();
  let commit!: (view: SeenView) => void;
  vi.spyOn(api, "instanceSeen").mockReturnValueOnce(new Promise((resolve) => { commit = resolve; }));
  const marked = hubStore.markSeen(ID);
  // At the click: both the list and the count lose it.
  expect(hubStore.getSnapshot().glance).toMatchObject({
    doneUnseenIds: ["ins_other"],
    counts: { doneUnseen: 1 },
  });
  commit({ instanceId: ID, unseen: false, seenSeq: "6", turnEndSeq: "6" });
  await marked;
  answer(glance([ID, "ins_other"]));
  await inFlight;
  expect(hubStore.getSnapshot().glance).toMatchObject({
    doneUnseenIds: ["ins_other"],
    counts: { doneUnseen: 1 },
  });

  // A read that began after the commit is the Hub's truth, verbatim.
  read.mockResolvedValueOnce(glance(["ins_other", "ins_third"]));
  await hubStore.refreshGlance();
  expect(hubStore.getSnapshot().glance).toMatchObject({
    doneUnseenIds: ["ins_other", "ins_third"],
    counts: { doneUnseen: 2 },
  });
  // …and the mark is retired: the Hub may list the row again (a newer turn).
  read.mockResolvedValueOnce(glance([ID]));
  await hubStore.refreshGlance();
  expect(hubStore.getSnapshot().glance?.doneUnseenIds).toEqual([ID]);
});
