import { afterEach, expect, it, vi } from "vitest";
import type { CommandResult } from "../types/command";
import type { Observation } from "../types/observation";
import { OUTBOX_LS_KEY, type OutboxRecord } from "./outbox";

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

async function mountFollow(api: Api, hubStore: Store, seedPage: JournalPage = EMPTY_PAGE) {
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  vi.spyOn(api, "eventsRead").mockResolvedValue(seedPage);
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
  vi.unstubAllGlobals();
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

it("a debt read released after logout and a new login folds nothing into the successor store", async () => {
  // Ruling AD r2 item 1: identity/lifetime ownership. A tail read parked
  // across an auth retirement must be dropped WHOLE when it resolves — no
  // journal fold, no outbox transition on the successor identity.
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    const commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });

  // Seed reads (no afterSeq) and the drain's own resume read (1st afterSeq)
  // resolve empty; every LATER afterSeq read (the debt tail) parks until
  // released — simulating the response arriving after logout.
  let afterSeqCalls = 0;
  let heldCommandId = "";
  let releaseHeld: (() => void) = () => undefined;
  const held = new Promise<JournalPage>((resolve) => {
    releaseHeld = () => resolve(userMessagePage(heldCommandId));
  });
  const read = vi.mocked(api.eventsRead);
  read.mockImplementation(((args: { afterSeq?: string }) => {
    if (!args.afterSeq) return Promise.resolve(EMPTY_PAGE);
    afterSeqCalls += 1;
    return afterSeqCalls <= 1 ? Promise.resolve(EMPTY_PAGE) : held;
  }) as Api["eventsRead"]);

  await hubStore.send(INSTANCE, "across an auth retirement");
  const bubble = () => hubStore.getSnapshot().bubbles[0]!;
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));
  const commandId = bubble().commandId!;

  // Debt recorded and its tail read parked (call #2).
  await vi.waitFor(() => expect(afterSeqCalls).toBeGreaterThanOrEqual(2), { timeout: 5_000 });
  expect(debtOf(hubStore).has(INSTANCE)).toBe(true);

  // Logout (epoch bump + outbox disposal), then a NEW login (new box,
  // same successor identity, restored durable row).
  hubStore.logout();
  await (hubStore as unknown as { ensureOutboxForTest: () => Promise<unknown> }).ensureOutboxForTest();
  expect(hubStore.getSnapshot().events[INSTANCE]).toBeUndefined();

  // Release the OLD identity's response carrying the old journal entry.
  heldCommandId = commandId;
  releaseHeld();
  for (let i = 0; i < 50; i += 1) await Promise.resolve();

  // Nothing of the old identity folded into the successor store…
  expect(hubStore.getSnapshot().events[INSTANCE]).toBeUndefined();
  // …and the successor's durable row was never flipped done by the stale fold.
  const successorRow = (
    hubStore as unknown as { outbox: { get: (id: string) => { state: string } | undefined } | null }
  ).outbox?.get(commandId);
  expect(successorRow?.state).not.toBe("done");
});

