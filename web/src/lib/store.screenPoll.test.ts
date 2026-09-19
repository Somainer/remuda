import { afterEach, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import { api, ScreenNodeBusyError } from "./api";
import { mockDb } from "./mock";
import { hubStore } from "./store";

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function instance(id: string, lifecycle: Instance["lifecycle"]): Instance {
  return { ...mockDb.instances[0], id: id as Instance["id"], journalId: `obj_${id}`, lifecycle };
}

const runningIds = (suffix: string) =>
  Array.from({ length: 6 }, (_, n) => `ins_screen_live_${suffix}_${n}`);
const exitedIds = (suffix: string) =>
  Array.from({ length: 3 }, (_, n) => `ins_screen_exited_${suffix}_${n}`);

async function hydrate(suffix: string) {
  const live = runningIds(suffix);
  const exited = exitedIds(suffix);
  const items = [
    ...live.map((id) => instance(id, "running")),
    ...exited.map((id) => instance(id, "exited")),
  ];
  vi.spyOn(api, "instanceList").mockResolvedValue({ items } as Awaited<ReturnType<typeof api.instanceList>>);
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  await hubStore.refresh();
  return { live, exited };
}

afterEach(() => {
  hubStore.logout();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

it("skips exited rows, caps reads at 4 in flight, and single-flights each row", async () => {
  const { live: runningIds, exited: exitedIds } = await hydrate("cap");
  const parked = new Map<string, ReturnType<typeof deferred<{ lines: string[] }>>>();
  let active = 0;
  let peak = 0;
  const read = vi.spyOn(api, "screenRead").mockImplementation(async (id) => {
    active += 1;
    peak = Math.max(peak, active);
    const gate = deferred<{ lines: string[] }>();
    parked.set(id, gate);
    try {
      return await gate.promise;
    } finally {
      active -= 1;
    }
  });

  hubStore.refreshScreens([...exitedIds, ...runningIds] as Instance["id"][]);
  // One scheduler tick: at most four parked, no exited rows.
  await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(4));
  expect(peak).toBeLessThanOrEqual(4);
  expect((read.mock.calls.flat() as string[]).some((id) => exitedIds.includes(id))).toBe(false);

  // Interval ticks while reads are parked must not stack duplicates.
  hubStore.refreshScreens([...exitedIds, ...runningIds] as Instance["id"][]);
  await Promise.resolve();
  expect(read).toHaveBeenCalledTimes(4);

  // Releasing one slot admits exactly one more row.
  parked.get(runningIds[0])!.resolve({ lines: ["screen"] });
  await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(5));
  expect(peak).toBeLessThanOrEqual(4);
  for (const id of runningIds.slice(1, 5)) parked.get(id)!.resolve({ lines: ["screen"] });
  await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(6));
  expect(parked.get(runningIds[5])).toBeDefined();
  parked.get(runningIds[5])!.resolve({ lines: ["screen"] });
  await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(runningIds.length));

  // A later tick re-reads live rows (previous calls settled) but still never
  // touches exited rows.
  hubStore.refreshScreens([...exitedIds, ...runningIds] as Instance["id"][]);
  await vi.waitFor(() => expect(read.mock.calls.length).toBeGreaterThan(runningIds.length));
  expect((read.mock.calls.flat() as string[]).some((id) => exitedIds.includes(id))).toBe(false);

  // Drain every parked gate so the shared store starts the next test with an
  // empty scheduler. Resolved gates stay keyed in `parked`, so iterating it
  // also releases reads spawned during this drain.
  for (let round = 0; round < 8; round += 1) {
    for (const gate of parked.values()) gate.resolve({ lines: ["screen"] });
    await new Promise((resolve) => setTimeout(resolve, 0));
    if (active === 0) break;
  }
  expect(active).toBe(0);
});

it("backs off a row that answers NODE_BUSY instead of hammering it", async () => {
  const { live: runningIds } = await hydrate("busy");
  const read = vi
    .spyOn(api, "screenRead")
    .mockRejectedValueOnce(new ScreenNodeBusyError(80))
    .mockResolvedValue({ lines: ["recovered"] });

  hubStore.refreshScreens([runningIds[0]] as Instance["id"][]);
  await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));

  // Ticks inside the back-off window do not re-enqueue the row.
  hubStore.refreshScreens([runningIds[0]] as Instance["id"][]);
  await Promise.resolve();
  expect(read).toHaveBeenCalledTimes(1);

  // After the retry hint the row is read again without an interval tick.
  await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
});
