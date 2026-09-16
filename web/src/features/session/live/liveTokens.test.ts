import { describe, expect, it } from "vitest";
import type { Observation } from "../../../types/generated";
import { usageOutputTokens } from "./liveTokens";

function phaseEvent(seq: number, phase: string): Observation {
  return {
    kind: "lifecycle",
    eventId: `ev_p_${seq}`,
    seq: String(seq),
    observedAt: "2026-09-16T10:00:00.000Z",
    journalId: "ins_1",
    instanceId: "ins_1",
    hostId: "hos_1",
    processGeneration: "1",
    runGeneration: "1",
    runId: "run_1",
    nativeAt: { state: "not-applicable" },
    source: {
      adapterVersion: "t",
      channel: "hook",
      delivery: "live",
      driverKind: "shell-pty",
      driverVersion: "t",
      nativeAgentId: { state: "not-applicable" },
      nativeEventId: { state: "not-applicable" },
      nativeItemId: { state: "not-applicable" },
      nativeRequestId: { type: "none" },
      nativeSessionId: { state: "known", value: "s" },
      nativeTurnId: { state: "not-applicable" },
      sourceCursor: { type: "runtime", value: { ledgerRevision: String(seq) } },
    },
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    payload: {
      type: "native",
      topic: "turn",
      nativeName: "turn.live",
      nativeId: { state: "not-applicable" },
      status: { state: "known", value: "working" },
      relatedIds: { phase, tier: "hook", provision: "native" },
      dataRef: null,
      severity: "info",
      affectsCompletion: false,
    },
  } as unknown as Observation;
}

function usageEvent(seq: number, output: string | null, scope = "message"): Observation {
  return {
    kind: "usage",
    eventId: `ev_u_${seq}`,
    seq: String(seq),
    observedAt: "2026-09-16T10:00:00.000Z",
    journalId: "ins_1",
    instanceId: "ins_1",
    hostId: "hos_1",
    processGeneration: "1",
    runGeneration: "1",
    runId: "run_1",
    nativeAt: { state: "not-applicable" },
    source: {
      adapterVersion: "t",
      channel: "transcript",
      delivery: "live",
      driverKind: "shell-pty",
      driverVersion: "t",
      nativeAgentId: { state: "not-applicable" },
      nativeEventId: { state: "not-applicable" },
      nativeItemId: { state: "not-applicable" },
      nativeRequestId: { type: "none" },
      nativeSessionId: { state: "known", value: "s" },
      nativeTurnId: { state: "not-applicable" },
      sourceCursor: { type: "runtime", value: { ledgerRevision: String(seq) } },
    },
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    payload: {
      usageId: "obj_u1",
      metricRevision: String(seq),
      scope,
      mode: "snapshot",
      accounting: "api",
      inputAccounting: "api",
      inputTokens: { state: "known", value: "10" },
      outputTokens: output == null ? { state: "unknown", reason: "pending", evidenceEventIds: [] } : { state: "known", value: output },
      totalTokens: { state: "known", value: String(Number(output ?? 0) + 10) },
      cacheReadTokens: { state: "known", value: "0" },
      cacheWriteTokens: { state: "known", value: "0" },
      reasoningTokens: { state: "known", value: "0" },
      cost: { state: "unknown", reason: "pending", evidenceEventIds: [] },
      nativeFieldsRef: null,
    },
  } as unknown as Observation;
}

describe("usageOutputTokens", () => {
  it("is null without usage", () => {
    expect(usageOutputTokens([phaseEvent(1, "prompt-accepted")])).toBeNull();
  });

  it("prefers the newest turn usage over the screen number elsewhere", () => {
    const events = [
      phaseEvent(1, "prompt-accepted"),
      usageEvent(2, "204"),
      usageEvent(5, "432"),
    ];
    expect(usageOutputTokens(events)).toBe(432);
  });

  it("ignores usage from a previous turn", () => {
    const events = [
      phaseEvent(1, "prompt-accepted"),
      usageEvent(2, "999"),
      phaseEvent(3, "turn-ended"),
      phaseEvent(4, "prompt-accepted"),
    ];
    expect(usageOutputTokens(events)).toBeNull();
  });

  it("ignores unknown counts and session scope", () => {
    expect(usageOutputTokens([phaseEvent(1, "prompt-accepted"), usageEvent(2, null)])).toBeNull();
    expect(
      usageOutputTokens([phaseEvent(1, "prompt-accepted"), usageEvent(2, "77", "session")]),
    ).toBeNull();
  });
});
