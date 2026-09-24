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
      // The flush's first-row POST stays pending; the steer lands immediately.
      return mode === "steer"
        ? Promise.resolve(commandResult(commandId, "accepted"))
        : firstPending.then(() => commandResult(commandId, "accepted"));
    }) as Api["instanceSend"],
  );

  // Begin the flush; it suspends on the first row's pending POST. The steer
  // lock is per-instance, so flushHeld's await yields before steering.
  const flushing = hubStore.flushHeld(INSTANCE);
  await vi.waitFor(() => expect(calls).toHaveLength(1));
  expect(calls[0]?.prompt).toBe("first held");
  expect(calls[0]?.mode).toBeUndefined();

  // Steer the SECOND row while the first POST is still pending. Its own
  // outbox row is delivered directly (steer awaits its POST for the receipt).
  const landed = await hubStore.steerHeld(INSTANCE, second);
  expect(landed).toBe(true);
  expect(calls).toHaveLength(2);
  expect(calls[1]?.prompt).toBe("second held");
  expect(calls[1]?.mode).toBe("steer");
  // Each attempt used the row's own client-generated id.
  expect(calls[0]?.commandId?.startsWith("cmd_")).toBe(true);
  expect(calls[1]?.commandId?.startsWith("cmd_")).toBe(true);
  expect(calls[0]?.commandId).not.toBe(calls[1]?.commandId);

  // Let the flush finish; the drain must not re-post either row.
  resolveFirst(commandResult(calls[0]?.commandId, "accepted"));
  await flushing;
  await new Promise((r) => setTimeout(r, 50));

  expect(calls).toHaveLength(2);
  expect(calls.filter((c) => c.prompt === "second held")).toHaveLength(1);

  const bubbles = hubStore.getSnapshot().bubbles;
  const steered = bubbles.find((b) => b.clientRequestId === second)!;
  expect(steered.promptMode).toBe("steer");
  expect(steered.held).toBe(false);
  expect(steered.commandId).toBe(calls[1]?.commandId);
  const flushed = bubbles.find((b) => b.clientRequestId === first)!;
  expect(flushed.promptMode).toBe("new-turn");
  expect(flushed.commandId).toBe(calls[0]?.commandId);
});

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
