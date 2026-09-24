import { describe, expect, it } from "vitest";
import {
  MATH_DOLLAR,
  protectMath,
  restoreMathSource,
  splitMathSegments,
} from "./mathSegments";

/** Concise segment dump: `T:` prose, `V:` verbatim, `$:` inline, `$$:` display. */
function kinds(input: string): string[] {
  return splitMathSegments(input).map((seg) => {
    const tag =
      seg.kind === "prose" ? "T" : seg.kind === "verbatim" ? "V" : seg.kind === "inlineMath" ? "$" : "$$";
    return `${tag}:${JSON.stringify(seg.value)}`;
  });
}

describe("math segmenter: delimiters", () => {
  it("parses dollar-dollar and bracket forms", () => {
    expect(kinds("$$x^2$$")).toEqual(["$$:\"x^2\""]);
    expect(kinds("a\n\\[x^2\\]\nb")).toEqual(["T:\"a\\n\"", "$$:\"x^2\"", "T:\"\\nb\""]);
    expect(kinds("a $x+1$ b")).toEqual(["T:\"a \"", "$:\"x+1\"", "T:\" b\""]);
    expect(kinds("a \\(y\\) b")).toEqual(["T:\"a \"", "$:\"y\"", "T:\" b\""]);
  });

  it("parses multiline display math", () => {
    expect(kinds("$$\na_b = 1\nc_d = 2\n$$")).toEqual(["$$:\"\\na_b = 1\\nc_d = 2\\n\""]);
  });

  it("keeps underscores, asterisks and backslashes inside math verbatim", () => {
    expect(splitMathSegments("$a_b * c$")[0]).toMatchObject({
      kind: "inlineMath",
      value: "a_b * c",
    });
    expect(splitMathSegments("$$\\sigma(\\mathbf{z})_i$$")[0]!.value).toBe(
      "\\sigma(\\mathbf{z})_i",
    );
  });

  it("allows a dollar inside a bracket formula", () => {
    expect(kinds("\\(a$b\\)")).toEqual(["$:\"a$b\""]);
    expect(kinds("\\[a$b\\]")).toEqual(["$$:\"a$b\""]);
  });
});

describe("math segmenter: pandoc single-dollar guard", () => {
  it("leaves currency sentences as prose", () => {
    expect(kinds("花了 $5 和 $10")).toEqual(["T:\"花了 $5 和 $10\""]);
    expect(kinds("price $100 total")).toEqual(["T:\"price $100 total\""]);
  });

  it("rejects a close followed by a digit", () => {
    expect(kinds("save $x$5 today")).toEqual(["T:\"save $x$5 today\""]);
    expect(kinds("save $x$ today")).toEqual(["T:\"save \"", "$:\"x\"", "T:\" today\""]);
  });

  it("requires non-space edges inside the span", () => {
    expect(kinds("$ x$")).toEqual(["T:\"$ x$\""]);
    expect(kinds("$x $")).toEqual(["T:\"$x $\""]);
  });

  it("leaves shell variables in prose", () => {
    // A lone variable has no closing $.
    expect(kinds("echo $HOME")).toEqual(["T:\"echo $HOME\""]);
    // Two vars separated by prose: whitespace directly before the candidate
    // close is pandoc's "no trailing space" rule, so the pair never opens.
    expect(kinds("set $HOME and $PATH please")).toEqual([
      "T:\"set $HOME and $PATH please\"",
    ]);
  });

  it("does not pair a currency dollar with a display opener on a later line", () => {
    // Regression: after the rejected `$10`, the scanner must not treat the
    // second `$` of `$$` (whitespace before the first) as that span's close
    // and swallow a later display formula into prose.
    const segs = splitMathSegments(
      "花了 $5 和 $10 都不渲染。\n坏的 $$\\frac{$$ 结束。\n$$x_1+x_2$$",
    );
    expect(segs).toEqual([
      { kind: "prose", value: "花了 $5 和 $10 都不渲染。\n坏的 " },
      { kind: "displayMath", value: "\\frac{" },
      { kind: "prose", value: " 结束。\n" },
      { kind: "displayMath", value: "x_1+x_2" },
    ]);
  });

  it("matches pandoc on colon-separated vars (the close rule says math)", () => {
    // Real pandoc renders $PATH:$HOME as inline math "PATH:" — ':' is a
    // non-space edge and 'H' is not a digit. Recorded deliberately so a
    // future "fix" here is a conscious divergence.
    expect(kinds("export PATH=$PATH:$HOME")).toEqual([
      "T:\"export PATH=\"",
      "$:\"PATH:\"",
      "T:\"HOME\"",
    ]);
  });

  it("respects an escaped dollar", () => {
    expect(kinds("price \\$5 and $x$")).toEqual(["T:\"price \\\\$5 and \"", "$:\"x\""]);
    expect(kinds("\\$100")).toEqual(["T:\"\\\\$100\""]);
  });
});