it("the debt tail makes FORWARD progress through the real adapter with 128 loaded rows and the command at 129", async () => {
  // r2 item 2: afterSeq "0" + limit 128 returns the same oldest 128 rows
  // forever (the real adapter slices to limit, api.ts eventsRead); a command
  // journaled at seq 129 must be reached by a FORWARD read, not by re-reading
  // the overlap window. Drives the REAL api.eventsRead adapter via fetch.
  const { api, hubStore } = await fresh();

  let commandId = "";
  const rows = Array.from({ length: 128 }, (_, i) => ({
    seq: i + 1,
    eventId: `evt_seed_${i + 1}`,
    kind: "raw_tty",
    payload: { text: `filler ${i + 1}` },
  }));
  let appended = false;

  // Emulate the Hub: newest up to 2000 rows in (afterSeq, beforeSeq],
  // ascending order with fromSeq/reachedAfterSeq metadata.
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = new URL(String(input), "http://hub.test");
    const after = Number(url.searchParams.get("afterSeq") ?? "0");
    const beforeRaw = url.searchParams.get("beforeSeq");
    const before = beforeRaw === null ? null : Number(beforeRaw);
    const userRow129 = {
      seq: 129,
      eventId: "evt_user_129",
      kind: "message",
      payload: { role: "user", commandId },
    };
    const visible = appended ? [...rows, userRow129] : rows;
    const page = visible
      .filter((r) => r.seq > after && (before === null || r.seq <= before))
      .sort((a, b) => b.seq - a.seq)
      .slice(-2000)
      .reverse();
    // The drain's empty resume (afterSeq=128) races the node append: the
    // command lands at seq 129 immediately AFTER that empty response.
    if (after >= 128 && !appended && page.length === 0) appended = true;
    const fromSeq = page[0]?.seq ?? null;
    return new Response(
      JSON.stringify({
        instanceId: INSTANCE,
        durableSeq: String(appended ? 129 : 128),
        fromSeq: fromSeq === null ? null : String(fromSeq),
        reachedAfterSeq: fromSeq === null || fromSeq === after + 1,
        events: page,
      }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  });
  vi.stubGlobal("fetch", fetchMock);

  // Everything except eventsRead is still mocked; eventsRead runs for real
  // through rest()/coerceObservationList.
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [
      { id: INSTANCE, journalId: JOURNAL, revision: "1", durableSeq: "128", lifecycle: "running" } as Awaited<
        ReturnType<Api["instanceList"]>
      >["items"][number],
    ],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as Awaited<ReturnType<Api["interactionList"]>>);
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_outbox_debt_realread",
    journalId: JOURNAL,
    durableSeq: "128",
    windowFromSeq: "1",
    reachedAfterSeq: true,
    getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_outbox_debt_realread",
      asOfSeq: "128",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "1", complete: true },
    },
  });
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });

  await hubStore.refresh();
  await hubStore.follow(INSTANCE);
  // Seed folded 128 rows through the REAL adapter.
  await vi.waitFor(() => expect(hubStore.getSnapshot().events[INSTANCE]).toHaveLength(128));

  await hubStore.send(INSTANCE, "the one at 129");
  const bubble = () => hubStore.getSnapshot().bubbles.find((b) => b.commandId === commandId)!;
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));

  // The forward tail read must reach seq 129 and settle the bubble (the old
  // fixed overlap window never got past row 128).
  await vi.waitFor(
    () => {
      expect(bubble().state).toBe("settled");
      expect(bubble().outboxState).toBe("done");
    },
    { timeout: 6_000 },
  );
  expect(debtOf(hubStore).has(INSTANCE)).toBe(false);
});

