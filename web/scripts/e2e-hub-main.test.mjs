import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { FAKE_NODE_SPECS, MAIN_ENV, MAIN_SPECS, MAIN_UI_SPECS } from "./e2e-hub-main.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

describe("test:e2e:hub:main (the gate's web-hub-e2e-main pass)", () => {
  it("runs exactly the hub specs gated on HUB_E2E_MAIN_UI", () => {
    // A new opt-in Main spec must join this pass, or the gate never runs it.
    const dir = join(root, "tests/e2e");
    const gated = readdirSync(dir)
      .filter((name) => /\.spec\.ts$/.test(name))
      .filter((name) => /process\.env\.HUB_E2E_MAIN_UI !== "1"/.test(readFileSync(join(dir, name), "utf8")))
      .map((name) => `tests/e2e/${name}`)
      .sort();
    expect([...MAIN_UI_SPECS].sort()).toEqual(gated);
  });

  it("also runs the listed fake-Node opt-ins, each gated on HUB_E2E_FAKE_NODE", () => {
    expect(FAKE_NODE_SPECS).toContain("tests/e2e/cmd-queue.hub.spec.ts");
    for (const spec of FAKE_NODE_SPECS) {
      expect(readFileSync(join(root, spec), "utf8"), spec).toMatch(
        /process\.env\.HUB_E2E_FAKE_NODE !== "1"/,
      );
    }
    expect(MAIN_SPECS).toEqual([...MAIN_UI_SPECS, ...FAKE_NODE_SPECS]);
  });

  it("sets every flag those specs skip without", () => {
    const flags = new Set();
    for (const spec of MAIN_SPECS) {
      const source = readFileSync(join(root, spec), "utf8");
      for (const match of source.matchAll(/process\.env\.([A-Z0-9_]+) !== "1"/g)) flags.add(match[1]);
    }
    // The scan really reads the skip guards (it is not vacuously empty).
    expect(flags.has("HUB_E2E_MAIN_UI")).toBe(true);
    for (const flag of flags) expect(MAIN_ENV[flag], flag).toBe("1");
  });

  it("is the package script the gate step runs", () => {
    const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
    expect(pkg.scripts["test:e2e:hub:main"]).toBe("node ./scripts/e2e-hub-main.mjs");
  });
});
