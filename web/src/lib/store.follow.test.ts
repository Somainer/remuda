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
  return { instance, event, history: { events: [event], durableSeq: "1", windowFromSeq: "1", reachedAfterSeq: true } };
}

function subscription(instance: Instance): Awaited<ReturnType<typeof api.eventsSubscribe>> {
  return {
    subscriptionId: `sub_${instance.id}`,
    journalId: instance.journalId,
    durableSeq: "1",
    windowFromSeq: "1",
    reachedAfterSeq: true,
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
  expect(subscribe).toHaveBeenCalledWith(instance.journalId, "1", expect.any(Function), expect.any(Function), expect.any(Object));
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

it("follows a bounded partial tail window without claiming earliestRetainedSeq 1", async () => {
  // Hub serves a bounded window: seed is a tail whose floor is above 1 and
  // reachedAfterSeq=false. follow() must not ascend for more pages, must not
  // claim history complete from seq 1, and must seed the journal client with
  // the window floor so load-earlier stays available.
  const suffix = "partial_window";
  const original = mockDb.instances[0];
  const instance: Instance = {
    ...original,
    id: `ins_follow_${suffix}`,
    journalId: `obj_follow_${suffix}`,
    durableSeq: "100",
  };
  const windowEvents: Observation[] = Array.from({ length: 10 }, (_, i) => ({
    ...mockDb.journals.get(original.journalId)![0],
    eventId: `evt_follow_${suffix}_${i + 91}`,
    instanceId: instance.id,
    journalId: instance.journalId,
    seq: String(i + 91),
  }));
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  const read = vi
    .spyOn(api, "eventsRead")
    .mockResolvedValue({ events: windowEvents, durableSeq: "100", windowFromSeq: "91", reachedAfterSeq: false });
  const subscribe = vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: `sub_${instance.id}`,
    journalId: instance.journalId,
    durableSeq: "100",
    windowFromSeq: "91",
    reachedAfterSeq: false,
    snapshot: {
      projectionVersion: "v1", projectionEpoch: "epoch_follow_partial", asOfSeq: "100", instance,
      runs: [], commands: [], pendingInteractions: [], nodes: [],
      history: { earliestRetainedSeq: "91", complete: false },
    },
  });

  await hubStore.follow(instance.id);

  // One tail seed, no ascending loop; follow resumes from the window's last row.
  expect(read).toHaveBeenCalledTimes(1);
  expect(subscribe).toHaveBeenCalledTimes(1);
  expect(subscribe).toHaveBeenCalledWith(instance.journalId, "100", expect.any(Function), expect.any(Function), expect.any(Object));
  const loaded = hubStore.getSnapshot().events[instance.id];
  expect(loaded?.map((e) => Number(e.seq))).toEqual(Array.from({ length: 10 }, (_, i) => i + 91));
  const journals = (hubStore as unknown as { journals: Map<string, { retainedFloorSeq: string }> }).journals;
  const client = journals.get(instance.journalId)!;
  expect(client.retainedFloorSeq).toBe("91");
  // No load-earlier/flag code may ever surface "1" as the retained floor.
  expect(client.retainedFloorSeq).not.toBe("1");
  expect(hubStore.getSnapshot().journalStatus[instance.id]).toBe("live");
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

it("applies authoritative turn activity from follow immediately and fences an older HTTP poll", async () => {
  const fixture_ = fixture("hook_activity");
  const instance: Instance = { ...fixture_.instance, durableSeq: "1", activity: { state: "known", value: "idle" } };
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "1", windowFromSeq: null, reachedAfterSeq: true });
  let deliver!: Parameters<typeof api.eventsSubscribe>[2];
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async (_journalId, _after, onBatch) => {
    deliver = onBatch;
    return subscription(instance);
  });
  await hubStore.follow(instance.id);

  const event = (seq: string, activity: "working" | "idle"): Observation => ({
    ...fixture_.event, seq, eventId: `evt_hook_activity_${seq}`, kind: "lifecycle",
    payload: {
      type: "entity", entityType: "instance", entityId: instance.id,
      revision: seq, previousState: null, state: "ready", reasonCode: "hook-activity", evidenceEventIds: [],
      entity: { ...instance, revision: seq, activity: { state: "known", value: activity },
        nativeRef: { ...instance.nativeRef, signalTier: "hook" } },
    },
  } as unknown as Observation);
  const receive = (observation: Observation) => deliver({
    subscriptionId: `sub_${instance.id}`, journalId: instance.journalId,
    fromSeq: observation.seq, toSeq: observation.seq, durableSeq: observation.seq, events: [observation],
  });
  const current = () => hubStore.getSnapshot().instances.find((row) => row.id === instance.id)!;

  receive(event("2", "working"));
  expect(current().activity).toEqual({ state: "known", value: "working" });
  expect(current().nativeRef.signalTier).toBe("hook");
  const stale = { ...current() };
  const pending = deferred<Awaited<ReturnType<typeof api.instanceList>>>();
  vi.spyOn(api, "instanceList").mockReturnValue(pending.promise);
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const refresh = hubStore.refresh();

  receive(event("3", "idle"));
  expect(current().activity).toEqual({ state: "known", value: "idle" });
  pending.resolve({ items: [stale], nextCursor: null });
  await refresh;
  expect(current().activity).toEqual({ state: "known", value: "idle" });
  expect(current().durableSeq).toBe("3");
});