it("a storage-aborted terminal write keeps the debt, shows 状态待确认 in memory and retries to not-confirmed", async () => {
  // r2 item 3: if the IndexedDB transaction writing "unknown" aborts,
  // deleting the debt/timer would strand the accepted bubble forever. The
  // obligation survives, the in-memory presentation is already
  // not-confirmed, and a bounded storage retry commits it.
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    await mountFollow(api, hubStore);
    vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
      const commandId = _args[4] as string;
      return Promise.resolve(commandResult(commandId, "accepted"));
    });

    await hubStore.send(INSTANCE, "storage aborts once");
    for (let i = 0; i < 30; i += 1) await vi.advanceTimersByTimeAsync(0);
    const bubble = () => hubStore.getSnapshot().bubbles[0]!;
    for (let i = 0; i < 30 && bubble().outboxState !== "sent"; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(bubble().outboxState).toBe("sent");
    const commandId = bubble().commandId!;
    expect(debtOf(hubStore).has(INSTANCE)).toBe(true);

    // The terminal-state ("unknown") IndexedDB transaction aborts ONCE.
    const outbox0 = (
      hubStore as unknown as { outbox: { storage: { mergeUnlessDone: (id: string, p: unknown) => Promise<unknown> } } }
    ).outbox;
    const storage = outbox0.storage;
    const realMerge = storage.mergeUnlessDone.bind(storage);
    let aborted = false;
    storage.mergeUnlessDone = vi.fn(async (id: string, patch: unknown) => {
      if ((patch as { state?: string }).state === "unknown" && !aborted) {
        aborted = true;
        throw new Error("IDB_TRANSACTION_ABORTED");
      }
      return realMerge(id, patch);
    });

    // Run the read deadline (every read answered empty) until the FIRST
    // terminal write aborts at exhaustion.
    for (let t = 0; t < 60_000 && !aborted; t += 100) {
      await vi.advanceTimersByTimeAsync(100);
      for (let i = 0; i < 5; i += 1) await vi.advanceTimersByTimeAsync(0);
    }
    for (let i = 0; i < 30; i += 1) await vi.advanceTimersByTimeAsync(0);

    // The abort happened…
    expect(aborted).toBe(true);
    // …the durable row is STILL "sent" (commit failed)…
    const rowState = () =>
      (hubStore as unknown as { outbox: { get: (id: string) => OutboxRecord | undefined } }).outbox.get(commandId)?.state;
    expect(rowState()).toBe("sent");
    // …but the obligation survived and the bubble already presents
    // not-confirmed (never a permanent accepted).
    expect(debtOf(hubStore).has(INSTANCE)).toBe(true);
    expect(bubble().outboxState).toBe("unknown");
    const { projectCommandStatus } = await import("./commandStatus");
    expect(projectCommandStatus({ hasServerCommandId: true, outboxState: "unknown" }).label).toBe("状态待确认");

    // The bounded storage retry fires and commits "unknown".
    await vi.advanceTimersByTimeAsync(1_100);
    for (let i = 0; i < 20; i += 1) await vi.advanceTimersByTimeAsync(0);
    expect(rowState()).toBe("unknown");
    expect(bubble().outboxState).toBe("unknown");
    expect(debtOf(hubStore).has(INSTANCE)).toBe(false);

    // No further tail work after the committed transition.
    const reads = vi.mocked(api.eventsRead).mock.calls.length;
    for (let t = 0; t < 10_000; t += 500) await vi.advanceTimersByTimeAsync(500);
    expect(vi.mocked(api.eventsRead).mock.calls.length).toBe(reads);

    hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("r3 item 2: a model-B degraded verdict in a debt read is left for the live callback to apply", async () => {
  // AD2: a debt forward read is EVIDENCE-ONLY. A current request's
  // model-degraded verdict riding the same page must NOT be applied (or have
  // its id consumed) by the debt path — only the owed user message settles;
  // the SAME event arriving on the live callback then shows B's refusal.
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    const commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });

  // Optimistic in-flight model request B.
  const h = hubStore as unknown as {
    state: {
      modelPending: Record<string, { id: string; queued: boolean } | undefined>;
      models: Record<string, string | undefined>;
    };
  };
  h.state.models[INSTANCE] = "model-B";
  const noteModel = (
    hubStore as unknown as { noteModelLifecycle: (id: string, ev: Observation) => void }
  ).noteModelLifecycle.bind(hubStore) as (id: string, ev: Observation) => void;
  const configureLifecycle = (seq: number, eventId: string, status: string): Observation =>
    ({
      eventId,
      instanceId: INSTANCE,
      journalId: JOURNAL,
      seq: String(seq),
      kind: "lifecycle",
      observedAt: `2026-10-18T00:0${seq}:00Z`,
      source: { channel: "runtime" },
      payload: {
        type: "native",
        nativeName: "instance.configure",
        nativeId: { state: "not-applicable" },
        status: { state: "known", value: status },
        relatedIds: {},
      },
    }) as unknown as Observation;
  noteModel(INSTANCE, configureLifecycle(1, "evt_cfg_B", "model-queued:model-B"));
  const pending = () => h.state.modelPending[INSTANCE];
  expect(pending()?.id).toBe("model-B");

  const degradedB = configureLifecycle(2, "evt_cfg_B_bad", "model-degraded:model-B:not-found");

  await hubStore.send(INSTANCE, "owed while B pending");
  const bubble = () => hubStore.getSnapshot().bubbles[0]!;
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));
  const commandId = bubble().commandId!;

  // The forward debt read returns BOTH the owed command entry and B's
  // verdict. Only settlement evidence is applied.
  vi.mocked(api.eventsRead).mockResolvedValue({
    events: [
      degradedB,
      {
        kind: "message",
        eventId: "evt_user_owed",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "3",
        completeness: "structured",
        payload: { role: "user", commandId },
      } as unknown as Observation,
    ],
    durableSeq: "3",
    windowFromSeq: "1",
    reachedAfterSeq: true,
  });
  // Drive the bounded debt tail read directly (its earlier drain resync
  // already consumed the empty seed/resync pages).
  (
    hubStore as unknown as { runOwedTailPass: (id: string) => void }
  ).runOwedTailPass(INSTANCE);
  for (let i = 0; i < 50; i += 1) await Promise.resolve();
  // The debt read neither applied nor consumed B's verdict: B stays pending
  // and the optimistic selection is untouched.
  expect(bubble().state).toBe("settled");
  expect(bubble().outboxState).toBe("done");
  expect(pending()?.id).toBe("model-B");
  expect(h.state.models[INSTANCE]).toBe("model-B");

  // Now the SAME verdict arrives through the live callback (contiguously
  // after a seq-1 assistant frame): it applies and shows the refusal —
  // pending cleared, picker reverted.
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
  onBatch({
    subscriptionId: "sub_outbox_debt",
    journalId: JOURNAL,
    fromSeq: "2",
    toSeq: "2",
    durableSeq: "2",
    events: [degradedB],
  });
  expect(pending()).toBeUndefined();
  expect(h.state.models[INSTANCE]).toBeUndefined();
});

