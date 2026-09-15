import { describe, expect, it } from "vitest";
import {
  expandCodeQuotes,
  lineRangeFromOffsets,
  parseFenceInfo,
  quoteExpansion,
  quotePreview,
  removeCodeToken,
  type CodeQuote,
} from "./codeAnchors";
import {
  anchorTokenFor,
  findAllAnchors,
  findAnchorsFor,
  insertAnchorsFor,
  referencedIndicesFor,
  removeAndRenumberFor,
} from "./imageAnchors";

const quote = (over: Partial<CodeQuote> = {}): CodeQuote => ({
  kind: "code",
  turnOrdinal: 1,
  blockOrdinal: 0,
  lang: "python",
  text: "def add(a, b):\n    return a + b",
  lineFrom: 1,
  lineTo: 2,
  ...over,
});

describe("parseFenceInfo", () => {
  it("reads a bare language", () => {
    expect(parseFenceInfo("ts")).toEqual({ lang: "ts" });
    expect(parseFenceInfo("python")).toEqual({ lang: "python" });
  });
  it("reads a language plus a path", () => {
    expect(parseFenceInfo("ts src/app.ts")).toEqual({ lang: "ts", path: "src/app.ts" });
  });
  it("treats a lone path-like token as path with no language", () => {
    expect(parseFenceInfo("scripts/run.sh")).toEqual({ lang: "", path: "scripts/run.sh" });
  });
  it("handles empty info", () => {
    expect(parseFenceInfo(null)).toEqual({ lang: "" });
    expect(parseFenceInfo("   ")).toEqual({ lang: "" });
  });
});

describe("Code token machinery (shared with images, no second syntax)", () => {
  it("parses [Code #n] independently of image tokens", () => {
    const text = "see [Image #1] vs [Code #1] then [Code #2]";
    expect(findAnchorsFor("Code", text).map((s) => s.index)).toEqual([1, 2]);
    expect(findAllAnchors(text).map((s) => `${s.kind}:${s.index}`)).toEqual([
      "Image:1",
      "Code:1",
      "Code:2",
    ]);
  });

  it("inserts at the caret with word padding", () => {
    expect(insertAnchorsFor("Code", "fix this", 8, [1]).text).toBe("fix this [Code #1]");
  });

  it("uses an index space independent of images", () => {
    const text = insertAnchorsFor("Image", "", 0, [1]).text;
    const both = insertAnchorsFor("Code", text, text.length, [1]).text;
    expect(both).toBe("[Image #1] [Code #1]");
    expect(referencedIndicesFor("Code", both)).toEqual(new Set([1]));
  });

  it("removes chip 2 and renumbers only code tokens", () => {
    const text = "[Image #1] [Code #1] [Code #2]";
    expect(removeAndRenumberFor("Code", text, 1)).toBe("[Image #1] [Code #1]");
    expect(removeCodeToken("[Code #1] x [Code #2]", 2)).toBe("[Code #1] x");
  });

  it("does not match #1 inside #12", () => {
    expect(anchorTokenFor("Code", 12)).toBe("[Code #12]");
    expect(findAnchorsFor("Code", "[Code #12]").map((s) => s.index)).toEqual([12]);
  });
});

