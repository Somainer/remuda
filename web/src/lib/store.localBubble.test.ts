import { afterEach, expect, it, vi } from "vitest";
import type { CommandResult } from "../types/command";
import type { Observation } from "../types/observation";

const INSTANCE = "ins_local_bubble";
const JOURNAL = "obj_local_bubble_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

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
async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
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
});

it("a fresh bubble has a local id, no command id, and projects 等待发送", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "instanceSend").mockReturnValue(new Promise(() => {}));
  void hubStore.send(INSTANCE, "queued prompt");
  const bubble = hubStore.getSnapshot().bubbles[0];
  expect(bubble.clientRequestId.startsWith("local_")).toBe(true);
  expect(bubble.commandId).toBeNull();
  const { projectCommandStatus } = await import("./commandStatus");
  const row = projectCommandStatus({ hasServerCommandId: false, localState: bubble.state });
  expect(row.label).toBe("等待发送");
});

it("the server response alone assigns commandId and keeps the local id distinct", async () => {
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  vi.spyOn(api, "instanceSend").mockResolvedValue(commandResult("cmd_server_1", "accepted"));

  await hubStore.send(INSTANCE, "accepted prompt");
  const bubble = hubStore.getSnapshot().bubbles[0];
  expect(bubble.commandId).toBe("cmd_server_1");
  expect(bubble.clientRequestId).not.toBe("cmd_server_1");
  expect(bubble.clientRequestId.startsWith("local_")).toBe(true);
  expect(bubble.state).toBe("accepted");
});

it("a failed POST leaves commandId null and projects 状态待确认", async () => {
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  vi.spyOn(api, "instanceSend").mockRejectedValue(new Error("HTTP 500"));

  await hubStore.send(INSTANCE, "failed prompt");
  const bubble = hubStore.getSnapshot().bubbles[0];
  expect(bubble.commandId).toBeNull();
  expect(bubble.state).toBe("unknown");
  const { projectCommandStatus } = await import("./commandStatus");
  const row = projectCommandStatus({
    hasServerCommandId: bubble.commandId !== null,
    localState: bubble.state,
  });
  expect(row.key).toBe("unconfirmed");
  expect(row.label).toBe("状态待确认");
});

it("concurrent sends get distinct clientRequestIds before either response lands", async () => {
  const { api, hubStore } = await fresh();
  stubPostSend(api);
  let resolveFirst: (value: CommandResult) => void = () => {};
  const held = new Promise<CommandResult>((resolve) => {
    resolveFirst = resolve;
  });
  vi.spyOn(api, "instanceSend").mockReturnValueOnce(held).mockResolvedValueOnce(commandResult("cmd_second", "accepted"));

  void hubStore.send(INSTANCE, "first");
  void hubStore.send(INSTANCE, "second");
  const ids = hubStore.getSnapshot().bubbles.map((b) => b.clientRequestId);
  expect(ids).toHaveLength(2);
  expect(new Set(ids).size).toBe(2);
  expect(ids.every((id) => id.startsWith("local_"))).toBe(true);
  resolveFirst(commandResult("cmd_first", "accepted"));
  await vi.waitFor(() => {
    const settled = hubStore.getSnapshot().bubbles.map((b) => b.commandId);
    expect(settled).toEqual(["cmd_first", "cmd_second"]);
  });
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
  vi.spyOn(api, "instanceSend").mockResolvedValue(commandResult("cmd_twin", "accepted"));

  await hubStore.send(INSTANCE, "twin prompt");
  const bubble = () => hubStore.getSnapshot().bubbles[0];
  expect(bubble().state).toBe("accepted");

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
          commandId: "cmd_twin",
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

  vi.spyOn(api, "instanceSend").mockResolvedValue(commandResult("cmd_fast", "accepted"));
  // Every reconciliation read now hangs: the old awaited chain could never
  // resolve this test; the background chain in the new code simply stays open.
  vi.spyOn(api, "eventsRead").mockReturnValue(new Promise(() => {}));
  vi.spyOn(api, "instanceList").mockReturnValue(new Promise(() => {}));
  vi.spyOn(api, "interactionList").mockReturnValue(new Promise(() => {}));

  let landed: boolean | undefined;
  void hubStore.send(INSTANCE, "steer prompt").then((value) => {
    landed = value;
  });
  await vi.waitFor(() => expect(landed).toBe(true), { timeout: 2_000 });
  // The commandId emit rides the POST response, not the reconciliation.
  expect(hubStore.getSnapshot().bubbles[0]?.commandId).toBe("cmd_fast");
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
