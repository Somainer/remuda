import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Observation } from "../../../types/generated";
import type { NativeRef } from "../../../types/nativeRef";
import { LiveStatusStrip } from "./LiveStatusStrip";

function turnLiveEvent(
  seq: number,
  tags: Record<string, string>,
  at: string,
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
    observedAt: at,
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
      relatedIds: { provision: "native", tier: "hook", ...tags },
      dataRef: null,
      severity: "info",
      affectsCompletion: false,
    },
  } as unknown as Observation;
}

const ref = (tiers: NativeRef["signalTier"][], caps: NativeRef["capabilities"] = []): NativeRef => ({
  hostId: "hos_1",
  nativeStoreId: "obj_1",
  kind: "claude",
  sessionId: { state: "known", value: "s1" },
  transcript: { state: "unknown", reason: "none", evidenceEventIds: [] },
  signalTier: tiers[0] ?? undefined,
  capabilities: caps,
});

function installRaf() {
  let frameId = 0;
  const queued = new Map<number, FrameRequestCallback>();
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback): number => {
    const id = ++frameId;
    queued.set(id, cb);
    return id;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number): void => {
    queued.delete(id);
  });
  return () => {
    const end = Date.now() + 16;
    while (Date.now() < end) vi.advanceTimersByTime(4);
    for (const [id, cb] of [...queued]) {
      queued.delete(id);
      cb(performance.now());
    }
  };
}

describe("LiveStatusStrip", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("renders nothing without phases, health notes, or expected tiers", () => {
    const { container } = render(<LiveStatusStrip events={[]} nativeRef={null} />);
    expect(container).toBeEmptyDOMElement();
  });

  it("shows the latched phase with its tier chip and wire spelling in data-phase", () => {
    const at = new Date().toISOString();
    const events = [
      turnLiveEvent(1, { phase: "prompt-accepted", since: at }, at),
      turnLiveEvent(2, { phase: "tool-started", since: at, toolCallId: "t1", toolName: "Bash" }, at),
    ];
    render(<LiveStatusStrip events={events} nativeRef={ref(["hook"])} />);
    const strip = screen.getByTestId("live-status-strip");
    expect(strip.getAttribute("data-phase")).toBe("tool-started");
    expect(screen.getByTestId("live-phase").textContent).toContain("工具运行中");
    expect(screen.getByTestId("live-phase").textContent).toContain("Bash");
    expect(screen.getByTestId("live-tier").textContent).toBe("hook");
  });

  it("derives elapsed locally: one turn.live for a 20 s tool yields 20 distinct second readings", () => {
    const frame = installRaf();
    const t0 = new Date().toISOString();
    // Exactly one live event for the whole tool — the reading is rendered
    // locally, never transported per second.
    const events = [turnLiveEvent(1, { phase: "tool-started", since: t0, toolCallId: "t1" }, t0)];
    render(<LiveStatusStrip events={events} nativeRef={ref(["hook"])} />);
    const readings = new Set<string>();
    for (let second = 0; second < 20; second += 1) {
      act(() => {
        for (let step = 0; step < 63; step += 1) frame();
      });
      readings.add(screen.getByTestId("live-elapsed").textContent ?? "");
    }
    // 0:00 through 0:20, strictly increasing across the 20 s window.
    expect(readings.size).toBeGreaterThanOrEqual(20);
    expect([...readings].slice(-1)[0]).toBe("0:20");
  });

  it("greys the elapsed and stops claiming freshness when the hook tier stalls", () => {
    const frame = installRaf();
    // Last hook record 10 s old at mount; hook cadence 2 s × 3 already passed.
    const old = new Date(Date.now() - 10_000).toISOString();
    const events = [turnLiveEvent(1, { phase: "tool-started", since: old, toolCallId: "t1" }, old)];
    render(<LiveStatusStrip events={events} nativeRef={ref(["hook"])} />);
    act(() => {
      for (let step = 0; step < 63; step += 1) frame();
    });
    const elapsed = screen.getByTestId("live-elapsed");
    expect(elapsed.getAttribute("data-stale")).toBe("1");
  });

  it("names a never-materialised expected tier explicitly (D-4), never as silence", () => {
    const at = new Date().toISOString();
    const events = [turnLiveEvent(1, { phase: "prompt-accepted", since: at }, at)];
    const native: NativeRef = {
      ...ref(["hook"]),
      capabilities: [{ name: "completion-native-turn", state: "supported", tier: "file", reasonCode: "" }],
    };
    render(<LiveStatusStrip events={events} nativeRef={native} />);
    const note = screen.getByTestId("live-health-file");
    expect(note.getAttribute("data-reason")).toBe("never-materialised");
    expect(note.textContent).toContain("没有记录");
  });

  it("never renders content from the screen/OSC tier: status text only", () => {
    const at = new Date().toISOString();
    const secret = "SECRET SCREEN CONTENT 12345";
    const screenFrame = {
      kind: "raw_tty",
      eventId: "ev_9",
      journalId: "jrn_1",
      instanceId: "ins_1",
      hostId: "hos_1",
      processGeneration: "1",
      runGeneration: "1",
      runId: "run_1",
      seq: "9",
      observedAt: at,
      nativeAt: { state: "not-applicable" },
      source: {
        adapterVersion: "t",
        channel: "pty",
        delivery: "live",
        driverKind: "shell-pty",
        driverVersion: "t",
        nativeAgentId: { state: "not-applicable" },
        nativeEventId: { state: "not-applicable" },
        nativeItemId: { state: "not-applicable" },
        nativeRequestId: { type: "none" },
        nativeSessionId: { state: "known", value: "s" },
        nativeTurnId: { state: "not-applicable" },
        sourceCursor: { type: "runtime", value: { ledgerRevision: "9" } },
      },
      completeness: "screen-derived",
      rawRef: null,
      evidenceEventIds: [],
      schemaVersion: 1,
      payload: { dataBase64: btoa(secret) },
    } as unknown as Observation;
    const events = [
      screenFrame,
      turnLiveEvent(1, { phase: "tool-started", since: at, toolCallId: "t1" }, at),
    ];
    render(<LiveStatusStrip events={events} nativeRef={ref(["hook"])} />);
    expect(screen.getByTestId("live-status-strip").textContent).not.toContain(secret);
  });
});
