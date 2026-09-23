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

  it("keeps the fully-revealed limit when total grows back after a shrink", () => {
    const { rerender, result } = renderHook(({ total }) => useIncrementalLimit(total, { step: 10 }), {
      initialProps: { total: 25 },
    });
    pumpFrames(2);
    expect(result.current).toBe(25);
    rerender({ total: 3 });
    expect(result.current).toBe(3);
    // resetKey did not change, so this is ordinary growth: the user already
    // revealed 25 rows — no restart, no single big commit of hidden rows.
    rerender({ total: 25 });
    expect(result.current).toBe(25);
    expect(pending.size).toBe(0);
  });

  it("restarts slicing only when the resetKey (filter) changes", () => {
    const { rerender, result } = renderHook(
      ({ total, resetKey }) => useIncrementalLimit(total, { step: 10, resetKey }),
      { initialProps: { total: 25, resetKey: "all" } },
    );
    pumpFrames(2);
    expect(result.current).toBe(25);
    // Same filter, different total -> stays revealed.
    rerender({ total: 15, resetKey: "all" });
    expect(result.current).toBe(15);
    rerender({ total: 25, resetKey: "all" });
    expect(result.current).toBe(25);
    // Filter actually changed -> restart from the first slice.
    rerender({ total: 25, resetKey: "approval" });
    expect(result.current).toBe(10);
    pumpFrames(1);
    expect(result.current).toBe(20);
  });
});