it("r3 item 3: an old permission readback for A recovered by a debt read leaves request B pending", async () => {
  // AD2: a debt read ignores every non-settlement event. An older permission
  // readback for request A in the page must never settle request B.
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    const commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });

  const h = hubStore as unknown as {
    state: { permissionPending: Record<string, { mode: string; queued: boolean } | undefined> };
  };
  h.state.permissionPending[INSTANCE] = { mode: "plan", queued: true };

  // A's OLD effective-permission readback (kind "permission"), timestamped
  // after any last loaded effective — the shape notePermissionObservation
  // uses to settle a pending walk.
  const oldPermissionA = {
    eventId: "evt_perm_A_old",
    instanceId: INSTANCE,
    journalId: JOURNAL,
    seq: "1",
    kind: "permission",
    observedAt: "2026-10-18T00:09:00Z",
    source: { channel: "runtime" },
    payload: {
      effective: { mode: "default", source: "remuda", observedAt: "2026-10-18T00:09:00Z" },
    },
  } as unknown as Observation;

  await hubStore.send(INSTANCE, "owed while permission B pending");
  const bubble = () => hubStore.getSnapshot().bubbles[0]!;
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));
  const commandId = bubble().commandId!;

  vi.mocked(api.eventsRead).mockResolvedValue({
    events: [
      oldPermissionA,
      {
        kind: "message",
        eventId: "evt_user_owed_perm",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "2",
        completeness: "structured",
        payload: { role: "user", commandId },
      } as unknown as Observation,
    ],
    durableSeq: "2",
    windowFromSeq: "1",
    reachedAfterSeq: true,
  });
  (
    hubStore as unknown as { runOwedTailPass: (id: string) => void }
  ).runOwedTailPass(INSTANCE);
  for (let i = 0; i < 50; i += 1) await Promise.resolve();
  expect(bubble().outboxState).toBe("done");
  // A's old permission readback never settled B.
  expect(h.state.permissionPending[INSTANCE]?.mode).toBe("plan");
});

it("r3 item 1: the FIRST post-drain resync held across logout and a real successor login applies nothing", async () => {
  // The drain's inline JournalClient.resumeAfterReconnect must carry an
  // ownership predicate. Without it (default always-true), a resync response
  // parked across logout is applied AFTER a real successor login, restoring
  // the retired identity's events and flipping the successor outbox row done.
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null } as Awaited<
    ReturnType<Api["hostList"]>
  >);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["deviceList"]>>);
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["passkeyList"]>>);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "login").mockResolvedValue({
    deviceId: "dev_successor",
    token: "tok_successor",
    name: "successor",
  } as Awaited<ReturnType<Api["login"]>>);
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    const commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });

  // Seed reads (no afterSeq) resolve empty; every RESYNC read (afterSeq set)
  // parks until released — the held FIRST post-drain resync.
  const commandIdRef = { value: "" };
  let releaseHeld: (() => void) | null = null;
  const held = new Promise<JournalPage>((resolve) => {
    releaseHeld = () => resolve(userMessagePage(commandIdRef.value));
  });
  vi.mocked(api.eventsRead).mockImplementation(((args: { afterSeq?: string }) =>
    args.afterSeq ? held : Promise.resolve(EMPTY_PAGE)) as Api["eventsRead"]);

  await hubStore.send(INSTANCE, "across a real re-login");
  const bubble = () => hubStore.getSnapshot().bubbles[0]!;
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));
  commandIdRef.value = bubble().commandId!;
  expect(debtOf(hubStore).has(INSTANCE)).toBe(true);

  // Logout (epoch bump, outbox/machine disposed, events wiped)…
  hubStore.logout();
  expect(hubStore.getSnapshot().events[INSTANCE]).toBeUndefined();
  // …then a REAL public login establishing the successor identity (its
  // bootstrap lists no instances, so it never follows INSTANCE itself).
  vi.mocked(api.instanceList).mockResolvedValue({ items: [], nextCursor: null });
  await expect(hubStore.login("bootstrap", "secret", "dev")).resolves.toBe("settled");
  const successorRow = () =>
    (
      hubStore as unknown as { outbox: { get: (id: string) => OutboxRecord | undefined } | null }
    ).outbox?.get(commandIdRef.value);
  expect(successorRow()?.state).toBe("sent");

  // Release the retired identity's parked resync carrying the old command.
  releaseHeld!();
  for (let i = 0; i < 50; i += 1) await Promise.resolve();

  // Nothing leaked into the successor store…
  expect(hubStore.getSnapshot().events[INSTANCE]).toBeUndefined();
  // …and its durable row was never flipped done by the stale application.
  expect(successorRow()?.state).toBe("sent");
});

