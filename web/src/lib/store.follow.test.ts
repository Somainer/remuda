import { afterEach, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import type { Observation } from "../types/observation";
import { api } from "./api";
import { mockDb } from "./mock";
import { hubStore } from "./store";

type History = Awaited<ReturnType<typeof api.eventsRead>>;

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

function fixture(suffix: string) {
  const original = mockDb.instances[0];
  const instance: Instance = { ...original, id: `ins_follow_${suffix}`, journalId: `obj_follow_${suffix}` };
  const event: Observation = {
    ...mockDb.journals.get(original.journalId)![0],
    eventId: `evt_follow_${suffix}`,
    instanceId: instance.id,
    journalId: instance.journalId,
    seq: "1",
  };
  return { instance, event, history: { events: [event], durableSeq: "1", floorSeq: "1" } };
}

function subscription(instance: Instance): Awaited<ReturnType<typeof api.eventsSubscribe>> {
  return {
    subscriptionId: `sub_${instance.id}`,
    journalId: instance.journalId,
    durableSeq: "1",
    floorSeq: "1",
    snapshot: {
      projectionVersion: "v1", projectionEpoch: "epoch_follow_test", asOfSeq: "1", instance,
      runs: [], commands: [], pendingInteractions: [], nodes: [],
      history: { earliestRetainedSeq: "1", complete: true },
    },
  };
}

afterEach(() => { hubStore.logout(); vi.restoreAllMocks(); });

it("shares one journal subscription when concurrent follows finish reading history together", async () => {
  const { instance, event, history } = fixture("shared");
  const barrier = deferred<History>();
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  const read = vi.spyOn(api, "eventsRead").mockReturnValue(barrier.promise);
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockResolvedValue(subscription(instance));

  const first = hubStore.follow(instance.id);
  const second = hubStore.follow(instance.id);
  await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
  expect(subscribe).not.toHaveBeenCalled();
  barrier.resolve(history);
  await Promise.all([first, second]);

  expect(subscribe).toHaveBeenCalledTimes(1);
  expect(subscribe).toHaveBeenCalledWith(instance.journalId, "1", expect.any(Function));
  expect(hubStore.getSnapshot().events[instance.id]).toEqual([event]);
});

it("keeps concurrent follows of distinct journals independent", async () => {
  const first = fixture("project_a");
  const second = fixture("project_b");
  const barrier = deferred<void>();
  vi.spyOn(api, "instanceGet").mockImplementation(async (instanceId) =>
    instanceId === first.instance.id ? first.instance : second.instance);
  const read = vi.spyOn(api, "eventsRead").mockImplementation(async ({ journalId }) => {
    await barrier.promise;
    return journalId === first.instance.journalId ? first.history : second.history;
  });
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockImplementation(async (journalId) =>
    subscription(journalId === first.instance.journalId ? first.instance : second.instance));

  const follows = [hubStore.follow(first.instance.id), hubStore.follow(second.instance.id)];
  await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
  barrier.resolve();
  await Promise.all(follows);

  expect(subscribe).toHaveBeenCalledTimes(2);
  expect(subscribe.mock.calls.map(([journalId]) => journalId).sort())
    .toEqual([first.instance.journalId, second.instance.journalId].sort());
  expect(hubStore.getSnapshot().events[first.instance.id]).toEqual([first.event]);
  expect(hubStore.getSnapshot().events[second.instance.id]).toEqual([second.event]);
});
