import { afterEach, expect, it, vi } from "vitest";
import { OUTBOX_LS_KEY, type OutboxRecord } from "./outbox";

const INSTANCE = "ins_lease_wakeup";
const JOURNAL = "obj_lease_wakeup_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

type Internal = {
  ensureOutbox: () => Promise<boolean>;
  flushAllOutbox: () => Promise<void>;
  leaseWakeupTimer: Map<string, unknown>;
  leaseWakeupUntil: Map<string, number>;
  outbox: { ownerId: string };
};

function internalOf(hubStore: Store): Internal {
  return hubStore as unknown as Internal;
}

/**
 * c-reconnfu round 2 item 4: lease-expiry wakeups are armed at another tab's
 * in-flight lease expiry. Two ways they were lost: a cancelled beforeunload
 * cleared them without re-arming, and an armed timer was never moved earlier
 * for a sooner foreign lease.
 */
afterEach(() => {
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

async function boot(api: Api, hubStore: Store) {
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
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  await hubStore.bootstrap();
  expect(await internalOf(hubStore).ensureOutbox()).toBe(true);
}

function foreignInflightRow(until: number): OutboxRecord {
  return {
    commandId: "cmd_foreign_inflight",
    clientRequestId: "local_foreign_inflight",
    instanceId: INSTANCE,
    journalId: JOURNAL,
    prompt: "owned by another tab",
    createdAt: Date.now() - 1_000,
    attempts: 1,
    state: "inflight",
    lease: { owner: "owner_other_tab", until },
  };
}

function writeRows(...rows: OutboxRecord[]) {
  localStorage.setItem(OUTBOX_LS_KEY, JSON.stringify(rows));
}

it("a cancelled beforeunload re-arms a cleared lease-expiry wakeup when the page stays", async () => {
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    await boot(api, hubStore);
    const internal = internalOf(hubStore);
    // The stolen POST is answered held (so the test needs no GET mocks);
    // held retries are fake-timered and cleared on logout.
    const send = vi.spyOn(api, "instanceSend").mockResolvedValue({
      relatedCommandIds: [],
      command: {
        commandId: "cmd_foreign_inflight",
        id: "cmd_foreign_inflight",
        state: "queued",
        dispatch: "not-dispatched",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    });

    writeRows(foreignInflightRow(Date.now() + 10_000));
    await internal.flushAllOutbox();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);

    // User triggers navigation but CANCELS the prompt: beforeunload clears
    // every wakeup, the page stays, the self-reset task must re-arm.
    window.dispatchEvent(new Event("beforeunload"));
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(false);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);

    // The re-armed wakeup still fires at lease expiry and steals the row.
    await vi.advanceTimersByTimeAsync(9_999);
    expect(send).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(send).toHaveBeenCalledTimes(1);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("a genuine pagehide does not self-reset the unload flag nor re-arm the wakeup", async () => {
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    await boot(api, hubStore);
    const internal = internalOf(hubStore);

    writeRows(foreignInflightRow(Date.now() + 10_000));
    await internal.flushAllOutbox();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);

    // Real navigation: pagehide must not schedule the beforeunload self-reset
    // (which would un-latch delivery and re-arm in a tearing-down page).
    const flushSpy = vi.spyOn(internal, "flushAllOutbox");
    window.dispatchEvent(new Event("pagehide"));
    expect(hubStore.pageIsUnloadingForTest).toBe(true);
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(false);
    await vi.advanceTimersByTimeAsync(11_000);
    expect(flushSpy).not.toHaveBeenCalled();
    expect(hubStore.pageIsUnloadingForTest).toBe(true);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("an armed lease wakeup is moved earlier when a sooner foreign lease appears", async () => {
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    await boot(api, hubStore);
    const internal = internalOf(hubStore);
    vi.spyOn(api, "instanceSend").mockResolvedValue({
      relatedCommandIds: [],
      command: {
        commandId: "cmd_foreign_inflight",
        id: "cmd_foreign_inflight",
        state: "queued",
        dispatch: "not-dispatched",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    });

    writeRows(foreignInflightRow(Date.now() + 10_000));
    await internal.flushAllOutbox();
    const first = internal.leaseWakeupTimer.get(INSTANCE);
    expect(first).toBeTruthy();

    // 2 s later another tab's NEWER in-flight row for this instance carries a
    // lease expiring 1 s from now (sooner than the 8 s still on the clock).
    await vi.advanceTimersByTimeAsync(2_000);
    writeRows(foreignInflightRow(Date.now() + 1_000));
    await internal.flushAllOutbox();
    expect(internal.leaseWakeupTimer.get(INSTANCE)).not.toBe(first);

    // The wakeup fires at the SOONER expiry (3 s total), never waits for 10 s.
    await vi.advanceTimersByTimeAsync(999);
    expect(vi.mocked(api.instanceSend)).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(vi.mocked(api.instanceSend)).toHaveBeenCalledTimes(1);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});
