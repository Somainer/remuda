import { afterEach, expect, it, vi } from "vitest";
import { LEASE_TTL_MS, OUTBOX_LS_KEY } from "./outbox";
import type { CommandResult } from "../types/command";
import type { Observation } from "../types/observation";

/** Well under the 30 s delivery lease: a post must not wait for lease expiry. */
const LEASE_GUARD_MS = LEASE_TTL_MS - 5_000;

const INSTANCE = "ins_local_bubble";
const JOURNAL = "obj_local_bubble_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

function sendSpyCallIds(api: Api): (string | undefined)[] {
  return vi.mocked(api.instanceSend).mock.calls.map((c) => c[4]);
}

function commandResult(commandId: string, state: CommandResult["command"]["state"]): CommandResult {
  return {
    relatedCommandIds: [],
    command: {
      commandId,
      id: commandId,
      revision: "1",
      createdAt: "2026-09-15T00:00:00.000Z",
      updatedAt: "2026-09-15T00:00:00.000Z",
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

/** Fresh store/api module pair per test — the store is a process singleton. */
const liveStores: Store[] = [];
async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  liveStores.push(storeModule.hubStore);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

function stubPostSend(api: Api) {
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_local_bubble",
    journalId: JOURNAL,
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
        getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_local_bubble",
      asOfSeq: "0",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "0", complete: true },
    },
  });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
}

afterEach(() => {
  vi.restoreAllMocks();
  // Tear down EVERY store this file booted: with real timers a previous
  // test's jittered outbox/held retry (or a bounded recovery wake) can fire
  // AFTER the next test enqueued — it would read the same localStorage
  // backend (the recovery owner id lives in sessionStorage) and deliver the
  // new test's row through a stale, un-mocked api. logout disposes the
  // outbox and clears every delivery/retry timer.
  for (const store of liveStores.splice(0)) store.logout();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

it("a fresh bubble carries a client cmd_ id from creation and projects 等待发送", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "instanceSend").mockReturnValue(new Promise(() => {}));
  await hubStore.send(INSTANCE, "queued prompt");
  await vi.waitFor(() => expect(hubStore.getSnapshot().bubbles[0]).toBeTruthy());
  const bubble = hubStore.getSnapshot().bubbles[0];
  expect(bubble.clientRequestId.startsWith("local_")).toBe(true);
  // D-055: the wire commandId exists before the POST (same-id retry).
  expect(bubble.commandId?.startsWith("cmd_")).toBe(true);
  expect(bubble.state).toBe("queued");
  const { projectCommandStatus } = await import("./commandStatus");
  // Before the first POST resolves the row is an ordinary live queued send.
  const row = projectCommandStatus({
    hasServerCommandId: true,
    localState: "queued",
    outboxState: "pending",
    offline: false,
  });
  expect(row.label).toBe("等待发送");
  // While offline the same pending row reads as 待发送（离线）.
  const offlineRow = projectCommandStatus({
    hasServerCommandId: true,
    localState: "queued",
    outboxState: "pending",
    offline: true,
  });
  expect(offlineRow.label).toBe("待发送（离线）");
});

it("every POST attempt uses the same client-generated commandId", async () => {
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  const sendSpy = vi.spyOn(api, "instanceSend").mockResolvedValue(commandResult("cmd_server_1", "accepted"));

  await hubStore.send(INSTANCE, "accepted prompt");
  const bubble = () => hubStore.getSnapshot().bubbles[0];
  // An accepted POST makes the row "sent" (delivered, awaiting the journal
  // join) — the bubble reads accepted, not settled and not 状态待确认
  // (D-055 item 13); it settles only on the matching journal event.
  await vi.waitFor(() => expect(bubble().outboxState).toBe("sent"));
  expect(bubble().state).toBe("accepted");
  // Local render id and wire commandId stay distinct, and the wire id is the
  // client-generated one (server echoes dedup, never assigns a new one).
  expect(bubble().commandId?.startsWith("cmd_")).toBe(true);
  expect(bubble().clientRequestId).not.toBe(bubble().commandId);
  expect(sendSpy.mock.calls[0]?.[4]).toBe(bubble().commandId);
});

