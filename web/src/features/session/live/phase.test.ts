import { describe, expect, it } from "vitest";
import type { Observation } from "../../../types/generated";

/** Minimal envelope; the projections read only the fields listed here. */
function obs(partial: Partial<Observation> & Pick<Observation, "kind" | "payload">, seq: number, observedAt: string): Observation {
  return {
    eventId: `ev_${String(seq).padStart(3, "0")}`,
    journalId: "jrn_1",
    instanceId: "ins_1",
    hostId: "hos_1",
    processGeneration: "1",
    runGeneration: "1",
    runId: "run_1",
    seq: String(seq),
    observedAt,
    nativeAt: { state: "unknown", reason: "not-emitted", evidenceEventIds: [] },
    source: {
      adapterVersion: "test",
      channel: "hook",
      delivery: "live",
      driverKind: "shell-pty",
      driverVersion: "test",
      nativeAgentId: { state: "not-applicable" },
      nativeEventId: { state: "not-applicable" },
      nativeItemId: { state: "not-applicable" },
      nativeRequestId: { type: "none" },
      nativeSessionId: { state: "known", value: "s1" },
      nativeTurnId: { state: "not-applicable" },
      sourceCursor: { type: "runtime", ledgerRevision: String(seq) },
    },
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    ...partial,
  } as Observation;
}

function turnLive(
  seq: number,
  relatedIds: Record<string, string>,
  at: string,
  channel: "hook" | "screen" | "osc" = "hook",
  nativeName = "turn.live",
): Observation {
  const base = obs(
    {
      kind: "lifecycle",
      payload: {
        type: "native",
        topic: "turn",
        nativeName,
        nativeId: { state: "unknown", reason: "not-emitted", evidenceEventIds: [] },
        status: { state: "known", value: "working" },
        relatedIds,
        dataRef: null,
        severity: "info",
        affectsCompletion: false,
      },
      source: {
        adapterVersion: "test",
        channel,
        delivery: "live",
        driverKind: "shell-pty",
        driverVersion: "test",
        nativeAgentId: { state: "not-applicable" },
        nativeEventId: { state: "not-applicable" },
        nativeItemId: { state: "not-applicable" },
        nativeRequestId: { type: "none" },
        nativeSessionId: { state: "known", value: "s1" },
        nativeTurnId: { state: "not-applicable" },
        sourceCursor: { type: "runtime", ledgerRevision: String(seq) },
      },
    },
    seq,
    at,
  );
  return base;
}

import { isActivePhase, livePhase, toolAnchors, toolFingerprint } from "./phase";

const T0 = "2026-09-16T00:00:00.000Z";

const phase = (p: string, since = T0, extra: Record<string, string> = {}) => ({
  phase: p,
  since,
  provision: "native",
  tier: "hook",
  ...extra,
});

describe("livePhase", () => {
  it("latches the newest tagged turn lifecycle in seq order, not array order", () => {
    const events = [
      turnLive(3, phase("tool-started", "2026-09-16T00:00:02.000Z", { toolCallId: "t1", toolName: "Bash" }), "2026-09-16T00:00:02.000Z"),
      turnLive(1, phase("prompt-accepted", T0), T0),
      turnLive(2, phase("text-streaming", "2026-09-16T00:00:01.000Z", { messageId: "m1" }), "2026-09-16T00:00:01.000Z"),
    ];
    // Gap-backfill order: the oldest event sits last in the array.
    const got = livePhase([events[1]!, events[2]!, events[0]!]);
    expect(got?.phase).toBe("tool-started");
    expect(got?.toolCallId).toBe("t1");
    expect(got?.toolName).toBe("Bash");
  });

  it("unknown never collapses: events without a phase tag leave the latch untouched", () => {
    const events = [
      turnLive(1, phase("prompt-accepted", T0), T0),
      // A raw hook lifecycle with no live tags (legacy producer / raw event).
      turnLive(2, {}, "2026-09-16T00:00:05.000Z", "hook", "UserPromptSubmit"),
    ];
    expect(livePhase(events)?.phase).toBe("prompt-accepted");
  });

  it("a re-delivered chunk of one episode keeps the original since anchor", () => {
    const events = [
      turnLive(1, phase("text-streaming", T0, { messageId: "m1" }), T0),
      turnLive(2, phase("text-streaming", "2026-09-16T00:00:00.400Z", { messageId: "m1" }), "2026-09-16T00:00:00.400Z"),
      turnLive(3, phase("text-streaming", "2026-09-16T00:00:00.900Z", { messageId: "m1" }), "2026-09-16T00:00:00.900Z"),
    ];
    expect(livePhase(events)?.since).toBe(T0);
  });

  it("a second tool call is a new tool-started episode with its own anchor", () => {
    const events = [
      turnLive(1, phase("tool-started", T0, { toolCallId: "a" }), T0),
      turnLive(2, phase("tool-finished", "2026-09-16T00:00:01.000Z", { toolCallId: "a" }), "2026-09-16T00:00:01.000Z"),
      turnLive(3, phase("tool-started", "2026-09-16T00:00:02.000Z", { toolCallId: "b" }), "2026-09-16T00:00:02.000Z"),
    ];
    const got = livePhase(events);
    expect(got?.phase).toBe("tool-started");
    expect(got?.toolCallId).toBe("b");
    expect(got?.since).toBe("2026-09-16T00:00:02.000Z");
  });

  it("ignores the screen tier's busy-bit lifecycles: they carry no turn.live tags", () => {
    // The OSC/screen tiers announce status as an *entity* lifecycle, never as
    // a native turn lifecycle with a `phase` tag (design §2.4: status only).
    const screenStatus = {
      kind: "lifecycle",
      eventId: "ev_002",
      seq: "2",
      observedAt: T0,
      source: { channel: "pty" },
      payload: { type: "entity", entityType: "instance" },
    } as unknown as Observation;
    expect(livePhase([turnLive(1, phase("prompt-accepted", T0), T0), screenStatus])?.phase).toBe(
      "prompt-accepted",
    );
  });

  it("renders null with no tagged events at all", () => {
    expect(livePhase([turnLive(1, {}, T0, "hook", "SessionStart")])).toBeNull();
  });
});

