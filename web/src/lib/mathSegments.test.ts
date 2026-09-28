import { describe, expect, it } from "vitest";
import { MATH_DOLLAR, prepareMath, restoreMathSource } from "./mathSegments";

const M = (s: string) => prepareMath(s).markdown;
const T = (s: string) => prepareMath(s).literalTail;

describe("prepareMath: accepted math is passed through", () => {
  it("keeps single-dollar and same-line $$ pairs", () => {
    expect(M("$a_b * c$")).toBe("$a_b * c$");
    expect(M("a $x+1$ b")).toBe("a $x+1$ b");
    expect(M("$$x^2$$")).toBe("$$x^2$$");
  });

  it("translates brackets in place on the same line", () => {
    expect(M("a \\(x^2\\) b")).toBe("a $x^2$ b");
    expect(M("- \\[x^2\\]")).toBe("- $$x^2$$");
    expect(M("> \\(x^2\\)")).toBe("> $x^2$");
  });

  it("pairs a multi-line display \\[ \\] within one paragraph (round-4 fix 2)", () => {
    expect(M("\\[\nx^2\n\\]")).toBe("$$\nx^2\n$$");
    expect(T("\\[\nx^2\n\\]")).toBeNull();
    // In place inside block quotes and list items: no markers touched.
    expect(M("> \\[\n> x^2\n> \\]")).toBe("> $$\n> x^2\n> $$");
    expect(M("- \\[\n  x^2\n  \\]")).toBe("- $$\n  x^2\n  $$");
  });

  it("never pairs display brackets across a blank line; only the opener stays literal", () => {
    // Rejected BEFORE EOF: the opener is emitted as an exact literal and the
    // rest of the message keeps rendering (no literal tail).
    const src = "\\[\n\nx\n\\]";
    expect(T(src)).toBeNull();
    expect(M(src)).toBe("\\\\\\[\n\nx\n\\]");
  });

  it("keeps inline \\( … \\) single-line even with a closer later (round-4 fix 2)", () => {
    const src = "a \\(\nx\n\\) b";
    expect(T(src)).toBeNull();
    expect(M(src)).toBe(src);
  });

  it("tokenizes a literal dollar inside a bracket body, restored later", () => {
    expect(M("\\(a$b\\)")).toBe(`$a${MATH_DOLLAR}b$`);
    expect(restoreMathSource(M("\\(a$b\\)"))).toContain("a$b");
  });

  it("does not inject newlines, fences or container markers", () => {
    expect(M("- before $$x$$ after")).toBe("- before $$x$$ after");
    expect(M("> before $$x$$ after")).toBe("> before $$x$$ after");
  });
});

describe("prepareMath: pandoc single-$ guard (D)", () => {
  it("escapes the currency dollars but keeps an explicit pair", () => {
    expect(M("Cost $5 and $10; use $x$.")).toBe("Cost \\$5 and \\$10; use $x$.");
  });

  it("leaves shell variables as text", () => {
    expect(M("echo $HOME and $PATH")).toBe("echo \\$HOME and \\$PATH");
  });

  it("rejects space-adjacent openers (pandoc: both dollars literal)", () => {
    expect(M("$ x$")).toBe("\\$ x\\$");
    expect(M("$x $")).toBe("\\$x \\$");
  });

  it("respects an escaped dollar", () => {
    expect(M("\\$100")).toBe("\\$100");
    expect(M("price \\$5 and $x$")).toBe("price \\$5 and $x$");
  });

  it("matches pandoc on colon-separated paths (close is non-digit)", () => {
    expect(M("export PATH=$PATH:$HOME")).toBe("export PATH=$PATH:$HOME");
  });
});

describe("prepareMath: code is parser-owned (A)", () => {
  it("never touches dollars in fenced/tilde/indented code or spans", () => {
    expect(M("```\n$x$ $$y$$\n```")).toBe("```\n$x$ $$y$$\n```");
    expect(M("~~~\n$x$\n~~~")).toBe("~~~\n$x$\n~~~");
    expect(M("use `$x$` here")).toBe("use `$x$` here");
    // 2-space-indented tilde fence (a form a hand scanner can miss).
    expect(M("  ~~~\n$x$\n  ~~~")).toBe("  ~~~\n$x$\n  ~~~");
  });

  it("keeps a ```math / ~~~math fence as code text", () => {
    expect(M("```math\nx^2\n```")).toBe("```math\nx^2\n```");
    expect(M("~~~mathinline\ny\n~~~")).toBe("~~~mathinline\ny\n~~~");
  });

  it("does not let a bracket lookahead swallow a following fence", () => {
    // The `\[` is rejected because the fence interrupts its paragraph: only
    // the opener goes literal; the fence stays code and the later `\]` is
    // ordinary markdown text — the message is NOT swallowed into a tail.
    const src = "\\[\n```\n$x$\n```\n\\]";
    expect(prepareMath(src).literalTail).toBeNull();
    expect(M(src)).toBe("\\\\\\[\n```\n$x$\n```\n\\]");
  });

  it("masks indented code nested in block quotes/list continuations (round-4 fix 3)", () => {
    // 4-space indent lives AFTER the `>` / list marker, never at column 0;
    // only the parser can tell it opens a code block.
    expect(M(">     $$x$$\n")).toBe(">     $$x$$\n");
    expect(M("- a\n\n      $$x$$\n")).toBe("- a\n\n      $$x$$\n");
  });

  it("does not pair a multi-line \\[ across a fenced block that interrupts the paragraph", () => {
    const src = "\\[\n```\n$x$\n```\n\\] tail";
    expect(T(src)).toBeNull();
    expect(M(src)).toBe("\\\\\\[\n```\n$x$\n```\n\\] tail");
  });
});

