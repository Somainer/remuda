import { afterEach, expect, it, vi } from "vitest";
import type { CommandResult } from "../types/command";
import { OUTBOX_LS_KEY } from "./outbox";

const INSTANCE = "ins_steer_held";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

function commandResult(commandId: string | undefined, state: CommandResult["command"]["state"]): CommandResult {
  return {
    relatedCommandIds: [],
    command: {
      commandId: commandId ?? "cmd_x",
      id: commandId ?? "cmd_x",
      revision: "1",
      createdAt: "2026-09-18T00:00:00.000Z",
      updatedAt: "2026-09-18T00:00:00.000Z",
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

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

it("a steer landing mid-flush is not posted again by the stale flush snapshot", async () => {
  const { api, hubStore } = await fresh();

  const first = hubStore.hold(INSTANCE, "first held", "turn");
  const second = hubStore.hold(INSTANCE, "second held", "turn");

  type Call = { prompt: string; mode?: string; commandId?: string };
  const calls: Call[] = [];
  let resolveFirst: (value: CommandResult) => void = () => {};
  const firstPending = new Promise<CommandResult>((resolve) => {
    resolveFirst = resolve;
  });
  vi.spyOn(api, "instanceSend").mockImplementation(
    ((_instanceId: string, prompt: string, _refs?: unknown[], mode?: string, commandId?: string) => {
      calls.push({ prompt, mode, commandId });
      return prompt === "first held"
        ? firstPending.then(() => commandResult(commandId, "accepted"))
        : Promise.resolve(commandResult(commandId, "accepted"));
    }) as Api["instanceSend"],
  );

  // Begin the flush; the single deliverer starts the first-row POST (held
  // pending). All delivery is serialized by the cross-tab lock.
  const flushing = hubStore.flushHeld(INSTANCE);
  await vi.waitFor(() => expect(calls).toHaveLength(1));
  expect(calls[0]?.prompt).toBe("first held");
  expect(calls[0]?.mode).toBeUndefined();

  // Steer the SECOND row while the first POST is in flight. Conversion is
  // idempotent (one commandId); delivery joins the single deliverer lock — it
  // cannot POST concurrently, which is the exact single-deliverer guarantee.
  const steerPromise = hubStore.steerHeld(INSTANCE, second);

  // Resolve the first POST; the drain then delivers the steer-promoted second
  // row exactly once.
  resolveFirst(commandResult(calls[0]?.commandId, "accepted"));
  const landed = await steerPromise;
  await flushing.catch(() => undefined);

  expect(landed).toBe(true);
  // Exactly two POSTs total; the second row went exactly once as a steer.
  await vi.waitFor(() => expect(calls).toHaveLength(2));
  const steerCalls = calls.filter((c) => c.prompt === "second held");
  expect(steerCalls).toHaveLength(1);
  expect(steerCalls[0]?.mode).toBe("steer");
  // Each attempt used the row's own distinct client-generated id.
  expect(calls[0]?.commandId?.startsWith("cmd_")).toBe(true);
  expect(steerCalls[0]?.commandId?.startsWith("cmd_")).toBe(true);
  expect(calls[0]?.commandId).not.toBe(steerCalls[0]?.commandId);

  const bubbles = hubStore.getSnapshot().bubbles;
  const steered = bubbles.find((b) => b.clientRequestId === second)!;
  expect(steered.promptMode).toBe("steer");
  expect(steered.held).toBe(false);
  expect(steered.commandId).toBe(calls[1]?.commandId);
  const flushed = bubbles.find((b) => b.clientRequestId === first)!;
  expect(flushed.promptMode).toBe("new-turn");
  expect(flushed.commandId).toBe(calls[0]?.commandId);
}, 15_000);

it("a steer whose POST fails with a retriable error resolves false and stays queued under the same id", async () => {
  const { api, hubStore } = await fresh();
  const id = hubStore.hold(INSTANCE, "failed steer", "turn");
  vi.spyOn(api, "instanceSend").mockRejectedValue(new Error("HTTP 500"));

  const landed = await hubStore.steerHeld(INSTANCE, id);
  // The interrupt did not land, so no 已打断 receipt; the row persists for
  // same-id auto-retry (not the old null-id 状态待确认 dead end).
  expect(landed).toBe(false);
  const bubble = hubStore.getSnapshot().bubbles.find((b) => b.clientRequestId === id)!;
  expect(bubble.state).toBe("queued");
  expect(bubble.commandId?.startsWith("cmd_")).toBe(true);
  expect(bubble.held).toBe(false);
  expect(bubble.promptMode).toBe("steer");
});

it("a steer is delivered immediately while live", async () => {
  const { api, hubStore } = await fresh();
  const id = hubStore.hold(INSTANCE, "live steer", "turn");
  const send = vi.spyOn(api, "instanceSend").mockResolvedValue(
    commandResult(undefined, "accepted"),
  );
  const landed = await hubStore.steerHeld(INSTANCE, id);
  expect(landed).toBe(true);
  expect(send).toHaveBeenCalledTimes(1);
  expect(send.mock.calls[0]?.[3]).toBe("steer");
  const bubble = hubStore.getSnapshot().bubbles.find((b) => b.clientRequestId === id)!;
  expect(bubble.commandId).toBe(send.mock.calls[0]?.[4]);
});

it("concurrent flush and steer of the SAME held row mint exactly one commandId (HIGH-1)", async () => {
  const { api, hubStore } = await fresh();
  const id = hubStore.hold(INSTANCE, "race row", "turn");

  const seenCommandIds = new Set<string | undefined>();
  vi.spyOn(api, "instanceSend").mockImplementation(
    ((_iid: string, _prompt: string, _refs?: unknown[], _mode?: string, commandId?: string) => {
      seenCommandIds.add(commandId);
      // Delay the POST so both conversions overlap before it resolves.
      return new Promise((resolve) =>
        setTimeout(() => resolve(commandResult(commandId, "accepted")), 5),
      );
    }) as Api["instanceSend"],
  );

  // Fire both conversions of the same row without awaiting between them.
  const flush = hubStore.flushHeld(INSTANCE);
  const steer = hubStore.steerHeld(INSTANCE, id);
  const [flushed, landed] = await Promise.all([flush.then(() => true), steer]);
  expect(flushed).toBe(true);
  expect(landed).toBe(true);

  // Exactly one commandId for the bubble's lifetime and exactly one POST.
  const bubble = hubStore.getSnapshot().bubbles.find((b) => b.clientRequestId === id)!;
  expect(bubble.commandId).toBeTruthy();
  expect(seenCommandIds.size).toBe(1);
  expect([...seenCommandIds][0]).toBe(bubble.commandId);
  expect(api.instanceSend).toHaveBeenCalledTimes(1);
});

it("a real Node rejection (settled+rejected) is terminal, never re-POSTed and shows 未送达 (HIGH-3)", async () => {
  const { api, hubStore } = await fresh();
  const id = hubStore.hold(INSTANCE, "doomed", "turn");
  const rejected = commandResult(undefined, "settled");
  rejected.command.dispatch = "transport-written";
  rejected.command.resolution = "clear";
  rejected.command.settlement = { outcome: "rejected", reason: "turn does not exist" };
  const send = vi.spyOn(api, "instanceSend").mockResolvedValue(rejected);
  const landed = await hubStore.steerHeld(INSTANCE, id);
  expect(landed).toBe(false); // no interrupt receipt
  expect(send).toHaveBeenCalledTimes(1);
  const bubble = hubStore.getSnapshot().bubbles.find((b) => b.clientRequestId === id)!;
  expect(bubble.state).toBe("unknown");
  expect(bubble.outboxState).toBe("rejected");
  // A second flush must not retry the rejected row.
  await hubStore.flushHeld(INSTANCE);
  expect(send).toHaveBeenCalledTimes(1);
});

it("a held bubble keeps ONE lifetime commandId when the first persistence attempt aborts, then retries it", async () => {
  const { api, hubStore } = await fresh();
  const heldId = hubStore.hold(INSTANCE, "abort once", "turn");
  vi.spyOn(api, "instanceSend").mockResolvedValue(commandResult("cmd_placeholder", "accepted"));

  // The FIRST durable enqueue aborts (IndexedDB transaction abort); the
  // second attempt succeeds.
  const realSetItem = Storage.prototype.setItem;
  let firstPut = true;
  const setSpy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, key: string, value: string) {
    // Only the first pending command-row write (the held bubble's enqueue).
    if (firstPut && /"commandId":"cmd_/.test(value) && !value.includes("__lock__")) {
      firstPut = false;
      throw new DOMException("simulated abort", "QuotaExceededError");
    }
    return realSetItem.call(this, key, value);
  });

  // First flush: the persistence aborts; the bubble stays held but its
  // lifetime commandId is already bound.
  await hubStore.flushHeld(INSTANCE);
  const boundAfterAbort = hubStore
    .getSnapshot()
    .bubbles.find((b) => b.clientRequestId === heldId)?.commandId;
  expect(boundAfterAbort?.startsWith("cmd_")).toBe(true);
  expect(api.instanceSend).not.toHaveBeenCalled();

  // Retry: the SAME commandId persists (exactly one durable row) and POSTs.
  setSpy.mockRestore();
  await hubStore.flushHeld(INSTANCE);
  await vi.waitFor(() => expect(api.instanceSend).toHaveBeenCalledTimes(1));
  expect(vi.mocked(api.instanceSend).mock.calls[0]?.[4]).toBe(boundAfterAbort);

  const rows = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<{ commandId: string }>;
  const durable = rows.filter((r) => r.commandId === boundAfterAbort);
  expect(durable).toHaveLength(1);
});

it("an offline steer resolves false (no 已打断 receipt) and stays a queued row with no POST", async () => {
  const { api, hubStore } = await fresh();
  const id = hubStore.hold(INSTANCE, "offline steer", "turn");
  const send = vi.spyOn(api, "instanceSend");
  hubStore.setConnectionStateForTest("offline");

  const landed = await hubStore.steerHeld(INSTANCE, id);
  // No authoritative acceptance while offline: the Composer must not raise
  // 已打断, and nothing was POSTed.
  expect(landed).toBe(false);
  expect(send).not.toHaveBeenCalled();
  const bubble = hubStore.getSnapshot().bubbles.find((b) => b.clientRequestId === id)!;
  // It degraded from steer to an ordinary queued turn under a durable id.
  expect(bubble.commandId?.startsWith("cmd_")).toBe(true);
  expect(bubble.promptMode).not.toBe("steer");
});

it("a direct steer sent while offline resolves false (no receipt) but enqueues under one id", async () => {
  const { api, hubStore } = await fresh();
  const send = vi.spyOn(api, "instanceSend");
  hubStore.setConnectionStateForTest("offline");
  const landed = await hubStore.send(INSTANCE, "urgent steer", [], [], "steer");
  expect(landed).toBe(false);
  expect(send).not.toHaveBeenCalled();
  const bubble = hubStore.getSnapshot().bubbles.find((b) => b.text === "urgent steer")!;
  expect(bubble.commandId?.startsWith("cmd_")).toBe(true);
});