it("a failed POST keeps the row pending under the same commandId and retries it", async () => {
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  const sendSpy = vi
    .spyOn(api, "instanceSend")
    .mockRejectedValueOnce(new Error("HTTP 500"))
    .mockResolvedValueOnce(commandResult("cmd_retry", "accepted"));

  await hubStore.send(INSTANCE, "failed prompt");
  const first = hubStore.getSnapshot().bubbles[0];
  const wireId = first.commandId;
  expect(wireId?.startsWith("cmd_")).toBe(true);
  // 5xx is a retriable network/server failure: the row stays queued under the
  // SAME id and the bounded live-retry re-POSTs that id (exactly-once).
  await vi.waitFor(() => expect(sendSpy).toHaveBeenCalledTimes(2), { timeout: 5_000 });
  expect(sendSpy.mock.calls[0]?.[4]).toBe(wireId);
  expect(sendSpy.mock.calls[1]?.[4]).toBe(wireId);
  // After the successful retry the row is delivered (sent), never unknown.
  await vi.waitFor(() =>
    expect(hubStore.getSnapshot().bubbles[0]?.outboxState).toBe("sent"),
  );
});

it("concurrent sends get distinct clientRequestIds before either response lands", async () => {
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  let resolveFirst: (value: CommandResult) => void = () => {};
  const held = new Promise<CommandResult>((resolve) => {
    resolveFirst = resolve;
  });
  vi.spyOn(api, "instanceSend").mockReturnValueOnce(held).mockResolvedValueOnce(commandResult("cmd_second", "accepted"));

  await hubStore.send(INSTANCE, "first");
  await hubStore.send(INSTANCE, "second");
  const bubbles = () => hubStore.getSnapshot().bubbles;
  await vi.waitFor(() => expect(bubbles()).toHaveLength(2));
  const localIds = () => bubbles().map((b) => b.clientRequestId);
  expect(new Set(localIds()).size).toBe(2);
  const wireIds = () => bubbles().map((b) => b.commandId);
  expect(new Set(wireIds()).size).toBe(2);
  expect(wireIds().every((id) => id?.startsWith("cmd_"))).toBe(true);
  resolveFirst(commandResult(wireIds()[0]!, "accepted"));
  await vi.waitFor(() => {
    expect(bubbles().length).toBe(2);
    // Both POSTs landed → "sent" (accepted, awaiting journal), distinct ids.
    expect(bubbles().every((b) => b.outboxState === "sent")).toBe(true);
  });
  // Each POST used its own generated id (order follows the serial flush).
  expect(sendSpyCallIds(api).sort()).toEqual(wireIds().sort());
});

