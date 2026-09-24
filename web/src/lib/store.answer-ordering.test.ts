import { afterEach, expect, it, vi } from "vitest";
import { canSubmitAnswer } from "./interactionStatus";
import type { Interaction } from "../types/interaction";
import type { Observation } from "../types/observation";

const INSTANCE = "ins_answer_ordering";
const JOURNAL = "obj_answer_ordering_journal";
const INTERACTION = "int_answer_ordering";

type Api = typeof import("./api").api;
type Store = typeof import("./store").hubStore;

/** Fresh store/api module pair per test — the store is a process singleton. */
async function fresh(): Promise<{ api: Api; hubStore: Store }> {
  vi.resetModules();
  const [apiModule, storeModule] = await Promise.all([import("./api"), import("./store")]);
  return { api: apiModule.api, hubStore: storeModule.hubStore };
}

function pendingPlanReview(): Interaction {
  return {
    id: INTERACTION,
    interactionId: INTERACTION,
    instanceId: INSTANCE,
    hostId: "hst_1",
    runId: null,
    blocking: true,
    answerable: true,
    carrier: "claude-control",
    kind: "plan-review",
    requestVersion: "1",
    state: "pending",
    request: {
      kind: "plan-review",
      title: "Plan review",
      planRef: "obj_1",
      planRevision: "1",
      planDigest: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
      options: [],
      allowFeedback: false,
      plan: "# plan",
    },
    deadline: { state: "unknown", reason: "none", evidenceEventIds: [] },
    answer: { state: "unknown", reason: "pending", evidenceEventIds: [] },
    delivery: { state: "unknown", reason: "not-sent", evidenceEventIds: [] },
    resolution: { state: "unknown", reason: "pending", evidenceEventIds: [] },
    createdAt: "2026-09-24T00:00:00.000Z",
    updatedAt: "2026-09-24T00:00:00.000Z",
  } as unknown as Interaction;
}

const ANSWERED_EVENT = {
  kind: "interaction.answered",
  eventId: "evt_answered_1",
  journalId: JOURNAL,
  instanceId: INSTANCE,
  seq: "1",
  payload: {
    interactionId: INTERACTION,
    requestVersion: "1",
    answerCommandId: "cmd_1",
  },
} as unknown as Observation;

afterEach(() => {
  vi.restoreAllMocks();
});

/**
 * After a successful answer POST, even if the list refresh FAILS, the
 * interaction.answered receipt must settle the row (answer-committed) and
 * clear the answering marker — the card must not return to an answerable,
 * re-submittable state for an already committed decision.
 */
it("an answer receipt settles the card even when the post-answer refresh fails", async () => {
  const { api, hubStore } = await fresh();
  // Initial state: one pending plan review and a followed session so the
  // answered journal receipt is present in state.events.
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [ANSWERED_EVENT],
    durableSeq: "1",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [{ id: INSTANCE, journalId: JOURNAL, revision: "1", durableSeq: "1" } as Awaited<
      ReturnType<Api["instanceList"]>
    >["items"][number]],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([pendingPlanReview()]);
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_ordering",
    journalId: JOURNAL,
    durableSeq: "1",
    windowFromSeq: null,
    reachedAfterSeq: true,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_ordering",
      asOfSeq: "1",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "0", complete: true },
    },
  });
  await hubStore.refresh();
  // Follow the session so the answered journal tail is seeded into
  // state.events (the receipt respond() looks for).
  await hubStore.follow(INSTANCE);
  await vi.waitFor(() =>
    expect((hubStore.getSnapshot().events[INSTANCE] ?? []).some((e) => e.kind === "interaction.answered")).toBe(true),
  );

  // The answer POST succeeds; the FOLLOW-UP refresh fails (instance list
  // throws), so the authoritative list never updates the pending row.
  vi.spyOn(api, "interactionRespond").mockResolvedValue({
    command: { commandId: "cmd_1", id: "cmd_1" } as never,
    relatedCommandIds: [],
  });
  vi.spyOn(api, "instanceList").mockRejectedValueOnce(new Error("list down"));
  // Keep interactionList returning the STILL-PENDING row on refresh (the
  // receipt arrives via the journal, not the list).
  vi.spyOn(api, "interactionList").mockResolvedValue([pendingPlanReview()]);

  await hubStore.respond(INTERACTION, {
    kind: "plan-review",
    optionId: "approve",
    planRevision: "1",
    planDigest: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
    feedback: null,
  });

  // The answering marker is gone and the row is projected as committed, even
  // though the authoritative list still says pending and the refresh threw.
  expect(hubStore.getSnapshot().answering[INTERACTION]).toBeUndefined();
  const row = hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION);
  expect(row?.state).toBe("answer-committed");
});

/**
 * A delayed pre-answer refresh (started before the POST) completes AFTER the
 * successful answer + failed post-answer refresh, carrying the stale pending
 * row: the committed review must survive it.
 */
