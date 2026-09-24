import { afterEach, expect, it, vi } from "vitest";
import type { CommandResult } from "../types/command";
import { OUTBOX_LS_KEY } from "./outbox";

const INSTANCE = "ins_reconciling";
const JOURNAL = "obj_reconciling_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;
type Command = CommandResult["command"];

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

function base(commandId: string, state: Command["state"]): Command {
  return {
    commandId,
    id: commandId,
    revision: "1",
    createdAt: "2026-09-24T00:00:00.000Z",
    updatedAt: "2026-09-24T00:00:00.000Z",
    actor: { principalId: "prn_1", type: "human", deviceId: "dev_1", instanceId: INSTANCE },
    origin: "ui",
    operation: "instance.send",
    target: { hostId: "hst_1", instanceId: INSTANCE, runId: null },
    payloadDigest: "sha256:00",
    state,
    dispatch: "intent-durable",
    resolution: "clear",
  };
}

/** A queued row the Hub already forwarded to the Node but is still reconciling. */
function reconciling(commandId: string): Command {
  return { ...base(commandId, "queued"), dispatch: "transport-written", resolution: "reconciling" };
}

function held(commandId: string): Command {
  return { ...base(commandId, "queued"), dispatch: "not-dispatched", resolution: "clear" };
}

function rejected(commandId: string, reason: string): Command {
  return {
    ...base(commandId, "settled"),
    dispatch: "transport-written",
    resolution: "clear",
    settlement: { outcome: "rejected", reason },
  };
}

function stubFollow(api: Api) {
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  vi.spyOn(api, "eventsRead").mockResolvedValue({ events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true });
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_reconciling",
    journalId: JOURNAL,
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
    getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_reconciling",
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
  vi.useRealTimers();
  vi.restoreAllMocks();
  localStorage.removeItem(OUTBOX_LS_KEY);
});

it("a reconciling forward is confirmed by GET (never re-POSTed) and ends sent", async () => {
  const { api, hubStore } = await fresh();
  stubFollow(api);
  const posts: string[] = [];
  vi.spyOn(api, "instanceSend").mockImplementation(
    (async (_iid: string, _p: string, _r?: unknown[], _m?: string, commandId?: string) => {
      posts.push(commandId!);
      return { relatedCommandIds: [], command: reconciling(commandId!) };
    }) as Api["instanceSend"],
  );
  // GET 1 still reconciling; GET 2 reports the Node accepted it.
  let gets = 0;
  vi.spyOn(api, "instanceCommandStatus").mockImplementation(
    (async (_iid: string, commandId: string) => {
      gets += 1;
      return {
        command: gets === 1 ? reconciling(commandId) : base(commandId, "accepted"),
      };
    }) as Api["instanceCommandStatus"],
  );
  hubStore.setConnectionStateForTest("live");
  await hubStore.send(INSTANCE, "reconciling turn");

  const bubble = await vi.waitFor(
    () => {
      const b = hubStore.getSnapshot().bubbles[0];
      if (b?.outboxState !== "sent") throw new Error(`state=${b?.outboxState}`);
      return b;
    },
    { timeout: 5_000 },
  );
  // Exactly ONE POST: the GET loop reconciles, it never re-forwards.
  expect(posts).toEqual([bubble.commandId]);
  expect(gets).toBe(2);
});

it("a reconciling forward rejected on the GET keeps and surfaces the rejection reason", async () => {
  const { api, hubStore } = await fresh();
  stubFollow(api);
  vi.spyOn(api, "instanceSend").mockImplementation(
    (async (_iid: string, _p: string, _r?: unknown[], _m?: string, commandId?: string) => ({
      relatedCommandIds: [],
      command: reconciling(commandId!),
    })) as Api["instanceSend"],
  );
  vi.spyOn(api, "instanceCommandStatus").mockImplementation(
    (async (_iid: string, commandId: string) => ({
      command: rejected(commandId, "node refused: unsafe tool"),
    })) as Api["instanceCommandStatus"],
  );
  hubStore.setConnectionStateForTest("live");
  await hubStore.send(INSTANCE, "do the unsafe thing");

  await vi.waitFor(
    () => {
      if (hubStore.getSnapshot().bubbles[0]?.outboxState !== "rejected") {
        throw new Error(`state=${hubStore.getSnapshot().bubbles[0]?.outboxState}`);
      }
    },
    { timeout: 5_000 },
  );
  const row = hubStore.getSnapshot().bubbles[0];
  // Local-bubble semantics: a definite rejection renders as the unknown row
  // chip; the durable outbox state (which drives the reason/status) is rejected.
  expect(row?.state).toBe("unknown");
  // The Node's rejection reason is retained on the durable row for display
  // (never collapsed into a generic 状态待确认).
  const durable = JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<{
    state: string;
    lastError?: string;
  }>;
  expect(durable.find((r) => r.state === "rejected")?.lastError).toBe("node refused: unsafe tool");
  expect(vi.mocked(api.instanceSend)).toHaveBeenCalledTimes(1);
});

