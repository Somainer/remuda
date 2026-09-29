import { afterEach, expect, it, vi } from "vitest";
import type { Observation } from "../types/observation";

const INSTANCE_A = "ins_rebind_gap_a";
const JOURNAL_A = "obj_rebind_gap_journal_a";
const INSTANCE_B = "ins_rebind_gap_b";
const JOURNAL_B = "obj_rebind_gap_journal_b";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;
type Hooks = NonNullable<Parameters<Api["eventsSubscribe"]>[4]>;

/**
 * c-reconnfu round 3 item 1: A is mounted with an OPEN socket while a REST
 * gap-backfill is running, and no fresh frame has arrived. Navigating
 * A → B → A must NOT reopen A's socket: the reopen bumps the journal's resume
 * generation and aborts the in-flight fillGap — the 正在补事件 banner has no
 * retry of its own, the missing seq range is never applied, and the machine
 * can certify live over the hole. The rebind reuses followBound()'s 15 s
 * bind deadline and leaves the existing socket + fill alone.
 */
afterEach(() => {
  vi.restoreAllMocks();
});

async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

function ttyEvent(seq: string): Observation {
  return {
    kind: "raw_tty",
    eventId: `evt_gap_${seq}`,
    journalId: JOURNAL_A,
    instanceId: INSTANCE_A,
    seq,
    payload: { text: `SCREEN-${seq}` },
  } as unknown as Observation;
}

it("navigating back to a gap-backfilling session keeps its socket and lets the fill complete before live", async () => {
  const { api, hubStore } = await fresh();

  const hooks = new Map<string, Hooks>();
  const subscribed: string[] = [];
  let releaseGapRead: (() => void) | null = null;
  let gapReadPending = false;
  let afterSeqOneCalls = 0;

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
    items: [
      { id: INSTANCE_A, journalId: JOURNAL_A, revision: "0", durableSeq: "0", lifecycle: "running" },
      { id: INSTANCE_B, journalId: JOURNAL_B, revision: "0", durableSeq: "0", lifecycle: "running" },
    ] as never,
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "screenRead").mockResolvedValue({ lines: [] });
  vi.spyOn(api, "eventsRead").mockImplementation(
    (async (args?: { journalId?: string; afterSeq?: string; beforeSeq?: string }) => {
      // The gap fill (afterSeq = gap start - 1 = "1") is the FIRST such
      // read and stays parked until the test releases it. A reopen-driven resume
      // would be a SECOND call: hand it an empty page immediately so the
      // regression (bumped generation strands the fill) is deterministic.
      if (args?.journalId === JOURNAL_A && args.afterSeq === "1" && args.beforeSeq === undefined) {
        afterSeqOneCalls += 1;
        if (afterSeqOneCalls === 1) {
          gapReadPending = true;
          await new Promise<void>((resolve) => {
            releaseGapRead = resolve;
          });
          gapReadPending = false;
          return {
            events: [ttyEvent("2")],
            durableSeq: "3",
            windowFromSeq: null,
            reachedAfterSeq: true,
          };
        }
        return { events: [], durableSeq: "3", windowFromSeq: null, reachedAfterSeq: true };
      }
      if (args?.journalId === JOURNAL_A) {
        // The REST seed already carries seq 1.
        return {
          events: [ttyEvent("1")],
          durableSeq: "1",
          windowFromSeq: null,
          reachedAfterSeq: true,
        };
      }
      return { events: [], durableSeq: "0", windowFromSeq: null, reachedAfterSeq: true };
    }) as Api["eventsRead"],
  );
  vi.spyOn(api, "eventsSubscribe").mockImplementation(
    (async (journalId: string, _afterSeq: unknown, _onBatch: unknown, _onGap: unknown, h?: Hooks) => {
      subscribed.push(journalId);
      if (h) hooks.set(journalId, h);
      return {
        subscriptionId: `sub_${journalId}`,
        journalId,
        durableSeq: "0",
        windowFromSeq: null,
        reachedAfterSeq: true,
        getReadyState: () => 1,
        snapshot: {
          projectionVersion: "v1",
          projectionEpoch: `epoch_${journalId}`,
          asOfSeq: "0",
          instance: {} as never,
          runs: [],
          commands: [],
          pendingInteractions: [],
          nodes: [],
          history: { earliestRetainedSeq: "0", complete: true },
        },
      };
    }) as Api["eventsSubscribe"],
  );

  await hubStore.bootstrap();

  // 1. A mounts with seq 1 and an OPEN, freshly framed socket (live).
  const mountA = hubStore.follow(INSTANCE_A);
  await vi.waitFor(() => expect(subscribed).toContain(JOURNAL_A));
  await mountA;
  expect(hubStore.connectionState).toBe("live");

  // 2. A gapped live batch (seq 3, seq 2 missing) starts a gap-backfill
  // whose REST read is parked.
  const subscribeA = vi.mocked(api.eventsSubscribe);
  const onBatchA = subscribeA.mock.calls.find((c) => c[0] === JOURNAL_A)?.[2] as (
    batch: Record<string, unknown>,
  ) => void;
  onBatchA({
    subscriptionId: "sub_obj_rebind_gap_journal_a",
    journalId: JOURNAL_A,
    fromSeq: "3",
    toSeq: "3",
    durableSeq: "3",
    events: [ttyEvent("3")],
  });
  await vi.waitFor(() => expect(gapReadPending).toBe(true));
  expect(hubStore.getSnapshot().journalStatus[INSTANCE_A]).toBe("gap-backfill");

  // 3. A's socket goes SILENT (OPEN, but no frame within the window) — the
  // exact condition a B → A rebind must not "fix" with an immediate resume.
  hubStore.setFollowLiveForTest(true, false);

  // 4. Navigate to B (mounts live) and back to A.
  const mountB = hubStore.follow(INSTANCE_B);
  await vi.waitFor(() => expect(subscribed).toContain(JOURNAL_B));
  await mountB;
  expect(hubStore.connectionState).toBe("live");
  const mountBAgain = hubStore.follow(INSTANCE_A);
  await mountBAgain;

  // The rebind must NOT reopen A's socket: the fill is still the owner of
  // this journal's resume generation.
  await new Promise((r) => setTimeout(r, 20));
  expect(subscribed.filter((j) => j === JOURNAL_A)).toHaveLength(1);
  expect(gapReadPending).toBe(true);
  expect(hubStore.getSnapshot().journalStatus[INSTANCE_A]).toBe("gap-backfill");

  // 5. The held fill completes: the missing range is applied IN ORDER and the
  // per-session status ends live only now (never live over the hole).
  releaseGapRead?.();
  await vi.waitFor(() =>
    expect(hubStore.getSnapshot().journalStatus[INSTANCE_A]).toBe("live"),
  );
  const seqs = hubStore.getSnapshot().events[INSTANCE_A]?.map((e) => Number(e.seq));
  expect(seqs).toContain(2);
  expect(seqs).toContain(3);
  // Still exactly one socket for A: the bind deadline never had to reopen.
  expect(subscribed.filter((j) => j === JOURNAL_A)).toHaveLength(1);

  hubStore.logout();
});
