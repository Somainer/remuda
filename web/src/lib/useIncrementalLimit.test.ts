import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useIncrementalLimit } from "./useIncrementalLimit";

// jsdom drives rAF on a timer; make its progression deterministic with a
// simple id -> callback queue (cancel deletes by id, no index aliasing).
const pending = new Map<number, FrameRequestCallback>();
let nextHandle = 1;
vi.stubGlobal(
  "requestAnimationFrame",
  vi.fn((cb: FrameRequestCallback) => {
    const handle = nextHandle++;
    pending.set(handle, cb);
    return handle;
  }),
);
vi.stubGlobal(
  "cancelAnimationFrame",
  vi.fn((handle: number) => {
    pending.delete(handle);
  }),
);

function pumpFrames(times: number) {
  for (let i = 0; i < times; i += 1) {
    const handle = pending.keys().next().value as number | undefined;
    if (handle == null) break;
    const cb = pending.get(handle)!;
    pending.delete(handle);
    act(() => cb(0));
  }
}

afterEach(() => {
  pending.clear();
  nextHandle = 1;
});

describe("useIncrementalLimit", () => {
  it("renders small lists fully in one commit", () => {
    const { result } = renderHook(() => useIncrementalLimit(5));
    expect(result.current).toBe(5);
  });

  it("grows toward a large total in slices and never exceeds it", () => {
    const { result } = renderHook(() => useIncrementalLimit(30, 10));
    expect(result.current).toBe(10);
    pumpFrames(1);
    expect(result.current).toBe(20);
    pumpFrames(1);
    expect(result.current).toBe(30);
    // Reached total: no more frames scheduled, value stays clamped.
    expect(pending.size).toBe(0);
    pumpFrames(2);
    expect(result.current).toBe(30);
  });

  it("shrinks back immediately when the total becomes smaller", () => {
    const { rerender, result } = renderHook(({ total }) => useIncrementalLimit(total, 10), {
      initialProps: { total: 25 },
    });
    pumpFrames(2);
    expect(result.current).toBe(25);
    rerender({ total: 8 });
    expect(result.current).toBe(8);
  });

  it("restarts slice growth after shrinking and growing again", () => {
    const { rerender, result } = renderHook(({ total }) => useIncrementalLimit(total, 10), {
      initialProps: { total: 25 },
    });
    pumpFrames(2);
    expect(result.current).toBe(25);
    rerender({ total: 3 });
    expect(result.current).toBe(3);
    // Filter cleared: must not re-mount all 25 in one commit — growth
    // restarts from the clamped slice and walks back up.
    rerender({ total: 25 });
    expect(result.current).toBe(3);
    pumpFrames(1);
    expect(result.current).toBe(13);
    pumpFrames(1);
    expect(result.current).toBe(23);
    pumpFrames(1);
    expect(result.current).toBe(25);
  });
});
