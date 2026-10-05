import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Host } from "../../types/instance";
import { isStaleOffline, STALE_OFFLINE_MS } from "./model";
import { useStaleCutoffTick, type StaleClockHost } from "./useStaleCutoffTick";

/**
 * c-perffu r3/r4 item 3: equal quiet hosts polls emit nothing, so the page
 * re-groups offline hosts into the stale (状态待确认) group only if the hook
 * schedules a wake-up strictly past each lastSeenAt + 30min cutoff and keeps
 * arming the next one.
 */
const T0 = new Date("2026-09-29T10:00:00Z").getTime();

function offlineHost(lastSeenAt: Date): StaleClockHost & { id: string } {
  return {
    id: "host-x",
    state: "offline",
    online: false,
    ssh: undefined,
    lastSeenAt: lastSeenAt.toISOString(),
  };
}

describe("useStaleCutoffTick", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("moves each host across the strict cutoff and arms the next one with equal polls", () => {
    // Two offline rows: host A goes stale at T0+30s, host B at T0+120s.
    const hostA = offlineHost(new Date(T0 - (STALE_OFFLINE_MS - 30_000)));
    hostA.id = "a";
    const hostB = offlineHost(new Date(T0 - (STALE_OFFLINE_MS - 120_000)));
    hostB.id = "b";
    const hosts: StaleClockHost[] = [hostA, hostB];
    const { result } = renderHook(({ list }) => useStaleCutoffTick(list), {
      initialProps: { list: hosts },
    });

    // Initially BOTH are still classified fresh offline (strict > cutoff),
    // so neither may be in the stale group yet.
    expect(isStaleOffline(hosts[0] as Host, result.current)).toBe(false);
    expect(isStaleOffline(hosts[1] as Host, result.current)).toBe(false);

    // Equal poll (identical reference) arrives; nothing changes.
    act(() => {
      vi.advanceTimersByTime(10_000);
    });
    expect(result.current).toBe(T0);

    // A's boundary: strictly past +30:00 — A is now stale, B is not.
    act(() => {
      vi.advanceTimersByTime(21_000);
    });
    expect(isStaleOffline(hosts[0] as Host, result.current)).toBe(true);
    expect(isStaleOffline(hosts[1] as Host, result.current)).toBe(false);

    // Equal polls again while waiting for B.
    act(() => {
      vi.advanceTimersByTime(30_000);
    });

    // B's boundary at +120s: the hook must have re-armed after A's firing.
    act(() => {
      vi.advanceTimersByTime(60_000);
    });
    expect(isStaleOffline(hosts[0] as Host, result.current)).toBe(true);
    expect(isStaleOffline(hosts[1] as Host, result.current)).toBe(true);
  });

  it("does not fire exactly on the cutoff (isStaleOffline is strict greater-than)", () => {
    const host = offlineHost(new Date(T0 - (STALE_OFFLINE_MS - 60_000)));
    const { result } = renderHook(({ list }) => useStaleCutoffTick(list), {
      initialProps: { list: [host] },
    });
    // Exactly the cutoff instant: still fresh.
    act(() => {
      vi.advanceTimersByTime(60_000);
    });
    expect(isStaleOffline(host as Host, result.current)).toBe(false);
    // One ms later the wake fires and classification flips.
    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(isStaleOffline(host as Host, result.current)).toBe(true);
  });

  it("arms when the cutoff equals the mount instant, and chains two cutoffs 1ms apart", () => {
    // Host A's cutoff is EXACTLY T0 (lastSeen at T0 - STALE_OFFLINE_MS):
    // fresh at T0 under the strict > test, but it must still arm a 1ms wake.
    const hostA = offlineHost(new Date(T0 - STALE_OFFLINE_MS));
    hostA.id = "a";
    // Host B's cutoff is one ms later (C and C+1ms pair).
    const hostB = offlineHost(new Date(T0 - STALE_OFFLINE_MS + 1));
    hostB.id = "b";
    const { result } = renderHook(({ list }) => useStaleCutoffTick(list), {
      initialProps: { list: [hostA, hostB] as StaleClockHost[] },
    });

    expect(isStaleOffline(hostA as Host, result.current)).toBe(false);
    expect(isStaleOffline(hostB as Host, result.current)).toBe(false);

    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(isStaleOffline(hostA as Host, result.current)).toBe(true);
    // B needs its own wake; with strict `>` selection it would be dropped now.
    expect(isStaleOffline(hostB as Host, result.current)).toBe(false);

    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(isStaleOffline(hostA as Host, result.current)).toBe(true);
    expect(isStaleOffline(hostB as Host, result.current)).toBe(true);
  });

  it("needs no timer for online, connecting, or already-stale rows", () => {
    const hosts: (StaleClockHost & { id: string })[] = [
      { id: "h-on", state: "online", online: true, ssh: undefined, lastSeenAt: new Date(T0 - 1_000).toISOString() },
      { id: "h-conn", state: "connecting", online: false, ssh: undefined, lastSeenAt: new Date(T0 - 100_000).toISOString() },
      offlineHost(new Date(T0 - 2 * STALE_OFFLINE_MS)),
      { id: "h-never", state: "offline", online: false, ssh: undefined, lastSeenAt: undefined },
    ];
    const { result } = renderHook(({ list }) => useStaleCutoffTick(list), {
      initialProps: { list: hosts },
    });
    act(() => {
      vi.advanceTimersByTime(60 * 60_000);
    });
    expect(result.current).toBe(T0);
  });

  it("keeps the pending timer across identity-stable poll data", () => {
    const host = offlineHost(new Date(T0 - (STALE_OFFLINE_MS - 60_000)));
    const { result, rerender } = renderHook(({ list }) => useStaleCutoffTick(list), {
      initialProps: { list: [host] },
    });
    rerender({ list: [host] });
    act(() => {
      vi.advanceTimersByTime(61_000);
    });
    expect(isStaleOffline(host as Host, result.current)).toBe(true);
  });
});

