import { defineConfig, devices } from "@playwright/test";

const browserChannel = process.env.PW_CHANNEL || "chrome";

export default defineConfig({
  testDir: "./tests/e2e",
  testIgnore: /hub-live\.spec\.ts|hub-security\.spec\.ts|terminal-live\.spec\.ts|pairing\.spec\.ts|providers-discovery\.spec\.ts|pty-toolcalls(-before)?\.spec\.ts|ux-quickfind\.spec\.ts|\.hub\.spec\.ts$/,
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 2 : 0,
  use: {
    baseURL: "http://127.0.0.1:4177",
    trace: "on-first-retry",
  },
  webServer: {
    command: "pnpm dev --host 127.0.0.1 --port 4177 --strictPort",
    url: "http://127.0.0.1:4177/sessions",
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
    env: { ...process.env, VITE_MOCK: "1", VITE_DEV_TTY: "1" },
  },
  projects: [
    {
      name: "chromium",
      // Google Chrome by default (matches CI runners); set PW_CHANNEL=chromium to use
      // Playwright's bundled Chromium on hosts without Chrome (e.g. the remote merge gate).
      use: { ...devices["Desktop Chrome"], ...(browserChannel === "chromium" ? {} : { channel: browserChannel }) },
    },
    { name: "mobile-webkit", use: { ...devices["iPhone 13"] } },
  ],
});