describe("math segmenter: code is verbatim", () => {
  it("never parses math inside a code span", () => {
    expect(kinds("use `$x$` here")).toEqual(["T:\"use \"", "V:\"`$x$`\"", "T:\" here\""]);
    expect(kinds("`` $$x$$ ``")).toEqual(["V:\"`` $$x$$ ``\""]);
  });

  it("never parses math inside a fenced block, including an unterminated one", () => {
    const closed = "```\n$$not math$$\n$also not$\n```\nafter";
    const segs = kinds(closed);
    expect(segs.some((s) => s.startsWith("$"))).toBe(false);
    expect(segs[segs.length - 1]).toBe("T:\"after\"");

    expect(kinds("```\n$$\n\\sigma")).toEqual(["V:\"```\\n$$\\n\\\\sigma\""]);
  });

  it("does not treat dollars inside a link destination as math", () => {
    expect(kinds("[buy](https://shop.test/p?id=$5) now")).toEqual([
      "T:\"[buy\"",
      "V:\"](https://shop.test/p?id=$5)\"",
      "T:\" now\"",
    ]);
  });

  it("needs an exact-length closing backtick run", () => {
    // The last two of a four-backtick run ARE a valid 2-run close (pandoc's
    // `count` + notFollowedBy agrees), so the dollars inside stay verbatim.
    expect(kinds("``a $$x$$ ```` b")).toEqual(["V:\"``a $$x$$ ````\"", "T:\" b\""]);
    expect(kinds("``a $$x$$ `` then $y$")).toEqual([
      "V:\"``a $$x$$ ``\"",
      "T:\" then \"",
      "$:\"y\"",
    ]);
  });
});

describe("math segmenter: streaming leaves unclosed delimiters as text", () => {
  it("unclosed $$ / \\[ / $ never starts math", () => {
    expect(kinds("intro $$\n\\sigma(z)")).toEqual(["T:\"intro $$\\n\\\\sigma(z)\""]);
    expect(kinds("intro \\[\nx")).toEqual(["T:\"intro \\\\[\\nx\""]);
    expect(kinds("intro $x_")).toEqual(["T:\"intro $x_\""]);
  });

  it("does not render an inner formula while a display fence is half open", () => {
    expect(kinds("$$\nsee $x$ and \\(y\\)")).toEqual([
      "T:\"$$\\nsee $x$ and \\\\(y\\\\)\"",
    ]);
  });

  it("turns into math once the closing delimiter arrives", () => {
    expect(kinds("intro $$\nx\n$$ end")).toEqual([
      "T:\"intro \"",
      "$$:\"\\nx\\n\"",
      "T:\" end\"",
    ]);
  });

  it("voids the opener on a blank line and keeps scanning afterwards (pandoc)", () => {
    // $$ … blank line … $$: the opener is voided, everything prose. The
    // second opener has no close, so the streaming-safe swallow covers the
    // remainder (nothing renders until a real close arrives).
    expect(kinds("$$x\n\n$$ then $y$")).toEqual(["T:\"$$x\\n\\n$$ then $y$\""]);
    expect(kinds("$$x\n\n$$")).toEqual(["T:\"$$x\\n\\n$$\""]);
    // A fresh inline span AFTER the blank line still opens on its own.
    expect(kinds("a $x\n\n$y$ b")).toEqual(["T:\"a $x\\n\\n\"", "$:\"y\"", "T:\" b\""]);
    // Bracket display voided by a blank line; a later bracket inline parses.
    expect(kinds("\\[x\n\n\\] then \\(z\\)")).toEqual([
      "T:\"\\\\[x\\n\\n\\\\] then \"",
      "$:\"z\"",
    ]);
  });

  it("keeps interior single newlines inside display math", () => {
    expect(kinds("$$\na = 1\nb = 2\n$$")).toEqual(["$$:\"\\na = 1\\nb = 2\\n\""]);
  });
});

describe("protectMath / restoreMathSource", () => {
  it("rewrites accepted spans into remark-math dollar syntax", () => {
    expect(protectMath("a $x$ b")).toBe("a $x$ b");
    // Bracket inline forms become dollar forms too.
    expect(protectMath("a \\(y\\) b")).toBe("a $y$ b");
  });

  it("hides dollars inside a formula from the markdown parser and restores them", () => {
    const out = protectMath("\\(a$b\\)");
    expect(out).toContain(MATH_DOLLAR);
    expect(restoreMathSource(out)).toContain("a$b");
  });

  it("escapes rejected dollars in prose so they stay literal", () => {
    expect(protectMath("花了 $5")).toBe("花了 \\$5");
    expect(protectMath("already \\$5")).toBe("already \\$5");
  });

  it("emits display math on its own lines and leaves code spans untouched", () => {
    const out = protectMath("$$a_b$$\n\nand `$x$`");
    expect(out).toMatch(/\n\n\$\$\n/);
    expect(out).toContain("`$x$`");
    expect(restoreMathSource(out)).toContain("a_b");
  });
});