it("r3 item 4: a journal-confirmed unknown row is promoted to durable done and stays settled across a reload", async () => {
  // Exhaust -> unknown committed -> command journals later -> a fresh store
  // (offline reload reconstruction) whose mount seed observes it must settle
  // the restored bubble durably, not resurrect it as 状态待确认.
  vi.useFakeTimers();
  try {
    const { api, hubStore } = await fresh();
    await mountFollow(api, hubStore);
    vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
      const commandId = _args[4] as string;
      return Promise.resolve(commandResult(commandId, "accepted"));
    });

    await hubStore.send(INSTANCE, "late confirmation");
    for (let i = 0; i < 30; i += 1) await vi.advanceTimersByTimeAsync(0);
    const bubble = () => hubStore.getSnapshot().bubbles[0]!;
    for (let i = 0; i < 30 && bubble().outboxState !== "sent"; i += 1) {
      await vi.advanceTimersByTimeAsync(0);
    }
    expect(bubble().outboxState).toBe("sent");
    const commandId = bubble().commandId!;

    // Run the read deadline with every read empty -> exhaustion commits unknown.
    for (let t = 0; t < 46_000; t += 250) await vi.advanceTimersByTimeAsync(250);
    const rowOf = (s: Store) =>
      (s as unknown as { outbox: { get: (id: string) => OutboxRecord | undefined } }).outbox?.get(commandId);
    expect(rowOf(hubStore)?.state).toBe("unknown");

    // The command journals later; a FRESH store reconstructs the outbox
    // (localStorage backend survives module reset) and mounts with a seed
    // carrying the confirmation — exactly an offline reload.
    const second = await fresh();
    const secondInternals = second.hubStore as unknown as {
      ensureOutboxForTest: () => Promise<unknown>;
      initConnection: (offline: boolean, epoch: number) => Promise<string>;
      extEpoch: number;
    };
    await secondInternals.ensureOutboxForTest();
    // Real offline-reload reconstruction: installs a machine AND restores the
    // durable rows as optimistic bubbles.
    await secondInternals.initConnection(true, secondInternals.extEpoch);
    await mountFollow(second.api, second.hubStore, {
      events: [
        {
          kind: "message",
          eventId: "evt_late_user",
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
    });
    for (let i = 0; i < 200; i += 1) await vi.advanceTimersByTimeAsync(0);

    const restored = () =>
      second.hubStore.getSnapshot().bubbles.find((b) => b.commandId === commandId)!;
    expect(restored().state).toBe("settled");
    expect(restored().outboxState).toBe("done");
    expect(rowOf(second.hubStore)?.state).toBe("done");
    second.hubStore.logout();
  } finally {
    vi.useRealTimers();
  }
});

it("r4 item 2: a queued stale drain job retired at entry neither recreates debt nor touches the successor row", async () => {
  // Distinct from the held-READ regression: here the drain job is QUEUED on
  // the per-instance chain behind a blocker when logout happens. Chained
  // promises survive logout (only timers are torn down), so the retired drain
  // callback must reject itself at ENTRY before recording debt or settling.
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null } as Awaited<
    ReturnType<Api["hostList"]>
  >);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["deviceList"]>>);
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["passkeyList"]>>);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "login").mockResolvedValue({
    deviceId: "dev_successor_r4",
    token: "tok_successor_r4",
    name: "successor",
  } as Awaited<ReturnType<Api["login"]>>);
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    const commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });
  // Resync reads answer empty immediately — the serialization that parks the
  // drain callback comes from the chain blocker below, not a held request.
  vi.mocked(api.eventsRead).mockResolvedValue(EMPTY_PAGE);

  // Hold the instance's journal chain BEFORE the send so the post-drain job
  // queues but never runs.
  let releaseBlock: (() => void) | null = null;
  (
    hubStore as unknown as { chainReconcile: (id: string, job: () => Promise<unknown>) => Promise<unknown> }
  ).chainReconcile(INSTANCE, () => new Promise<void>((resolve) => (releaseBlock = resolve)));

  await hubStore.send(INSTANCE, "queued behind a blocker");
  const bubble = () => hubStore.getSnapshot().bubbles[0]!;
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));
  const commandId = bubble().commandId!;
  // The drain recorded its debt as part of delivery; clear it to model the
  // retired state the queued job must not resurrect.
  (hubStore as unknown as { reconcileOwed: Map<string, unknown> }).reconcileOwed.delete(INSTANCE);
  expect(debtOf(hubStore).has(INSTANCE)).toBe(false);

  // Retire WHILE the drain callback is still queued, then a real successor
  // login restores the persisted sent row under a new Outbox.
  hubStore.logout();
  vi.mocked(api.instanceList).mockResolvedValue({ items: [], nextCursor: null });
  await expect(hubStore.login("bootstrap", "secret", "dev")).resolves.toBe("settled");
  const successorRow = () =>
    (
      hubStore as unknown as { outbox: { get: (id: string) => OutboxRecord | undefined } | null }
    ).outbox?.get(commandId);
  expect(successorRow()?.state).toBe("sent");

  // Release the blocker: the retired drain callback now enters.
  releaseBlock!();
  for (let i = 0; i < 80; i += 1) await Promise.resolve();

  // It rejected itself at entry: no debt resurrected…
  expect(debtOf(hubStore).has(INSTANCE)).toBe(false);
  // …and the successor's restored row is untouched.
  expect(successorRow()?.state).toBe("sent");
});

