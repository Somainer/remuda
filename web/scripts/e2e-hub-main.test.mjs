import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { EXEMPT_FLAGS, GROUPS } from "./e2e-hub-main.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const specDir = join(root, "tests/e2e");

/** Every hub spec that self-skips on `process.env.FLAG !== "1"`, with its flags. */
function gatedSpecs() {
  const gated = new Map();
  for (const name of readdirSync(specDir).filter((n) => n.endsWith(".hub.spec.ts"))) {
    const source = readFileSync(join(specDir, name), "utf8");
    const flags = new Set(
      [...source.matchAll(/process\.env\.([A-Z0-9_]+)\s*!==\s*"1"/g)].map((m) => m[1]),
    );
    if (flags.size > 0) gated.set(`tests/e2e/${name}`, [...flags]);
  }
  return gated;
}

describe("test:e2e:hub:main (the gate's web-hub-e2e-main pass)", () => {
  it("runs every hub spec that self-skips on a gate flag, exactly once", () => {
    // A new trigger-gated spec must join a group here, or the gate never runs
    // it (the audit in docs/design/evidence/hub-e2e-skips-1.md).
    const gated = [...gatedSpecs()]
      .filter(([, flags]) => flags.some((flag) => !EXEMPT_FLAGS.includes(flag)))
      .map(([spec]) => spec);
    const covered = GROUPS.flatMap((group) => group.specs).sort();
    expect(covered).toEqual([...gated].sort());
    expect(new Set(covered).size).toBe(covered.length);
  });

  it("sets every non-exempt flag each group's specs skip without", () => {
    const gated = gatedSpecs();
    for (const group of GROUPS) {
      for (const spec of group.specs) {
        for (const flag of gated.get(spec) ?? []) {
          if (EXEMPT_FLAGS.includes(flag)) continue;
          expect(group.env[flag], `${group.name}: ${flag} (${spec})`).toBe("1");
        }
      }
    }
  });

  it("keeps the Main group equal to the specs gated on HUB_E2E_MAIN_UI", () => {
    const gated = [...gatedSpecs()].filter(([, flags]) => flags.includes("HUB_E2E_MAIN_UI"));
    expect([...GROUPS[0].specs].sort()).toEqual(gated.map(([spec]) => spec).sort());
    expect(GROUPS[0].name).toBe("main");
  });

  it("is the package script the gate step runs", () => {
    const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
    expect(pkg.scripts["test:e2e:hub:main"]).toBe("node ./scripts/e2e-hub-main.mjs");
  });
});
