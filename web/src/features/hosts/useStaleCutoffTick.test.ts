import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { STALE_OFFLINE_MS } from "./model";
import { useStaleCutoffTick, type StaleClockHost } from "./useStaleCutoffTick";

/**
 * c-perffu r3 item 3: equal quiet hosts polls emit nothing, so the page
 * re-groups an offline host into the stale (状态待确认) group only if it
 * schedules a local wake-up at the lastSeenAt + 30min cutoff.
 */
const T0 = new Date("2026-09-29T10:00:00Z").getTime();

function offlineHost(lastSeenAt: Date): StaleClockHost & { id: string } {
  return { id: "host-x", state: "offline", online: false, ssh: undefined, lastSeenAt: lastSeenAt.toISOString() };
}

describe("useStaleCutoffTick", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("re-ticks exactly when the host crosses the 30-minute stale cutoff", () => {
    // 29 min ago at T0: offline but NOT yet stale; goes stale at T0 + 60s.
    const host = offlineHost(new Date(T0 - (STALE_OFFLINE_MS - 60_000)));
    const { result } = renderHook(({ hosts }: { hosts: StaleClockHost[] }) => useStaleCutoffTick(hosts), {
      initialProps: { hosts: [host] },
    });
    expect(result.current).toBe(T0);

    // No re-render before the cutoff.
    act(() => {
      vi.advanceTimersByTime(59_000);
    });
    expect(result.current).toBe(T0);

    // Cross it: one wake-up at the scheduled instant.
    act(() => {
      vi.advanceTimersByTime(1_000);
    });
    expect(result.current).toBe(T0 + 60_000);
  });

  it("arms for the nearest cutoff when multiple hosts are pending", () => {
    const soon = offlineHost(new Date(T0 - (STALE_OFFLINE_MS - 30_000)));
    const later = offlineHost(new Date(T0 - (STALE_OFFLINE_MS - 120_000)));
    const { result } = renderHook(({ hosts }) => useStaleCutoffTick(hosts), {
      initialProps: { hosts: [later, soon] },
    });
    act(() => {
      vi.advanceTimersByTime(30_000);
    });
    expect(result.current).toBe(T0 + 30_000);
  });

  it("needs no timer for online hosts, connecting hosts, or already-stale rows", () => {
    const hosts = [
      { id: "h-on", state: "online", online: true, ssh: undefined, lastSeenAt: new Date(T0 - 1_000).toISOString() },
      { id: "h-conn", state: "connecting", online: false, ssh: undefined, lastSeenAt: new Date(T0 - 100_000).toISOString() },
      // lastSeenAt two hours ago: already stale at mount, nothing pending.
      offlineHost(new Date(T0 - 2 * STALE_OFFLINE_MS)),
      // Never seen: treated stale immediately.
      { id: "h-never", state: "offline", online: false, ssh: undefined, lastSeenAt: null },
    ];
    const { result } = renderHook(({ list }: { list: StaleClockHost[] }) => useStaleCutoffTick(list), {
      initialProps: { list: hosts as StaleClockHost[] },
    });
    act(() => {
      vi.advanceTimersByTime(60 * 60_000);
    });
    expect(result.current).toBe(T0);
  });

  it("keeps the pending timer across equal (identity-stable) poll data", () => {
    const host = offlineHost(new Date(T0 - (STALE_OFFLINE_MS - 60_000)));
    const { result, rerender } = renderHook(({ hosts }: { hosts: StaleClockHost[] }) => useStaleCutoffTick(hosts), {
      initialProps: { hosts: [host] },
    });
    // "Equal polls" = same reference, no store emission-driven new array.
    rerender({ hosts: [host] });
    act(() => {
      vi.advanceTimersByTime(60_000);
    });
    expect(result.current).toBe(T0 + 60_000);
  });
});
