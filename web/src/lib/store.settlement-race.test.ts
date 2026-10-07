import { afterEach, expect, it, vi } from "vitest";
import type { Interaction } from "../types/interaction";

const INTERACTION = "int_settlement_race";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

/** Fresh store/api module pair per test — the store is a process singleton. */
async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

function card(state: Interaction["state"]): Interaction {
  const terminal =
    state === "invalidated"
      ? {
          resolution: {
            state: "known",
            value: { reason: "generation-ended", eventIds: [] },
          },
        }
      : {};
  return {
    id: INTERACTION,
    interactionId: INTERACTION,
    instanceId: "ins_settlement_race",
    hostId: "hst_1",
    runId: null,
    blocking: state === "pending",
    answerable: state === "pending",
    carrier: "harness-hook",
    kind: "approval",
    requestVersion: "1",
    state,
    request: {
      kind: "approval",
      title: "Bash",
      description: "ls",
      toolCallId: null,
      actionRef: "obj_1" as never,
      options: [],
      requestedPermissionsRef: null,
      inputDigest: "sha256:00",
    },
    deadline: { state: "not-applicable" },
    delivery: "not-sent",
    answer: { state: "not-applicable" },
    ...terminal,
  } as unknown as Interaction;
}

afterEach(() => {
  vi.restoreAllMocks();
});

/**
 * c-cardsettle r3 item 3: drive the REAL store through its settlement
 * subscription callback against deferred API responses.
 *
 *  - the bootstrap list seeds a pending card;
 *  - an OLD refresh starts and stays in flight (its interaction.list is
 *    held, and it still carries the stale pending row);
 *  - the Hub settlement frame arrives: the store installs the terminal pin
 *    against the current seq, flips the local row immediately, then runs a
 *    trailing refresh;
 *  - that NEWER refresh completes with the durable invalidated row;
 *  - only THEN does the old held fetch resolve last, still pending.
 *
 * The store must stay invalidated (the pin suppresses the stale copy) and the
 * pin must be released once the newer page confirms and the older request has
 * drained. With settlement handling a no-op the stale pending row lands last
 * and resurrects the card, so this fails without the fix.
 */
it("an older in-flight poll resolving after the settlement frame cannot resurrect the card", async () => {
  const { api, hubStore } = await fresh();

  // Capture the subscription callback the store installs at bootstrap.
  let settlementCallback: ((interactionId: string) => void) | null = null;
  vi.spyOn(api, "settlementSubscribe").mockImplementation((callback) => {
    settlementCallback = callback;
    return () => undefined;
  });

  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] });
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);

  // Response per interaction.list call:
  //  1 = bootstrap seed (pending);
  //  2 = the old poll, held open until released, resolving with stale pending;
  //  3 = the settlement-driven newer refresh (durable invalidated);
  //  later = terminal/empty.
  let releaseOldPoll: () => void = () => undefined;
  const oldPollGate = new Promise<void>((resolve) => {
    releaseOldPoll = resolve;
  });
  let listCalls = 0;
  vi.spyOn(api, "interactionList").mockImplementation(async () => {
    const n = ++listCalls;
    if (n === 1) return [card("pending")];
    if (n === 2) {
      await oldPollGate;
      return [card("pending")];
    }
    if (n === 3) return [card("invalidated")];
    return [card("invalidated")];
  });

  await hubStore.bootstrap();
  expect(settlementCallback, "bootstrap installed the settlement subscription").toBeTypeOf(
    "function",
  );
  expect(
    hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION)?.state,
  ).toBe("pending");

  // The old poll starts before the settlement and stays in flight.
  const oldRefresh = hubStore.refresh();
  await vi.waitFor(() => expect(listCalls).toBeGreaterThanOrEqual(2));

  // The Hub settlement frame arrives. The local row flips immediately — and
  // the IMMEDIATE projection already carries generation-ended (r3 item 7), so
  // the desktop wording is correct even before (or if) the refresh settles.
  settlementCallback!(INTERACTION);
  const immediate = hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION);
  expect(immediate?.state, "the frame flips the row before its refresh resolves").toBe(
    "invalidated",
  );
  expect(
    immediate?.answerable,
    "the immediate projection is not answerable",
  ).toBe(false);
  expect(
    immediate?.resolution.state === "known"
      ? immediate.resolution.value.reason
      : "missing",
    "the immediate projection stamps generation-ended (r3 item 7)",
  ).toBe("generation-ended");

  // The store's trailing refresh (300 ms) completes first with the durable
  // invalidated row.
  await vi.waitFor(() => expect(listCalls).toBeGreaterThanOrEqual(3));
  await vi.waitFor(() =>
    expect(
      hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION)?.state,
    ).toBe("invalidated"),
  );

  // Then the OLD held poll resolves last, still carrying pending. It must not
  // resurrect the card.
  releaseOldPoll();
  await oldRefresh;
  expect(
    hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION)?.state,
  ).toBe("invalidated");

  // Pin release: the newer page (call 3) reported non-pending and the old
  // request has now drained, so the pin is reaped instead of leaking.
  const pins = (
    hubStore as unknown as { settledInteractions: Map<string, unknown> }
  ).settledInteractions;
  await vi.waitFor(() => expect(pins.has(INTERACTION)).toBe(false));

  hubStore.logout();
});