it("settles by the journal observation carrying the same commandId, even with identical text", async () => {
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  // `catchup` short-circuits when the instance is absent from state; preload
  // one so the send path actually drives the journal client, then follow to
  // open the subscription.
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "1", durableSeq: "0" } as Awaited<ReturnType<Api["instanceList"]>>["items"][number]],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([] as Awaited<ReturnType<Api["interactionList"]>>);
  await hubStore.refresh();
  await hubStore.follow(INSTANCE);
  // queued (host offline): the outbox row stays pending and only the journal
  // observation settles the bubble.
  vi.spyOn(api, "instanceSend").mockImplementation(async (_id, _p, _a, _m, commandId) =>
    commandResult(commandId ?? "cmd_twin", "queued"),
  );

  await hubStore.send(INSTANCE, "twin prompt");
  const bubble = () => hubStore.getSnapshot().bubbles[0];
  expect(bubble().state).toBe("queued");
  const wireId = bubble().commandId;

  // Simulate the live journal batch: a user node with the delivering
  // commandId — the shape the Node now emits for hook+transcript joins.
  const subscribe = vi.mocked(api.eventsSubscribe);
  await vi.waitFor(() => expect(subscribe).toHaveBeenCalledTimes(1));
  const onBatch = subscribe.mock.calls[0]?.[2] as ((batch: Record<string, unknown>) => void) | undefined;
  expect(typeof onBatch).toBe("function");
  const batch: Record<string, unknown> = {
    subscriptionId: "sub_local_bubble",
    journalId: JOURNAL,
    fromSeq: "1",
    toSeq: "1",
    durableSeq: "1",
    events: [
      {
        kind: "message",
        eventId: "evt_user_twin",
        journalId: JOURNAL,
        instanceId: INSTANCE,
        seq: "1",
        completeness: "structured",
        source: {
          driverKind: "claude-pty",
          driverVersion: "fake",
          adapterVersion: "test",
          channel: "transcript",
          delivery: "live",
          nativeSessionId: { state: "unknown", reason: "not-emitted" },
          nativeTurnId: { state: "unknown", reason: "not-emitted" },
          nativeAgentId: { state: "not-applicable" },
          nativeItemId: { state: "unknown", reason: "not-emitted" },
          nativeEventId: { state: "unknown", reason: "not-emitted" },
          nativeRequestId: "none",
          sourceCursor: { type: "runtime", ledgerRevision: "1" },
        },
        payload: {
          nodeId: "obj_native_twin",
          messageId: "obj_native_twin",
          revision: "3",
          baseRevision: "2",
          operation: "replace",
          role: "user",
          phase: "input",
          blocks: [{ type: "text", text: "twin prompt" }],
          targetBlock: null,
          parentToolCallId: null,
          nativeOrigin: { state: "known", value: "ui" },
          origin: "human",
          commandId: wireId,
          status: "complete",
        },
      } as unknown as Observation,
    ],
  };
  onBatch!(batch);
  await vi.waitFor(() => expect(bubble().state).toBe("settled"));

  // A repeated delivery (reconnect backfill) is deduplicated by event id and
  // never revives or duplicates the bubble.
  const count = hubStore.getSnapshot().bubbles.length;
  onBatch!({ ...batch, events: [] });
  expect(hubStore.getSnapshot().bubbles).toHaveLength(count);
  expect(hubStore.getSnapshot().bubbles.every((b) => b.state === "settled")).toBe(true);
});

it("send returns as soon as the POST lands, without awaiting catchup's HTTP chain", async () => {
  // Gate flake (hub-live "composer queue / steer"): the 插队 receipt is raised
  // when send() resolves; awaiting the journal backfill + list refresh + screen
  // read inside that promise made the receipt wait on a reconciliation chain
  // that under gate load took longer than the UI's 5 s, even though the POST
  // (the interrupt landing) had already returned. Reconciliation now runs in
  // the background, so callers resolve on the POST alone.
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  await hubStore.follow(INSTANCE);

  vi.spyOn(api, "instanceSend").mockImplementation(async (_id, _p, _a, _m, commandId) =>
    commandResult(commandId ?? "cmd_fast", "accepted"),
  );
  // Every reconciliation read now hangs: the background chain in the new code
  // simply stays open and cannot delay the send resolution.
  vi.spyOn(api, "eventsRead").mockReturnValue(new Promise(() => {}));
  vi.spyOn(api, "instanceList").mockReturnValue(new Promise(() => {}));
  vi.spyOn(api, "interactionList").mockReturnValue(new Promise(() => {}));

  let landed: boolean | undefined;
  void hubStore.send(INSTANCE, "steer prompt").then((value) => {
    landed = value;
  });
  await vi.waitFor(() => expect(landed).toBe(true), { timeout: 2_000 });
  // The client-generated commandId is known before the POST returns.
  expect(hubStore.getSnapshot().bubbles[0]?.commandId?.startsWith("cmd_")).toBe(true);
  await vi.waitFor(() => expect(hubStore.getSnapshot().bubbles[0]?.outboxState).toBe("sent"));
  expect(hubStore.getSnapshot().bubbles[0]?.state).toBe("accepted");
});

it("create returns the instance without awaiting the post-create list refresh", async () => {
  // Gate flake (hub-live "native PTY default"): NewSessionPage navigates only
  // after create() resolves; the awaited two-GET list refresh after the POST
  // kept the sheet on /sessions/new under gate load even though the create had
  // landed. The create response already carries the instance the /s/:id route
  // mounts, so the refresh is fire-and-forget.
  const { api, hubStore } = await fresh();
  const instance = {
    id: INSTANCE,
    journalId: JOURNAL,
    hostId: "hst_1",
    kind: "claude",
    driver: "claude-pty",
    lifecycle: "running",
  } as Awaited<ReturnType<Api["instanceCreate"]>>["instance"];
  vi.spyOn(api, "instanceCreate").mockResolvedValue({
    instance,
    command: commandResult("cmd_create", "accepted").command,
  });
  vi.spyOn(api, "instanceList").mockReturnValue(new Promise(() => {}));
  vi.spyOn(api, "interactionList").mockReturnValue(new Promise(() => {}));

  let created: typeof instance | undefined;
  void hubStore
    .create({
      hostId: "hst_1",
      kind: "claude",
      driver: "claude-pty",
      model: "e2e/auto",
      permissionMode: "default",
      prompt: "p",
    })
    .then((value) => {
      created = value;
    });
  await vi.waitFor(() => expect(created?.id).toBe(INSTANCE), { timeout: 2_000 });
});

