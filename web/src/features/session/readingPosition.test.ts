import { describe, expect, it, beforeEach } from "vitest";
import { clearPosition, readPosition, writePosition } from "./readingPosition";

describe("readingPosition", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("round-trips a saved position per instance", () => {
    expect(readPosition("ins_1")).toBeNull();
    writePosition("ins_1", { anchorId: "node-7", offset: 42, ratio: 0.25, avgRow: 60, follow: false });
    writePosition("ins_2", { anchorId: "node-9", offset: 0, ratio: 1, avgRow: 96, follow: true });
    expect(readPosition("ins_1")).toEqual({ anchorId: "node-7", offset: 42, ratio: 0.25, avgRow: 60, follow: false });
    expect(readPosition("ins_2")?.follow).toBe(true);
    clearPosition("ins_1");
    expect(readPosition("ins_1")).toBeNull();
  });

  it("rejects corrupt payloads instead of crashing", () => {
    localStorage.setItem("runtime.reading.v1.ins_bad", "{not json");
    expect(readPosition("ins_bad")).toBeNull();
    localStorage.setItem("runtime.reading.v1.ins_bad2", JSON.stringify({ anchorId: 3, follow: "yes" }));
    expect(readPosition("ins_bad2")).toBeNull();
  });

  it("coerces a non-finite offset to 0 and clamps the ratio", () => {
    localStorage.setItem(
      "runtime.reading.v1.ins_x",
      JSON.stringify({ anchorId: "a", offset: NaN, ratio: 7, avgRow: 0, follow: false }),
    );
    const pos = readPosition("ins_x");
    expect(pos?.offset).toBe(0);
    expect(pos?.ratio).toBe(1);
    expect(pos?.avgRow).toBe(0);
  });

  it("accepts older records without ratio/avgRow fields", () => {
    localStorage.setItem(
      "runtime.reading.v1.ins_old",
      JSON.stringify({ anchorId: "legacy", offset: 10, follow: false }),
    );
    expect(readPosition("ins_old")).toEqual({ anchorId: "legacy", offset: 10, ratio: 0, avgRow: 0, follow: false });
  });
});
