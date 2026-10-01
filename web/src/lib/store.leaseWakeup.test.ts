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
  rearmLeaseWakeups: () => Promise<void>;
  leaseWakeupTimer: Map<string, unknown>;
  leaseWakeupUntil: Map<string, number>;
  unloadClearTimer: ReturnType<typeof setTimeout> | null;
  pageIsUnloading: boolean;
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

it("a cancelled beforeunload re-arms a cleared lease-expiry wakeup without flushing", async () => {
  // beforeunload disarms the timers; a CANCELLED prompt fires no pagehide,
  // the page stays, and the self-reset re-latches delivery and re-arms the
  // lease wakeups READ-ONLY (it never flushes itself: in a real reload the
  // task runs before pagehide inside teardown).
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

    // User triggers navigation but CANCELS the prompt: beforeunload disarms;
    // the reset task then re-arms the wakeup from durable rows.
    window.dispatchEvent(new Event("beforeunload"));
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(false);
    await vi.advanceTimersByTimeAsync(0);
    expect(hubStore.pageIsUnloadingForTest).toBe(false);
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);
    // The self-reset re-armed only: the foreign lease is live, so no POST.
    expect(send).not.toHaveBeenCalled();

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

it("a genuine reload: the beforeunload self-reset task never flushes and pagehide disarms", async () => {
  // Chromium runs beforeunload's setTimeout(0) ~0.1 ms BEFORE pagehide: the
  // self-reset must clear only the flag, never flush (an inflight lease
  // written during teardown would strand the reloaded document 30 s).
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    await boot(api, hubStore);
    const internal = internalOf(hubStore);
    const flushSpy = vi.spyOn(internal, "flushAllOutbox");

    writeRows(foreignInflightRow(Date.now() + 10_000));
    await internal.flushAllOutbox();
    flushSpy.mockClear();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);

    // Real navigation ordering: beforeunload's reset task runs first …
    window.dispatchEvent(new Event("beforeunload"));
    await vi.advanceTimersByTimeAsync(0);
    // … then pagehide disarms every timer; the reset task never flushed.
    window.dispatchEvent(new Event("pagehide"));
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

/**
 * Block the beforeunload self-reset like a native dialog does and sit past the
 * 10 s foreign-lease deadline with the page still "unloading". Returns the
 * armed test handles at the overdue moment; the caller plays the user's
 * decision (cancel → self-reset, or a real pagehide).
 */
async function overduePromptBlocked(): Promise<{
  api: Api;
  hubStore: Store;
  internal: Internal;
  send: ReturnType<typeof vi.fn>;
}> {
  const { api, hubStore } = await fresh();
  await boot(api, hubStore);
  const internal = internalOf(hubStore);
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

  // beforeunload disarms the wakeup and queues the self-reset; hold the reset
  // out of the fake queue (the native dialog blocks the event loop).
  window.dispatchEvent(new Event("beforeunload"));
  const blockedReset = internal.unloadClearTimer;
  expect(blockedReset).toBeTruthy();
  clearTimeout(blockedReset!);
  internal.unloadClearTimer = null;
  expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(false);

  // The prompt stays open PAST the lease deadline: nothing armed, no delivery.
  await vi.advanceTimersByTimeAsync(10_001);
  expect(send).not.toHaveBeenCalled();
  expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(false);
  expect(hubStore.pageIsUnloadingForTest).toBe(true);
  return { api, hubStore, internal, send };
}

it("a cancelled prompt with an ALREADY-EXPIRED foreign lease schedules an immediate guarded delivery", async () => {
  // c-reconnfu round 3 item 4: the scheduler filtered expired leases out of
  // the cancelled-prompt re-arm, so a lease that went overdue while the
  // beforeunload prompt was open stranded its stealable row forever. The
  // read-only re-arm now includes overdue leases as immediate wakeups.
  vi.useFakeTimers();
  try {
    const { hubStore, internal, send } = await overduePromptBlocked();

    // The user CANCELS: the self-reset body runs (the setTimeout(0) task the
    // dialog held), clears the flag and re-arms read-only. The overdue lease
    // arms an IMMEDIATE wakeup — the old filter armed nothing.
    internal.pageIsUnloading = false;
    await internal.rearmLeaseWakeups();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);

    // The immediate wakeup delivers the stealable row once.
    await vi.advanceTimersByTimeAsync(1);
    expect(send).toHaveBeenCalledTimes(1);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("a real pagehide after the overdue re-arm cancels the immediate delivery", async () => {
  // The overdue wakeup must stay guarded: if the navigation actually
  // proceeds (the prompt allowed it) the immediate timer is disarmed with
  // every other timer and must never POST into teardown.
  vi.useFakeTimers();
  try {
    const { hubStore, internal, send } = await overduePromptBlocked();

    internal.pageIsUnloading = false;
    await internal.rearmLeaseWakeups();
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(true);

    window.dispatchEvent(new Event("pagehide"));
    expect(internal.leaseWakeupTimer.has(INSTANCE)).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    expect(send).not.toHaveBeenCalled();

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});