describe("toolAnchors", () => {
  const known = <T,>(value: T) => ({ state: "known", value });
  const toolCallEvent = (
    nodeId: string,
    seq: number,
    name = "Bash",
    input: unknown = { command: "for i in 1 2 3 4 5; do sleep 3; done" },
  ): Observation =>
    ({
      kind: "tool_call",
      seq: String(seq),
      observedAt: T0,
      eventId: `ev_${seq}`,
      source: { channel: "hook" },
      payload: { toolCallId: nodeId, state: "running", toolName: known(name), input: known(input) },
    }) as unknown as Observation;

  it("pairs the tagged start with the adjacent hook ToolCall, keyed by content fingerprint", () => {
    // The tag carries the native tool_use_id; the anchor is keyed by the
    // name+input fingerprint so the transcript node (a different wire id on
    // the promoted path) joins the same clock. Gap-backfill order tolerated.
    const input = { command: "sleep 1" };
    const events = [
      toolCallEvent("obj-derived-1", 4, "Bash", input),
      turnLive(3, phase("tool-started", "2026-09-16T00:00:03.000Z", { toolCallId: "t1" }), "2026-09-16T00:00:03.000Z"),
      toolCallEvent("obj-derived-1", 2, "Bash", input),
      turnLive(1, phase("tool-started", T0, { toolCallId: "t1" }), T0),
    ];
    const anchors = toolAnchors(events);
    expect([...anchors.keys()]).toEqual([toolFingerprint("Bash", input)]);
    expect([...anchors.values()][0]?.since).toBe(T0);
  });

  it("a re-fired PreToolUse claims neither a new anchor nor a later tool's clock", () => {
    const inputA = { command: "a" };
    const inputB = { command: "b" };
    const events = [
      turnLive(1, phase("tool-started", T0, { toolCallId: "a" }), T0),
      toolCallEvent("obj-a", 2, "Bash", inputA),
      turnLive(3, phase("tool-started", "2026-09-16T00:00:01.000Z", { toolCallId: "a" }), "2026-09-16T00:00:01.000Z"),
      turnLive(4, phase("tool-started", "2026-09-16T00:00:02.000Z", { toolCallId: "b" }), "2026-09-16T00:00:02.000Z"),
      toolCallEvent("obj-b", 5, "Bash", inputB),
    ];
    const anchors = toolAnchors(events);
    expect([...anchors.keys()].sort()).toEqual(
      [toolFingerprint("Bash", inputA), toolFingerprint("Bash", inputB)].sort(),
    );
    expect(anchors.get(toolFingerprint("Bash", inputA))?.since).toBe(T0);
  });
});

describe("isActivePhase", () => {
  it("treats turn-ended and interrupted as inactive, blocked as active", () => {
    expect(isActivePhase("turn-ended")).toBe(false);
    expect(isActivePhase("interrupted")).toBe(false);
    expect(isActivePhase("blocked")).toBe(true);
    expect(isActivePhase("tool-started")).toBe(true);
  });
});
