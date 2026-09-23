import { afterEach, expect, it, vi } from "vitest";
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