it("an aborted inflight-state claim never POSTs; recovered storage delivers the same commandId", async () => {
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  const sendSpy = vi
    .spyOn(api, "instanceSend")
    .mockImplementation(
      ((_iid: string, _p: string, _r?: unknown[], _m?: string, commandId?: string) =>
        Promise.resolve(commandResult(commandId ?? "cmd_x", "accepted"))) as Api["instanceSend"],
    );
  hubStore.setConnectionStateForTest("live");

  // Storage accepts the durable enqueue but ABORTS the inflight-claim write for
  // a command row (the lease row uses the __lock__ key and must still commit).
  const realSetItem = Storage.prototype.setItem;
  const setSpy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, key: string, value: string) {
    if (/"state":"inflight"[^{]*"commandId":"cmd_/.test(value)) {
      throw new DOMException("simulated IndexedDB transaction abort", "QuotaExceededError");
    }
    return realSetItem.call(this, key, value);
  });

  await hubStore.send(INSTANCE, "abort then recover");
  const commandId = (await vi.waitFor(() => {
    const b = hubStore.getSnapshot().bubbles.find((x) => x.text === "abort then recover");
    if (!b?.commandId) throw new Error("bubble not persisted yet");
    return b.commandId;
  })) as string;

  // The aborted flush settles without a POST; the row stays deliverable.
  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  expect(sendSpy).not.toHaveBeenCalled();

  // Storage recovers: the SAME intent delivers under the SAME commandId.
  setSpy.mockRestore();
  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  await vi.waitFor(() => expect(sendSpy).toHaveBeenCalledTimes(1));
  expect(sendSpy.mock.calls[0]?.[4]).toBe(commandId);
});

/**
 * GATE22 item 1 — coordinator ruling: a durable mutation that has COMMITTED
 * (put / mergeUnlessDone / delete succeeded) is a success, full stop. A
 * failure of the follow-up authoritative read never turns it into a failure;
 * it only starts the bounded, lifetime-aware read recovery. Both regressions
 * inject faults ONLY into reads after the commit, through the real storage
 * adapter (never hand-editing storage), and let the store's real delivery
 * flow do everything else.
 */
type FaultableOutboxStorage = {
  all: () => Promise<unknown>;
  put: (rec: { commandId?: string }) => Promise<void>;
  mergeUnlessDone: (id: string, patch: Partial<Record<string, unknown>>) => Promise<unknown>;
};

async function bootLiveWithFaultableStorage(api: Api, hubStore: Store) {
  const sendSpy = vi.spyOn(api, "instanceSend").mockImplementation(
    (async (_iid: string, _p: string, _a?: unknown[], _m?: string, commandId?: string) =>
      commandResult(commandId ?? "cmd_x", "accepted")) as Api["instanceSend"],
  );
  hubStore.setConnectionStateForTest("live");
  const internals = hubStore as unknown as {
    ensureOutbox: () => Promise<boolean>;
    flushAllOutbox: () => Promise<void>;
    outbox: { storage: FaultableOutboxStorage } | null;
  };
  expect(await internals.ensureOutbox()).toBe(true);
  // Settle the empty boot flush BEFORE faults are armed so its reads cannot
  // consume the one-shot faults below.
  await internals.flushAllOutbox();
  const storage = internals.outbox!.storage as FaultableOutboxStorage;
  return { sendSpy, storage };
}

