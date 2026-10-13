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
  expect(await internals.ensureOutbox()).toBe("ok");
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

/**
 * Pump microtasks ONLY (fake timers stay parked): resolve once `fn` returns a
 * non-nullish value. Lets real delivery flows progress through their storage
 * /lock microturns without firing any setTimeout wake.
 */
async function pump<T>(fn: () => T | null | undefined, tries = 1_000): Promise<T> {
  for (let i = 0; i < tries; i += 1) {
    const v = fn();
    if (v != null) return v;
    await Promise.resolve();
  }
  throw new Error("pump timed out");
}

/**
 * GATE23 item 1 — claim identity ruling. A rejected claim promise counts as a
 * committed claim ONLY when a FRESH AUTHORITATIVE durable read shows a row
 * carrying THIS attempt's unique claim token (persisted with the lease). An
 * older lease of the SAME owner is not proof: a prior delivery whose finishing
 * write aborted leaves exactly such a stale lease behind.
 */

/** Outbox test surface for the claim-identity regressions. */
type ClaimIdentityOutbox = {
  ownerId: string;
  pendingLeaseWakeups: () => string[];
  isDeliveryExhausted: (instanceId: string) => boolean;
};

it("GATE23 item 1a: an older same-owner lease is not a committed claim — a genuinely aborted next claim POSTs nothing on the stale token", async () => {
  // Sequence (every transition a REAL flow): P's first claim commits with
  // token T1 and the POST is accepted, but the finishing (sent) write aborts
  // BEFORE commit, so P durably stays inflight under the same owner's live T1
  // lease. After the lease deadline the real timer flush writes a NEW claim
  // (token T2, attempts 2) whose transaction genuinely aborts BEFORE commit,
  // while every READ succeeds. Pre-fix the catch's authoritative read saw the
  // row inflight under the SAME owner and treated the stale T1 lease as a won
  // claim: an immediate second POST on a lease this attempt never owned.
  // Post-fix the fresh read carries T1, not T2: claim-aborted, no POST; the
  // bounded wake retries and, once real commits succeed, delivers under T3.
  vi.useFakeTimers();
  vi.setSystemTime(700_000);
  try {
    const { api, hubStore } = await fresh();
    stubPostSend(api);
    const { sendSpy, storage } = await bootLiveWithFaultableStorage(api, hubStore);

    let finishAbort = true;
    let abortNewClaims = false;
    const realMerge = storage.mergeUnlessDone.bind(storage);
    storage.mergeUnlessDone = vi.fn(async (id, patch) => {
      const p = patch as { state?: string; lease?: { claimToken?: string } };
      if (String(id).startsWith("cmd_") && p.state === "sent" && finishAbort) {
        finishAbort = false;
        throw new Error("g23 1a finishing write aborted before commit");
      }
      if (p.state === "inflight" && abortNewClaims) {
        throw new Error("g23 1a claim genuinely aborted before commit");
      }
      return await realMerge(id, patch);
    });

    // 1) Real delivery: claim T1 commits, POST accepted, finishing write
    //    aborts once — the row stays inflight under the live T1 lease.
    void hubStore.send(INSTANCE, "gate23 1a");
    const pId = await pump(() => {
      const b = hubStore.getSnapshot().bubbles.find((x) => x.text === "gate23 1a");
      return b?.commandId && sendSpy.mock.calls.some((c) => c[4] === b.commandId)
        ? (b.commandId as string)
        : null;
    });
    const stored = () =>
      (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<{
        commandId: string;
        state: string;
        attempts: number;
        lease?: { owner: string; until: number; claimToken?: string };
      }>).find((r) => r.commandId === pId)!;
    await pump(() => (stored()?.state === "inflight" ? true : null));
    const box = (
      hubStore as unknown as { outbox: ClaimIdentityOutbox | null }
    ).outbox!;
    expect(sendSpy).toHaveBeenCalledTimes(1);
    expect(stored().state).toBe("inflight");
    expect(stored().attempts).toBe(1);
    expect(stored().lease?.owner).toBe(box.ownerId);
    const t1 = stored().lease?.claimToken;
    expect(stored().lease!.until - Date.now()).toBeGreaterThan(20_000);

    // 2) From now on every NEW claim aborts before committing; reads succeed.
    abortNewClaims = true;

    // 3) Advance past the T1 lease deadline: the REAL deadline wake flushes;
    //    its T2 claim aborts pre-commit and the fresh read finds only the
    //    stale T1 lease → claim-aborted. No POST on the stale lease. (The
    //    token assertion below the behavioral one: pre-fix code never mints
    //    tokens at all, and the ruling violation is the unauthorized POST.)
    await vi.advanceTimersByTimeAsync(LEASE_TTL_MS + 250);
    expect(sendSpy).toHaveBeenCalledTimes(1);
    expect(stored().state).toBe("inflight");
    // The aborted T2 claim never committed: attempts and token stay T1.
    expect(stored().attempts).toBe(1);
    // The bounded recovery kept exactly one cover for the row.
    expect(box.pendingLeaseWakeups()).toContain(pId);
    // The fresh claim-identity machinery the ruling requires: THIS attempt's
    // token rode along with the real claim and is durable.
    expect(t1).toBeTruthy();
    expect(stored().lease?.claimToken).toBe(t1);

    // 4) Several more backed-off fires: claims keep genuinely aborting, never
    //    POSTing. No third-party lease is ever mistaken for a won claim.
    await vi.advanceTimersByTimeAsync(2_000);
    expect(sendSpy).toHaveBeenCalledTimes(1);
    expect(stored().attempts).toBe(1);
    expect(stored().lease?.claimToken).toBe(t1);

    // 5) Storage really recovers: the next real wake commits a fresh claim
    //    (T3) and the POST proceeds under the same command id.
    abortNewClaims = false;
    await vi.advanceTimersByTimeAsync(5_000);
    await pump(() => (stored()?.state === "sent" ? true : null));
    expect(sendSpy).toHaveBeenCalledTimes(2);
    expect(sendSpy.mock.calls.map((c) => c[4])).toEqual([pId, pId]);
    expect(stored().state).toBe("sent");
    expect(stored().attempts).toBe(2);
    expect(stored().lease).toBeUndefined();
  } finally {
    vi.useRealTimers();
  }
});

