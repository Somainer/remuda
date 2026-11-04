import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  SECRET_SHAPE_DUMMY_PART,
  SECRET_SHAPE_MASK,
  SECRET_SHAPE_PLACEHOLDER,
  SECRET_SHAPE_RULES,
} from "./secretShapes.generated";

/**
 * The generated module is the shared shape list, not a copy that can drift:
 * this test re-reads `scripts/ci/secret-shapes.json` (the gate scan's and the
 * Hub's one source) and fails if a regenerate is due.
 */

const shapes = JSON.parse(
  readFileSync(`${import.meta.dirname}/../../../scripts/ci/secret-shapes.json`, "utf8"),
) as {
  rules: { name: string; pattern: string; minPayload: number }[];
  mask: string;
  placeholder: string;
  dummyPart: string;
};

describe("secretShapes.generated.ts follows scripts/ci/secret-shapes.json", () => {
  it("carries every rule, in order, with the same pattern and minimum", () => {
    expect(SECRET_SHAPE_RULES.map((rule) => rule.name)).toEqual(shapes.rules.map((rule) => rule.name));
    for (const [index, rule] of shapes.rules.entries()) {
      expect(SECRET_SHAPE_RULES[index]).toEqual({
        name: rule.name,
        pattern: rule.pattern,
        minPayload: rule.minPayload,
      });
    }
  });

  it("carries the mask, placeholder and dummy-part patterns verbatim", () => {
    expect(SECRET_SHAPE_MASK).toBe(shapes.mask);
    expect(SECRET_SHAPE_PLACEHOLDER).toBe(shapes.placeholder);
    expect(SECRET_SHAPE_DUMMY_PART).toBe(shapes.dummyPart);
  });
});
