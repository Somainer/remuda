import { describe, expect, it } from "vitest";
import { structuralEqual } from "./structuralEqual";

describe("structuralEqual", () => {
  it("treats primitives and nullish as Object.is", () => {
    expect(structuralEqual(1, 1)).toBe(true);
    expect(structuralEqual(0, -0)).toBe(false);
    expect(structuralEqual(NaN, NaN)).toBe(true);
    expect(structuralEqual("a", "a")).toBe(true);
    expect(structuralEqual(true, false)).toBe(false);
    expect(structuralEqual(null, null)).toBe(true);
    expect(structuralEqual(null, undefined)).toBe(false);
    expect(structuralEqual(null, {})).toBe(false);
    expect(structuralEqual({}, null)).toBe(false);
  });

  it("compares arrays element-wise including order", () => {
    expect(structuralEqual([1, 2, 3], [1, 2, 3])).toBe(true);
    expect(structuralEqual([1, 2, 3], [1, 3, 2])).toBe(false);
    expect(structuralEqual([1], [1, 2])).toBe(false);
    expect(structuralEqual([{ a: 1 }], [{ a: 1 }])).toBe(true);
    expect(structuralEqual([{ a: 1 }], [{ a: 2 }])).toBe(false);
  });

  it("compares plain records by their keys, independent of key order", () => {
    expect(structuralEqual({ a: 1, b: [1, 2] }, { b: [1, 2], a: 1 })).toBe(true);
    expect(structuralEqual({ a: 1 }, { a: 1, b: 2 })).toBe(false);
    expect(structuralEqual({ a: 1, b: 2 }, { a: 1 })).toBe(false);
    expect(structuralEqual({ a: undefined }, {})).toBe(false);
  });

  it("short-circuits on reference-identical subtrees", () => {
    const shared = { deep: { x: [1, 2, 3] } };
    expect(structuralEqual({ a: shared, b: 1 }, { a: shared, b: 1 })).toBe(true);
  });

  it("refuses to compare across types and prototypes", () => {
    expect(structuralEqual([1], { 0: 1, length: 1 })).toBe(false);
    class Point {
      x: number;
      constructor(x: number) {
        this.x = x;
      }
    }
    expect(structuralEqual(new Point(1), { x: 1 })).toBe(false);
  });

  it("matches the poll payloads it stabilizes (nested records/arrays)", () => {
    const a = {
      id: "inst_1",
      durableSeq: "12",
      lifecycle: "running",
      usageRollup: { turns: 3, windows: { "5m": 1 } },
      tags: ["x"],
      nested: { b: [true, null, "s"] },
    };
    const b = JSON.parse(JSON.stringify(a));
    expect(structuralEqual(a, b)).toBe(true);
    b.usageRollup.turns = 4;
    expect(structuralEqual(a, b)).toBe(false);
  });
});