it("r4 item 3: the debt fallback trigger applies evidence only and leaves permission request B pending", async () => {
  // The ACTUAL fallback path is runOwedReconcile (resume failure / failed
  // reopen / bind-away / watchdog). On 87712402 it drives the mounted
  // JournalClient.resumeAfterReconnect, whose onEvents runs the permission
  // reducer on an OLD readback for A and clears request B. AD3: it must be a
  // self-contained evidence-only REST read.
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    const commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });

  // Permission request B is live-pending; seed an older effective observation.
  const h = hubStore as unknown as {
    state: {
      permissionPending: Record<string, { mode: string; queued: boolean } | undefined>;
      permissionEffective: Record<string, { mode: string; source: string; observedAt: string }>;
    };
  };
  h.state.permissionEffective[INSTANCE] = { mode: "default", source: "remuda", observedAt: "2026-10-18T00:00:00Z" };
  h.state.permissionPending[INSTANCE] = { mode: "plan", queued: true };

  await hubStore.send(INSTANCE, "owed when permission B pending");
  const bubble = () => hubStore.getSnapshot().bubbles[0]!;
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));
  const commandId = bubble().commandId!;
  expect(debtOf(hubStore).has(INSTANCE)).toBe(true);

  // The fallback's read returns an OLD permission effective readback for A
  // (newer than the seeded effective) AND the owed command entry.
  vi.mocked(api.eventsRead).mockResolvedValue({
    events: [
      {
        eventId: "evt_perm_A_r4",
        instanceId: INSTANCE,
        journalId: JOURNAL,
        seq: "1",
        kind: "permission",
        observedAt: "2026-10-18T00:09:00Z",
        source: { channel: "runtime" },
        payload: { effective: { mode: "default", source: "remuda", observedAt: "2026-10-18T00:09:00Z" } },
      } as unknown as Observation,
      {
        kind: "message",
        eventId: "evt_user_fallback",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "2",
        completeness: "structured",
        payload: { role: "user", commandId },
      } as unknown as Observation,
    ],
    durableSeq: "2",
    windowFromSeq: "1",
    reachedAfterSeq: true,
  });

  // Fire the actual fallback handler.
  (hubStore as unknown as { runOwedReconcile: (id: string) => void }).runOwedReconcile(INSTANCE);
  for (let i = 0; i < 80; i += 1) await Promise.resolve();

  // Owed command settled durably…
  expect(bubble().outboxState).toBe("done");
  // …but A's old permission readback never settled B.
  expect(h.state.permissionPending[INSTANCE]?.mode).toBe("plan");
  // And the page never entered the live journal-client state.
  const evs = hubStore.getSnapshot().events[INSTANCE] ?? [];
  expect(evs.some((e) => e.kind === "permission" && e.eventId === "evt_perm_A_r4")).toBe(false);
});

