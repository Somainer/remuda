import { describe, expect, it } from "vitest";
import {
  fittedTerminalFont,
  rowsFittingHeight,
  TERMINAL_FONT_FAMILY,
} from "./terminalFit";

describe("offscreen terminal sizing", () => {
  const bounds = { width: 1260, height: 840, cols: 200, rows: 80, lineHeight: 1, letterSpacing: 0, dpr: 1 };
  const measure = (size: number) => ({ width: size * 0.6, height: size });

  it("fits regular and high-DPI cell boundaries without overflowing a row", () => {
    expect(fittedTerminalFont(bounds, measure)).toBe(10);
    expect(fittedTerminalFont({ ...bounds, dpr: 2 }, measure)).toBe(10.5);
  });

  it("uses IBM Plex Mono as the Night Corral terminal face", () => {
    expect(TERMINAL_FONT_FAMILY.startsWith('"IBM Plex Mono"')).toBe(true);
    expect(TERMINAL_FONT_FAMILY.endsWith("monospace")).toBe(true);
  });
});

describe("rowsFittingHeight (A4 painted-cell trim, UO-10 round-7)", () => {
  it("keeps a proposal that fits at the currently painted cell", () => {
    // Container 640 → 592px: 37 rows of 16px fit exactly. The screen was
    // painted at 40 rows (640px); the proposal's 37 rows must not be
    // re-derived against painted/proposalRows (640/37 ≈ 17.3 → 34).
    expect(rowsFittingHeight(37, 640 / 40, 592)).toBe(37);
  });

  it("trims exactly the rows that overflow, not the whole height loss twice", () => {
    // Same 40→37 shrink, but a 591px box clips the 37th row by 1px: the
    // single overflowing row is dropped.
    expect(rowsFittingHeight(37, 16, 591)).toBe(36);
    expect(rowsFittingHeight(37, 16, 576)).toBe(36);
  });

  it("never shrinks a proposal on grow (painted shorter than the box)", () => {
    // 34 rows currently painted (544px), box grew to 640, proposal 40.
    expect(rowsFittingHeight(40, 544 / 34, 640)).toBe(40);
  });

  it("floors the trimmed count at the minimum row floor", () => {
    expect(rowsFittingHeight(10, 20, 40)).toBe(3);
  });

  it("leaves the proposal untouched when the cell or height is unmeasurable", () => {
    expect(rowsFittingHeight(24, 0, 600)).toBe(24);
    expect(rowsFittingHeight(24, 16, 0)).toBe(24);
  });
});
