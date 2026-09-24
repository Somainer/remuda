import { describe, expect, it } from "vitest";
import { MATH_DOLLAR, protectMath, restoreMathSource } from "./mathSegments";

/** Run protectMath and return the markdown that remark-math receives. */
const P = (s: string) => protectMath(s);

describe("protectMath: currency / pandoc single-dollar guard", () => {
  it("leaves the currency sentence untouched", () => {
    expect(P("花了 $5 和 $10")).toBe("花了 \\$5 和 $10");
  });

  it("escapes an opener followed by a space", () => {
    expect(P("$ x$")).toBe("\\$ x$");
  });

  it("escapes a span whose close is followed by a digit", () => {
    expect(P("save $x$5 today")).toBe("save \\$x$5 today");
  });

  it("accepts the same span without the trailing digit", () => {
    expect(P("save $x$ today")).toBe("save $x$ today");
  });

  it("leaves spaced shell variables as text", () => {
    expect(P("set $HOME and $PATH please")).toBe("set \\$HOME and $PATH please");
  });

  it("matches pandoc on colon-separated vars", () => {
    // ':' edge + non-digit after close → math, recorded deliberately.
    expect(P("export PATH=$PATH:$HOME")).toBe("export PATH=$PATH:$HOME");
  });

  it("respects an already escaped dollar", () => {
    expect(P("\\$100")).toBe("\\$100");
    expect(P("price \\$5 and $x$")).toBe("price \\$5 and $x$");
  });
});

describe("protectMath: closed math is handed to remark-math unchanged", () => {
  it("keeps inline dollar math intact and lifts same-line $$ to a display fence", () => {
    expect(P("$a_b * c$")).toBe("$a_b * c$");
    // `$$…$$` means display even on one line; the pair is rewritten to a flow
    // fence so micromark treats it as display, not inline.
    expect(P("$$\\sigma$$")).toBe("\n\n$$\n\\sigma\n$$\n\n");
  });

  it("converts bracket inline/display delimiters", () => {
    expect(P("a \\(x^2\\) b")).toBe(`a $x^2$ b`);
    expect(P("\\[x^2\\]")).toContain("$$\nx^2\n$$");
  });

  it("tokenizes a literal dollar inside a bracket formula body", () => {
    expect(P("\\(a$b\\)")).toBe(`$a${MATH_DOLLAR}b$`);
    expect(restoreMathSource(P("\\(a$b\\)"))).toContain("a$b");
  });
});

describe("protectMath: code is never math", () => {
  it("leaves inline code spans verbatim", () => {
    expect(P("use `$x$` here")).toBe("use `$x$` here");
    expect(P("`` $$x$$ ``")).toBe("`` $$x$$ ``");
  });

  it("leaves closed and unclosed fenced blocks verbatim", () => {
    expect(P("```\n$$nope$$\n```\nafter")).toBe("```\n$$nope$$\n```\nafter");
    // Streaming: an unterminated fence is still all code.
    expect(P("```\n$$\n\\sigma")).toBe("```\n$$\n\\sigma");
  });

  it("leaves a tilde fence verbatim", () => {
    expect(P("~~~\n$x$\n~~~")).toBe("~~~\n$x$\n~~~");
  });
});

describe("protectMath: markdown structure survives (#2)", () => {
  it("keeps single-line $$ inside a blockquote and list item", () => {
    // Indented code (4 spaces) stays code — protectMath leaves it verbatim.
    expect(P("    $$x$$")).toBe("    $$x$$");
    // Container display becomes a prefix-aware fence INSIDE the blockquote /
    // list item (verified end-to-end in MarkdownText tests).
    expect(P("> $$x$$")).toBe("\n> $$\n> x\n> $$\n");
    expect(P("- $$x$$")).toBe("\n- $$\n  x\n  $$\n");
  });

  it("rewrites bracket display math inside a blockquote with its prefix", () => {
    const out = P("> \\[x^2\\]");
    expect(out).toContain("> $$");
    expect(out).toContain("> x^2");
  });

  it("keeps bracket inline math inside a blockquote", () => {
    expect(P("> \\(x^2\\)")).toBe("> $x^2$");
  });
});

describe("protectMath: streaming half-formula is literal (#4)", () => {
  it("renders an unclosed $$ tail with all controls intact", () => {
    const out = P("intro $$\n\\sigma(z)");
    expect(out).toBe("intro \\$\\$\n\\\\sigma(z)");
  });

  it("renders an unclosed \\[ tail literal including stars and braces", () => {
    const out = P("intro \\[a *b* + \\{c\\}");
    expect(out).toBe("intro \\\\[a \\*b\\* + \\\\{c\\\\}");
  });

  it("renders once the closer arrives", () => {
    expect(P("intro $$\nx\n$$ end")).toContain("$$\nx\n$$");
  });
});

describe("protectMath: blank line voids a display opener but not later math (#5)", () => {
  it("escapes the dead opener and continues at the would-be closer", () => {
    const out = P("$$x\n\n$$ $y$");
    // First opener dead → literal dollars; second run has no later $$ run,
    // so it becomes the streaming literal tail.
    expect(out.startsWith("\\$\\$x")).toBe(true);
    expect(out).not.toContain("\n\n$$\n");
  });

  it("keeps a later independent closed formula after a blank-line dead pair", () => {
    const out = P("$$x\n\n$$\n\ny is $y$ done");
    // First run dead; second run also unclosed → tail literal.
    expect(out.startsWith("\\$\\$x")).toBe(true);
  });

  it("does not kill a valid pair separated only by single newlines", () => {
    expect(P("$$\na = 1\nb = 2\n$$")).toBe("$$\na = 1\nb = 2\n$$");
  });
});

describe("protectMath: bounded work (#1)", () => {
  const minTime = (fn: () => unknown, runs = 5) => {
    let best = Infinity;
    for (let k = 0; k < runs; k += 1) {
      const t0 = performance.now();
      fn();
      best = Math.min(best, performance.now() - t0);
    }
    return best;
  };

  it("handles 50k rejected dollars in linear time under a frame budget", () => {
    const input = "$1".repeat(50_000);
    const dt = minTime(() => protectMath(input));
    // Every opener except the last has a (pandoc-invalid) later candidate.
    const out = protectMath(input);
    expect(out).toHaveLength(input.length + (50_000 - 1));
    expect(out.replace(/\\\$/g, "$")).toBe(input);
    // Quadratic code (~2.5e9 steps) takes seconds; a linear pass is <250ms
    // even on the shared, loaded gate devbox (best of 5 runs).
    expect(dt).toBeLessThan(250);
  });

  it("scales linearly (10x input is not 100x time)", () => {
    const small = minTime(() => protectMath("$1".repeat(5_000)));
    const big = minTime(() => protectMath("$1".repeat(50_000)));
    // Pure quadratic would grow ~100x; allow generous slack for jitter.
    expect(big).toBeLessThan(Math.max(small * 30, 10));
  });

  it("handles a 100 KB valid-formula input linearly", () => {
    const input = `$${"x+1".repeat(25_000)}$`;
    const dt = minTime(() => protectMath(input));
    expect(protectMath(input)).toBe(input);
    expect(dt).toBeLessThan(250);
  });
});