it("r4 item 4: the debt scan cursor advances through a full unrelated page to the owed command at 257", async () => {
  // AD3 item 4: filtering a full unrelated page must not stall the scan. A
  // separate debt cursor advances through every inspected page. Real API
  // adapter over a fetch stub: frontier 128, unrelated 129-256, owed at 257.
  const { api, hubStore } = await fresh();

  const sentCommandIdRef = { value: "" };
  const seedRows = Array.from({ length: 128 }, (_, i) => ({
    seq: i + 1,
    eventId: `evt_seed_${i + 1}`,
    kind: "raw_tty",
    payload: { text: `filler ${i + 1}` },
  }));
  const unrelatedRows = Array.from({ length: 128 }, (_, i) => ({
    seq: i + 129,
    eventId: `evt_noise_${i + 129}`,
    kind: "raw_tty",
    payload: { text: `unrelated ${i + 129}` },
  }));
  const all = [...seedRows, ...unrelatedRows, {
    seq: 257,
    eventId: "evt_user_257",
    kind: "message",
    payload: { role: "user" },
  }];

  // The unrelated 129-256 burst and the owed 257 only become durable AFTER the
  // drain's empty resync (the command journals late, like the real race).
  let appended = false;
  // Hub window semantics: newest 2000 in (afterSeq, beforeSeq], ascending.
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = new URL(String(input), "http://hub.test");
      const after = Number(url.searchParams.get("afterSeq") ?? "0");
      const beforeRaw = url.searchParams.get("beforeSeq");
      const before = beforeRaw === null ? null : Number(beforeRaw);
      const visible = appended ? all : seedRows;
      const resolved = visible.map((r) =>
        r.seq === 257
          ? { ...r, payload: { role: "user", commandId: sentCommandIdRef.value } }
          : r,
      );
      const page = resolved
        .filter((r) => r.seq > after && (before === null || r.seq <= before))
        .sort((a, b) => b.seq - a.seq)
        .slice(-2000)
        .reverse();
      if (after >= 128 && !appended && page.length === 0) appended = true;
      const fromSeq = page[0]?.seq ?? null;
      return new Response(
        JSON.stringify({
          instanceId: INSTANCE,
          durableSeq: appended ? "257" : "128",
          fromSeq: fromSeq === null ? null : String(fromSeq),
          reachedAfterSeq: fromSeq === null || fromSeq === after + 1,
          events: page,
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }),
  );

  vi.spyOn(api, "instanceGet").mockResolvedValue({ id: INSTANCE, journalId: JOURNAL } as never);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "1", durableSeq: "128", lifecycle: "running" } as never],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as never);
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_debt_257",
    journalId: JOURNAL,
    durableSeq: "128",
    windowFromSeq: "1",
    reachedAfterSeq: true,
    getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_debt_257",
      asOfSeq: "128",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "1", complete: true },
    },
  });
  let sentCommandId = "";
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    sentCommandId = _args[4] as string;
    sentCommandIdRef.value = sentCommandId;
    return Promise.resolve(commandResult(sentCommandId, "accepted"));
  });
  await hubStore.refresh();
  await hubStore.follow(INSTANCE);
  await vi.waitFor(() => expect(hubStore.getSnapshot().events[INSTANCE]).toHaveLength(128));
  await hubStore.send(INSTANCE, "owed at 257");
  const bubble = () => hubStore.getSnapshot().bubbles.find((b) => b.commandId === sentCommandId)!;
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));

  await vi.waitFor(
    () => {
      expect(bubble().state).toBe("settled");
      expect(bubble().outboxState).toBe("done");
    },
    { timeout: 6_000 },
);
  // Only the owed node entered store state — the unrelated 128 did not.
  expect(hubStore.getSnapshot().events[INSTANCE]).toHaveLength(129);
});

it("r4 item 2: a queued tail pass whose debt generation was cleared still lets the NEXT debt's tail run", async () => {
  // Liveness of the entry gate. A tail pass is queued behind a chain blocker
  // for debt D1; live evidence clears D1 while it waits and a second drained
  // send records a NEW debt D2 behind it. The stale pass must reject itself
  // at entry WITHOUT keeping the per-instance running marker, or every later
  // D2 tail pass is coalesced away and D2's row stays accepted forever.
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    const commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });
  const h = hubStore as unknown as {
    chainReconcile: (id: string, job: () => Promise<unknown>) => Promise<unknown>;
    runOwedTailPass: (id: string) => void;
    owedTail: Map<string, { timer: unknown }>;
  };

  await hubStore.send(INSTANCE, "first owed");
  const bubbleOf = (text: string) => hubStore.getSnapshot().bubbles.find((b) => b.text === text)!;
  await vi.waitFor(() => expect(bubbleOf("first owed").outboxState).toBe("sent"));
  await vi.waitFor(() => expect(debtOf(hubStore).has(INSTANCE)).toBe(true));
  const firstId = bubbleOf("first owed").commandId!;
  const firstDebt = debtOf(hubStore).get(INSTANCE);

  // Park the journal chain, then queue a D1 tail pass behind it.
  let releaseBlock: (() => void) | null = null;
  void h.chainReconcile(INSTANCE, () => new Promise<void>((resolve) => (releaseBlock = resolve)));
  h.runOwedTailPass(INSTANCE);

  // Live evidence for the first command clears D1 while the pass waits.
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
        eventId: "evt_user_first_live",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "1",
        completeness: "structured",
        payload: { role: "user", commandId: firstId },
      } as unknown as Observation,
    ],
  });
  await vi.waitFor(() => expect(bubbleOf("first owed").outboxState).toBe("done"));
  expect(debtOf(hubStore).has(INSTANCE)).toBe(false);

  // A second drained send queues its post-drain job behind the stale pass.
  await hubStore.send(INSTANCE, "second owed");
  await vi.waitFor(() => expect(bubbleOf("second owed").outboxState).toBe("sent"));
  const secondId = bubbleOf("second owed").commandId!;

  // Release: the stale pass enters and rejects; the drain records D2 and its
  // empty resync arms D2's tail timer.
  releaseBlock!();
  await vi.waitFor(() => {
    const debt = debtOf(hubStore).get(INSTANCE);
    expect(debt).toBeDefined();
    expect(debt).not.toBe(firstDebt);
    expect(h.owedTail.get(INSTANCE)?.timer).not.toBeNull();
  });

  // The Node's journal write for the second command is now durable; only a
  // D2 tail pass can read it (the live socket missed it).
  vi.mocked(api.eventsRead).mockResolvedValue({
    events: [
      {
        kind: "message",
        eventId: "evt_user_second_tail",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "2",
        completeness: "structured",
        payload: { role: "user", commandId: secondId },
      } as unknown as Observation,
    ],
    durableSeq: "2",
    windowFromSeq: "2",
    reachedAfterSeq: true,
  });
  await vi.waitFor(
    () => {
      expect(bubbleOf("second owed").state).toBe("settled");
      expect(bubbleOf("second owed").outboxState).toBe("done");
    },
    { timeout: 4_000 },
  );
  expect(debtOf(hubStore).has(INSTANCE)).toBe(false);
}, 15_000);