describe("lineRangeFromOffsets", () => {
  const code = ["def add(a, b):", "    return a + b", "", "print(add(1, 2))"].join("\n");

  it("returns the whole block for a collapsed selection", () => {
    expect(lineRangeFromOffsets(code, 5, 5)).toEqual({
      lineFrom: 1,
      lineTo: 4,
      text: code,
    });
  });

  it("expands partial-line selections to whole lines", () => {
    // Select the word "return" on line 2 (offset 19..25).
    const range = lineRangeFromOffsets(code, code.indexOf("return"), code.indexOf("return") + 6);
    expect(range).toEqual({ lineFrom: 2, lineTo: 2, text: "    return a + b" });
  });

  it("spans several lines and returns exactly those lines", () => {
    const start = code.indexOf("    return");
    const end = code.indexOf("print") + 3;
    const range = lineRangeFromOffsets(code, start, end);
    expect(range.lineFrom).toBe(2);
    expect(range.lineTo).toBe(4);
    expect(range.text).toBe("    return a + b\n\nprint(add(1, 2))");
  });

  it("a selection ending on a newline does not include the next line", () => {
    const end = code.indexOf("\n", code.indexOf("    return")) + 1;
    const range = lineRangeFromOffsets(code, code.indexOf("    return"), end);
    expect(range.lineTo).toBe(2);
  });

  it("normalises backwards selections", () => {
    const start = code.indexOf("add(1") + 2;
    const end = code.indexOf("def");
    const range = lineRangeFromOffsets(code, start, end);
    expect([range.lineFrom, range.lineTo]).toEqual([1, 4]);
  });
});

describe("quoteExpansion / expandCodeQuotes", () => {
  it("builds the header + fence for a language block", () => {
    const expanded = quoteExpansion(quote(), 2);
    expect(expanded).toBe(
      [
        "[Code #2] quoted from the assistant's message (lines 1-2 of python block):",
        "```python",
        "def add(a, b):\n    return a + b",
        "```",
      ].join("\n"),
    );
  });

  it("names the path when the fence carried one", () => {
    const expanded = quoteExpansion(
      quote({ lang: "ts", path: "src/app.ts", lineFrom: 3, lineTo: 4 }),
      1,
    );
    expect(expanded).toContain("(lines 3-4 of src/app.ts):");
    expect(expanded).toContain("```ts");
  });

  it("falls back to 'code block' with no lang or path", () => {
    expect(quoteExpansion(quote({ lang: "", lineFrom: 1, lineTo: 1 }), 1)).toContain(
      "(lines 1-1 of code block):",
    );
  });

  it("places blocks immediately before the prompt text, in token order", () => {
    const quotes = [
      quote({ blockOrdinal: 0, text: "first()", lineFrom: 1, lineTo: 1 }),
      quote({ blockOrdinal: 1, lang: "ts", text: "second()", lineFrom: 9, lineTo: 9 }),
    ];
    const expanded = expandCodeQuotes("[Code #2] after [Code #1]?", quotes);
    // The draft mentions #2 first, so its block leads.
    const line1 = expanded.indexOf("[Code #1]");
    const line2 = expanded.indexOf("[Code #2]");
    const prompt = expanded.indexOf("[Code #2] after");
    expect(line2).toBeLessThan(line1);
    expect(line1).toBeLessThan(prompt);
    expect(expanded.endsWith("[Code #2] after [Code #1]?")).toBe(true);
    expect(expanded).toContain("first()");
    expect(expanded).toContain("second()");
    expect(expanded).toContain("(lines 9-9 of ts block)");
  });

  it("still sends quotes whose token was edited away, trailing the rest", () => {
    const quotes = [
      quote({ text: "first()", lineFrom: 1, lineTo: 1 }),
      quote({ lang: "ts", text: "second()", lineFrom: 2, lineTo: 2 }),
    ];
    const expanded = expandCodeQuotes("only [Code #2] here", quotes);
    expect(expanded.indexOf("[Code #2]")).toBeLessThan(expanded.indexOf("[Code #1]"));
    expect(expanded).toContain("first()");
  });

  it("leaves a plain prompt untouched when there are no quotes", () => {
    expect(expandCodeQuotes("hello", [])).toBe("hello");
  });
});

describe("quotePreview", () => {
  it("uses the first non-empty line, trimmed, capped", () => {
    expect(quotePreview(quote({ text: "\n    short()" }))).toBe("short()");
    const long = quotePreview(quote({ text: "x".repeat(80) }));
    expect(long).toHaveLength(48);
    expect(long.endsWith("…")).toBe(true);
  });
});
