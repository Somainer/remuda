import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HOLD_IDLE_RELEASE_MS, useHeldOrder } from "./useHeldOrder";

describe("useHeldOrder", () => {
  let errors: string[];
  let errorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    errors = [];
    errorSpy = vi.spyOn(console, "error").mockImplementation((...args: unknown[]) => {
      errors.push(args.map(String).join(" "));
    });
  });

  afterEach(() => {
    errorSpy.mockRestore();
    vi.useRealTimers();
  });

  it("clears its timer and region listeners on unmount, with no late state update", () => {
    vi.useFakeTimers();
    const region = document.createElement("div");
    const added = vi.spyOn(region, "addEventListener");
    const removed = vi.spyOn(region, "removeEventListener");

    const { result, unmount } = renderHook(() => {
      const hold = useHeldOrder<number>();
      // The real caller attaches the region through JSX; give the hook one
      // before its effects run.
      if (!hold.regionRef.current) hold.regionRef.current = region;
      return hold;
    });

    act(() => {
      result.current.begin("pointer", () => 1);
    });
    expect(result.current.held).toBe(true);
    const attached = added.mock.calls.map(([type]) => type);
    expect(attached).toEqual(expect.arrayContaining(["pointerdown", "pointermove", "keydown", "focusin"]));

    unmount();
    const detached = removed.mock.calls.map(([type]) => type);
    expect(detached).toEqual(expect.arrayContaining(["pointerdown", "pointermove", "keydown", "focusin"]));

    // The 10 s timer would have fired after unmount: it must have been cleared.
    act(() => {
      vi.advanceTimersByTime(HOLD_IDLE_RELEASE_MS * 3);
    });
    expect(errors.filter((line) => /unmounted|not wrapped in act/i.test(line))).toEqual([]);
  });

  it("keeps the snapshot until every source ends", () => {
    const { result } = renderHook(() => useHeldOrder<number>());
    act(() => {
      result.current.begin("pointer", () => 1);
    });
    act(() => {
      result.current.begin("sheet", () => 2);
    });
    expect(result.current.snapshot).toBe(1);

    act(() => {
      result.current.end("pointer");
    });
    // The Sheet still owns the hold; its snapshot stays.
    expect(result.current.held).toBe(true);
    expect(result.current.snapshot).toBe(1);

    act(() => {
      result.current.end("sheet");
    });
    expect(result.current.held).toBe(false);
    expect(result.current.snapshot).toBeNull();
  });

  it("does not let the idle timeout end a Sheet-only hold", () => {
    vi.useFakeTimers();
    const { result } = renderHook(() => useHeldOrder<number>());
    act(() => {
      result.current.begin("sheet", () => 1);
    });
    act(() => {
      vi.advanceTimersByTime(HOLD_IDLE_RELEASE_MS * 2);
    });
    expect(result.current.held).toBe(true);
  });
});
