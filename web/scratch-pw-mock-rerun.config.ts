import { defineConfig, devices } from "@playwright/test";

/**
 * c-endreason gate rerun ONLY (untracked scratch file, never committed): the
 * committed mock config hardcodes 4177 with reuseExistingServer=true, which
 * another worker's dev server can hold; this per-worker copy pins 58777 and
 * never reuses.
 */
export default defineConfig({
  testDir: "./tests/e2e",
  fullyParallel: true,
  workers: 2,
  retries: 1,
  timeout: 90_000,
  use: {
    baseURL: "http://127.0.0.1:58777",
    trace: "on-first-retry",
  },
  webServer: {
    command: "./node_modules/.bin/vite --host 127.0.0.1 --port 58777 --strictPort",
    url: "http://127.0.0.1:58777/sessions",
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
  ],
});
