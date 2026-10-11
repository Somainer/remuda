import { defineConfig, devices } from "@playwright/test";

/**
 * c-inboxfu mock rerun ONLY (untracked scratch file, never committed): the
 * committed mock config hardcodes 4177 with reuseExistingServer=true, which
 * another worker's dev server can hold; this per-worker copy pins 58787 and
 * never reuses. Both engines, so the plan-disclosure marker is asserted in
 * Chromium and WebKit.
 */
export default defineConfig({
  testDir: "./tests/e2e",
  fullyParallel: true,
  workers: 2,
  retries: 1,
  timeout: 90_000,
  use: {
    baseURL: "http://127.0.0.1:58787",
    trace: "on-first-retry",
  },
  webServer: {
    command: "./node_modules/.bin/vite --host 127.0.0.1 --port 58787 --strictPort",
    url: "http://127.0.0.1:58787/sessions",
    reuseExistingServer: false,
    timeout: 120_000,
    stdout: "pipe",
    stderr: "pipe",
    env: { ...process.env, VITE_MOCK: "1", VITE_DEV_TTY: "1" },
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
    { name: "mobile-webkit", use: { ...devices["iPhone 13"] } },
  ],
});
