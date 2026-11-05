#!/usr/bin/env node
/**
 * The gate's `web-hub-e2e-main` pass (`pnpm run test:e2e:hub:main`).
 *
 * The specs below seat the Hub's one address-owner Main, so the main
 * `test:e2e:hub` pass skips them. This runs them on their own Hub with the
 * flags they need, after the main pass:
 *  - HUB_E2E_MAIN_UI=1 lets them seat Main;
 *  - REMUDA_DELEGATED_DECISIONS=1 and HUB_E2E_LINEAGE_FENCE=1 are for
 *    routed-to: the D-051 switch, and the fake Node that dispatches for an
 *    Agent.
 * The env is set here rather than in package.json so the command is the same
 * on every platform. Extra arguments go to Playwright.
 */

import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** Every hub spec gated on HUB_E2E_MAIN_UI (a unit test keeps this list complete). */
export const MAIN_SPECS = [
  "tests/e2e/home-extras.hub.spec.ts",
  "tests/e2e/main-ui.hub.spec.ts",
  "tests/e2e/owner-notify.hub.spec.ts",
  "tests/e2e/routed-to.hub.spec.ts",
];

export const MAIN_ENV = {
  HUB_E2E_MAIN_UI: "1",
  REMUDA_DELEGATED_DECISIONS: "1",
  HUB_E2E_LINEAGE_FENCE: "1",
};

if (process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url))) {
  const root = join(dirname(fileURLToPath(import.meta.url)), "..");
  const bin = join(root, "node_modules/@playwright/test/cli.js");
  const result = spawnSync(
    process.execPath,
    [bin, "test", "-c", "playwright.hub.config.ts", ...MAIN_SPECS, ...process.argv.slice(2)],
    { cwd: root, stdio: "inherit", env: { ...process.env, ...MAIN_ENV } },
  );
  if (result.error) {
    console.error(`test:e2e:hub:main: ${result.error.message}`);
    process.exit(1);
  }
  process.exit(result.status ?? 1);
}
