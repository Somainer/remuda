import { afterEach, expect, it, vi } from "vitest";
import type { JournalClient } from "./journal";

const INSTANCE = "ins_journal_retry";
const JOURNAL = "obj_journal_retry_journal";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

/**
 * c-reconnfu item 5: the 只读 banner Retry must force a journal catch-up even
 * while the follow socket is OPEN and recently framed. In that state the
 * connection machine's "resume" trusts the live link and no-ops, so the
 * readonly-stale journal (its backfill read failed) was stuck with a Retry
 * button that did nothing.
 */
afterEach(() => {
  vi.restoreAllMocks();
});

it("banner Retry forces a backfill read over a healthy socket and clears readonly-stale", async () => {
  const { api, hubStore } = await fresh();

  vi.spyOn(api, "hello").mockResolvedValue({} as Awaited<ReturnType<Api["hello"]>>);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null } as Awaited<
    ReturnType<Api["hostList"]>
  >);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] } as Awaited<ReturnType<Api["deviceList"]>>);
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as Awaited<
    ReturnType<Api["passkeyList"]>
  >);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "0", durableSeq: "0", lifecycle: "running" } as never],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  const read = vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "0",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async () => ({
      subscriptionId: "sub_journal_retry",
      journalId: JOURNAL,
      durableSeq: "0",
      windowFromSeq: null,
      reachedAfterSeq: true,
      // OPEN the whole time: the machine considers the link live.
      getReadyState: () => 1,
      snapshot: {
        projectionVersion: "v1",
        projectionEpoch: "epoch_journal_retry",
        asOfSeq: "0",
        instance: {} as never,
        runs: [],
        commands: [],
        pendingInteractions: [],
        nodes: [],
        history: { earliestRetainedSeq: "0", complete: true },
      },
    })) as Api["eventsSubscribe"],
  );

  await hubStore.bootstrap();
  await hubStore.follow(INSTANCE);
  expect(hubStore.connectionState).toBe("live");

  // A backfill/resync read fails: the journal settles readonly-stale while
  // the socket stays perfectly healthy.
  const internal = hubStore as unknown as { journals: Map<string, JournalClient> };
  const client = internal.journals.get(JOURNAL)!;
  read.mockRejectedValueOnce(new Error("BACKFILL_READ_FAILED"));
  await client.resumeAfterReconnect().catch(() => undefined);
  expect(client.status).toBe("readonly-stale");
  const readsBeforeRetry = read.mock.calls.length;

  // The Retry button calls catchup(). It must issue a forced read even though
  // the socket is OPEN + recently framed (the machine resume would no-op).
  await hubStore.catchup(INSTANCE);
  expect(read.mock.calls.length).toBeGreaterThan(readsBeforeRetry);
  expect(client.status).toBe("live");
  // The link itself was never in question.
  expect(hubStore.connectionState).toBe("live");

  hubStore.logout();
});
