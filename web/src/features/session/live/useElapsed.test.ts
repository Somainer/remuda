import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { formatElapsed, useElapsed, useNow } from "./useElapsed";

describe("formatElapsed", () => {
  it("formats zero, seconds, minutes, and hours with tabular m:ss", () => {
    expect(formatElapsed(0)).toBe("0:00");
    expect(formatElapsed(499)).toBe("0:00");
    expect(formatElapsed(5000)).toBe("0:05");
    expect(formatElapsed(65_000)).toBe("1:05");
    expect(formatElapsed(3_725_000)).toBe("1:02:05");
  });

  it("clamps negative values (clock skew at the phase boundary)", () => {
    expect(formatElapsed(-5000)).toBe("0:00");
  });
});

/**
 * Deterministic frame loop that models the browser: callbacks queued while the
 * tab is hidden do not run (the property the pause guarantee depends on), and
 * each animation frame reschedules ~16 ms later.
 */
function installRaf() {
  const hidden = { value: false };
  let frameId = 0;
  const queued = new Map<number, FrameRequestCallback>();
  const raf = (cb: FrameRequestCallback): number => {
    const id = ++frameId;
    queued.set(id, cb);
    return id;
  };
  const caf = (id: number): void => {
    queued.delete(id);
  };
  vi.stubGlobal("requestAnimationFrame", raf);
  vi.stubGlobal("cancelAnimationFrame", caf);
  Object.defineProperty(document, "hidden", {
    configurable: true,
    get: () => hidden.value,
  });
  const setHidden = (value: boolean): void => {
    hidden.value = value;
    document.dispatchEvent(new Event("visibilitychange"));
  };
  return {
    setHidden,
    /** Run every frame that would be presented in `ms` of wall time. */
    advanceFrames(ms: number): void {
      const end = Date.now() + ms;
      while (Date.now() < end) {
        vi.advanceTimersByTime(10);
        for (const [id, cb] of [...queued]) {
          queued.delete(id);
          if (!hidden.value) cb(performance.now());
        }
      }
    },
  };
}

describe("useNow / useElapsed", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("updates at 1 Hz, never per frame", () => {
    const frames = installRaf();
    const { result } = renderHook(() => useNow(true));
    const start = result.current;
    act(() => frames.advanceFrames(1_000));
    expect(result.current - start).toBeGreaterThanOrEqual(1_000);
    // Two seconds of frames still produce whole-second readings.
    act(() => frames.advanceFrames(2_000));
    expect(result.current - start).toBeGreaterThanOrEqual(3_000);
  });

  it("pauses while the tab is hidden and recomputes immediately on return", () => {
    const frames = installRaf();
    const { result } = renderHook(() => useNow(true));
    const visible = result.current;
    act(() => frames.setHidden(true));
    act(() => frames.advanceFrames(5_000));
    // No frames presented → no ticks.
    expect(result.current).toBe(visible);
    act(() => frames.setHidden(false));
    expect(result.current - visible).toBeGreaterThanOrEqual(5_000);
  });

  it("renders elapsed off one since anchor and greys on stale health", () => {
    installRaf();
    const since = new Date(Date.now() - 20_000).toISOString();
    const ok = renderHook(() =>
      useElapsed(since, true, { tier: "hook", expected: true, lastRecordAt: null, reason: "ok" }),
    );
    expect(ok.result.current?.text).toBe("0:20");
    expect(ok.result.current?.stale).toBe(false);

    const stale = renderHook(() =>
      useElapsed(since, true, { tier: "hook", expected: true, lastRecordAt: null, reason: "stalled" }),
    );
    expect(stale.result.current?.stale).toBe(true);
  });

  it("returns null without an anchor and when inactive", () => {
    installRaf();
    const none = renderHook(() => useElapsed(null, true));
    expect(none.result.current).toBeNull();
    const bad = renderHook(() => useElapsed("not-a-date", true));
    expect(bad.result.current).toBeNull();
  });
});