describe("prepareMath: blank-line display (E)", () => {
  it("kills only the broken opener, later $$ and $ pairs render", () => {
    const out = M("$$\n\nx\n\n$$y$$\n\n$z$");
    expect(out).toContain("$$y$$");
    expect(out).toContain("$z$");
    // First broken run's dollars are escaped.
    expect(out.startsWith("\\$\\$")).toBe(true);
  });

  it("binds a normal multi-line $$ pair", () => {
    expect(M("$$\na\nb\n$$")).toBe("$$\na\nb\n$$");
  });
});

describe("prepareMath: rejected display opener keeps later markdown (c-mathfu 1)", () => {
  it("renders a bold paragraph and a later closer after a blank line", () => {
    const src = "\\[ x\n\n**bold**\n\n\\]";
    expect(T(src)).toBeNull();
    const out = M(src);
    // Only the opener was made literal (exact `\[` source); bold survives.
    expect(out).toBe("\\\\\\[ x\n\n**bold**\n\n\\]");
  });

  it("renders lists, code blocks and later math past the rejected opener", () => {
    const src = "\\[ x\n\n- **item**\n\n```\ncode\n```\n\n$y$";
    const out = M(src);
    expect(T(src)).toBeNull();
    expect(out.startsWith("\\\\\\[ x\n\n")).toBe(true);
    expect(out).toContain("- **item**");
    expect(out).toContain("```\ncode\n```");
    expect(out).toContain("$y$");
  });

  it("keeps each rejected opener literal when several fail in one paragraph", () => {
    const src = "\\[a \\[b\n\n**bold**";
    expect(M(src)).toBe("\\\\\\[a \\\\\\[" + "b\n\n**bold**");
    expect(T(src)).toBeNull();
  });

  it("fully inert opener: no link/reference/image can start at the escaped bracket", () => {
    expect(M("\\[label](https://example.org)\n\n**bold**")).toBe(
      "\\\\\\[label](https://example.org)\n\n**bold**",
    );
    expect(M("\\[label][ref]\n\nx")).toBe("\\\\\\[label][ref]\n\nx");
    expect(M("!\\[alt](https://example.org/x.png)\n\nx")).toBe(
      "!\\\\\\[alt](https://example.org/x.png)\n\nx",
    );
  });

  it("still takes the literal tail when the rejected opener reaches EOF", () => {
    // The streaming case is unchanged: the closer search reaches EOF, so the
    // half-formula is exact literal source and bold inside it is NOT parsed.
    const src = "intro \\[a *b* + \\{c\\}";
    expect(T(src)).toBe("\\[a *b* + \\{c\\}");
    expect(M(src)).toBe("intro ");
  });
});

describe("prepareMath: streaming literal tail (G)", () => {  it("returns the exact unclosed display source, markdown untouched", () => {
    expect(T("intro $$\n\\sigma(z)")).toBe("$$\n\\sigma(z)");
    expect(M("intro $$\n\\sigma(z)")).toBe("intro ");
    expect(T("intro \\[a *b* + \\{c\\}")).toBe("\\[a *b* + \\{c\\}");
  });

  it("renders once the closer arrives (no tail)", () => {
    expect(T("intro $$\nx\n$$")).toBeNull();
    expect(M("intro $$\nx\n$$")).toContain("$$\nx\n$$");
  });

  it("does not treat an unclosed inline \\( as a tail", () => {
    expect(T("intro \\(a b")).toBeNull();
  });
});

describe("prepareMath: bounded work (B)", () => {
  const minTime = (fn: () => unknown, runs = 5) => {
    let best = Infinity;
    for (let k = 0; k < runs; k += 1) {
      const t0 = performance.now();
      fn();
      best = Math.min(best, performance.now() - t0);
    }
    return best;
  };

  it('"\\\\(".repeat(50000) is linear', () => {
    const input = "\\(".repeat(50_000); // 100 KB
    const dt = minTime(() => prepareMath(input));
    expect(dt).toBeLessThan(250);
  });

  it('1 MB single line of "\\(" stays well under 100 ms (round-4 fix 1)', () => {
    const input = "\\(".repeat(500_000); // 1,000,000 chars, one line
    expect(input.length).toBe(1_000_000);
    const dt = minTime(() => prepareMath(input), 10);
    // Measured ~30 ms on the dev box; 80 ms keeps the "well under 100 ms"
    // contract with headroom for a slower CI runner.
    expect(dt).toBeLessThan(80);
  });

  it('"$$x\\n\\n".repeat(20000) is linear', () => {
    const input = "$$x\n\n".repeat(20_000); // 120 KB
    const dt = minTime(() => prepareMath(input));
    expect(dt).toBeLessThan(250);
  });

  it('"$1".repeat(50000) is linear', () => {
    const input = "$1".repeat(50_000); // 100 KB
    const dt = minTime(() => prepareMath(input));
    expect(dt).toBeLessThan(250);
    // No accepted pairs; markdown escapes every opener-like dollar.
    expect(M(input).replace(/\\\$/g, "$")).toBe(input);
  });

  it("a 100 KB valid formula is linear", () => {
    const input = `$${"x+1".repeat(33_334)}$`; // 100 KB
    const dt = minTime(() => prepareMath(input));
    expect(dt).toBeLessThan(250);
    expect(M(input)).toBe(input);
  });
});