describe("useStaleCutoffTick drives the real host-status path (c-perffu r7)", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("the hook clock is what flips a fresh-offline row stale, while an online row never is", () => {
    // r6 test bypassed the clock for the online row, so it passed with a
    // frozen/zero nowMs. Both assertions here must come from the hook's
    // emitted nowMs (the same value HostsPage feeds statusText/grouping),
    // advancing through the real setTimeout chain.
    const online: StaleClockHost = {
      state: "online",
      online: true,
      ssh: undefined,
      lastSeenAt: new Date(T0 - 1_000).toISOString(),
    };
    // Offline row whose cutoff is T0+1ms: at the hook's initial nowMs it is
    // fresh, and the classification can flip ONLY because the hook ticks.
    const offlineAtCutoff: StaleClockHost = {
      state: "offline",
      online: false,
      ssh: undefined,
      lastSeenAt: new Date(T0 - (STALE_OFFLINE_MS - 1)).toISOString(),
    };
    const hosts = [online, offlineAtCutoff];
    const { result, rerender } = renderHook(({ list }) => useStaleCutoffTick(list), {
      initialProps: { list: hosts },
    });

    // At mount the hook's clock reads T0; the offline row is 1ms inside the
    // window and the online row is online — both fresh.
    expect(result.current).toBe(T0);
    expect(isStaleOffline(hosts[0] as Host, result.current)).toBe(false);
    expect(isStaleOffline(hosts[1] as Host, result.current)).toBe(false);

    // With the timers NOT advanced, a frozen clock keeps the offline row
    // fresh forever (a zero/frozen clock would never group it): re-render
    // alone does nothing.
    rerender({ list: hosts });
    expect(isStaleOffline(hosts[1] as Host, result.current)).toBe(false);

    // Advancing the hook's timer past the cutoff updates the emitted nowMs;
    // the offline row flips stale (proving the clock drives the decision)…
    act(() => {
      vi.advanceTimersByTime(2);
    });
    expect(result.current).toBeGreaterThan(T0);
    expect(isStaleOffline(hosts[1] as Host, result.current)).toBe(true);
    // …and the online row, at the SAME clock instant and after a further full
    // hour of ticks, is never stale.
    expect(isStaleOffline(hosts[0] as Host, result.current)).toBe(false);
    act(() => {
      vi.advanceTimersByTime(60 * 60_000);
    });
    expect(isStaleOffline(hosts[0] as Host, result.current)).toBe(false);
    expect(isStaleOffline(hosts[1] as Host, result.current)).toBe(true);
  });
});
