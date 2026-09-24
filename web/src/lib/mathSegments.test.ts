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
    // The `\[` is never closed on this message → the whole thing is the exact
    // literal tail (the fence inside is plain source, not parsed at all).
    const src = "\\[\n```\n$x$\n```\n\\]";
    expect(prepareMath(src).literalTail).toBe(src);
    expect(M(src)).toBe("");
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

describe("prepareMath: streaming literal tail (G)", () => {
  it("returns the exact unclosed display source, markdown untouched", () => {
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
