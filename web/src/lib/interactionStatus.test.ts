import { describe, expect, it } from "vitest";
import type { Host } from "../types/instance";
import type { Interaction } from "../types/interaction";
import { known, unknownKnowledge, type Id } from "../types/wire";
import {
  answerPendingNative,
  canSubmitAnswer,
  hostOnline,
  nativeCleared,
  projectInteraction,
} from "./interactionStatus";

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

/** An interaction whose answer was committed on `deviceId`, with no native clearance. */
function committed(deviceId: string, patch: Partial<Interaction> = {}): Interaction {
  return { ...interaction("answer-committed", deviceId), delivery: "written", ...patch };
}

describe("answered but not natively cleared", () => {
  it("is recognised as still awaiting the native side", () => {
    expect(answerPendingNative(committed("dev_a"))).toBe(true);
    expect(answerPendingNative(interaction("pending"))).toBe(false);
  });

  it("is no longer pending once the harness clears it", () => {
    const cleared = committed("dev_a", { resolution: known({ reason: "native-cleared", eventIds: [] }) });
    expect(nativeCleared(cleared)).toBe(true);
    expect(answerPendingNative(cleared)).toBe(false);
  });

  it("other resolution reasons do not count as native clearance", () => {
    for (const reason of ["answered", "native-cancelled", "generation-ended", "timed-out"] as const) {
      const other = committed("dev_a", { resolution: known({ reason, eventIds: [] }) });
      expect(nativeCleared(other)).toBe(false);
      // Still waiting on the native side, so still not resubmittable.
      expect(answerPendingNative(other)).toBe(true);
    }
  });

  it("cannot be submitted a second time", () => {
    // The form may still be on screen; the old request must not resubmit.
    expect(canSubmitAnswer(committed("dev_a"), { deviceId: "dev_a", host: host("online") })).toBe(false);
    expect(projectInteraction(committed("dev_a"), { deviceId: "dev_a", host: host("online") })).toBe("settled");
  });

  it("stays unsubmittable while the local submit is in flight", () => {
    expect(canSubmitAnswer(interaction("pending"), { answering: true, host: host("online") })).toBe(false);
  });
});

describe("concurrent answers", () => {
  it("an answer from another device blocks this device and reads as superseded", () => {
    const elsewhere = committed("dev_other");
    expect(projectInteraction(elsewhere, { deviceId: "dev_a", host: host("online") })).toBe("superseded");
    expect(canSubmitAnswer(elsewhere, { deviceId: "dev_a", host: host("online") })).toBe(false);
  });

  it("the answering device sees its own answer as settled, and also cannot resubmit", () => {
    const mine = committed("dev_a");
    expect(projectInteraction(mine, { deviceId: "dev_a", host: host("online") })).toBe("settled");
    expect(canSubmitAnswer(mine, { deviceId: "dev_a", host: host("online") })).toBe(false);
  });

  it("a local submit racing a remote commit still cannot double-submit", () => {
    // This device believes it is submitting; the record already carries
    // another device's answer. `answering` must not reopen the form.
    const raced = committed("dev_other");
    expect(canSubmitAnswer(raced, { answering: true, deviceId: "dev_a", host: host("online") })).toBe(false);
  });

  it("resolved-elsewhere interactions are never submittable regardless of answerable", () => {
    const resolved = { ...committed("dev_other"), state: "resolved" as const };
    expect(canSubmitAnswer(resolved, { deviceId: "dev_a", host: host("online") })).toBe(false);
  });
});

describe("canSubmitAnswer — every other unsubmittable case", () => {
  it("a plain pending interaction on an online host is submittable", () => {
    expect(canSubmitAnswer(interaction("pending"), { host: host("online") })).toBe(true);
  });

  it("non-answerable requests are display-only", () => {
    const readOnly = { ...interaction("pending"), answerable: false };
    expect(canSubmitAnswer(readOnly, { host: host("online") })).toBe(false);
    // Still projects as pending — it is visible, just not actionable here.
    expect(projectInteraction(readOnly, { host: host("online") })).toBe("pending");
  });

  it("expired, invalidated, offline and disconnected all block submission", () => {
    expect(canSubmitAnswer(interaction("expired"), { host: host("online") })).toBe(false);
    expect(canSubmitAnswer(interaction("invalidated"), { host: host("online") })).toBe(false);
    expect(canSubmitAnswer(interaction("pending"), { host: host("offline") })).toBe(false);
    expect(canSubmitAnswer(interaction("pending"), { host: host("online"), connectivity: "disconnected" })).toBe(false);
  });

  it("a passed deadline blocks submission even while the state says pending", () => {
    const stale = {
      ...interaction("pending"),
      deadline: known("2000-01-01T00:00:00.000Z"),
    };
    expect(canSubmitAnswer(stale, { host: host("online") })).toBe(false);
  });
});
