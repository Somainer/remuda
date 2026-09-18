import { describe, expect, it } from "vitest";
import type { TierHealth } from "./channelHealth";
import type { ScreenLiveStatus } from "./liveStatus";
import type { LivePhase, LivePhaseName } from "./phase";
import { lastAssistantMessageAt, turnEnd, type TurnEndInput } from "./turnEnd";
import type { Observation } from "../../../types/generated";

function phase(name: LivePhaseName, since: string, observedAt = since): LivePhase {
  return {
    phase: name,
    since,
    observedAt,
    tier: "hook",
    provision: "native",
    toolCallId: null,
    toolName: null,
    phrase: null,
    outcome: name === "turn-ended" ? "completed" : null,
    promptId: null,
  };
}

function screen(active: boolean, observedAt: string): ScreenLiveStatus {
  return {
    active,
    verb: active ? "Sauteing" : null,
    phrase: null,
    tokensLabel: null,
    tokensDown: null,
    elapsedScreen: null,
    since: active ? observedAt : null,
    interruptible: active,
    observedAt,
  };
}

function health(reason: TierHealth["reason"]): TierHealth {
  return { tier: "hook", expected: true, lastRecordAt: null, reason };
}

function input(partial: Partial<TurnEndInput>): TurnEndInput {
  return {
    phase: null,
    screen: null,
    hookHealth: health("ok"),
    hasPending: false,
    lastAssistantAt: null,
    ...partial,
  };
}