it("a delayed older poll cannot revert a committed review to pending", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [ANSWERED_EVENT],
    durableSeq: "1",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_ordering2",
    journalId: JOURNAL,
    durableSeq: "1",
    windowFromSeq: null,
    reachedAfterSeq: true,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_ordering2",
      asOfSeq: "1",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "0", complete: true },
    },
  });

  // Initial refresh seeds the pending row normally.
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([pendingPlanReview()]);
  await hubStore.refresh();
  await hubStore.follow(INSTANCE);
  expect(
    hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION)?.state,
  ).toBe("pending");

  // A NEW poll starts before the answer and is held open; it later resolves
  // with the stale pending row. This is the delayed-older-poll race.
  let releaseDelayedPoll: () => void = () => {};
  const delayedPoll = new Promise<void>((resolve) => {
    releaseDelayedPoll = resolve;
  });
  let interactionCalls = 0;
  vi.spyOn(api, "interactionList").mockImplementation(async () => {
    const n = ++interactionCalls;
    if (n === 1) {
      await delayedPoll;
      return [pendingPlanReview()]; // stale pending row lands late
    }
    // Any later refresh (the one respond() triggers) fails fast so it does
    // not consume the held poll; the receipt settles the card regardless.
    throw new Error("post-answer interaction list fails fast");
  });
  const delayedRefresh = hubStore.refresh(); // held on the delayed interaction poll
  await vi.waitFor(() => expect(interactionCalls).toBeGreaterThanOrEqual(1));

  // Answer POST succeeds; its refresh rejects fast (does not touch the held
  // delayed poll); the 200 settles the card optimistically.
  vi.spyOn(api, "interactionRespond").mockResolvedValue({
    command: { commandId: "cmd_1", id: "cmd_1" } as never,
    relatedCommandIds: [],
  });
  await hubStore.respond(INTERACTION, {
    kind: "plan-review",
    optionId: "approve",
    planRevision: "1",
    planDigest: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
    feedback: null,
  });
  expect(
    hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION)?.state,
  ).toBe("answer-committed");

  // Release the delayed pre-answer poll carrying the stale pending row and
  // let its merge complete; the local settlement guard keeps it committed.
  releaseDelayedPoll();
  await delayedRefresh;
  expect(
    hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION)?.state,
  ).toBe("answer-committed");
});

/**
 * The omission case the pending-only list normally returns: the answer
 * succeeds and the post-answer refresh resolves with an EMPTY list (so the
 * committed local row is removed from the projection), and only THEN does the
 * older poll held open since before the answer complete with the stale
 * pending row. The committed card must neither stay answerable nor be
 * resurrected by the late poll — it stays committed (or gone), never
 * submittable.
 */
it("a newer empty list plus a late older poll cannot resurrect a committed review", async () => {
  const { api, hubStore } = await fresh();
  vi.spyOn(api, "instanceGet").mockResolvedValue({
    id: INSTANCE,
    journalId: JOURNAL,
  } as Awaited<ReturnType<Api["instanceGet"]>>);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [ANSWERED_EVENT],
    durableSeq: "1",
    windowFromSeq: null,
    reachedAfterSeq: true,
  });
  vi.spyOn(api, "eventsSubscribe").mockResolvedValue({
    subscriptionId: "sub_ordering3",
    journalId: JOURNAL,
    durableSeq: "1",
    windowFromSeq: null,
    reachedAfterSeq: true,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_ordering3",
      asOfSeq: "1",
      instance: {} as never,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "0", complete: true },
    },
  });

  // Initial refresh seeds the pending row normally.
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([pendingPlanReview()]);
  await hubStore.refresh();
  expect(
    hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION)?.state,
  ).toBe("pending");

  // Poll A starts BEFORE the answer and is held open; it later resolves with
  // the stale pending row.
  let releaseDelayedPoll: () => void = () => {};
  const delayedPoll = new Promise<void>((resolve) => {
    releaseDelayedPoll = resolve;
  });
  let interactionCalls = 0;
  vi.spyOn(api, "interactionList").mockImplementation(async () => {
    const n = ++interactionCalls;
    if (n === 1) {
      await delayedPoll;
      return [pendingPlanReview()]; // stale pending row lands last
    }
    // Every newer response (the post-answer refresh and anything after)
    // resolves successfully with the row OMITTED — the normal pending-only
    // response after answer-committed.
    return [];
  });
  const delayedRefresh = hubStore.refresh(); // held on poll A
  await vi.waitFor(() => expect(interactionCalls).toBeGreaterThanOrEqual(1));

  // Answer POST succeeds; its follow-up refresh resolves with the EMPTY list
  // while poll A is still outstanding.
  vi.spyOn(api, "interactionRespond").mockResolvedValue({
    command: { commandId: "cmd_1", id: "cmd_1" } as never,
    relatedCommandIds: [],
  });
  await hubStore.respond(INTERACTION, {
    kind: "plan-review",
    optionId: "approve",
    planRevision: "1",
    planDigest: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
    feedback: null,
  });
  // The post-answer refresh really did run (it did not fail): the newer empty
  // list was applied while poll A was still in flight.
  expect(interactionCalls).toBeGreaterThanOrEqual(2);
  expect(hubStore.getSnapshot().answering[INTERACTION]).toBeUndefined();

  // Now poll A completes with the stale pending row.
  releaseDelayedPoll();
  await delayedRefresh;

  // The review is committed (or gone from the list) — never pending — and the
  // approve/deny controls cannot be submitted again.
  const row = hubStore.getSnapshot().interactions.find((i) => i.id === INTERACTION);
  expect(row === undefined || row.state !== "pending").toBe(true);
  if (row) expect(canSubmitAnswer(row)).toBe(false);
});
