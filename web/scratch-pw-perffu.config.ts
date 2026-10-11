import { defineConfig, devices } from "@playwright/test";

/**
 * c-perffu scratch-only mock rerun (untracked, never committed): like the
 * other per-worker scratch configs, pins a unique port and never reuses a
 * foreign vite on the hardcoded 4177. Mock backend only (agent-board.spec).
 */
export default defineConfig({
  testDir: "./tests/e2e",
  fullyParallel: true,
  workers: 2,
  retries: 0,
  timeout: 90_000,
  reporter: [["list"]],
  use: {
    baseURL: "http://127.0.0.1:58787",
    trace: "off",
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
      use: { ...devices["Desktop Chrome"], channel: "chromium" },
    },
  ],
});
