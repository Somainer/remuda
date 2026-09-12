import { describe, expect, it } from "vitest";
import type { Host } from "../types/instance";
import type { Interaction } from "../types/interaction";
import { known, unknownKnowledge, type Id } from "../types/wire";
import { hostOnline, projectInteraction } from "./interactionStatus";

function host(state: Host["state"]): Host {
  return {
    id: "hst" as Id,
    revision: "1",
    createdAt: "2026-09-12T00:00:00.000Z",
    updatedAt: "2026-09-12T00:00:00.000Z",
    label: "box",
    ownerPrincipalId: "prn" as Id,
    state,
    transport: { mode: "outbound-wss", endpointRef: "obj" as Id },
  };
}

function interaction(state: Interaction["state"], deviceId: string | null = null): Interaction {
  return {
    id: "int" as Id,
    revision: "1",
    createdAt: "2026-09-12T00:00:00.000Z",
    updatedAt: "2026-09-12T00:00:00.000Z",
    instanceId: "ins" as Id,
    runId: null,
    hostId: "hst" as Id,
    kind: "approval",
    requestKey: {
      native: { type: "none" },
      processGeneration: "1",
      runGeneration: null,
      connectionEpoch: "epoch" as Id,
    },
    requestVersion: "1",
    state,
    blocking: true,
    answerable: true,
    carrier: "claude-control",
    request: {
      kind: "approval",
      title: "Bash",
      description: "ls",
      toolCallId: null,
      actionRef: "obj" as Id,
      options: [],
      requestedPermissionsRef: null,
      inputDigest: "sha256:00",
    },
    deadline: unknownKnowledge("none"),
    deadlineSource: "none",
    answer: deviceId
      ? known({
          commandId: "cmd" as Id,
          actor: { principalId: "prn" as Id, type: "human", deviceId, instanceId: null },
          value: { kind: "approval", optionId: "allow-once", inputDigest: "sha256:00" },
          committedAt: "2026-09-12T00:00:00.000Z",
        })
      : { state: "not-applicable" },
    delivery: "not-sent",
    resolution: { state: "not-applicable" },
  };
}

describe("projectInteraction", () => {
  it("pending / answering / settled / expired / superseded / paused", () => {
    expect(projectInteraction(interaction("pending"), { host: host("online") })).toBe("pending");
    expect(projectInteraction(interaction("pending"), { answering: true, host: host("online") })).toBe("answering");
    expect(projectInteraction(interaction("answer-committed", "dev_a"), { deviceId: "dev_a", host: host("online") })).toBe("settled");
    expect(projectInteraction(interaction("answer-committed", "dev_other"), { deviceId: "dev_a", host: host("online") })).toBe("superseded");
    expect(projectInteraction(interaction("expired"), { host: host("online") })).toBe("expired");
    expect(projectInteraction(interaction("pending"), { host: host("offline") })).toBe("paused");
    expect(projectInteraction(interaction("invalidated"), { host: host("online") })).toBe("superseded");
  });

  it("hostOnline", () => {
    expect(hostOnline(host("online"))).toBe(true);
    expect(hostOnline(host("offline"))).toBe(false);
    expect(hostOnline(host("online"), "disconnected")).toBe(false);
  });
});
