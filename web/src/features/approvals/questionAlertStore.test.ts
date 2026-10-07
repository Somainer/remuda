import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { hubStore } from "../../lib/store";
import { notifyStore } from "../../lib/notify";
import { questionAlertWatcher } from "./questionAlertStore";
import type { Interaction } from "../../types/interaction";
import { known } from "../../types/wire";
import type { Instance } from "../../types/instance";

function question(id: string, deadlineOffsetMs = 15 * 60_000): Interaction {
  return {
    id,
    revision: "1",
    createdAt: new Date().toISOString(),
    updatedAt: new Date().toISOString(),
    instanceId: "ins-q",
    runId: "run-q",
    hostId: "hst-q",
    kind: "question",
    requestKey: {
      native: { type: "rpc", valueType: "string", value: "ask" },
      processGeneration: "1",
      runGeneration: "1",
      connectionEpoch: "ep",
    },
    requestVersion: "1",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "claude-control",
    request: { kind: "question", title: "Q", fields: [] },
    deadline: known(new Date(Date.now() + deadlineOffsetMs).toISOString()),
    deadlineSource: "runtime-policy",
    answer: { state: "not-applicable" },
    delivery: "written",
    resolution: { state: "not-applicable" },
  };
}

function instance(id: string): Instance {
  return {
    id,
    revision: "1",
    createdAt: new Date().toISOString(),
    updatedAt: new Date().toISOString(),
    hostId: "hst-q",
    workspaceId: "ws",
    kind: "claude",
    driver: "claude-sdk",
    lifecycle: "running",
    activity: known("waiting-interaction"),
    connectivity: "connected",
    ownership: "managed",
    nativeRef: {
      hostId: "hst-q",
      nativeStoreId: id,
      kind: "claude",
      sessionId: { state: "unknown", reason: "x", evidenceEventIds: [] },
      transcript: { state: "unknown", reason: "x", evidenceEventIds: [] },
    },
    processRef: {
      processGeneration: "1",
      processIdentity: { state: "unknown", reason: "x", evidenceEventIds: [] },
      connectionEpoch: "hst-q",
    },
    specRevision: "1",
    launchId: { state: "unknown", reason: "x", evidenceEventIds: [] },
    capabilities: { signalTier: "none", capabilities: {} },
    ownerFence: "1",
    activeRunIds: [],
    parent: null,
    journalId: "obj",
    durableSeq: "1",
    exit: { state: "not-applicable" },
  } as unknown as Instance;
}

/** Publish an interactions page. `hydrated=false` simulates the
 * ready-with-empty-list state a FAILED first bootstrap fetch leaves. */
function setHub(
  interactions: Interaction[],
  opts: { hydrated?: boolean; instances?: Instance[] } = {},
): void {
  hubStore.setSlicesForTest({
    interactions,
    instances: opts.instances ?? [instance("ins-q")],
    hosts: [],
    interactionsHydrated: opts.hydrated ?? true,
  });
}

function blockingAlerts() {
  return notifyStore
    .getState()
    .blocking.filter((n) => n.key?.startsWith("question-alert:"));
}

beforeEach(() => {
  questionAlertWatcher.reset();
  questionAlertWatcher.activeSessionOverride = null;
  notifyStore.reset();
  document.title = "Remuda";
  // An unhydrated, empty page: the watcher must not baseline from this.
  setHub([], { hydrated: false });
});

afterEach(() => {
  questionAlertWatcher.stop();
  questionAlertWatcher.reset();
  notifyStore.reset();
  document.title = "Remuda";
});

