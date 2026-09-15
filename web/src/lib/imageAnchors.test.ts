import { describe, expect, it } from "vitest";
import {
  anchorToken,
  findAnchors,
  insertAnchor,
  insertAnchors,
  referencedIndices,
  removeAndRenumber,
  renumberAnchors,
  type AnchorSpan,
} from "./imageAnchors";

describe("anchorToken / findAnchors", () => {
  it("builds the literal token", () => {
    expect(anchorToken(1)).toBe("[Image #1]");
    expect(anchorToken(12)).toBe("[Image #12]");
  });

  it("parses tokens with offsets", () => {
    const spans = findAnchors("see [Image #1] and [Image #12]");
    expect(spans).toEqual([
      { index: 1, start: 4, end: 14 },
      { index: 12, start: 19, end: 30 },
    ] satisfies AnchorSpan[]);
  });

  it("does not confuse #1 with #12", () => {
    expect(findAnchors("[Image #12]").map((s) => s.index)).toEqual([12]);
    expect(referencedIndices("[Image #1][Image #12]")).toEqual(new Set([1, 12]));
  });

  it("keeps duplicate tokens and finds nothing in plain text", () => {
    expect(findAnchors("[Image #1] [Image #1]").map((s) => s.index)).toEqual([1, 1]);
    expect(findAnchors("image #1 is nice")).toEqual([]);
  });
});

describe("insertAnchor", () => {
  it("inserts at the end without a trailing gap", () => {
    expect(insertAnchor("hello", 5, 1)).toEqual({ text: "hello [Image #1]", caret: 16 });
  });

  it("inserts at the start", () => {
    expect(insertAnchor("hello", 0, 1)).toEqual({ text: "[Image #1] hello", caret: 10 });
  });

  it("pads inside a word on both sides", () => {
    // "hel|lo"
    const r = insertAnchor("hello", 3, 1);
    expect(r.text).toBe("hel [Image #1] lo");
    expect(r.text.slice(r.caret)).toBe(" lo");
  });

  it("does not double spaces at an existing gap", () => {
    // "hello | world"
    expect(insertAnchor("hello world", 6, 1).text).toBe("hello [Image #1] world");
    // "hello  | world" (two spaces)
    expect(insertAnchor("hello  world", 7, 1).text).toBe("hello  [Image #1] world");
  });

  it("inserts on its own in an empty draft", () => {
    expect(insertAnchor("", 0, 1)).toEqual({ text: "[Image #1]", caret: 10 });
  });

  it("clamps an out-of-range caret", () => {
    expect(insertAnchor("hi", 99, 1).text).toBe("hi [Image #1]");
    expect(insertAnchor("hi", -3, 1).text).toBe("[Image #1] hi");
  });

  it("inserts several tokens in order for a multi-image paste", () => {
    const r = insertAnchors("compare these:", 14, [1, 2]);
    expect(r.text).toBe("compare these: [Image #1] [Image #2]");
    expect(r.caret).toBe(r.text.length);
  });

  it("is safe around CJK text (IME-composed) and never splits a character", () => {
    const r = insertAnchor("你好世界", 2, 1);
    expect(r.text).toBe("你好 [Image #1] 世界");
    expect([...r.text].length).toBe([...r.text].length);
    expect(r.text.includes("�")).toBe(false);
  });
});

describe("removeAndRenumber", () => {
  it("removes the first token and shifts #2 down to #1", () => {
    const text = "see [Image #1] then [Image #2] ok";
    expect(removeAndRenumber(text, 1)).toBe("see then [Image #1] ok");
  });

  it("round-trips a two-image paste followed by first-chip removal", () => {
    const pasted = insertAnchors("look", 4, [1, 2]).text;
    expect(pasted).toBe("look [Image #1] [Image #2]");
    expect(removeAndRenumber(pasted, 1)).toBe("look [Image #1]");
  });

  it("removes every occurrence of one token", () => {
    expect(removeAndRenumber("[Image #1] x [Image #1]", 1)).toBe("x");
  });

  it("leaves a separating space when the token sat between two words", () => {
    expect(removeAndRenumber("a[Image #1]b", 1)).toBe("a b");
  });

  it("at the start consumes its own trailing space", () => {
    expect(removeAndRenumber("[Image #1] x", 1)).toBe("x");
  });

  it("only renumbers tokens above the removed one", () => {
    const text = "[Image #1] [Image #2] [Image #3]";
    expect(removeAndRenumber(text, 2)).toBe("[Image #1] [Image #2]");
  });

  it("removing a chip shifts every higher number, including #12 -> #11", () => {
    // Parse never confuses the two; the shift rule is dense by chip position.
    expect(findAnchors("[Image #1] [Image #12]").map((s) => s.index)).toEqual([1, 12]);
    expect(removeAndRenumber("[Image #1] [Image #12]", 1)).toBe("[Image #11]");
  });

  it("keeps the text byte-stable for CJK around a removed token", () => {
    expect(removeAndRenumber("你好 [Image #1] 世界", 1)).toBe("你好 世界");
  });
});

describe("renumberAnchors", () => {
  it("can delete without renumbering", () => {
    expect(renumberAnchors("[Image #1] [Image #2]", () => null)).toBe("");
  });

  it("rebuilds arbitrary mappings", () => {
    expect(
      renumberAnchors("[Image #1] [Image #2] [Image #3]", (n) => (n === 2 ? null : n)),
    ).toBe("[Image #1] [Image #3]");
  });
});

describe("referencedIndices drives the 未引用 chip state", () => {
  it("marks a chip unreferenced when its token was edited away", () => {
    const text = "only [Image #2] here";
    const refs = referencedIndices(text);
    expect(refs.has(1)).toBe(false);
    expect(refs.has(2)).toBe(true);
  });
});
