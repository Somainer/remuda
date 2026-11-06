#!/usr/bin/env node
/**
 * The gate's `web-hub-e2e-main` pass (`pnpm run test:e2e:hub:main`).
 *
 * Every hub spec that self-skips for a harness flag no other gate pass sets
 * runs here, on its own Hub, after the main `test:e2e:hub` pass. The specs are
 * grouped by the trigger they need; a group starts one Hub with exactly its
 * flags, so one fixture trigger (an extra host, a mutated hello) never changes
 * another group's Hub:
 *  - `main`: HUB_E2E_MAIN_UI=1 seats Main; REMUDA_DELEGATED_DECISIONS=1 and
 *    HUB_E2E_LINEAGE_FENCE=1 are for routed-to's D-051 switch and the fake
 *    Node that dispatches for an Agent.
 *  - `fake-node`: HUB_E2E_FAKE_NODE=1 for ux-permmode and cmd-queue; `effort-sentinels`
 *    runs only effort-sync's sentinel describe (its other cases already run
 *    in the main pass), with the same flag.
 *  - `task-bind`, `project-switcher`, `dir-picker`, `cardsettle`, `api-route`:
 *    one harness trigger each (see crates/remuda-hub/examples/hub_e2e.rs).
 * The env is set here rather than in package.json so the command is the same
 * on every platform. Extra arguments go to Playwright for every group.
 *
 * The main pass is near its 3600 s budget, so nothing is added there; a unit
 * test (e2e-hub-main.test.mjs) keeps this list equal to the specs that
 * self-skip on a flag the gate must set.
 */

import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Flags a spec skips without that are deliberately NOT gate flags: evidence
 * capture only feeds committed screenshots (refreshed by hand), and the perf
 * probe runs through playwright.perf.config.ts.
 */
export const EXEMPT_FLAGS = ["REMUDA_EVIDENCE", "REMUDA_PERF"];

/** The gate's opt-in groups, in run order. */
export const GROUPS = [
  {
    name: "main",
    specs: [
      "tests/e2e/home-extras.hub.spec.ts",
      "tests/e2e/main-ui.hub.spec.ts",
      "tests/e2e/owner-notify.hub.spec.ts",
      "tests/e2e/routed-to.hub.spec.ts",
      "tests/e2e/seat-profile.hub.spec.ts",
    ],
    env: {
      HUB_E2E_MAIN_UI: "1",
      REMUDA_DELEGATED_DECISIONS: "1",
      HUB_E2E_LINEAGE_FENCE: "1",
    },
  },
  {
    name: "fake-node",
    specs: ["tests/e2e/ux-permmode.hub.spec.ts", "tests/e2e/cmd-queue.hub.spec.ts"],
    env: { HUB_E2E_FAKE_NODE: "1" },
  },
  {
    // Only the sentinel describe needs the flag; its other cases already run
    // in the main pass, so grep keeps each gate test to one run.
    name: "effort-sentinels",
    specs: ["tests/e2e/effort-sync.hub.spec.ts"],
    grep: "fake-node sentinel configure outcomes",
    env: { HUB_E2E_FAKE_NODE: "1" },
  },
  {
    name: "task-bind",
    specs: ["tests/e2e/task-model-bind.hub.spec.ts", "tests/e2e/task-model-boardui.hub.spec.ts"],
    env: { HUB_E2E_TASK_BIND: "1" },
  },
  {
    name: "project-switcher",
    specs: ["tests/e2e/task-model-project.hub.spec.ts"],
    env: { HUB_E2E_PROJECT_SWITCHER: "1" },
  },
  {
    name: "dir-picker",
    specs: ["tests/e2e/dir-picker.hub.spec.ts"],
    env: { HUB_E2E_DIR_PICKER: "1" },
  },
  {
    name: "cardsettle",
    specs: ["tests/e2e/cardsettle.hub.spec.ts"],
    env: { HUB_E2E_CARDSETTLE: "1" },
  },
  {
    name: "api-route",
    specs: ["tests/e2e/api-route.hub.spec.ts"],
    env: { HUB_E2E_API_ROUTE: "1" },
  },
];

/** The Main group under the names older branches and reviews use for it. */
export const MAIN_SPECS = GROUPS[0].specs;
export const MAIN_ENV = GROUPS[0].env;

if (process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url))) {
  const root = join(dirname(fileURLToPath(import.meta.url)), "..");
  const bin = join(root, "node_modules/@playwright/test/cli.js");
  for (const group of GROUPS) {
    const args = [bin, "test", "-c", "playwright.hub.config.ts", ...group.specs];
    if (group.grep) args.push("--grep", group.grep);
    args.push(...process.argv.slice(2));
    const result = spawnSync(process.execPath, args, {
      cwd: root,
      stdio: "inherit",
      env: { ...process.env, ...group.env },
    });
    if (result.error) {
      console.error(`test:e2e:hub:main (${group.name}): ${result.error.message}`);
      process.exit(1);
    }
    if (result.status !== 0) process.exit(result.status ?? 1);
  }
  process.exit(0);
}
