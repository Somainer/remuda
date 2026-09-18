import { describe, expect, it } from "vitest";
import type { Observation } from "../../../types/generated";
import type { NativeRef } from "../../../types/nativeRef";
import { projectTurnDecision, turnStartAnchor } from "./turnDecision";

/**
 * These reproduce the owner's own incident (journal of ins_01a0b3b2,
 * 2026-09-18 17:03): hook Stop idle, then pty agent_status idle, then a hook
 * Notification carrying Claude Code's idle prompt. The Notification used to be
 * classified as blocked and latched; now it must neither raise the phase nor
 * stop the Stop/pty from ending the turn and freeing the held queue.
 */

function base(seq: number, observedAt: string, channel: string): Partial<Observation> {
  return {
    eventId: `ev_${seq}`,
    journalId: "jrn_1",
    instanceId: "ins_1",
    hostId: "hos_1",
    processGeneration: "1",
    runGeneration: "1",
    runId: "run_1",
    seq: String(seq),
    observedAt,
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
      sourceCursor: { type: "runtime", ledgerRevision: String(seq) },
    } as unknown as Observation["source"],
    completeness: channel === "hook" ? "structured" : "screen-derived",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
  };
}

function native(
  seq: number,
  at: string,
  channel: string,
  nativeName: string,
  status: string,
  relatedIds: Record<string, string>,
): Observation {
  return {
    ...base(seq, at, channel),
    kind: "lifecycle",
    payload: {
      type: "native",
      topic: "turn",
      nativeName,
      nativeId: { state: "not-applicable" },
      status: { state: "known", value: status },
      relatedIds: { tier: channel, provision: channel === "hook" ? "native" : "emulated", ...relatedIds },
      dataRef: null,
      severity: "info",
      affectsCompletion: false,
    },
  } as unknown as Observation;
}

const hookRef: NativeRef = {
  hostId: "hos_1",
  nativeStoreId: "obj_1",
  kind: "claude",
  sessionId: { state: "known", value: "s1" },
  transcript: { state: "unknown", reason: "none", evidenceEventIds: [] },
  signalTier: "hook",
  capabilities: [],
};

describe("projectTurnDecision — the exact incident event order", () => {
  it("Stop, then pty agent_status idle, then an idle_prompt Notification ends once and never waits", () => {
    const now = Date.now();
    const t = (offsetMs: number) => new Date(now + offsetMs).toISOString();
    const events = [
      native(1, t(-200), "hook", "Stop", "idle", {
        phase: "turn-ended",
        since: t(-3000),
        outcome: "completed",
      }),
      native(2, t(-100), "pty", "agent_status", "idle", {}),
      native(3, t(0), "hook", "Notification", "observed", {
        notificationType: "idle_prompt",
        message: "Claude is waiting for your input",
      }),
    ];
    const decision = projectTurnDecision(events, hookRef, false, now);
    expect(decision.state).toBe("ended");
    expect(decision.decidedBy).toBe("hook");
    expect(decision.endedAt).toBe(t(-3000));
  });

  it("with the Stop lost, a stale hook tier and the pty screen going idle ends from the screen", () => {
    // The missing-Stop hole: the only hook record is the idle_prompt advisory,
    // delivered long enough ago that the hook tier reads stalled; the screen
    // then clears. The trailing Notification must not hold 等待操作.
    const now = Date.now();
    const t = (offsetMs: number) => new Date(now + offsetMs).toISOString();
    const events = [
      native(1, t(-10_000), "hook", "Notification", "observed", {
        notificationType: "idle_prompt",
        message: "waiting for your input",
      }),
      native(2, t(-300), "pty", "live.status", "idle", { liveStatus: "0" }),
    ];
    const decision = projectTurnDecision(events, hookRef, false, now);
    expect(decision.state).toBe("ended");
    expect(decision.decidedBy).toBe("screen");
    expect(decision.endedAt).toBe(t(-300));
  });

  it("a fresh real PermissionRequest still reads as waiting even with a quiet screen", () => {
    const now = Date.now();
    const t = (offsetMs: number) => new Date(now + offsetMs).toISOString();
    const events = [
      native(1, t(-200), "hook", "PermissionRequest", "waiting", {
        phase: "blocked",
        since: t(-200),
        toolName: "Bash",
      }),
    ];
    const decision = projectTurnDecision(events, hookRef, false, now);
    expect(decision.state).toBe("waiting");
  });

  it("turnStartAnchor is the LATEST prompt-accepted (current turn), not the first ever", () => {
    const now = Date.now();
    const t = (offsetMs: number) => new Date(now + offsetMs).toISOString();
    const events = [
      native(1, t(-60_000), "hook", "turn.live", "working", { phase: "prompt-accepted", since: t(-60_000) }),
      native(2, t(-50_000), "hook", "turn.live", "idle", { phase: "turn-ended", since: t(-50_000) }),
      native(3, t(-10_000), "hook", "turn.live", "working", { phase: "prompt-accepted", since: t(-10_000) }),
      native(4, t(0), "hook", "turn.live", "idle", { phase: "turn-ended", since: t(0) }),
    ];
    expect(turnStartAnchor(events)).toBe(t(-10_000));
  });
});
