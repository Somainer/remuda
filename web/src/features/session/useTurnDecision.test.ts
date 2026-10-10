import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import type { Observation } from "../../types/generated";
import type { NativeRef } from "../../types/nativeRef";
import { useTurnDecision } from "./useTurnDecision";

function native(
  seq: number,
  observedAt: string,
  channel: string,
  nativeName: string,
  status: string,
  relatedIds: Record<string, string>,
): Observation {
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
    },
    completeness: channel === "hook" ? "structured" : "screen-derived",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
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

const hidden = { value: false };

function setHidden(value: boolean) {
  hidden.value = value;
  document.dispatchEvent(new Event("visibilitychange"));
}

/** A hook turn that opened now, plus a screen idle edge just after it. */
function openTurnWithScreenIdle(now: number): Observation[] {
  const t = (offsetMs: number) => new Date(now + offsetMs).toISOString();
  return [
    native(1, t(0), "hook", "turn.live", "working", { phase: "prompt-accepted", since: t(0) }),
    native(2, t(100), "pty", "live.status", "idle", { liveStatus: "0" }),
  ];
}

describe("useTurnDecision", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    hidden.value = false;
    Object.defineProperty(document, "hidden", { configurable: true, get: () => hidden.value });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("flips on the clock alone once the hook tier stalls, then stops ticking", () => {
    const events = openTurnWithScreenIdle(Date.now());
    let renders = 0;
    const { result } = renderHook(() => {
      renders += 1;
      return useTurnDecision(events, hookRef, false);
    });
    expect(result.current.state).toBe("working");
    const first = result.current;

    // Fresh hook tier (stall budget 6 s): unchanged ticks commit nothing and
    // keep the same object.
    const before = renders;
    act(() => vi.advanceTimersByTime(3_000));
    expect(renders).toBe(before);
    expect(result.current).toBe(first);

    // Past the stall budget the screen's idle edge ends the turn.
    act(() => vi.advanceTimersByTime(5_000));
    expect(result.current.state).toBe("ended");
    expect(result.current.decidedBy).toBe("screen");
    const ended = result.current;

    // `ended` cannot move by time alone: no timer, no renders.
    const afterEnd = renders;
    act(() => vi.advanceTimersByTime(10_000));
    expect(renders).toBe(afterEnd);
    expect(result.current).toBe(ended);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("an idle session with no evidence never re-renders on the clock", () => {
    const events: Observation[] = [];
    let renders = 0;
    const { result } = renderHook(() => {
      renders += 1;
      return useTurnDecision(events, null, false);
    });
    expect(result.current.state).toBe("unknown");
    const before = renders;
    act(() => vi.advanceTimersByTime(30_000));
    expect(renders).toBe(before);
  });

  it("re-projects in the same render when inputs change", () => {
    const events = openTurnWithScreenIdle(Date.now());
    const { result, rerender } = renderHook(
      ({ pending }: { pending: boolean }) => useTurnDecision(events, hookRef, pending),
      { initialProps: { pending: false } },
    );
    expect(result.current.state).toBe("working");
    rerender({ pending: true });
    expect(result.current.state).toBe("waiting");
    rerender({ pending: false });
    expect(result.current.state).toBe("working");
  });

  it("pauses while hidden and re-projects once on return", () => {
    const events = openTurnWithScreenIdle(Date.now());
    const { result } = renderHook(() => useTurnDecision(events, hookRef, false));
    expect(result.current.state).toBe("working");
    act(() => setHidden(true));
    expect(vi.getTimerCount()).toBe(0);
    act(() => vi.advanceTimersByTime(20_000));
    expect(result.current.state).toBe("working");
    act(() => setHidden(false));
    expect(result.current.state).toBe("ended");
  });
});