it("a reconciliation that stays inconclusive for the bounded deadline ends unknown", async () => {
  vi.useFakeTimers();
  const { api, hubStore } = await fresh();
  stubFollow(api);
  vi.spyOn(api, "instanceSend").mockImplementation(
    (async (_iid: string, _p: string, _r?: unknown[], _m?: string, commandId?: string) => ({
      relatedCommandIds: [],
      command: reconciling(commandId!),
    })) as Api["instanceSend"],
  );
  vi.spyOn(api, "instanceCommandStatus").mockImplementation(
    (async (_iid: string, commandId: string) => ({ command: reconciling(commandId) })) as Api["instanceCommandStatus"],
  );
  hubStore.setConnectionStateForTest("live");
  await hubStore.send(INSTANCE, "never resolves");

  // Walk the bounded 1 s GET loop past its 30 s deadline, flushing the
  // immediate promise resolutions between timer advances.
  for (let i = 0; i < 35; i += 1) {
    await vi.advanceTimersByTimeAsync(1_000);
  }
  expect(hubStore.getSnapshot().bubbles[0]?.outboxState).toBe("unknown");
  // The POST stayed exactly once even though the GET polled many times.
  expect(vi.mocked(api.instanceSend)).toHaveBeenCalledTimes(1);
});

it("a held row (queued, not forwarded) is re-forwarded under the same id when the host returns, without spending attempts", async () => {
  const { api, hubStore } = await fresh();
  stubFollow(api);
  let posts = 0;
  vi.spyOn(api, "instanceSend").mockImplementation(
    (async (_iid: string, _p: string, _r?: unknown[], _mode?: string, commandId?: string) => {
      posts += 1;
      return {
        relatedCommandIds: [],
        // The first POST finds the Node offline (held); the next (host back
        // online) is accepted.
        command: posts === 1 ? held(commandId!) : base(commandId!, "accepted"),
      };
    }) as Api["instanceSend"],
  );
  hubStore.setConnectionStateForTest("live");
  await hubStore.send(INSTANCE, "wait for the host");
  const bubble = await vi.waitFor(() => {
    const b = hubStore.getSnapshot().bubbles[0];
    if (!b?.commandId) throw new Error("no bubble");
    return b;
  });
  // Wait for the first delivery to settle at "held": queued at the Hub, but
  // never forwarded to the Node, and the attempt budget was not spent.
  await vi.waitFor(
    () => {
      if (hubStore.getSnapshot().bubbles[0]?.outboxState !== "held") throw new Error("not held yet");
    },
    { timeout: 5_000 },
  );
  expect(posts).toBe(1);
  // A held answer spends ZERO attempts (the host being down must not drain
  // the 20-attempt envelope).
  const attemptsOf = () =>
    (JSON.parse(localStorage.getItem(OUTBOX_LS_KEY) ?? "[]") as Array<{ commandId: string; attempts: number }>).find(
      (r) => r.commandId === bubble.commandId,
    )?.attempts;
  expect(attemptsOf()).toBe(0);

  // The host's offline->online recovery flushes: held rows stay deliverable
  // and re-forward with the SAME commandId.
  await (hubStore as unknown as { flushAllOutbox: () => Promise<void> }).flushAllOutbox();
  await vi.waitFor(
    () => expect(hubStore.getSnapshot().bubbles[0]?.outboxState).toBe("sent"),
    { timeout: 5_000 },
  );
  expect(posts).toBe(2);
  const callIds = vi.mocked(api.instanceSend).mock.calls.map((c) => c[4]);
  expect(callIds[0]).toBe(bubble.commandId);
  expect(callIds[1]).toBe(bubble.commandId);
});