it("GATE22 item 1a: a committed enqueue whose follow-up read fails still yields exactly one intent and one command id, delivered once", async () => {
  // Pre-fix enqueue awaited the post-commit read and REJECTED when it failed,
  // although storage.put had committed; store.send retried the whole enqueue
  // once and then returned false BEFORE rendering the bubble — one committed
  // row with zero visible intent and no delivery trigger. The ruling makes
  // the commit success; the read failure only enters bounded read recovery
  // (250 ms), after which the real guarded flush delivers exactly once.
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  const { sendSpy, storage } = await bootLiveWithFaultableStorage(api, hubStore);

  let failReads = 0;
  const realAll = storage.all.bind(storage);
  storage.all = vi.fn(async () => {
    if (failReads > 0) {
      failReads -= 1;
      throw new Error("post-commit read abort");
    }
    return realAll();
  });

  // Two failing reads: pre-fix BOTH enqueue attempts (send retries the whole
  // enqueue once after the first rejection) consume one. Post-fix there is no
  // second enqueue for the same intent.
  failReads = 2;
  const accepted = await hubStore.send(INSTANCE, "gate22 1a");
  expect(accepted).toBe(true);

  await vi.waitFor(() => expect(sendSpy).toHaveBeenCalledTimes(1), { timeout: 5_000 });
  const bubbles = hubStore.getSnapshot().bubbles.filter((b) => b.text === "gate22 1a");
  expect(bubbles).toHaveLength(1);
  const commandId = bubbles[0]?.commandId;
  expect(commandId?.startsWith("cmd_")).toBe(true);
  expect(sendSpy.mock.calls[0]?.[4]).toBe(commandId);

  const mine = (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<{
    prompt: string;
    state: string;
    attempts: number;
  }>).filter((r) => r.prompt === "gate22 1a");
  expect(mine).toHaveLength(1);
  expect(mine[0]?.state).toBe("sent");
  expect(mine[0]?.attempts).toBe(1);

  // Well after every bounded recovery delay: no duplicate enqueue/POST.
  await new Promise((r) => setTimeout(r, 300));
  expect(sendSpy).toHaveBeenCalledTimes(1);
});

it("GATE22 item 1b: a committed claim whose follow-up read fails proceeds to the POST at once — no 30 s lease wait, no burned attempt slot", async () => {
  // Pre-fix the claim's box.patch awaited the post-commit read; its rejection
  // was misclassified as "claim-aborted" even though mergeUnlessDone had
  // COMMITTED the inflight lease. No POST ran; the row kept a live 30 s lease
  // (and attempts already incremented), so delivery waited for lease expiry
  // and the eventual POST spent attempts=2 although the first one never
  // reached the wire. Under the ruling patch returns the committed claim and
  // the POST proceeds immediately; the failed read enters recovery alone.
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  const { sendSpy, storage } = await bootLiveWithFaultableStorage(api, hubStore);

  let failNextRead = 0;
  const realAll = storage.all.bind(storage);
  storage.all = vi.fn(async () => {
    if (failNextRead > 0) {
      failNextRead -= 1;
      throw new Error("post-claim read abort");
    }
    return realAll();
  });
  const realMerge = storage.mergeUnlessDone.bind(storage);
  storage.mergeUnlessDone = vi.fn(async (id, patch) => {
    const stored = await realMerge(id, patch);
    // Arm the fault from the claim's OWN real commit transition (never a
    // hand-written state change).
    if (id.startsWith("cmd_") && patch.state === "inflight") failNextRead = 1;
    return stored;
  });

  const startedAt = Date.now();
  const accepted = await hubStore.send(INSTANCE, "gate22 1b");
  expect(accepted).toBe(true);
  await vi.waitFor(() => expect(sendSpy).toHaveBeenCalledTimes(1), { timeout: 5_000 });
  // Delivered NOW, not after the 30 s lease TTL.
  expect(Date.now() - startedAt).toBeLessThan(LEASE_GUARD_MS);

  const bubbles = hubStore.getSnapshot().bubbles.filter((b) => b.text === "gate22 1b");
  expect(bubbles).toHaveLength(1);
  expect(sendSpy.mock.calls[0]?.[4]).toBe(bubbles[0]?.commandId);
  const mine = (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<{
    prompt: string;
    state: string;
    attempts: number;
  }>).filter((r) => r.prompt === "gate22 1b");
  expect(mine).toHaveLength(1);
  expect(mine[0]?.state).toBe("sent");
  // The claim that POSTed is the only attempt ever counted.
  expect(mine[0]?.attempts).toBe(1);

  await new Promise((r) => setTimeout(r, 300));
  expect(sendSpy).toHaveBeenCalledTimes(1);
});

