import { afterEach, expect, it, vi } from "vitest";
import type { CommandResult } from "../types/command";
import type { Observation } from "../types/observation";
import { OUTBOX_LS_KEY } from "./outbox";

/**
 * Ruling AD (outboxdebt r1): catch-up debt for a drained (sent-while-offline)
 * command is discharged ONLY BY EVIDENCE that the command reached the journal
 * — the follow/journal client observes the journal entry for the command id.
 * An empty resync certifies the connection live but must NOT delete the debt;
 * a bounded debt-driven tail re-read keeps re-reading, and exhaustion moves
 * the bubble to the existing 状态待确认 (outbox "unknown") state.
 */
const INSTANCE = "ins_outbox_debt";
const JOURNAL = "obj_outbox_debt_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

function commandResult(commandId: string, state: CommandResult["command"]["state"]): CommandResult {
  return {
    relatedCommandIds: [],
    command: {
      commandId,
      id: commandId,
      revision: "1",
      createdAt: "2026-10-18T00:00:00.000Z",
      updatedAt: "2026-10-18T00:00:00.000Z",
      actor: { principalId: "prn_1", type: "human", deviceId: "dev_1", instanceId: INSTANCE },
      origin: "ui",
      operation: "instance.send",
      target: { hostId: "hst_1", instanceId: INSTANCE, runId: null },
      payloadDigest: "sha256:00",
      state,
      dispatch: "intent-durable",
      resolution: "clear",
    },
  };
}

const liveStores: Store[] = [];
async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  liveStores.push(storeModule.hubStore);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

type JournalPage = Awaited<ReturnType<Api["eventsRead"]>>;
const EMPTY_PAGE: JournalPage = {
  events: [],
  durableSeq: "0",
  windowFromSeq: null,
  reachedAfterSeq: true,
};

function userMessagePage(commandId: string): JournalPage {
  return {
    events: [
      {
        kind: "message",
        eventId: "evt_user_debt_1",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "1",
        completeness: "structured",
        payload: { role: "user", commandId },
      } as unknown as Observation,
    ],
    durableSeq: "1",
    windowFromSeq: "1",
    reachedAfterSeq: true,
  };
}

async function mountFollow(api: Api, hubStore: Store) {
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  vi.spyOn(api, "eventsRead").mockResolvedValue(EMPTY_PAGE);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      { id: INSTANCE, journalId: JOURNAL, revision: "1", durableSeq: "0", lifecycle: "running" } as Awaited<
        ReturnType<Api["instanceList"]>
      >["items"][number],
    ],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as Awaited<ReturnType<Api["interactionList"]>>);
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_outbox_debt",
    journalId: JOURNAL,
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
    getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_outbox_debt",
      asOfSeq: "0",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "0", complete: true },
    },
  });
  await hubStore.refresh();
  await hubStore.follow(INSTANCE);
}

function debtOf(hubStore: Store): Map<string, unknown> {
  return (hubStore as unknown as { reconcileOwed: Map<string, unknown> }).reconcileOwed;
}

/** The live-batch injector the follow subscription mock captured. */
async function liveBatch(api: Api) {
  const subscribe = vi.mocked(api.eventsSubscribe);
  await vi.waitFor(() => expect(subscribe.mock.calls.length).toBeGreaterThan(0));
  return subscribe.mock.calls[0]?.[2] as (batch: Record<string, unknown>) => void;
}

afterEach(() => {
  vi.restoreAllMocks();
  for (const store of liveStores.splice(0)) store.logout();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

it("an empty resync keeps the drained command's catch-up debt; the bubble resolves when the journal entry is finally read", async () => {
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);

  // The POST is accepted (Hub forwarded it) but the Node's journal write is
  // held: every journal read returns empty for now.
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    const commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });

  await hubStore.send(INSTANCE, "offline across reload");
  const bubble = () => hubStore.getSnapshot().bubbles[0]!;
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));
  const commandId = bubble().commandId!;

  // The drain's catch-up ran on the mounted client: the seed read plus the
  // post-delivery resync, both EMPTY (the Node has not journaled yet).
  const read = vi.mocked(api.eventsRead);
  await vi.waitFor(() => expect(read.mock.calls.length).toBeGreaterThanOrEqual(2));
  const emptyReads = read.mock.calls.length;

  // Ruling AD: an empty resync must NOT discharge the debt. The per-command
  // debt survives even though the link is certified live.
  expect(debtOf(hubStore).has(INSTANCE)).toBe(true);
  // The bounded debt-driven tail re-read keeps re-reading while evidence is
  // missing (the old code never read again after the empty resync).
  await vi.waitFor(() => expect(read.mock.calls.length).toBeGreaterThan(emptyReads), { timeout: 5_000 });
  // …and the bubble honestly stays accepted — never settled without evidence.
  expect(bubble().state).toBe("accepted");
  expect(bubble().outboxState).toBe("sent");

  // The Node's journal write lands (late bus publish missed / replay — only a
  // durable read can observe it now): the next bounded tail read returns the
  // user message carrying the command id.
  vi.mocked(api.eventsRead).mockResolvedValue(userMessagePage(commandId));

  await vi.waitFor(
    () => {
      const b = hubStore.getSnapshot().bubbles.find((x) => x.commandId === commandId)!;
      expect(b.state).toBe("settled");
      expect(b.outboxState).toBe("done");
    },
    { timeout: 5_000 },
  );
  // Evidence discharges the debt.
  expect(debtOf(hubStore).has(INSTANCE)).toBe(false);
});

