import { describe, expect, it } from "vitest";
import { DEFAULT_ROW, indexAtOffset, visibleRange } from "./virtualWindow";

describe("visibleRange", () => {
  it("windows 2000 estimated rows to a small slice", () => {
    const count = 2000;
    const sizes: number[] = [];
    const range = visibleRange(count, sizes, 80_000, 640, 4, 80);
    expect(range.total).toBe(count * 80);
    expect(range.end - range.start).toBeLessThan(20);
    expect(range.start).toBeGreaterThan(900);
    expect(range.padTop + range.padBottom).toBeGreaterThan(100_000);
  });

  it("keeps overscan around the viewport", () => {
    const sizes = Array.from({ length: 50 }, () => 100);
    const range = visibleRange(50, sizes, 1000, 300, 2, 100);
    expect(range.start).toBe(8);
    expect(range.end).toBe(15);
    expect(range.padTop).toBe(800);
  });
});

describe("indexAtOffset", () => {
  it("maps a scroll offset onto the covering row", () => {
    const sizes = [10, 20, 30, 40];
    expect(indexAtOffset(4, sizes, 0)).toBe(0);
    expect(indexAtOffset(4, sizes, 15)).toBe(1);
    expect(indexAtOffset(4, sizes, 29)).toBe(1);
    expect(indexAtOffset(4, sizes, 30)).toBe(2);
    expect(indexAtOffset(4, sizes, 10_000)).toBe(3);
  });

  it("uses the estimate when a size is missing", () => {
    expect(indexAtOffset(10, [], DEFAULT_ROW * 3 + 1, DEFAULT_ROW)).toBe(3);
  });
});
