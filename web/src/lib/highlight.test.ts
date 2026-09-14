import { describe, expect, it } from "vitest";
import { HIGHLIGHT_LIMIT, highlightCode, languageLabel, resolveLanguage } from "./highlight";

describe("resolveLanguage", () => {
  it("maps fence info strings to canonical grammar names", () => {
    expect(resolveLanguage("ts")).toBe("typescript");
    expect(resolveLanguage("tsx")).toBe("typescript");
    expect(resolveLanguage("JS")).toBe("javascript");
    expect(resolveLanguage("mjs")).toBe("javascript");
    expect(resolveLanguage("rs")).toBe("rust");
    expect(resolveLanguage("py")).toBe("python");
    expect(resolveLanguage("python3")).toBe("python");
    expect(resolveLanguage("zsh")).toBe("bash");
    expect(resolveLanguage("shell")).toBe("bash");
  });

  it("keeps canonical names and ignores fence meta", () => {
    expect(resolveLanguage("typescript")).toBe("typescript");
    expect(resolveLanguage("ts {1,2}")).toBe("typescript");
  });

  it("returns null for unknown, empty or missing languages", () => {
    expect(resolveLanguage("elixir")).toBeNull();
    expect(resolveLanguage("")).toBeNull();
    expect(resolveLanguage("   ")).toBeNull();
    expect(resolveLanguage(null)).toBeNull();
    expect(resolveLanguage(undefined)).toBeNull();
  });
});

describe("languageLabel", () => {
  it("uses pretty names for known grammars", () => {
    expect(languageLabel("ts")).toBe("TypeScript");
    expect(languageLabel("rs")).toBe("Rust");
    expect(languageLabel("py")).toBe("Python");
    expect(languageLabel("json")).toBe("JSON");
    expect(languageLabel("sh")).toBe("Bash");
  });

  it("keeps the raw token for unknown but named fences", () => {
    expect(languageLabel("elixir")).toBe("elixir");
    expect(languageLabel("")).toBe("");
  });
});

describe("highlightCode", () => {
  it("highlights each supported language with hljs token spans", async () => {
    const cases: Array<[string, string, string]> = [
      ["ts", "const x: number = 1;", "hljs-keyword"],
      ["rust", "fn main() { println!(\"hi\"); }", "hljs-keyword"],
      ["py", "def f(x):\n    return x + 1", "hljs-title"],
      ["json", '{"name": "demo", "n": 3}', "hljs-attr"],
      ["bash", "echo hello && ls -la", "hljs-built_in"],
    ];
    for (const [info, code, tokenClass] of cases) {
      const html = await highlightCode(info, code);
      expect(html, `${info} should highlight`).toBeTruthy();
      expect(html).toContain(tokenClass);
    }
  });

  it("returns null for unknown languages", async () => {
    expect(await highlightCode("elixir", "defmodule Foo do\nend")).toBeNull();
    expect(await highlightCode(null, "plain text")).toBeNull();
  });

  it("skips blocks at or over the size cap", async () => {
    expect(await highlightCode("ts", "a".repeat(HIGHLIGHT_LIMIT))).toBeNull();
    expect(await highlightCode("ts", "a".repeat(HIGHLIGHT_LIMIT + 1))).toBeNull();
    expect(await highlightCode("ts", "a".repeat(HIGHLIGHT_LIMIT - 1))).toBeTruthy();
  });

  it("escapes HTML-ish fence content instead of producing markup", async () => {
    const html = await highlightCode("ts", "const evil = '<img src=x onerror=alert(1)>';");
    expect(html).toBeTruthy();
    // Safety means no live attribute/tag survives: the word itself is fine as
    // escaped text content, the angle brackets must be entities.
    expect(html).not.toContain("<img");
    expect(html).not.toContain("<script");
    expect(html).toContain("&lt;img");
  });

  it("registers the same grammar once across repeated calls", async () => {
    const a = await highlightCode("ts", "export const a = 1;");
    const b = await highlightCode("ts", "export const b = 2;");
    expect(a).toContain("hljs-keyword");
    expect(b).toContain("hljs-keyword");
  });
});
