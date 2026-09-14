import { afterEach, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import type { Observation } from "../types/observation";
import { api } from "./api";
import { mockDb } from "./mock";
import type { LocalBubble } from "./store";
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

/** A user-message live batch attributed to `commandId`. */
function userBatch(instance: Instance, commandId: string | null, text: string, seq: string) {
  return {
    subscriptionId: `sub_${instance.id}`,
    journalId: instance.journalId,
    fromSeq: seq,
    toSeq: seq,
    durableSeq: seq,
    events: [
      {
        ...fixture("live").event,
        eventId: `evt_live_${seq}_${commandId ?? "native"}`,
        instanceId: instance.id,
        journalId: instance.journalId,
        seq,
        payload: {
          nodeId: `obj_node_${seq}_${commandId ?? "native"}`,
          messageId: `obj_node_${seq}_${commandId ?? "native"}`,
          revision: "3",
          baseRevision: "2",
          operation: "replace",
          role: "user",
          phase: "input",
          blocks: [{ type: "text", text }],
          targetBlock: null,
          parentToolCallId: null,
          nativeOrigin: { state: "known", value: "ui" },
          origin: "human",
          ...(commandId ? { commandId } : {}),
          status: "complete",
        },
      },
    ],
  };
}

it("reconciles bubbles by commandId on the live stream, even for identical text", async () => {
  const { instance, history } = fixture("reconcile");
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  vi.spyOn(api, "eventsRead").mockResolvedValue(history);
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockResolvedValue(subscription(instance));

  await hubStore.follow(instance.id);
  const onBatch = subscribe.mock.calls[0]?.[2] as (batch: unknown) => void;

  // Two sends with the same text, each with its own server command.
  const twins: LocalBubble[] = [
    { clientRequestId: "local_a", instanceId: instance.id, text: "twin", commandId: "cmd_a", state: "accepted", createdAt: "t" },
    { clientRequestId: "local_b", instanceId: instance.id, text: "twin", commandId: "cmd_b", state: "accepted", createdAt: "t" },
  ];
  (hubStore as unknown as { emit: (patch: { bubbles: typeof twins }) => void }).emit({
    bubbles: hubStore.getSnapshot().bubbles.concat(twins),
  });
  expect(hubStore.getSnapshot().bubbles).toHaveLength(2);

  // Only cmd_a's journal node arrives. The text is identical, so a text-only
  // rule could not tell which bubble it settles; the commandId rule can.
  onBatch(userBatch(instance, "cmd_a", "twin", "2"));
  await vi.waitFor(() => {
    const settled = hubStore.getSnapshot().bubbles.map((b) => b.state);
    expect(settled).toEqual(["settled", "accepted"]);
  });

  // cmd_b's node settles the remaining one; a natively-typed node
  // (no commandId) with the same text does not settle it prematurely.
  onBatch(userBatch(instance, null, "twin", "3"));
  expect(hubStore.getSnapshot().bubbles.map((b) => b.state)).toEqual(["settled", "accepted"]);
  onBatch(userBatch(instance, "cmd_b", "twin", "4"));
  await vi.waitFor(() => {
    expect(hubStore.getSnapshot().bubbles.map((b) => b.state)).toEqual(["settled", "settled"]);
  });
});