it("r4 item 2: a queued stale drain then a queued past-deadline tail pass never terminalise the successor's rows", async () => {
  // The review's exact trigger: the old identity owes a command, a pending
  // journal job holds the chain, a second accepted drain queues behind it,
  // then the tail pass queues. Logout + a public successor login restore the
  // persisted sent rows under a new Outbox. Releasing the chain after the
  // captured tail's deadline must neither recreate the debt (retired drain)
  // nor patch the successor's rows to unknown (retired exhaustion).
  const { api, hubStore } = await fresh();
  await mountFollow(api, hubStore);
  vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null } as Awaited<
    ReturnType<Api["hostList"]>
  >);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["deviceList"]>>);
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["passkeyList"]>>);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "login").mockResolvedValue({
    deviceId: "dev_successor_r4_tail",
    token: "tok_successor_r4_tail",
    name: "successor",
  } as Awaited<ReturnType<Api["login"]>>);
  vi.spyOn(api, "instanceSend").mockImplementation((..._args) => {
    const commandId = _args[4] as string;
    return Promise.resolve(commandResult(commandId, "accepted"));
  });
  const h = hubStore as unknown as {
    chainReconcile: (id: string, job: () => Promise<unknown>) => Promise<unknown>;
    reconcileChain: Map<string, Promise<unknown>>;
    runOwedTailPass: (id: string) => void;
    owedTail: Map<string, { timer: ReturnType<typeof setTimeout> | null; firstAt: number }>;
    outbox: { get: (id: string) => OutboxRecord | undefined } | null;
  };

  await hubStore.send(INSTANCE, "old identity owed");
  const bubbleOf = (text: string) => hubStore.getSnapshot().bubbles.find((b) => b.text === text)!;
  await vi.waitFor(() => expect(bubbleOf("old identity owed").outboxState).toBe("sent"));
  await vi.waitFor(() => expect(h.owedTail.get(INSTANCE)?.timer).not.toBeNull());
  const firstId = bubbleOf("old identity owed").commandId!;
  // Take over the tail timer so the pass is queued exactly once, in order.
  const tail = h.owedTail.get(INSTANCE)!;
  clearTimeout(tail.timer!);
  tail.timer = null;

  // A pending journal job holds the chain…
  let releaseBlock: (() => void) | null = null;
  void h.chainReconcile(INSTANCE, () => new Promise<void>((resolve) => (releaseBlock = resolve)));
  const blocker = h.reconcileChain.get(INSTANCE);
  // …another accepted drain queues behind it…
  await hubStore.send(INSTANCE, "second accepted drain");
  await vi.waitFor(() => expect(bubbleOf("second accepted drain").outboxState).toBe("sent"));
  await vi.waitFor(() => expect(h.reconcileChain.get(INSTANCE)).not.toBe(blocker));
  const secondId = bubbleOf("second accepted drain").commandId!;
  // …followed by the tail timer's job, whose captured deadline has passed.
  h.runOwedTailPass(INSTANCE);
  tail.firstAt = 0;

  hubStore.logout();
  vi.mocked(api.instanceList).mockResolvedValue({ items: [], nextCursor: null });
  await expect(hubStore.login("bootstrap", "secret", "dev")).resolves.toBe("settled");
  expect(h.outbox?.get(firstId)?.state).toBe("sent");
  expect(h.outbox?.get(secondId)?.state).toBe("sent");

  releaseBlock!();
  for (let i = 0; i < 120; i += 1) await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 50));

  // Neither retired job wrote anything into the successor identity.
  expect(debtOf(hubStore).has(INSTANCE)).toBe(false);
  expect(h.outbox?.get(firstId)?.state).toBe("sent");
  expect(h.outbox?.get(secondId)?.state).toBe("sent");
});
