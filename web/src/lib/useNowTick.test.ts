import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useNowTick } from "./useNowTick";

describe("useNowTick", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("ticks on its own interval without any store emission", () => {
    const start = new Date("2026-09-29T10:00:00Z").getTime();
    vi.setSystemTime(start);
    const { result, unmount } = renderHook(() => useNowTick(30_000));
    expect(result.current).toBe(start);

    act(() => {
      vi.advanceTimersByTime(30_000);
    });
    expect(result.current).toBe(start + 30_000);

    act(() => {
      vi.advanceTimersByTime(30_000);
    });
    expect(result.current).toBe(start + 60_000);

    unmount();
    const value = result.current;
    act(() => {
      vi.advanceTimersByTime(120_000);
    });
    // No setState after unmount (no React warning either).
    expect(result.current).toBe(value);
  });
});
