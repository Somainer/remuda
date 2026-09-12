import { describe, expect, it } from "vitest";
import { fittedTerminalFont, TERMINAL_FONT_FAMILY } from "./terminalFit";

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