it("GATE23 item 1b: a claim whose merge promise rejects AFTER committing THIS attempt's token is a won claim — the POST proceeds immediately", async () => {
  // Positive half of the identity ruling: mergeUnlessDone's promise rejects
  // AFTER it committed the inflight lease carrying THIS attempt's fresh
  // claimToken. The catch's fresh authoritative read finds that exact token →
  // won claim → POST now (no 30 s wait, one attempt). The persisted lease
  // really carries the per-attempt token that makes the verdict possible.
  vi.useFakeTimers();
  vi.setSystemTime(750_000);
  try {
    const { api, hubStore } = await fresh();
    stubPostSend(api);
    const { sendSpy, storage } = await bootLiveWithFaultableStorage(api, hubStore);

    let claimTokenSeen: string | undefined;
    const realMerge = storage.mergeUnlessDone.bind(storage);
    storage.mergeUnlessDone = vi.fn(async (id, patch) => {
      const p = patch as { state?: string; lease?: { claimToken?: string } };
      const stored = await realMerge(id, patch);
      // Post-COMMIT rejection of the claim's own real transition; the finish
      // patch passes untouched.
      if (String(id).startsWith("cmd_") && p.state === "inflight") {
        claimTokenSeen = p.lease?.claimToken;
        throw new Error("g23 1b post-commit claim rejection");
      }
      return stored;
    });

    const startedAt = Date.now();
    void hubStore.send(INSTANCE, "gate23 1b");
    // Bounded microtask pump (no fake-timer advance): the POST is immediate.
    await pump(() => {
      const b = hubStore.getSnapshot().bubbles.find((x) => x.text === "gate23 1b");
      return b?.commandId && sendSpy.mock.calls.some((c) => c[4] === b.commandId)
        ? (b.commandId as string)
        : null;
    });
    expect(Date.now() - startedAt).toBeLessThan(LEASE_GUARD_MS);
    expect(claimTokenSeen).toBeTruthy();
    const pId = sendSpy.mock.calls[0]?.[4] as string;
    const mine = () =>
      (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<{
        commandId: string;
        state: string;
        attempts: number;
      }>).find((r) => r.commandId === pId)!;
    await pump(() => (mine()?.state === "sent" ? true : null));
    expect(sendSpy).toHaveBeenCalledTimes(1);
    expect(mine().state).toBe("sent");
    expect(mine().attempts).toBe(1);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(sendSpy).toHaveBeenCalledTimes(1);
  } finally {
    vi.useRealTimers();
  }
});

