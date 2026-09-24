import { describe, expect, it, vi } from "vitest";
import type katexType from "katex";
import {
  MATH_MAX_SOURCE,
  renderMath,
  resetMathEngineForTest,
} from "./mathRender";

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

  it("passes maxSize and maxExpand to bound layout and macro expansion (#1)", async () => {
    const seen: Record<string, unknown>[] = [];
    const katex = {
      renderToString: (_src: string, opts: Record<string, unknown>) => {
        seen.push(opts);
        // Mirror KaTeX: an over-cap \rule is clamped, a macro bomb errors.
        if (_src.includes("\\def")) return "<span class=\"katex-error\"></span>";
        return "<span class=\"katex\"></span>";
      },
    } as unknown as typeof katexType;
    renderMath(katex, "\\rule{100000em}{100000em}", true);
    expect(seen[0]!.maxSize).toBe(20);
    expect(seen[0]!.maxExpand).toBe(1000);
    resetMathEngineForTest();
  });

  it("KaTeX clamps an over-cap rule and turns a macro bomb into an error", async () => {
    const katex: typeof katexType = (await import("katex")).default;
    const rule = renderMath(katex, "\\rule{100000em}{100000em}", true);
    expect(rule.ok).toBe(true);
    // No inline height exceeds the 20em cap.
    for (const m of rule.html.matchAll(/height:\s*([\d.]+)em/g)) {
      expect(Number(m[1])).toBeLessThanOrEqual(20);
    }
    const bomb = renderMath(katex, "\\def\\a{\\a}\\a", false);
    expect(bomb.ok).toBe(false);
  });

  it("skips KaTeX entirely for an oversized source (#1)", async () => {
    const katex = {
      renderToString: vi.fn(() => "<span class=\"katex\"></span>"),
    } as unknown as typeof katexType;
    const big = "x".repeat(MATH_MAX_SOURCE + 1);
    const result = renderMath(katex, big, true);
    expect(result.ok).toBe(false);
    expect(result.error).toBe("too-large");
    expect(result.html).toBe("");
    expect((katex as unknown as { renderToString: ReturnType<typeof vi.fn> }).renderToString).not.toHaveBeenCalled();
  });

  it("memoises the same (source, display) so streaming re-renders parse once", async () => {
    resetMathEngineForTest();
    const fn = vi.fn(() => "<span class=\"katex\"></span>");
    const katex = { renderToString: fn } as unknown as typeof katexType;
    renderMath(katex, "unique-memo-source", false);
    renderMath(katex, "unique-memo-source", false);
    renderMath(katex, "unique-memo-source", false);
    expect(fn).toHaveBeenCalledTimes(1);
  });

  it("does not share cache between inline and display of the same source", async () => {
    resetMathEngineForTest();
    const fn = vi.fn(() => "<span class=\"katex\"></span>");
    const katex = { renderToString: fn } as unknown as typeof katexType;
    renderMath(katex, "unique-inline-display", false);
    renderMath(katex, "unique-inline-display", true);
    expect(fn).toHaveBeenCalledTimes(2);
  });
});
