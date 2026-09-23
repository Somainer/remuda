import { act } from "react";
import { renderHook } from "@testing-library/react";
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
    const { result } = renderHook(() => useIncrementalLimit(30, { step: 10 }));
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

  it("only clamps (never resets) when a card leaves: rows 13+ stay mounted", () => {
    // Regression for the draft-loss bug: answering one card shrank total by 1
    // and reset the limit to the step, unmounting every revealed later row.
    const { rerender, result } = renderHook(({ total }) => useIncrementalLimit(total, { step: 12 }), {
      initialProps: { total: 20 },
    });
    pumpFrames(1);
    expect(result.current).toBe(20);
    // One card answered elsewhere: total 20 -> 19.
    rerender({ total: 19 });
    expect(result.current).toBe(19);
    // Rows 13-19 are still inside the limit; nothing below 19 is mounted.
    expect(result.current).toBeGreaterThanOrEqual(19);
  });

  it("re-slices from the lowered mark when total grows after a shrink (no tail jump)", () => {
    // Regression for the high-water-mark bug: clamping only the RETURNED
    // value left the stored limit at the pre-shrink count, so growth mounted
    // the whole tail in one commit. The clamp must be persisted.
    const { rerender, result } = renderHook(({ total }) => useIncrementalLimit(total, { step: 12 }), {
      initialProps: { total: 20 },
    });
    pumpFrames(1);
    expect(result.current).toBe(20);
    // Shrink hard: 20 -> 5, stored limit lowers to exactly what is mounted.
    rerender({ total: 5 });
    expect(result.current).toBe(5);
    // Grow back to 40 with the SAME filter: no single 35-card commit. The
    // first render reveals only the 5 already mounted, then step slices.
    rerender({ total: 40 });
    expect(result.current).toBe(5);
    pumpFrames(1);
    expect(result.current).toBe(17);
    pumpFrames(1);
    expect(result.current).toBe(29);
    pumpFrames(1);
    expect(result.current).toBe(40);
    expect(pending.size).toBe(0);
  });

  it("restarts slicing only when the resetKey (filter) changes", () => {
    const { rerender, result } = renderHook(
      ({ total, resetKey }) => useIncrementalLimit(total, { step: 10, resetKey }),
      { initialProps: { total: 25, resetKey: "all" } },
    );
    pumpFrames(2);
    expect(result.current).toBe(25);
    // Same filter, shrink clamps the stored mark to what is mounted.
    rerender({ total: 15, resetKey: "all" });
    expect(result.current).toBe(15);
    // Same filter, grow back: re-slice from the lowered mark, no single
    // 10-row tail commit.
    rerender({ total: 25, resetKey: "all" });
    expect(result.current).toBe(15);
    pumpFrames(1);
    expect(result.current).toBe(25);
    // Filter actually changed -> restart from the first slice.
    rerender({ total: 25, resetKey: "approval" });
    expect(result.current).toBe(10);
    pumpFrames(1);
    expect(result.current).toBe(20);
  });
});
