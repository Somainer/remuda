import { defineConfig, devices } from "@playwright/test";

const port = process.env.SPACES_E2E_WEB_PORT ?? "60189";
const origin = `http://127.0.0.1:${port}`;

export default defineConfig({
  testDir: "./tests/e2e",
  testMatch: "spaces.spec.ts",
  workers: 1,
  timeout: 90_000,
  forbidOnly: Boolean(process.env.CI),
  use: { baseURL: origin, trace: "on-first-retry" },
  webServer: {
    command: `pnpm dev --host 127.0.0.1 --port ${port} --strictPort`,
    url: origin,
    reuseExistingServer: false,
    timeout: 120_000,
    env: { ...process.env, VITE_MOCK: "1", VITE_DEV_TTY: "1" },
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"], channel: "chrome" } }],
});
