import { describe, expect, it } from "vitest";
import type katexType from "katex";
import { renderMath } from "./mathRender";

describe("renderMath", () => {
  it("renders valid TeX with htmlAndMathml output", async () => {
    const katex: typeof katexType = (await import("katex")).default;
    const result = renderMath(katex, "\\frac{a}{b}", true);
    expect(result.ok).toBe(true);
    expect(result.html).toContain("katex");
    expect(result.html).toContain("katex-mathml");
    expect(result.html).toContain("mfrac");
  });

  it("marks a broken formula rather than throwing (throwOnError false)", async () => {
    const katex: typeof katexType = (await import("katex")).default;
    const result = renderMath(katex, "\\frac{", false);
    expect(result.ok).toBe(false);
    expect(typeof result.error).toBe("string");
    expect(result.error).toBeTruthy();
  });

  it("never throws when the engine itself blows up", () => {
    const broken = {
      renderToString: () => {
        throw new Error("engine down");
      },
    } as unknown as typeof katexType;
    const result = renderMath(broken, "x^2", false);
    expect(result).toEqual({ html: "", ok: false, error: "engine down" });
  });

  it("with trust false emits no link/handler for href/url/html commands", async () => {
    // KaTeX renders a denied command as visible red command text rather than
    // a link (it does not throw and does not add katex-error), so this asserts
    // the actual security property: no anchor, href, onclick or htmlClass
    // class can ever reach the DOM from a message.
    const katex: typeof katexType = (await import("katex")).default;
    for (const source of [
      "\\href{https://evil.test}{x}",
      "\\url{https://evil.test}",
      "\\htmlClass{evil}{x}",
      "\\htmlId{x}{y}",
      "\\htmlStyle{color:red}{z}",
    ]) {
      const result = renderMath(katex, source, false);
      const lower = result.html.toLowerCase();
      expect(lower, source).not.toContain("<a ");
      expect(lower, source).not.toContain("href=");
      expect(lower, source).not.toContain("onclick");
      expect(lower, source).not.toContain('class="evil"');
    }
  });
});