it("bounded tail re-read exhaustion moves the accepted bubble to the existing 状态待确认 state, never leaves it accepted", async () => {
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    await mountFollow(api, hubStore);
    vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
      const commandId = _args[4] as string;
      return Promise.resolve(commandResult(commandId, "accepted"));
    });

    await hubStore.send(INSTANCE, "node never journals");
    // Pump the send + drain chain (POST resolves, empty resync certifies
    // live, debt is recorded).
    for (let i = 0; i < 20; i += 1) await vi.advanceTimersByTimeAsync(0);
    const bubble = () => hubStore.getSnapshot().bubbles[0]!;
    for (let i = 0; i < 20 && bubble().outboxState !== "sent"; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(bubble().outboxState).toBe("sent");
    expect(debtOf(hubStore).has(INSTANCE)).toBe(true);

    // Journal evidence never arrives. Run the whole bounded backoff/deadline
    // window; every tail read returns the same empty window.
    const read = vi.mocked(api.eventsRead);
    const readsBefore = read.mock.calls.length;
    for (let t = 0; t < 40_000; t += 500) {
      await vi.advanceTimersByTimeAsync(500);
      for (let i = 0; i < 5; i += 1) await Promise.resolve();
    }
    expect(read.mock.calls.length).toBeGreaterThan(readsBefore);

    // Exhaustion: the existing not-confirmed state (outbox "unknown" ->
    // 状态待确认), never a permanent accepted bubble, never silently gone.
    expect(bubble().outboxState).toBe("unknown");
    expect(bubble().state).toBe("unknown");
    const { projectCommandStatus } = await import("./commandStatus");
    const row = projectCommandStatus({ hasServerCommandId: true, outboxState: "unknown" });
    expect(row.label).toBe("状态待确认");
    expect(debtOf(hubStore).has(INSTANCE)).toBe(false);

    // The bound is a real stop: no further tail reads after exhaustion.
    const readsAtExhaustion = read.mock.calls.length;
    for (let t = 0; t < 10_000; t += 500) await vi.advanceTimersByTimeAsync(500);
    expect(read.mock.calls.length).toBe(readsAtExhaustion);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("a debt read BELOW the live cursor folds the missed command entry and settles the bubble", async () => {
  // The e2e shape: the same turn's assistant frames advance the journal
  // client's applied cursor past the missed user entry, and a normal
  // resumeAfterReconnect read (generation-gated, cursor-anchored) can never
  // fold it. The debt tail reads a window BELOW the cursor and folds by
  // command id.
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    const commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });

  await hubStore.send(INSTANCE, "missed below cursor");
  const bubble = () => hubStore.getSnapshot().bubbles[0]!;
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));
  const commandId = bubble().commandId!;

  // The turn continues live: an assistant frame at seq 1 advances the
  // client cursor (it is not the owed user message).
  const onBatch = await liveBatch(api);
  onBatch({
    subscriptionId: "sub_outbox_debt",
    journalId: JOURNAL,
    fromSeq: "1",
    toSeq: "1",
    durableSeq: "1",
    events: [
      {
        kind: "message",
        eventId: "evt_assistant_1",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "1",
        completeness: "structured",
        payload: { role: "assistant", text: "working" },
      } as unknown as Observation,
    ],
  });

  // From now on the durable read sees the missed user entry; the debt read
  // starts BELOW the applied cursor (afterSeq 0), so it still finds it.
  vi.mocked(api.eventsRead).mockResolvedValue(userMessagePage(commandId));

  await vi.waitFor(
    () => {
      const b = hubStore.getSnapshot().bubbles.find((x) => x.commandId === commandId)!;
      expect(b.state).toBe("settled");
      expect(b.outboxState).toBe("done");
    },
    { timeout: 5_000 },
  );
  const folded = hubStore
    .getSnapshot()
    .events[INSTANCE]?.some((e) => (e.payload as { commandId?: string }).commandId === commandId);
  expect(folded).toBe(true);
  expect(debtOf(hubStore).has(INSTANCE)).toBe(false);
});
