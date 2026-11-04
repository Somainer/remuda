import { describe, expect, it } from "vitest";
import corpusRaw from "../../../scripts/ci/secret-shapes-corpus.txt?raw";
import { SECRET_REDACTED, redactSecretShapes } from "./secrets";
import { SECRET_SHAPE_RULES } from "./secretShapes.generated";

/**
 * The redactor reads the shared list; `scripts/ci/secret-shapes-corpus.txt`
 * is the agreement corpus the scan and the Hub's lint both test. Every `+`
 * line must be flagged and every `-` line must pass, with `<<NAME>>` replaced
 * by that rule's own needle (the rule name).
 */

const needles = new Map(SECRET_SHAPE_RULES.map((rule) => [rule.name, rule.name]));

function expand(line: string): string {
  return line.replace(/<<([^>]+)>>/g, (_match: string, name: string) => {
    const needle = needles.get(name);
    if (needle === undefined) throw new Error(`corpus names an unknown shape: ${name}`);
    return needle;
  });
}

const lines = corpusRaw
  .split("\n")
  .map((line: string) => line.trimEnd())
  .filter((line: string) => line.length > 0 && !line.startsWith("#"));
const flagged = lines.filter((line: string) => line.startsWith("+ "));
const passing = lines.filter((line: string) => line.startsWith("- "));

describe("redactSecretShapes matches the shared agreement corpus", () => {
  it("flags every + line and withholds its token", () => {
    expect(flagged.length).toBeGreaterThan(0);
    for (const raw of flagged) {
      const text = expand(raw.slice(2));
      const out = redactSecretShapes(text);
      expect(out, text).toContain(SECRET_REDACTED);
      expect(out, text).not.toBe(text);
    }
  });

  it("leaves every - line untouched", () => {
    expect(passing.length).toBeGreaterThan(0);
    for (const raw of passing) {
      const text = expand(raw.slice(2));
      expect(redactSecretShapes(text), text).toBe(text);
    }
  });

  it("keeps the prefix and masks the payload", () => {
    const key = ["sk", "-ant-api03-", "Zq8vR2nL5tK9wX3mB7cJ4hP"].join("");
    const out = redactSecretShapes(`401 from upstream: Authorization: Bearer ${key}`);
    expect(out).not.toContain(key);
    expect(out).toContain("Bearer sk-");
    expect(out).toContain(SECRET_REDACTED);
  });
});
