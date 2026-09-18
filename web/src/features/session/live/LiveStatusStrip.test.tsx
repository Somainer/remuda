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

function screenStatusEvent(seq: number, tags: Record<string, string>, at = new Date().toISOString()): Observation {
  return {
    ...turnLiveEvent(seq, tags, at, "pty"),
    completeness: "screen-derived",
    payload: {
      type: "native",
      topic: "turn",
      nativeName: "live.status",
      nativeId: { state: "not-applicable" },
      status: { state: "not-applicable" },
      relatedIds: { tier: "screen", provision: "emulated", ...tags },
      dataRef: null,
      severity: "info",
      affectsCompletion: false,
    },
  } as unknown as Observation;
}

function usageEvent(seq: number, output: string, at = new Date().toISOString()): Observation {
  return {
    kind: "usage",
    eventId: `ev_u_${seq}`,
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
      scope: "message",
      mode: "snapshot",
      accounting: "api",
      inputAccounting: "api",
      inputTokens: { state: "known", value: "10" },
      outputTokens: { state: "known", value: output },
      totalTokens: { state: "known", value: String(Number(output) + 10) },
      cacheReadTokens: { state: "known", value: "0" },
      cacheWriteTokens: { state: "known", value: "0" },
      reasoningTokens: { state: "known", value: "0" },
      cost: { state: "unknown", reason: "pending", evidenceEventIds: [] },
      nativeFieldsRef: null,
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

  it("freezes the elapsed on the turn duration at a hook end (not 0:00, not stale)", () => {
    const frame = installRaf();
    // Turn started 10 s ago and ended now. The latched phase is turn-ended, but
    // the start anchor survives in the event list; the reading must be the
    // duration, frozen, not anchored at endedAt (which would read 0:00).
    const start = new Date(Date.now() - 10_000).toISOString();
    const end = new Date().toISOString();
    const events = [
      turnLiveEvent(1, { phase: "prompt-accepted", since: start }, start),
      turnLiveEvent(2, { phase: "turn-ended", since: end, outcome: "completed" }, end),
    ];
    render(<LiveStatusStrip events={events} nativeRef={ref(["hook"])} />);
    act(() => {
      for (let step = 0; step < 63; step += 1) frame();
    });
    const elapsed = screen.getByTestId("live-elapsed");
    expect(elapsed.textContent).toBe("0:10");
    expect(elapsed.getAttribute("data-stale")).toBe("0");
    expect(screen.getByTestId("live-decided-by").getAttribute("data-channel")).toBe("hook");
    expect(screen.queryByTestId("live-interrupt")).toBeNull();
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

  it("screen-only spinner line: thinking label, verb, screen tokens, muted phrase", () => {
    const at = new Date().toISOString();
    const events = [
      screenStatusEvent(
        1,
        {
          liveStatus: "1",
          verb: "Razzmatazzing",
          elapsedScreen: "49m 38s",
          since: new Date(Date.now() - 5_000).toISOString(),
          tokensLabel: "66.0k",
          tokensDown: "66000",
          phrase: "thinking some more with xhigh effort",
        },
        at,
      ),
    ];
    render(<LiveStatusStrip events={events} nativeRef={null} />);
    const strip = screen.getByTestId("live-status-strip");
    expect(strip.getAttribute("data-phase")).toBe("thinking");
    expect(screen.getByTestId("live-phase").textContent).toContain("思考中");
    expect(screen.getByTestId("live-verb").textContent).toBe("Razzmatazzing…");
    expect(screen.getByTestId("live-token-count").textContent).toContain("66.0k");
    expect(screen.getByTestId("live-token-count").getAttribute("data-source")).toBe("screen");
    expect(screen.getByTestId("live-phrase").textContent).toContain("xhigh");
  });

  it("screen-only spinner line without a thinking phrase reads as working, not thinking", () => {
    const events = [
      screenStatusEvent(1, {
        liveStatus: "1",
        verb: "Effecting",
        elapsedScreen: "34s",
        tokensLabel: "120",
      }),
    ];
    render(<LiveStatusStrip events={events} nativeRef={null} />);
    expect(screen.getByTestId("live-status-strip").getAttribute("data-phase")).toBe("working");
    expect(screen.getByTestId("live-phase").textContent).toContain("工作中");
  });

  it("clears screen fields after liveStatus:0", () => {
    const at = new Date().toISOString();
    const events = [
      screenStatusEvent(1, { liveStatus: "1", verb: "Working", tokensLabel: "5" }, at),
      screenStatusEvent(2, { liveStatus: "0" }, at),
    ];
    const { container } = render(<LiveStatusStrip events={events} nativeRef={null} />);
    expect(container).toBeEmptyDOMElement();
  });

  it("shows one token count, preferring real usage over the screen number", () => {
    const at = new Date().toISOString();
    const events = [
      turnLiveEvent(1, { phase: "prompt-accepted", since: at }, at),
      screenStatusEvent(
        2,
        {
          liveStatus: "1",
          verb: "Forging",
          elapsedScreen: "12s",
          since: at,
          tokensLabel: "138",
          tokensDown: "138",
          phrase: "thought for 9s",
        },
        at,
      ),
      usageEvent(3, "204", at),
    ];
    render(<LiveStatusStrip events={events} nativeRef={ref(["hook"])} />);
    const count = screen.getByTestId("live-token-count");
    expect(count.textContent).toContain("204");
    expect(count.getAttribute("data-source")).toBe("usage");
    expect(screen.getAllByTestId("live-token-count")).toHaveLength(1);
  });

  it("renders the Esc affordance only while interruptible and sends through the callback", () => {
    const at = new Date().toISOString();
    const onInterrupt = vi.fn();
    const events = [
      turnLiveEvent(1, { phase: "prompt-accepted", since: at }, at),
      screenStatusEvent(
        2,
        { liveStatus: "1", verb: "Working", elapsedScreen: "2s", since: at, interruptible: "1" },
        at,
      ),
    ];
    const { rerender } = render(
      <LiveStatusStrip events={events} nativeRef={ref(["hook"])} onInterrupt={onInterrupt} />,
    );
    const button = screen.getByTestId("live-interrupt");
    expect(button.textContent).toContain("Esc");
    act(() => {
      button.click();
    });
    expect(onInterrupt).toHaveBeenCalledOnce();

    // A blocked phase owns the keyboard itself: no strip interrupt.
    const blocked = [
      turnLiveEvent(1, { phase: "blocked", since: at }, at),
      screenStatusEvent(2, { liveStatus: "1", verb: "Working", interruptible: "1" }, at),
    ];
    rerender(
      <LiveStatusStrip events={blocked} nativeRef={ref(["hook"])} onInterrupt={onInterrupt} />,
    );
    expect(screen.queryByTestId("live-interrupt")).toBeNull();

    // No callback wired: the affordance is absent, not dead.
    rerender(<LiveStatusStrip events={events} nativeRef={ref(["hook"])} />);
    expect(screen.queryByTestId("live-interrupt")).toBeNull();
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
