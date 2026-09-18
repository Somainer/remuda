import { afterEach, expect, it, vi } from "vitest";
import type { CommandResult } from "../types/command";

const INSTANCE = "ins_steer_held";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

function commandResult(commandId: string, state: CommandResult["command"]["state"]): CommandResult {
  return {
    relatedCommandIds: [],
    command: {
      commandId,
      id: commandId,
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
});

it("a steer landing mid-flush is not posted again by the stale flush snapshot", async () => {
  const { api, hubStore } = await fresh();

  const first = hubStore.hold(INSTANCE, "first held", "turn");
  const second = hubStore.hold(INSTANCE, "second held", "turn");

  type Call = { prompt: string; mode?: string };
  const calls: Call[] = [];
  let resolveFirst: (value: CommandResult) => void = () => {};
  const firstPending = new Promise<CommandResult>((resolve) => {
    resolveFirst = resolve;
  });
  vi.spyOn(api, "instanceSend").mockImplementation(
    ((_instanceId: string, prompt: string, _refs?: unknown[], mode?: string) => {
      calls.push({ prompt, mode });
      // The flush's first-row POST stays pending until the test says so; the
      // steer (mode=steer) lands immediately, while that first POST is in
      // flight — exactly the blocked→working answered-flush window.
      return mode === "steer"
        ? Promise.resolve(commandResult("cmd_steer", "accepted"))
        : firstPending;
    }) as Api["instanceSend"],
  );

  // Begin the flush; it suspends on the first row's pending POST.
  const flushing = hubStore.flushHeld(INSTANCE);
  await Promise.resolve();
  expect(calls).toEqual([{ prompt: "first held", mode: undefined }]);

  // Steer the SECOND row while the first POST is still pending.
  const landed = await hubStore.steerHeld(INSTANCE, second);
  expect(landed).toBe(true);
  expect(calls).toEqual([
    { prompt: "first held", mode: undefined },
    { prompt: "second held", mode: "steer" },
  ]);

  // Let the flush's first POST land and the flush run to completion.
  resolveFirst(commandResult("cmd_first", "accepted"));
  await flushing;

  // Exactly two POSTs total; the steered text appears exactly once, as a steer.
  expect(calls).toHaveLength(2);
  expect(calls.filter((c) => c.prompt === "second held")).toHaveLength(1);
  expect(calls[1]).toEqual({ prompt: "second held", mode: "steer" });

  // The flush must not have overwritten the steer's commandId/promptMode.
  const bubbles = hubStore.getSnapshot().bubbles;
  const steered = bubbles.find((b) => b.clientRequestId === second)!;
  expect(steered.commandId).toBe("cmd_steer");
  expect(steered.promptMode).toBe("steer");
  expect(steered.held).toBe(false);
  const flushed = bubbles.find((b) => b.clientRequestId === first)!;
  expect(flushed.commandId).toBe("cmd_first");
  expect(flushed.promptMode).toBe("new-turn");
});

it("a steer whose POST fails resolves false and leaves the row 状态待确认", async () => {
  const { api, hubStore } = await fresh();
  const id = hubStore.hold(INSTANCE, "failed steer", "turn");
  vi.spyOn(api, "instanceSend").mockRejectedValue(new Error("HTTP 500"));

  const landed = await hubStore.steerHeld(INSTANCE, id);
  expect(landed).toBe(false);
  const bubble = hubStore.getSnapshot().bubbles.find((b) => b.clientRequestId === id)!;
  expect(bubble.state).toBe("unknown");
  expect(bubble.commandId).toBeNull();
  // Marker/promptMode still flipped before the POST; no held row lingers.
  expect(bubble.held).toBe(false);
});