it("GATE22 item 2: a live steer whose rejected finishing write aborts once is recovered at its lease deadline — the direct route ends through the shared epilogue", async () => {
  // The guarded flush ends every pass with endDeliveryPasses (fresh
  // authoritative reconcile through bounded recovery); the LIVE DIRECT STEER
  // route used to end with only endActiveDelivery. Sequence: steer POST hangs;
  // another instance's flush finishes while the steer is ACTIVE (its
  // reconciliations arm nothing); the steer verdict is rejected but the
  // finishing write ABORTS ONCE, so durable state stays inflight under this
  // owner's live lease with no patch epilogue and no wake. Pre-fix the row
  // stranded past its lease; post-fix the direct route's shared epilogue arms
  // the one deadline wake, and the store's real timer recovers the row at
  // lease expiry (same-id redelivery surfaces the rejected verdict).
  vi.useFakeTimers();
  vi.setSystemTime(600_000);
  const I_STEER = INSTANCE;
  const I_OTHER = "ins_gate22_2_other";
  const steerPrompt = "gate22 2 steer";
  const internalsOf = (s: Store) =>
    s as unknown as {
      ensureOutbox: () => Promise<boolean>;
      flushAllOutbox: () => Promise<void>;
      outbox: {
        ownerId: string;
        storage: FaultableOutboxStorage;
        pendingLeaseWakeups: () => string[];
        leaseWakeAt: (id: string) => number | undefined;
      } | null;
    };
  try {
    const { api, hubStore } = await fresh();
    stubPostSend(api);
    hubStore.setConnectionStateForTest("live");
    const internals = internalsOf(hubStore);
    expect(await internals.ensureOutbox()).toBe("ok");
    await internals.flushAllOutbox(); // settle the empty boot flush
    const storage = internals.outbox!.storage;

    // One-shot abort of the rejected finishing write, armed from the REAL
    // patch transition — never a hand-edited state change.
    let abortOnce = true;
    const realMerge = storage.mergeUnlessDone.bind(storage);
    storage.mergeUnlessDone = vi.fn(async (id, patch) => {
      if (abortOnce && String(id).startsWith("cmd_") && patch.state === "rejected") {
        abortOnce = false;
        throw new Error("gate22 2 finishing write abort");
      }
      return await realMerge(id, patch);
    });

    // First steer POST hangs until released with a settled+rejected verdict;
    // the deadline redelivery gets the same deduplicated verdict.
    let releaseSteer: (() => void) | null = null;
    const steerGate = new Promise<void>((resolve) => {
      releaseSteer = resolve;
    });
    let steerCalls = 0;
    const rejectedFor = (commandId: string): CommandResult => {
      const r = commandResult(commandId, "settled");
      r.command.settlement = { outcome: "rejected", reason: "turn does not exist" };
      return r;
    };
    const sendSpy = vi.spyOn(api, "instanceSend").mockImplementation(
      (async (_iid: string, prompt: string, _a?: unknown[], _m?: string, commandId?: string) => {
        if (prompt === steerPrompt) {
          steerCalls += 1;
          if (steerCalls === 1) await steerGate;
          return rejectedFor(commandId ?? "cmd_x");
        }
        return commandResult(commandId ?? "cmd_x", "accepted");
      }) as Api["instanceSend"],
    );

    // 1) Live steer: real claim, POST pending.
    const steerP = hubStore.send(I_STEER, steerPrompt, [], [], "steer");
    const steerId = await pump(() => {
      const b = hubStore.getSnapshot().bubbles.find((x) => x.text === steerPrompt);
      return b?.commandId && sendSpy.mock.calls.some((c) => c[4] === b.commandId)
        ? (b.commandId as string)
        : null;
    });

    // 2) Another instance's REAL flush runs and finishes while the steer POST
    //    is active; every reconciliation then sees the row as its own active
    //    delivery and arms nothing for it.
    await hubStore.send(I_OTHER, "gate22 2 other");
    await pump(() =>
      sendSpy.mock.calls.filter((c) => c[1] === "gate22 2 other").length === 1 ? true : null,
    );
    // Deterministic barrier: wait until NO authoritative read is in flight and
    // none starts across a wide microtask window, so the other instance's flush
    // AND its endDeliveryPasses epilogue have fully finished — all while the
    // steer is still active. A late epilogue after the steer finishes would
    // arm the wake and hide the direct-route regression.
    let readsInFlight = 0;
    const realAll = storage.all.bind(storage);
    storage.all = vi.fn(async () => {
      readsInFlight += 1;
      try {
        return await realAll();
      } finally {
        readsInFlight -= 1;
      }
    });
    for (;;) {
      const before = vi.mocked(storage.all).mock.calls.length;
      for (let i = 0; i < 100; i += 1) await Promise.resolve();
      if (vi.mocked(storage.all).mock.calls.length === before && readsInFlight === 0) break;
    }
    const storedRows = () =>
      JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<{
        commandId: string;
        prompt: string;
        state: string;
        attempts: number;
        lease?: { owner: string; until: number };
      }>;
    expect(internals.outbox!.pendingLeaseWakeups()).not.toContain(steerId);
    const live = storedRows().find((r) => r.commandId === steerId);
    expect(live?.state).toBe("inflight");
    expect(live?.lease?.owner).toBe(internals.outbox!.ownerId);

    // 3) Verdict rejected, finishing write aborts once. The direct route's
    //    shared epilogue re-reads the still-inflight row and arms its one
    //    deadline wake (no earlier reconciliation left it one).
    releaseSteer!();
    await expect(steerP).resolves.toBe(false);
    expect(steerCalls).toBe(1);
    const stranded = storedRows().find((r) => r.commandId === steerId);
    expect(stranded?.state).toBe("inflight");
    expect(stranded?.attempts).toBe(1);
    expect(stranded?.lease?.owner).toBe(internals.outbox!.ownerId);
    const until = stranded!.lease!.until;
    expect(until - Date.now()).toBeGreaterThan(20_000);
    expect(internals.outbox!.pendingLeaseWakeups()).toContain(steerId);
    expect(internals.outbox!.leaseWakeAt(steerId)).toBe(until);

    // 4) The store's REAL lease-deadline timer is the only recovery trigger:
    //    same-id redelivery surfaces the rejected verdict durably.
    await vi.advanceTimersByTimeAsync(until - Date.now() + 10);
    expect(steerCalls).toBe(2);
    expect(sendSpy.mock.calls.filter((c) => c[4] === steerId)).toHaveLength(2);
    const recovered = storedRows().find((r) => r.commandId === steerId);
    expect(recovered?.state).toBe("rejected");
    expect(recovered?.lease).toBeUndefined();

    // 5) Well past every deadline: no third POST, one steer intent ever.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(sendSpy.mock.calls.filter((c) => c[4] === steerId)).toHaveLength(2);
    expect(hubStore.getSnapshot().bubbles.filter((b) => b.text === steerPrompt)).toHaveLength(1);
  } finally {
    vi.useRealTimers();
  }
});
