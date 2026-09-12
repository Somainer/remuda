import { describe, expect, it } from "vitest";
import { mockBoardIds, mockKeys, mockScreenRead, mockSend } from "./mock";
import { doneFromLines, lastLines, parseScreenBody } from "./screen";

describe("screen snippet", () => {
  it("keeps the last three nonempty lines", () => {
    expect(lastLines(["a", "", "b", "c", "d"], 3)).toEqual(["b", "c", "d"]);
  });

  it("flags DONE when a line matcher fires", () => {
    expect(doneFromLines(["working", "DONE 8d3144d"])).toBe(true);
    expect(doneFromLines(["DONE"])).toBe(true);
    expect(doneFromLines(["not done yet"])).toBe(false);
  });

  it("parses lines or text bodies", () => {
    expect(parseScreenBody({ lines: ["x", "y"] }).lines).toEqual(["x", "y"]);
    expect(parseScreenBody({ text: "a\nb" }).lines).toEqual(["a", "b"]);
  });
});

describe("mock pty screen", () => {
  it("serves last three lines and records send/keys", () => {
    const grok = mockScreenRead(mockBoardIds.insGrokPty, 3);
    expect(doneFromLines(grok.lines)).toBe(true);
    mockSend(mockBoardIds.insCodexPty, "PAUSE");
    mockKeys(mockBoardIds.insCodexPty, "enter");
    const snippet = mockScreenRead(mockBoardIds.insCodexPty, 3).lines.join("\n");
    expect(snippet).toContain("PAUSE");
    expect(snippet).toContain("^ENTER");
  });
});
