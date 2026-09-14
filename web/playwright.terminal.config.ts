import { defineConfig, devices } from "@playwright/test";

const hubUrl = process.env.VITE_HUB_URL ?? "http://127.0.0.1:58280";
const webPort = process.env.TERMINAL_E2E_WEB_PORT ?? "58290";
const origin = `http://127.0.0.1:${webPort}`;

export default defineConfig({
  testDir: "./tests/e2e",
  testMatch: /terminal-live\.spec\.ts/,
  fullyParallel: false,
  workers: 1,
  forbidOnly: Boolean(process.env.CI),
  retries: 0,
  timeout: 120_000,
  use: {
    baseURL: origin,
    trace: "on-first-retry",
  },
  webServer: {
    command: `./node_modules/.bin/vite --host 127.0.0.1 --port ${webPort} --strictPort`,
    url: origin,
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
    stdout: "pipe",
    stderr: "pipe",
    env: {
      ...process.env,
      VITE_MOCK: "0",
      VITE_HUB_URL: hubUrl,
      VITE_DEV_TTY: "1",
      VITE_ACCESS_CODE: process.env.VITE_ACCESS_CODE ?? "",
    },
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        // Google Chrome locally; bundled Chromium on CI or when PW_CHANNEL=chromium (hosts without Chrome).
        ...(process.env.CI || process.env.PW_CHANNEL === "chromium" ? {} : { channel: "chrome" as const }),
      },
    },
  ],
});
