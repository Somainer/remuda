import { beforeEach, describe, expect, it } from "vitest";
import { hubStore } from "../../lib/store";
import { notifyStore } from "../../lib/notify";
import { questionAlertWatcher } from "./questionAlertStore";
import type { Interaction } from "../../types/interaction";
import { known } from "../../types/wire";
import type { Instance } from "../../types/instance";

const DEADLINE = new Date(Date.now() + 15 * 60_000).toISOString();

function question(id: string): Interaction {
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
    deadline: known(DEADLINE),
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
  } as Instance;
}

function setHub(interactions: Interaction[], instances: Instance[] = [instance("ins-q")]) {
  hubStore.setSlicesForTest({ interactions, instances, hosts: [] });
}

describe("questionAlertWatcher", () => {
  beforeEach(() => {
    questionAlertWatcher.reset();
    questionAlertWatcher.activeSessionOverride = null;
    notifyStore.reset();
    setHub([]);
  });

  it("baselines questions present at hydration (history) without toasting, then alerts new arrivals", () => {
    questionAlertWatcher.start();
    // Baseline: an already-present question badges but does not toast.
    setHub([question("int-old")]);
    questionAlertWatcher.reset(); // stop/reset to control baseline timing cleanly
    questionAlertWatcher.start();
    setHub([question("int-old")]);
    expect(questionAlertWatcher.posted).toHaveLength(0);
    expect(questionAlertWatcher.activeAlerts()).toHaveLength(1);
    expect(document.title).toBe("(1) Remuda");

    // A fresh arrival toasts.
    setHub([question("int-old"), question("int-new")]);
    expect(questionAlertWatcher.posted.map((a) => a.interactionId)).toEqual(["int-new"]);
    expect(document.title).toBe("(2) Remuda");

    // A re-evaluation with no new id does not re-toast.
    setHub([question("int-old"), question("int-new")]);
    expect(questionAlertWatcher.posted).toHaveLength(1);
    questionAlertWatcher.stop();
  });

  it("does not alert a question on the session being viewed", () => {
    questionAlertWatcher.activeSessionOverride = "ins-q";
    questionAlertWatcher.start();
    setHub([]);
    questionAlertWatcher.reset();
    questionAlertWatcher.activeSessionOverride = "ins-q";
    questionAlertWatcher.start();
    setHub([question("int-viewing")]);
    expect(questionAlertWatcher.posted).toHaveLength(0);
    expect(questionAlertWatcher.activeAlerts()).toHaveLength(0);
    expect(document.title).not.toContain("(");
    questionAlertWatcher.stop();
  });

  it("clears the title badge when the question is resolved", () => {
    questionAlertWatcher.start();
    setHub([question("int-live")]);
    expect(document.title).toBe("(1) Remuda");
    const resolved = { ...question("int-live"), state: "resolved" as const };
    setHub([resolved]);
    expect(questionAlertWatcher.activeAlerts()).toHaveLength(0);
    expect(document.title).toBe("Remuda");
    questionAlertWatcher.stop();
  });
});
