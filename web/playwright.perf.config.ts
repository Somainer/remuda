import { defineConfig } from "@playwright/test";

/**
 * c-perfaudit: standalone performance-scenario project.
 *
 * Deliberately NOT part of any gate suite:
 *  - testDir is ./tests/perf (the gate configs scan ./tests/e2e);
 *  - the fake Node runs with HUB_E2E_PERF=1 so the perf sentinels
 *    (`__perf_transcript__`, `__perf_tty__`, `__perf_interactions__`) are live;
 *  - scenarios skip unless that trigger is present, so a misconfigured run
 *    fails loudly rather than measuring the normal echo path.
 *
 * Run:
 *   pnpm --dir web exec playwright install chromium webkit   # one-time
 *   HUB_E2E_PERF=1 pnpm --dir web test:perf
 *   HUB_E2E_PERF=1 pnpm --dir web exec playwright test -c playwright.perf.config.ts --project=webkit
 *
 * One script, both engines: the engine name comes from Playwright's
 * browserName and lands in the JSON result; on macOS the co-ordinator runs the
 * same command with --project=webkit for the Safari-engine comparison.
 */

const listen = process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880";
const hubUrl = process.env.VITE_HUB_URL ?? `http://${listen}`;
const webPort = process.env.HUB_E2E_WEB_PORT ?? "58889";
const origin = `http://127.0.0.1:${webPort}`;

// Mark this runner process so the spec can tell the trigger is present (it
// also feeds the fake Node via webServer.env below).
process.env.HUB_E2E_PERF = "1";

export default defineConfig({
  testDir: "./tests/perf",
  // Perf scenarios are named `*.perf.ts`; declare the pattern explicitly so
  // they are never confused with spec/test files and the gate configs never
  // pick them up.
  testMatch: /\.perf\.ts$/,
  fullyParallel: false,
  workers: 1,
  forbidOnly: Boolean(process.env.CI),
  retries: 0,
  timeout: 300_000,
  reporter: [["list"], ["json", { outputFile: "tests/perf/results/playwright-report.json" }]],
  use: {
    baseURL: origin,
    viewport: { width: 1440, height: 900 },
    trace: "off",
  },
  webServer: [
    {
      command: "cargo run -p remuda-hub --example hub_e2e --locked",
      cwd: "..",
      url: `${hubUrl}/healthz`,
      reuseExistingServer: false,
      timeout: 300_000,
      stdout: "pipe",
      stderr: "pipe",
      env: {
        ...process.env,
        HUB_E2E_LISTEN: new URL(hubUrl).host,
        HUB_E2E_ORIGINS: `${origin},http://localhost:${webPort}`,
        HUB_E2E_PERF: "1",
      },
    },
    {
      command: `./node_modules/.bin/vite --host 127.0.0.1 --port ${webPort} --strictPort`,
      url: origin,
      reuseExistingServer: false,
      timeout: 120_000,
      stdout: "pipe",
      stderr: "pipe",
      env: {
        ...process.env,
        VITE_MOCK: "0",
        VITE_HUB_URL: hubUrl,
        VITE_NO_WATCH: "1",
      },
    },
  ],
  projects: [
    {
      name: "chromium",
      // No devices[] descriptor: it hard-codes a Windows UA string. Let the
      // engine report its own UA so the OS/engine evidence is truthful on
      // Linux and macOS; PW_CHANNEL=chromium picks bundled Chromium when no
      // Google Chrome is installed.
      use: process.env.PW_CHANNEL ? { channel: process.env.PW_CHANNEL } : {},
    },
    {
      // Run on macOS for the Safari-engine column (Playwright WebKit == the
      // WKWebView engine family the shell decision is about). browserName
      // must be set explicitly: the project `name` is only a label, so an
      // empty `use` would still launch the default (chromium) browser.
      name: "webkit",
      use: { browserName: "webkit" },
    },
  ],
});