it("applies Node-validated native activity before its full Instance and ignores unvalidated or foreign events", async () => {
  const fixture_ = fixture("validated_native_activity");
  const instance: Instance = {
    ...fixture_.instance, driver: "shell-pty", durableSeq: "1",
    activity: { state: "known", value: "idle" },
    nativeRef: { ...fixture_.instance.nativeRef, signalTier: "hook" },
  };
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance);
  vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "1", windowFromSeq: null, reachedAfterSeq: true });
  let deliver!: Parameters<typeof api.eventsSubscribe>[2];
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async (_journalId, _after, onBatch) => {
    deliver = onBatch;
    return subscription(instance);
  });
  await hubStore.follow(instance.id);
  const native = (seq: string, nativeName: string, activity?: string, instanceId = instance.id): Observation => ({
    ...fixture_.event, seq, instanceId, eventId: `evt_validated_native_${seq}`, kind: "lifecycle",
    observedAt: "2026-09-14T00:00:01.000Z",
    payload: {
      type: "native", nativeName, topic: "turn", severity: "info", affectsCompletion: false,
      nativeId: { state: "not-applicable" }, dataRef: null,
      status: { state: "known", value: activity ?? "working" },
      relatedIds: activity ? { remudaActivity: activity } : {},
    },
  } as Observation);
  const receive = (observation: Observation) => deliver({
    subscriptionId: `sub_${instance.id}`, journalId: instance.journalId,
    fromSeq: observation.seq, toSeq: observation.seq, durableSeq: observation.seq, events: [observation],
  });
  const current = () => hubStore.getSnapshot().instances.find((row) => row.id === instance.id)!;

  const prompt = native("2", "UserPromptSubmit", "working");
  receive(prompt);
  expect(current().activity).toEqual({ state: "known", value: "working" });
  expect(current().activityEvidenceEventIds).toEqual([prompt.eventId]);
  expect(current().updatedAt).toBe(prompt.observedAt);
  expect(current().nativeRef).toBe(instance.nativeRef);
  expect(current().durableSeq).toBe("2");

  receive(native("3", "Stop"));
  receive(native("4", "Stop", "idle", "ins_foreign_native_activity"));
  expect(current().activity).toEqual({ state: "known", value: "working" });
  expect(current().durableSeq).toBe("2");

  receive({
    ...fixture_.event, seq: "5", eventId: "evt_validated_native_instance", kind: "lifecycle",
    payload: {
      type: "entity", entityType: "instance", entityId: instance.id,
      revision: "2", previousState: null, state: "ready", reasonCode: "hook-activity", evidenceEventIds: [],
      entity: { ...current(), revision: "2" },
    },
  } as unknown as Observation);
  expect(current().activity).toEqual({ state: "known", value: "working" });
  expect(current().durableSeq).toBe("5");

  const interrupted = native("6", "interrupted", "idle");
  receive(interrupted);
  expect(current().activity).toEqual({ state: "known", value: "idle" });
  expect(current().activityEvidenceEventIds).toEqual([interrupted.eventId]);
  expect(current().nativeRef).toEqual(instance.nativeRef);
  receive(native("7", "SubagentStop", "working"));
  receive(native("8", "UserPromptSubmit", "invalid"));
  receive(native("9", "UserPromptSubmit"));
  expect(current().activity).toEqual({ state: "known", value: "idle" });
  expect(current().durableSeq).toBe("6");

  const list = vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ ...current(), durableSeq: "10" }], nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  await hubStore.refresh();
  receive(native("10", "UserPromptSubmit", "working"));
  expect(current().activity).toEqual({ state: "known", value: "idle" });
  list.mockResolvedValue({ items: [{ ...current(), driver: "claude-print" }], nextCursor: null });
  await hubStore.refresh();
  receive(native("11", "UserPromptSubmit", "working"));
  expect(current().activity).toEqual({ state: "known", value: "idle" });
  expect(current().durableSeq).toBe("10");
});