/**
 * c-cardsettle r3 item 7: when the settlement frame's follow-up refresh
 * REJECTS, the immediate projection still carries generation-ended — the card
 * must not fall back to the generic invalidated wording (「已在其它设备处理」)
 * while it awaits the next authoritative list.
 */
it("the immediate settlement projection keeps generation-ended when its refresh rejects", async () => {
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    let settlementCallback: ((interactionId: string) => void) | null = null;
    vi.spyOn(api, "settlementSubscribe").mockImplementation((callback) => {
      settlementCallback = callback;
      return () => undefined;
    });
    vi.spyOn(api, "hello").mockResolvedValue({} as never);
    vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
    vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null });
    vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
    vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
    vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] });
    vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
    vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
    let listCalls = 0;
    vi.spyOn(api, "interactionList").mockImplementation(async () => {
      const n = ++listCalls;
      if (n === 1) return [card("pending")];
      // Every post-settlement refresh rejects.
      throw new Error("list down");
    });
    await hubStore.bootstrap();
    expect(
      hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION)?.state,
    ).toBe("pending");

    settlementCallback!(INTERACTION);
    // Flush the trailing settlement refresh (300 ms); it rejects and must not
    // revert the immediate projection.
    await vi.advanceTimersByTimeAsync(500);

    const row = hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION);
    expect(row?.state).toBe("invalidated");
    expect(
      row?.resolution.state === "known" ? row.resolution.value.reason : "missing",
    ).toBe("generation-ended");
    expect(row?.answerable).toBe(false);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

/**
 * c-cardsettle r6 item 5: a settlement frame carries its resolution REASON
 * through the subscription API into the immediate pin. A non-process-end
 * settlement (transcript-picker demotion = agent-demoted) must keep that label
 * even when the follow-up refresh rejects — never overwritten as
 * generation-ended.
 */
it("an agent-demoted settlement keeps its reason through the pin and a rejecting refresh", async () => {
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    let settlementCallback:
      | ((interactionId: string, reason?: string) => void)
      | null = null;
    vi.spyOn(api, "settlementSubscribe").mockImplementation((callback) => {
      settlementCallback = callback;
      return () => undefined;
    });
    vi.spyOn(api, "hello").mockResolvedValue({} as never);
    vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
    vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null });
    vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
    vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
    vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] });
    vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
    vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
    let listCalls = 0;
    vi.spyOn(api, "interactionList").mockImplementation(async () => {
      const n = ++listCalls;
      if (n === 1) return [card("pending")];
      // The follow-up refresh rejects: the immediate pin must hold the reason.
      throw new Error("list down");
    });
    await hubStore.bootstrap();
    expect(
      hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION)?.state,
    ).toBe("pending");

    settlementCallback!(INTERACTION, "agent-demoted");
    await vi.advanceTimersByTimeAsync(500);

    const row = hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION);
    expect(row?.state).toBe("invalidated");
    expect(
      row?.resolution.state === "known" ? row.resolution.value.reason : "missing",
    ).toBe("agent-demoted");
    expect(row?.answerable).toBe(false);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

/** r6 item 5: a frame with no/empty reason still defaults to generation-ended. */
it("a settlement frame without a reason defaults the pin to generation-ended", async () => {
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    let settlementCallback:
      | ((interactionId: string, reason?: string) => void)
      | null = null;
    vi.spyOn(api, "settlementSubscribe").mockImplementation((callback) => {
      settlementCallback = callback;
      return () => undefined;
    });
    vi.spyOn(api, "hello").mockResolvedValue({} as never);
    vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
    vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null });
    vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
    vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
    vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] });
    vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
    vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
    vi.spyOn(api, "interactionList").mockResolvedValue([card("pending")]);
    await hubStore.bootstrap();

    settlementCallback!(INTERACTION);
    await vi.advanceTimersByTimeAsync(500);

    const row = hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION);
    expect(
      row?.resolution.state === "known" ? row.resolution.value.reason : "missing",
    ).toBe("generation-ended");

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});
