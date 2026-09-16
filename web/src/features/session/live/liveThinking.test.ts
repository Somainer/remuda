import { describe, expect, it } from "vitest";
import type { Observation } from "../../../types/generated";
import type { TranscriptNode } from "../assemble";
import { LIVE_THINKING_ID, mountLiveThinking } from "./liveThinking";

type LifecycleObs = Extract<Observation, { kind: "lifecycle" }>;

function lifecycle(
  seq: number,
  nativeName: string,
  relatedIds: Record<string, string>,
  channel: Observation["source"]["channel"] = "hook",
): Observation {
  return {
    kind: "lifecycle",
    eventId: `ev_${seq}`,
    journalId: "jrn_1",
    instanceId: "ins_1",
    hostId: "hos_1",
    processGeneration: "1",
    runGeneration: "1",
    runId: "run_1",
    seq: String(seq),
    observedAt: "2026-09-16T10:00:00.000Z",
    nativeAt: { state: "not-applicable" },
    source: {
      adapterVersion: "t",
      channel,
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
    completeness: channel === "screen" ? "screen-derived" : "structured",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    payload: {
      type: "native",
      topic: "turn",
      nativeName,
      nativeId: { state: "not-applicable" },
      status: { state: "known", value: "working" },
      relatedIds,
      dataRef: null,
      severity: "info",
      affectsCompletion: false,
    },
  } as unknown as LifecycleObs;
}

const promptAccepted = lifecycle(1, "turn.live", {
  phase: "prompt-accepted",
  since: "2026-09-16T10:00:00.000Z",
  tier: "hook",
  provision: "native",
});

const thinkingScreen = lifecycle(
  2,
  "live.status",
  {
    liveStatus: "1",
    tier: "screen",
    provision: "emulated",
    verb: "Forging",
    phrase: "thinking with xhigh effort",
  },
  "pty",
);

const toolStarted = lifecycle(3, "turn.live", {
  phase: "tool-started",
  since: "2026-09-16T10:00:05.000Z",
  tier: "hook",
  provision: "native",
  toolCallId: "t1",
});

const clear = lifecycle(
  4,
  "live.status",
  { liveStatus: "0", tier: "screen", provision: "emulated" },
  "pty",
);

const thinkingRow = () => ({
  type: "thought" as const,
  id: LIVE_THINKING_ID,
  text: "",
  completeness: "screen-derived" as const,
});

describe("mountLiveThinking", () => {
  it("mounts a collapsed screen-derived thought row while the phrase says thinking", () => {
    const nodes = mountLiveThinking([], [promptAccepted, thinkingScreen]);
    expect(nodes).toHaveLength(1);
    expect(nodes[0]).toEqual(thinkingRow());
  });

  it("is idempotent across re-renders", () => {
    const once = mountLiveThinking([], [promptAccepted, thinkingScreen]);
    const twice = mountLiveThinking(once, [promptAccepted, thinkingScreen]);
    expect(twice).toHaveLength(1);
    expect(twice[0]!.id).toBe(LIVE_THINKING_ID);
  });

  it("removes the hint the moment the first tool starts", () => {
    const nodes = mountLiveThinking([thinkingRow()], [promptAccepted, thinkingScreen, toolStarted]);
    expect(nodes).toHaveLength(0);
  });

  it("removes the hint on the live-status clear", () => {
    const nodes = mountLiveThinking([thinkingRow()], [promptAccepted, thinkingScreen, clear]);
    expect(nodes).toHaveLength(0);
  });

  it("never mounts for a non-thinking phrase", () => {
    const hookPhrase = lifecycle(
      2,
      "live.status",
      { liveStatus: "1", tier: "screen", provision: "emulated", verb: "Contemplating", phrase: "running UserPromptSubmit hook" },
      "pty",
    );
    expect(mountLiveThinking([], [promptAccepted, hookPhrase])).toHaveLength(0);
  });

  it("never duplicates a real thought node", () => {
    const real: TranscriptNode = {
      type: "thought",
      id: "obj_real_thought",
      text: "actual reasoning",
      completeness: "structured",
    } as TranscriptNode;
    const nodes = mountLiveThinking([real], [promptAccepted, thinkingScreen]);
    expect(nodes).toHaveLength(1);
    expect(nodes[0]!.id).toBe("obj_real_thought");
  });
});
