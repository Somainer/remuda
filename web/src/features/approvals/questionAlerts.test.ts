import { describe, expect, it } from "vitest";
import type { Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import { known } from "../../types/wire";
import {
  formatQuestionCountdown,
  selectQuestionAlerts,
} from "./questionAlerts";

const NOW = Date.parse("2026-10-06T12:00:00.000Z");

function interaction(partial: Partial<Interaction>): Interaction {
  return {
    id: "int-1",
    revision: "1",
    createdAt: new Date(NOW).toISOString(),
    updatedAt: new Date(NOW).toISOString(),
    instanceId: "ins-1",
    runId: "run-1",
    hostId: "hst-1",
    kind: "question",
    requestKey: {
      native: { type: "rpc", valueType: "string", value: "ask" },
      processGeneration: "1",
      runGeneration: "1",
      connectionEpoch: "ep-1",
    },
    requestVersion: "1",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "claude-control",
    request: { kind: "question", title: "Q", fields: [] },
    deadline: known(new Date(NOW + 15 * 60_000).toISOString()),
    deadlineSource: "runtime-policy",
    answer: { state: "not-applicable" },
    delivery: "written",
    resolution: { state: "not-applicable" },
    ...partial,
  };
}

function instance(partial: Partial<Instance> = {}): Instance {
  return {
    id: "ins-1",
    revision: "1",
    createdAt: new Date(NOW).toISOString(),
    updatedAt: new Date(NOW).toISOString(),
    hostId: "hst-1",
    workspaceId: "ws-1",
    kind: "claude",
    driver: "claude-sdk",
    lifecycle: "running",
    activity: known("waiting-interaction"),
    connectivity: "connected",
    ownership: "managed",
    nativeRef: {
      hostId: "hst-1",
      nativeStoreId: "ins-1",
      kind: "claude",
      sessionId: { state: "unknown", reason: "x", evidenceEventIds: [] },
      transcript: { state: "unknown", reason: "x", evidenceEventIds: [] },
    },
    processRef: {
      processGeneration: "1",
      processIdentity: { state: "unknown", reason: "x", evidenceEventIds: [] },
      connectionEpoch: "hst-1",
    },
    specRevision: "1",
    launchId: { state: "unknown", reason: "x", evidenceEventIds: [] },
    capabilities: {
      signalTier: "none",
      capabilities: {},
    },
    ownerFence: "1",
    activeRunIds: [],
    parent: null,
    journalId: "obj-j",
    durableSeq: "1",
    exit: { state: "not-applicable" },
    ...partial,
  } as Instance;
}

function source(interactions: Interaction[], extra: Record<string, unknown> = {}) {
  return {
    interactions,
    instances: [instance()],
    activeSessionId: null,
    titleOf: () => "main",
    nowMs: NOW,
    ...extra,
  };
}

describe("selectQuestionAlerts", () => {
  it("alerts for an arriving pending question the owner is not viewing", () => {
    const alerts = selectQuestionAlerts(source([interaction({})]));
    expect(alerts).toHaveLength(1);
    expect(alerts[0].interactionId).toBe("int-1");
    expect(alerts[0].title).toBe("main");
    expect(alerts[0].deadline).toBe(new Date(NOW + 15 * 60_000).toISOString());
  });

  it("alerts for elicitation and plan-review but not approval", () => {
    const kinds: Interaction["kind"][] = ["elicitation", "plan-review", "approval"];
    const rows = kinds.map((kind, _i) =>
      interaction({
        id: `int-${kind}`,
        kind,
        request:
          kind === "elicitation"
            ? {
                kind: "elicitation",
                title: "E",
                mode: "form",
                schemaRef: null,
                schemaDialect: null,
                url: null,
                nativeExtension: null,
                allowedActions: ["accept"],
              }
            : kind === "plan-review"
              ? {
                  kind: "plan-review",
                  title: "P",
                  planRef: "obj-p",
                  planRevision: "1",
                  planDigest: "sha256:" + "0".repeat(64),
                  options: [],
                  allowFeedback: false,
                }
              : {
                  kind: "approval",
                  title: "A",
                  description: "",
                  toolCallId: null,
                  actionRef: "obj-a",
                  options: [],
                  requestedPermissionsRef: null,
                  inputDigest: "sha256:" + "0".repeat(64),
                },
      }),
    );
    const alerts = selectQuestionAlerts(source(rows));
    expect(alerts.map((a) => a.interactionId).sort()).toEqual([
      "int-elicitation",
      "int-plan-review",
    ]);
  });

  it("suppresses a question on the session the owner is actively viewing", () => {
    const alerts = selectQuestionAlerts(source([interaction({})], { activeSessionId: "ins-1" }));
    expect(alerts).toHaveLength(0);
  });

  it("suppresses a question the owner is answering on this device", () => {
    const alerts = selectQuestionAlerts(
      source([interaction({})], { answering: new Set(["int-1"]) }),
    );
    expect(alerts).toHaveLength(0);
  });

  it("does not alert for non-pending rows (expired, settled, paused/offline)", () => {
    const expired = interaction({ id: "exp", state: "expired" });
    const resolved = interaction({
      id: "res",
      state: "resolved",
      answer: {
        state: "known",
        value: {
          commandId: "cmd-1",
          actor: { principalId: "d1", type: "human", deviceId: "d1", instanceId: null },
          value: { kind: "question", answers: {} },
          committedAt: new Date(NOW).toISOString(),
        },
      },
    });
    const paused = interaction({ id: "pause" });
    const offlineInstance = instance({ connectivity: "disconnected" });
    const alerts = selectQuestionAlerts({
      interactions: [expired, resolved, paused],
      instances: [offlineInstance],
      activeSessionId: null,
      titleOf: () => "main",
      nowMs: NOW,
    });
    expect(alerts).toHaveLength(0);
  });

  it("surfaces a null deadline when the interaction reported none", () => {
    const alerts = selectQuestionAlerts(
      source([interaction({ id: "x", deadline: { state: "not-applicable" } })]),
    );
    expect(alerts[0].deadline).toBeNull();
  });
});

describe("formatQuestionCountdown", () => {
  it("renders whole minutes above a minute", () => {
    const deadline = new Date(NOW + 12 * 60_000 + 30_000).toISOString();
    expect(formatQuestionCountdown(deadline, NOW)).toBe("还剩 12 分钟，超时将自动拒绝");
  });

  it("renders seconds under a minute (ceil, at least 1)", () => {
    const deadline = new Date(NOW + 45_000).toISOString();
    expect(formatQuestionCountdown(deadline, NOW)).toBe("还剩 45 秒，超时将自动拒绝");
  });

  it("returns null with no deadline or after it passes", () => {
    expect(formatQuestionCountdown(null, NOW)).toBeNull();
    expect(
      formatQuestionCountdown(new Date(NOW - 1000).toISOString(), NOW),
    ).toBeNull();
    expect(formatQuestionCountdown("not-a-date", NOW)).toBeNull();
  });
});