it("GATE22 item 1a-commit: a put whose promise rejects AFTER it committed still returns send success — one bubble, one command id, delivered once", async () => {
  // The store-side half of the ruling: even if storage.put's PROMISE rejects
  // after the durable row landed (request errors post-commit), send must ask
  // the durable backend once and, finding THIS commandId committed, render
  // the single bubble and let the real flow deliver it. Pre-fix send retried
  // the enqueue, failed again and returned false before any bubble existed.
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  const { sendSpy, storage } = await bootLiveWithFaultableStorage(api, hubStore);

  const realPut = storage.put.bind(storage);
  storage.put = vi.fn(async (rec) => {
    await realPut(rec);
    // The real write committed; the promise fails afterwards.
    if (rec.commandId?.startsWith("cmd_")) throw new Error("post-commit put rejection");
  });

  const accepted = await hubStore.send(INSTANCE, "gate22 1a-commit");
  expect(accepted).toBe(true);

  await vi.waitFor(() => expect(sendSpy).toHaveBeenCalledTimes(1), { timeout: 5_000 });
  const bubbles = hubStore.getSnapshot().bubbles.filter((b) => b.text === "gate22 1a-commit");
  expect(bubbles).toHaveLength(1);
  const commandId = bubbles[0]?.commandId;
  expect(commandId?.startsWith("cmd_")).toBe(true);
  expect(sendSpy.mock.calls[0]?.[4]).toBe(commandId);

  const mine = (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<{
    prompt: string;
    state: string;
  }>).filter((r) => r.prompt === "gate22 1a-commit");
  expect(mine).toHaveLength(1);
  expect(mine[0]?.state).toBe("sent");

  await new Promise((r) => setTimeout(r, 300));
  expect(sendSpy).toHaveBeenCalledTimes(1);
});

it("GATE22 item 1b-commit: a claim whose merge promise rejects after committing THIS owner's lease POSTs immediately — no 30 s wait, no burned attempt", async () => {
  // Store-side claim half of the ruling: mergeUnlessDone's promise rejects
  // AFTER the inflight lease (owner = this tab) and attempts=1 committed.
  // Pre-fix the catch meant claim-aborted: no POST, the live 30 s lease held,
  // and the deadline recovery POSTed with attempts=2 — a burned slot for a
  // POST that never reached the wire. The post-fix catch reads durable once,
  // sees THIS owner's committed lease, treats it as a won claim and POSTs.
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  const { sendSpy, storage } = await bootLiveWithFaultableStorage(api, hubStore);

  const realMerge = storage.mergeUnlessDone.bind(storage);
  storage.mergeUnlessDone = vi.fn(async (id, patch) => {
    const stored = await realMerge(id, patch);
    // Fault armed from the claim's OWN real commit transition — never a
    // hand-written state change. The finish patch (state sent) passes through.
    if (id.startsWith("cmd_") && patch.state === "inflight") {
      throw new Error("post-commit claim rejection");
    }
    return stored;
  });

  const startedAt = Date.now();
  const accepted = await hubStore.send(INSTANCE, "gate22 1b-commit");
  expect(accepted).toBe(true);
  await vi.waitFor(() => expect(sendSpy).toHaveBeenCalledTimes(1), { timeout: 5_000 });
  expect(Date.now() - startedAt).toBeLessThan(LEASE_GUARD_MS);

  const bubbles = hubStore.getSnapshot().bubbles.filter((b) => b.text === "gate22 1b-commit");
  expect(bubbles).toHaveLength(1);
  expect(sendSpy.mock.calls[0]?.[4]).toBe(bubbles[0]?.commandId);
  const mine = (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<{
    prompt: string;
    state: string;
    attempts: number;
  }>).filter((r) => r.prompt === "gate22 1b-commit");
  expect(mine).toHaveLength(1);
  expect(mine[0]?.state).toBe("sent");
  expect(mine[0]?.attempts).toBe(1);

  await new Promise((r) => setTimeout(r, 300));
  expect(sendSpy).toHaveBeenCalledTimes(1);
});
