import { afterEach, expect, it, vi } from "vitest";
import { OUTBOX_LS_KEY, type OutboxRecord } from "./outbox";

const INSTANCE = "ins_lease_wakeup";
const JOURNAL = "obj_lease_wakeup_journal";
const COMMAND = "cmd_foreign_inflight";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

type OutboxInternal = {
  ownerId: string;
  pendingLeaseWakeups: () => string[];
  leaseWakeAt: (id: string) => number | undefined;
  clearUnloading: () => void;
};

type Internal = {
  ensureOutbox: () => Promise<boolean>;
  flushAllOutbox: () => Promise<void>;
  rearmLeaseWakeups: () => Promise<void>;
  unloadClearTimer: ReturnType<typeof setTimeout> | null;
  pageIsUnloading: boolean;
  outbox: OutboxInternal;
};

function internalOf(hubStore: Store): Internal {
  return hubStore as unknown as Internal;
}

/**
 * c-reconnfu gate 20: lease-expiry wakes are the CENTRAL per-command wakes in
 * the Outbox (the store's per-instance scheduler is gone). A cancelled
 * beforeunload clears them then re-arms read-only; a sooner lease moves a
 * wake earlier; a lease expiring while a prompt is open is recovered once
 * the page stays — all via the one reconciliation.
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
  expect(await internalOf(hubStore).ensureOutbox()).toBe("ok");
}

function foreignInflightRow(until: number): OutboxRecord {
  return {
    commandId: COMMAND,
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

it("a cancelled beforeunload re-arms the cleared lease wake without flushing", async () => {
  // beforeunload disarms the wakes; a CANCELLED prompt fires no pagehide, the
  // page stays, and the self-reset re-latches delivery and re-arms from
  // durable rows READ-ONLY (it never flushes itself: in a real reload the
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
        commandId: COMMAND,
        id: COMMAND,
        state: "queued",
        dispatch: "not-dispatched",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    });

    writeRows(foreignInflightRow(Date.now() + 10_000));
    await internal.flushAllOutbox();
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([COMMAND]);

    // User triggers navigation but CANCELS the prompt: beforeunload disarms;
    // the reset task re-arms the wake from durable rows.
    window.dispatchEvent(new Event("beforeunload"));
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([]);
    await vi.advanceTimersByTimeAsync(0);
    expect(hubStore.pageIsUnloadingForTest).toBe(false);
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([COMMAND]);
    // The self-reset re-armed only: the foreign lease is live, so no POST.
    expect(send).not.toHaveBeenCalled();

    // The re-armed wake still fires at lease expiry and steals the row.
    await vi.advanceTimersByTimeAsync(9_999);
    expect(send).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    for (let i = 0; i < 20 && send.mock.calls.length === 0; i += 1) await Promise.resolve();
    expect(send).toHaveBeenCalledTimes(1);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("GATE21 item 4: a cancelled navigation whose rearm read rejects once re-arms through the bounded recovery and a later timer still POSTs", async () => {
  // The cancelled-beforeunload self-reset used to perform ONE bare durable
  // read: a one-shot rejection there lost every lease wake permanently (until
  // an unrelated flush). It now goes through the bounded, lifetime-aware
  // recovery — the rejected read retries at 250 ms, reconciles and re-arms;
  // the re-armed wake's later fire still delivers.
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    await boot(api, hubStore);
    const internal = internalOf(hubStore);
    const send = vi.spyOn(api, "instanceSend").mockResolvedValue({
      relatedCommandIds: [],
      command: {
        commandId: COMMAND,
        id: COMMAND,
        state: "queued",
        dispatch: "not-dispatched",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    });

    writeRows(foreignInflightRow(Date.now() + 10_000));
    await internal.flushAllOutbox();
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([COMMAND]);

    // The self-reset's authoritative read (#1 after the cancel) rejects ONCE;
    // every later read is the real backend. onClaimWake reads before
    // rearmLeaseWakeups runs, so arm the one-shot off the setTimeout(0) reset
    // by counting reads while unloading has already been cleared.
    const box = internal.outbox as unknown as {
      readDurable: () => Promise<unknown>;
    };
    const realRead = box.readDurable.bind(box) as () => Promise<unknown>;
    let cancelled = false;
    let postResetReads = 0;
    vi.spyOn(box, "readDurable").mockImplementation(function (this: unknown) {
      if (!cancelled) return realRead.call(this);
      postResetReads += 1;
      if (postResetReads === 1) {
        return Promise.reject(new Error("g21-item4 one-shot rearm read failure"));
      }
      return realRead.call(this);
    });

    window.dispatchEvent(new Event("beforeunload"));
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([]);
    cancelled = true;
    await vi.advanceTimersByTimeAsync(0); // reset task runs; read #1 rejects
    expect(hubStore.pageIsUnloadingForTest).toBe(false);
    // Still no wake: the bounded retry is due 250 ms out.
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([]);
    expect(send).not.toHaveBeenCalled();

    // The bounded recovery read succeeds and re-arms the one lease wake.
    await vi.advanceTimersByTimeAsync(250);
    for (let i = 0; i < 20; i += 1) await Promise.resolve();
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([COMMAND]);
    expect(send).not.toHaveBeenCalled();

    // The re-armed wake still fires at lease expiry and delivers.
    await vi.advanceTimersByTimeAsync(9_750);
    for (let i = 0; i < 40 && send.mock.calls.length === 0; i += 1) {
      await vi.advanceTimersByTimeAsync(1);
      await Promise.resolve();
    }
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
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([COMMAND]);

    // Real navigation ordering: beforeunload's reset task runs first …
    window.dispatchEvent(new Event("beforeunload"));
    await vi.advanceTimersByTimeAsync(0);
    // … then pagehide disarms every wake; the reset task never flushed.
    window.dispatchEvent(new Event("pagehide"));
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([]);
    await vi.advanceTimersByTimeAsync(11_000);
    expect(flushSpy).not.toHaveBeenCalled();
    expect(hubStore.pageIsUnloadingForTest).toBe(true);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("an armed lease wake moves earlier when a sooner lease deadline appears", async () => {
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    await boot(api, hubStore);
    const internal = internalOf(hubStore);
    vi.spyOn(api, "instanceSend").mockResolvedValue({
      relatedCommandIds: [],
      command: {
        commandId: COMMAND,
        id: COMMAND,
        state: "queued",
        dispatch: "not-dispatched",
        resolution: "clear",
      } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
    });

    writeRows(foreignInflightRow(Date.now() + 10_000));
    await internal.flushAllOutbox();
    const firstDeadline = internal.outbox.leaseWakeAt(COMMAND);
    expect(firstDeadline).toBeTruthy();

    // 2 s later the same row's lease is renewed (or re-claimed) with a
    // deadline 1 s from now — sooner than the ~8 s left on the old wake.
    await vi.advanceTimersByTimeAsync(2_000);
    writeRows(foreignInflightRow(Date.now() + 1_000));
    await internal.flushAllOutbox();
    const soonerDeadline = internal.outbox.leaseWakeAt(COMMAND);
    expect(soonerDeadline).toBeLessThan(firstDeadline!);

    // The wake fires at the SOONER expiry (~3 s total), never waits for 10 s.
    await vi.advanceTimersByTimeAsync(999);
    expect(vi.mocked(api.instanceSend)).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    for (let i = 0; i < 20 && vi.mocked(api.instanceSend).mock.calls.length === 0; i += 1) {
      await Promise.resolve();
    }
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
      commandId: COMMAND,
      id: COMMAND,
      state: "queued",
      dispatch: "not-dispatched",
      resolution: "clear",
    } as Awaited<ReturnType<Api["instanceSend"]>>["command"],
  });

  writeRows(foreignInflightRow(Date.now() + 10_000));
  await internal.flushAllOutbox();
  expect(internal.outbox.pendingLeaseWakeups()).toEqual([COMMAND]);

  // beforeunload disarms the wakes and queues the self-reset; hold the reset
  // out of the fake queue (the native dialog blocks the event loop).
  window.dispatchEvent(new Event("beforeunload"));
  const blockedReset = internal.unloadClearTimer;
  expect(blockedReset).toBeTruthy();
  clearTimeout(blockedReset!);
  internal.unloadClearTimer = null;
  expect(internal.outbox.pendingLeaseWakeups()).toEqual([]);

  // The prompt stays open PAST the lease deadline: nothing armed, no delivery.
  await vi.advanceTimersByTimeAsync(10_001);
  expect(send).not.toHaveBeenCalled();
  expect(internal.outbox.pendingLeaseWakeups()).toEqual([]);
  expect(hubStore.pageIsUnloadingForTest).toBe(true);
  return { api, hubStore, internal, send };
}

it("a cancelled prompt with an ALREADY-EXPIRED lease schedules an immediate guarded delivery", async () => {
  // The lease went overdue while the beforeunload prompt was open. The user
  // CANCELS: the self-reset clears the flag and reconciles read-only; the
  // overdue claim arms an IMMEDIATE delivery wake.
  vi.useFakeTimers();
  try {
    const { hubStore, internal, send } = await overduePromptBlocked();

    internal.pageIsUnloading = false;
    internal.outbox.clearUnloading();
    await internal.rearmLeaseWakeups();
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([COMMAND]);

    // The immediate wake delivers the stealable row once.
    await vi.advanceTimersByTimeAsync(1);
    for (let i = 0; i < 20 && send.mock.calls.length === 0; i += 1) await Promise.resolve();
    expect(send).toHaveBeenCalledTimes(1);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("a real pagehide after the overdue re-arm cancels the immediate delivery", async () => {
  // The overdue wake must stay guarded: if the navigation actually proceeds
  // (the prompt allowed it) the immediate timer is disarmed with every other
  // timer and must never POST into teardown.
  vi.useFakeTimers();
  try {
    const { hubStore, internal, send } = await overduePromptBlocked();

    internal.pageIsUnloading = false;
    internal.outbox.clearUnloading();
    await internal.rearmLeaseWakeups();
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([COMMAND]);

    window.dispatchEvent(new Event("pagehide"));
    expect(internal.outbox.pendingLeaseWakeups()).toEqual([]);
    await vi.advanceTimersByTimeAsync(1);
    expect(send).not.toHaveBeenCalled();

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});