describe("questionAlertWatcher — baseline vs arrival", () => {
  it("baselines questions present at hydration (history) without toasting, then alerts new arrivals", () => {
    questionAlertWatcher.start();
    // First successful page already carries a pending question: badge only.
    setHub([question("int-old")]);
    expect(questionAlertWatcher.posted).toHaveLength(0);
    expect(blockingAlerts()).toHaveLength(0);
    expect(questionAlertWatcher.activeAlerts()).toHaveLength(1);
    expect(document.title).toBe("(1) Remuda");

    // A fresh arrival posts the standing notification and bumps the badge.
    setHub([question("int-old"), question("int-new")]);
    expect(questionAlertWatcher.posted.map((a) => a.interactionId)).toEqual(["int-new"]);
    expect(blockingAlerts()).toHaveLength(1);
    expect(document.title).toBe("(2) Remuda");

    // A re-evaluation with no new id does not re-toast.
    setHub([question("int-old"), question("int-new")]);
    expect(questionAlertWatcher.posted).toHaveLength(1);
  });

  it("(r2-3a) a failed first fetch does not baseline; a later success must not toast already-pending questions", () => {
    questionAlertWatcher.start();
    // Failed bootstrap left the store ready with an EMPTY unhydrated list.
    setHub([], { hydrated: false });
    expect(questionAlertWatcher.activeAlerts()).toHaveLength(0);

    // The first SUCCESSFUL interaction-list read arrives with a question
    // already pending — this is the real baseline, not an arrival.
    setHub([question("int-existing")]);
    expect(questionAlertWatcher.posted).toHaveLength(0);
    expect(blockingAlerts()).toHaveLength(0);
    expect(document.title).toBe("(1) Remuda");

    // Only a genuinely later arrival alerts.
    setHub([question("int-existing"), question("int-later")]);
    expect(questionAlertWatcher.posted.map((a) => a.interactionId)).toEqual(["int-later"]);
  });

  it("(r2-3b) a question seen while viewing its session never toasts when the owner leaves the session", () => {
    questionAlertWatcher.activeSessionOverride = "ins-q";
    questionAlertWatcher.start();
    setHub([question("int-viewed")]);
    // Viewing: no alert, but the id is recorded as observed.
    expect(questionAlertWatcher.posted).toHaveLength(0);
    expect(questionAlertWatcher.activeAlerts()).toHaveLength(0);

    // The owner navigates away; the same question is still pending. It must
    // not be toasted as new (it was observed on every earlier pass).
    questionAlertWatcher.activeSessionOverride = null;
    setHub([question("int-viewed")]);
    expect(questionAlertWatcher.posted).toHaveLength(0);
    expect(blockingAlerts()).toHaveLength(0);
    expect(questionAlertWatcher.activeAlerts()).toHaveLength(1);
  });

  it("(r2-3b) a question observed while answering on this device never toasts once the answer view closes", () => {
    questionAlertWatcher.start();
    // The question arrives while the owner is mid-answer on this device.
    hubStore.setSlicesForTest({ answering: { "int-answering": true as const } });
    setHub([question("int-answering")]);
    expect(questionAlertWatcher.activeAlerts()).toHaveLength(0);
    expect(questionAlertWatcher.posted).toHaveLength(0);

    // Answer view clears with the question still pending: not a new arrival.
    hubStore.setSlicesForTest({ answering: {} });
    setHub([question("int-answering")]);
    expect(questionAlertWatcher.posted).toHaveLength(0);
    expect(blockingAlerts()).toHaveLength(0);
  });
});

describe("questionAlertWatcher — alerts leave when the interaction does", () => {
  it("(r2-2) answering the question on another device dismisses the standing alert and badge", () => {
    questionAlertWatcher.start();
    setHub([]); // hydrated empty baseline
    setHub([question("int-live")]);
    expect(blockingAlerts()).toHaveLength(1);
    expect(document.title).toBe("(1) Remuda");

    // The pending list drops the interaction (resolved/removed server-side).
    setHub([]);
    expect(blockingAlerts().length, "the standing notification must be dismissed").toBe(0);
    expect(questionAlertWatcher.activeAlerts()).toHaveLength(0);
    expect(document.title).toBe("Remuda");

    // It never re-toasts if a stale page briefly brings it back resolved.
    const resolved = { ...question("int-live"), state: "resolved" as const };
    setHub([resolved]);
    expect(questionAlertWatcher.posted).toHaveLength(1);
    expect(blockingAlerts()).toHaveLength(0);
  });

  it("(r2-2) expiry dismisses the standing alert and badge", () => {
    vi.useFakeTimers();
    try {
      questionAlertWatcher.start();
      setHub([]);
      setHub([question("int-expiring", 30_000)]);
      expect(blockingAlerts()).toHaveLength(1);

      // Past the deadline: no Hub update happens in this test; the watcher's
      // own deadline re-evaluation must clear both surfaces.
      vi.advanceTimersByTime(32_000);
      expect(blockingAlerts()).toHaveLength(0);
      expect(questionAlertWatcher.activeAlerts()).toHaveLength(0);
      expect(document.title).toBe("Remuda");
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("questionAlertWatcher — deadline-driven re-evaluation", () => {
  it("(r2-4) the title badge clears at the deadline with NO hub update while polling stalls", () => {
    vi.useFakeTimers();
    try {
      questionAlertWatcher.start();
      // Baseline already carries the question (badge, no toast).
      setHub([question("int-deadline", 60_000)]);
      expect(questionAlertWatcher.posted).toHaveLength(0);
      expect(document.title).toBe("(1) Remuda");

      // No setHub call for the rest of the test: the poll has stalled.
      vi.advanceTimersByTime(62_000);
      expect(document.title).toBe("Remuda");
      expect(questionAlertWatcher.activeAlerts()).toHaveLength(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("(r2-5) the standing notification carries the countdown text", () => {
    questionAlertWatcher.start();
    setHub([]);
    setHub([question("int-count")]);
    const note = blockingAlerts()[0];
    expect(note).toBeTruthy();
    expect(note.reason).toMatch(/还剩 \d+ 分钟，超时将自动拒绝/);
  });
});