describe("turnEnd", () => {
  it("hook fresh + active phase stays working", () => {
    const out = turnEnd(input({ phase: phase("tool-started", "2026-09-18T17:00:00.000Z") }));
    expect(out.state).toBe("working");
    expect(out.decidedBy).toBe("hook");
    expect(out.endedAt).toBeNull();
  });

  it("hook stalled + screen inactive ends with decidedBy screen", () => {
    const out = turnEnd(
      input({
        phase: phase("tool-started", "2026-09-18T17:00:00.000Z"),
        hookHealth: health("stalled"),
        screen: screen(false, "2026-09-18T17:03:00.000Z"),
      }),
    );
    expect(out.state).toBe("ended");
    expect(out.decidedBy).toBe("screen");
    expect(out.endedAt).toBe("2026-09-18T17:03:00.000Z");
  });

  it("hook stalled + screen active stays working (decidedBy screen)", () => {
    const out = turnEnd(
      input({
        phase: phase("tool-started", "2026-09-18T17:00:00.000Z"),
        hookHealth: health("stalled"),
        screen: screen(true, "2026-09-18T17:03:00.000Z"),
      }),
    );
    expect(out.state).toBe("working");
    expect(out.decidedBy).toBe("screen");
    expect(out.endedAt).toBeNull();
  });

  it("no hook and no screen: an assistant message after `since` ends with decidedBy transcript", () => {
    const out = turnEnd(
      input({
        phase: phase("text-streaming", "2026-09-18T17:00:00.000Z"),
        hookHealth: health("never-materialised"),
        screen: null,
        lastAssistantAt: "2026-09-18T17:00:05.000Z",
      }),
    );
    expect(out.state).toBe("ended");
    expect(out.decidedBy).toBe("transcript");
    expect(out.endedAt).toBe("2026-09-18T17:00:05.000Z");
  });

  it("an assistant message older than `since` does not end the turn", () => {
    const out = turnEnd(
      input({
        phase: phase("text-streaming", "2026-09-18T17:00:00.000Z"),
        hookHealth: health("never-materialised"),
        screen: null,
        lastAssistantAt: "2026-09-18T16:59:00.000Z",
      }),
    );
    expect(out.state).toBe("unknown");
    expect(out.endedAt).toBeNull();
  });

  it("blocked phase + stalled hook + no pending interaction is unknown, not 等待操作", () => {
    const out = turnEnd(
      input({
        phase: phase("blocked", "2026-09-18T17:00:00.000Z"),
        hookHealth: health("stalled"),
        hasPending: false,
      }),
    );
    expect(out.state).toBe("unknown");
    expect(out.decidedBy).toBeNull();
  });

  it("blocked phase + a real pending interaction is waiting", () => {
    const out = turnEnd(
      input({
        phase: phase("blocked", "2026-09-18T17:00:00.000Z"),
        hasPending: true,
      }),
    );
    expect(out.state).toBe("waiting");
  });

  it("the screen latch reporting blocked is waiting even with no pending list entry", () => {
    const out = turnEnd(
      input({
        phase: null,
        hookHealth: health("never-materialised"),
        screenBlocked: true,
      }),
    );
    expect(out.state).toBe("waiting");
  });

  it("a late hook turn-ended after a screen-decided end wins outright (hook precedence)", () => {
    // The screen decided the end at 17:03; a hook Stop then lands at 17:04.
    // A hook boundary is the harness's own terminal word, so decidedBy flips
    // to the hook with the hook anchor. The composer's ended→idle edge is what
    // is idempotent, so this neither re-flushes nor moves the duration clock
    // (which anchors at the turn start, not endedAt).
    const withHook = turnEnd(
      input({
        phase: phase("turn-ended", "2026-09-18T17:04:00.000Z"),
        hookHealth: health("ok"),
        screen: screen(false, "2026-09-18T17:03:00.000Z"),
      }),
    );
    expect(withHook.state).toBe("ended");
    expect(withHook.decidedBy).toBe("hook");
    expect(withHook.endedAt).toBe("2026-09-18T17:04:00.000Z");
  });

  it("a pending interaction outranks a stale-hook screen end (the parked-dialog case)", () => {
    // A parked permission hook sends nothing by definition, so after the
    // hook-stall budget its tier reads stalled and the spinner clears behind
    // the visible dialog. The turn must stay waiting — never ended.
    const out = turnEnd(
      input({
        phase: phase("blocked", "2026-09-18T17:00:00.000Z"),
        hookHealth: health("stalled"),
        screen: screen(false, "2026-09-18T17:03:00.000Z"),
        hasPending: true,
      }),
    );
    expect(out.state).toBe("waiting");
    expect(out.endedAt).toBeNull();
  });

  it("the screen latch's blocked verdict outranks a stale-hook screen end too", () => {
    const out = turnEnd(
      input({
        phase: phase("tool-started", "2026-09-18T17:00:00.000Z"),
        hookHealth: health("stalled"),
        screen: screen(false, "2026-09-18T17:03:00.000Z"),
        screenBlocked: true,
      }),
    );
    expect(out.state).toBe("waiting");
  });

  it("ignores a stale screen clear older than the latched phase (a previous turn's leave)", () => {
    // The live.status clear is emitted once per leave and survives into the
    // next turn. A short turn that never repaints a spinner must not be ended
    // by last turn's clear anchored before this turn's phase `since`.
    const out = turnEnd(
      input({
        phase: phase("prompt-accepted", "2026-09-18T17:10:00.000Z"),
        hookHealth: health("stalled"),
        screen: screen(false, "2026-09-18T17:03:00.000Z"),
      }),
    );
    expect(out.state).toBe("unknown");
    expect(out.decidedBy).toBeNull();
  });

  it("accepts a screen clear exactly at the phase anchor as an end", () => {
    const at = "2026-09-18T17:03:00.000Z";
    const out = turnEnd(
      input({
        phase: phase("tool-finished", at),
        hookHealth: health("stalled"),
        screen: screen(false, at),
      }),
    );
    expect(out.state).toBe("ended");
    expect(out.decidedBy).toBe("screen");
    expect(out.endedAt).toBe(at);
  });

  it("a hook turn-ended wins outright when it is the only end evidence", () => {
    const out = turnEnd(
      input({ phase: phase("turn-ended", "2026-09-18T17:04:00.000Z"), hookHealth: health("ok") }),
    );
    expect(out.state).toBe("ended");
    expect(out.decidedBy).toBe("hook");
    expect(out.endedAt).toBe("2026-09-18T17:04:00.000Z");
  });

  it("the mid-turn screen idle edge is ignored while the hook tier is fresh (rule 6)", () => {
    const out = turnEnd(
      input({
        phase: phase("tool-started", "2026-09-18T17:00:00.000Z"),
        hookHealth: health("ok"),
        screen: screen(false, "2026-09-18T17:00:01.000Z"),
      }),
    );
    expect(out.state).toBe("working");
    expect(out.decidedBy).toBe("hook");
  });

  it("a fresh hook blocked phase is a real wait even with no pending row yet", () => {
    const out = turnEnd(
      input({
        phase: phase("blocked", "2026-09-18T17:00:00.000Z"),
        hookHealth: health("ok"),
        hasPending: false,
      }),
    );
    expect(out.state).toBe("waiting");
  });

  it("a pure-screen session (no hook tier expected) never invents an end on spinner clear", () => {
    const out = turnEnd(
      input({
        phase: null,
        hookHealth: undefined,
        screen: screen(false, "2026-09-18T17:03:00.000Z"),
      }),
    );
    expect(out.state).toBe("unknown");
    expect(out.decidedBy).toBeNull();
    expect(out.endedAt).toBeNull();
  });

  it("a pure-screen active spinner is still reported working", () => {
    const out = turnEnd(
      input({ phase: null, hookHealth: undefined, screen: screen(true, "2026-09-18T17:03:00.000Z") }),
    );
    expect(out.state).toBe("working");
    expect(out.decidedBy).toBe("screen");
  });
});

function message(role: string, seq: number, observedAt: string): Observation {
  return {
    kind: "message",
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
    source: {} as Observation["source"],
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    payload: { role } as unknown as Observation["payload"],
  } as unknown as Observation;
}

describe("lastAssistantMessageAt", () => {
  it("returns the newest assistant message by seq, ignoring user rows", () => {
    const events = [
      message("user", 1, "2026-09-18T17:00:00.000Z"),
      message("assistant", 2, "2026-09-18T17:00:01.000Z"),
      message("assistant", 4, "2026-09-18T17:00:03.000Z"),
      message("user", 5, "2026-09-18T17:00:04.000Z"),
    ];
    expect(lastAssistantMessageAt(events)).toBe("2026-09-18T17:00:03.000Z");
  });

  it("is null with no assistant message", () => {
    expect(lastAssistantMessageAt([message("user", 1, "2026-09-18T17:00:00.000Z")])).toBeNull();
  });
});
