import { afterEach, expect, it, vi } from "vitest";
import { OUTBOX_LS_KEY } from "./outbox";

const INSTANCE = "ins_localid_reload";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

/**
 * c-reconnfu item 3: the local bubble id (ids.ts) is a restart-local counter,
 * so after a reload the FIRST bubble sent in the new tab gets the same
 * clientRequestId an unresolved bubble from the previous tab already owns.
 * Bubble merge/React keys are clientRequestId-based: the restored row is
 * overwritten/hidden. Local ids must be random and survive the module reset.
 */
afterEach(() => {
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

async function bootOffline(api: Api, hubStore: Store) {
  vi.spyOn(api, "hello").mockRejectedValue(new Error("network off"));
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  // Keep rows pending: the reconnect flush must not settle/drop them.
  vi.spyOn(api, "instanceSend").mockRejectedValue(new Error("still offline"));
  await hubStore.bootstrap();
}

it("a bubble sent after reload never reuses a restored bubble's clientRequestId", async () => {
  // --- First tab: send while offline; the row is durably queued. ---
  {
    const { api, hubStore } = await fresh();
    await bootOffline(api, hubStore);
    const ok = await hubStore.send(INSTANCE, "first tab prompt");
    expect(ok).toBe(true);
    const bubble = hubStore.getSnapshot().bubbles.find((b) => b.text === "first tab prompt");
    expect(bubble).toBeDefined();
    // Persist the id like the previous tab left it in the durable outbox.
    expect(JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]")[0].clientRequestId).toBe(
      bubble!.clientRequestId,
    );
    hubStore.logout();
  }

  // --- Reload: modules reset (the id counter restarts at 1). ---
  {
    const { api, hubStore } = await fresh();
    await bootOffline(api, hubStore);

    // The unresolved bubble is restored from the outbox.
    const before = hubStore.getSnapshot().bubbles;
    const restored = before.find((b) => b.text === "first tab prompt");
    expect(restored).toBeDefined();

    // Send again in the new tab: its clientRequestId must be fresh.
    await hubStore.send(INSTANCE, "second tab prompt");
    const bubbles = hubStore.getSnapshot().bubbles.filter((b) => b.instanceId === INSTANCE);
    const first = bubbles.find((b) => b.text === "first tab prompt");
    const second = bubbles.find((b) => b.text === "second tab prompt");
    expect(first).toBeDefined();
    expect(second).toBeDefined();
    expect(second!.clientRequestId).not.toBe(first!.clientRequestId);
    // Exactly the two distinct rows, both visible (no duplicate-key merge).
    expect(bubbles).toHaveLength(2);
    expect(new Set(bubbles.map((b) => b.clientRequestId)).size).toBe(2);

    hubStore.logout();
  }
});
