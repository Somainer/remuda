import { describe, expect, it } from "vitest";
import type { Interaction } from "../../types/interaction";
import { optionAnswerFor } from "./answers";

function interaction(kind: Interaction["request"]["kind"]): Interaction {
  const base = {
    id: "int_1",
    revision: "1",
    createdAt: "2026-09-20T10:00:00.000Z",
    updatedAt: "2026-09-20T10:00:00.000Z",
    instanceId: "ins_1",
    runId: null,
    hostId: "hst_1",
    requestKey: {
      native: { type: "rpc", valueType: "string", value: "tool" },
      processGeneration: "1",
      runGeneration: "1",
      connectionEpoch: "hst_1",
    },
    requestVersion: "1",
    state: "pending" as const,
    blocking: true,
    answerable: true,
    carrier: "claude-control" as const,
    deadline: { state: "unknown", reason: "none", evidenceEventIds: [] },
    deadlineSource: "none" as const,
    answer: { state: "not-applicable" },
    delivery: "not-sent" as const,
    resolution: { state: "not-applicable" },
  };
  if (kind === "approval") {
    return {
      ...base,
      kind,
      request: {
        kind: "approval",
        title: "Bash",
        description: "rm -rf /tmp/x",
        toolCallId: null,
        actionRef: "act_1",
        options: [],
        requestedPermissionsRef: null,
        inputDigest: "sha256:aaaa",
      },
    } as Interaction;
  }
  if (kind === "plan-review") {
    return {
      ...base,
      kind,
      request: {
        kind: "plan-review",
        title: "实施计划",
        planRef: "plan_1",
        planRevision: "7",
        planDigest: "sha256:bbbb",
        options: [],
        allowFeedback: false,
      },
    } as Interaction;
  }
  return {
    ...base,
    kind: "question",
    request: { kind: "question", title: "AskUserQuestion", fields: [] },
  } as Interaction;
}

describe("optionAnswerFor", () => {
  it("builds the approval answer with the option id and request digest", () => {
    expect(optionAnswerFor(interaction("approval"), "allow-once")).toEqual({
      kind: "approval",
      optionId: "allow-once",
      inputDigest: "sha256:aaaa",
    });
  });

  it("builds the plan-review answer with revision, digest and null feedback", () => {
    expect(optionAnswerFor(interaction("plan-review"), "reject-plan")).toEqual({
      kind: "plan-review",
      optionId: "reject-plan",
      planRevision: "7",
      planDigest: "sha256:bbbb",
      feedback: null,
    });
  });

  it("has no option answer for questions", () => {
    expect(optionAnswerFor(interaction("question"), "x")).toBeNull();
  });
});
