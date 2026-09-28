import { afterEach, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import { instanceHasTtyAttach } from "../features/session/tty/gate";
import { mockDb } from "./mock";
import { OUTBOX_LS_KEY, type OutboxRecord } from "./outbox";

const INSTANCE = "ins_offline_restore";
const JOURNAL = "obj_offline_restore_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

it("an offline reload restores the full instance projection (capabilities/nativeRef), never a cast stub, and renders offline", async () => {
  const { api, hubStore } = await fresh();

  // A COMPLETE last-known instance projection, persisted with the outbox row.
  const known: Instance = {
    ...mockDb.instances[0],
    id: INSTANCE,
    journalId: JOURNAL,
  };
  const row: OutboxRecord = {
    commandId: "cmd_restore_1",
    clientRequestId: "local_restore_1",
    instanceId: INSTANCE,
    journalId: JOURNAL,
    instanceSnapshot: known,
    prompt: "sent while the network was down",
    createdAt: Date.now() - 1000,
    attempts: 0,
    state: "pending",
  };
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify([row]));

  // The Hub is unreachable on reload: hello fails WITH a device session still
  // present, so bootstrap takes the offline-reload branch.
  vi.spyOn(api, "hello").mockRejectedValue(new Error("network off"));
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  const send = vi.spyOn(api, "instanceSend");

  await hubStore.bootstrap();

  const state = hubStore.getSnapshot();
  // Honest link state with the network still off: offline or recovering,
  // NEVER live (property B — a frozen transcript must not show 已连接). The
  // offline machine may already have kicked its immediate reconnect attempt.
  expect(["offline", "recovering"]).toContain(state.connection);
  // No POST attempted while offline.
  expect(send).not.toHaveBeenCalled();

  // The restored instance is the FULL projection: dereferencing capabilities
  // and nativeRef (as SessionPage and tty/gate do) must not throw, and the
  // old cast-stub sentinels (empty hostId / missing capabilities) are gone.
  const restored = state.instances.find((i) => i.id === INSTANCE);
  expect(restored).toBeDefined();
  expect(restored?.hostId).toBe(known.hostId);
  expect(restored?.hostId).not.toBe("");
  expect(restored?.nativeRef).toEqual(known.nativeRef);
  expect(restored?.capabilities).toEqual(known.capabilities);
  expect(() => instanceHasTtyAttach(restored!)).not.toThrow();
  expect(instanceHasTtyAttach(restored!)).toBe(instanceHasTtyAttach(known));

  // The unsent row restores as a bubble keyed to the restored instance.
  const bubble = state.bubbles.find((b) => b.clientRequestId === "local_restore_1");
  expect(bubble?.commandId).toBe("cmd_restore_1");
  expect(bubble?.instanceId).toBe(INSTANCE);

  // The restored session has no events (the offline seed/follow never ran),
  // yet the restored bubble is renderable: SessionPage's snapshotLoading gate
  // must treat "events missing BUT bubbles present" as ready, not as a
  // perpetual "加载 snapshot…" that hides the outbox row and the composer.
  expect(state.events[INSTANCE]).toBeUndefined();
  const bubblesForInstance = state.bubbles.filter((b) => b.instanceId === INSTANCE);
  const wouldGateTranscript =
    state.events[INSTANCE] === undefined && bubblesForInstance.length === 0;
  expect(wouldGateTranscript).toBe(false);
});

it("an offline-reloaded session stays bound to the connection machine across the failed follow open", async () => {
  const { api, hubStore } = await fresh();
  const known: Instance = {
    ...mockDb.instances[0],
    id: INSTANCE,
    journalId: JOURNAL,
  };
  // A device session + reachable hello builds the connection machine
  // (bootstrap), then follow() fails with the Hub unreachable.
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
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  await hubStore.bootstrap();
  vi.spyOn(api, "instanceGet").mockResolvedValue(known);
  vi.spyOn(api, "eventsRead").mockRejectedValue(new Error("offline"));
  vi.spyOn(api, "eventsSubscribe").mockRejectedValue(new Error("offline"));
  await hubStore.follow(INSTANCE).catch(() => undefined);

  // Even though the socket (and its seed read) never succeeded, the machine
  // already knows which session to reopen on recovery (pre-bind). A journal
  // client is registered only once a seed read succeeds; reopenFollow handles
  // the missing client by running follow() again on recovery.
  const internal = hubStore as unknown as {
    connectionBoundTo: string | null;
    connectionBoundJournal: string | null;
  };
  expect(internal.connectionBoundTo).toBe(INSTANCE);
  expect(internal.connectionBoundJournal).toBe(JOURNAL);
});
