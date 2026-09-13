import { defineConfig, devices } from "@playwright/test";

const listen = process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880";
const hubUrl = process.env.VITE_HUB_URL ?? `http://${listen}`;
const webPort = process.env.HUB_E2E_WEB_PORT ?? "58889";
const origin = `http://127.0.0.1:${webPort}`;

export default defineConfig({
  testDir: "./tests/e2e",
  testMatch: /hub-live\.spec\.ts|pairing\.spec\.ts/,
  fullyParallel: false,
  workers: 1,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 1 : 0,
  timeout: 90_000,
  use: {
    baseURL: origin,
    trace: "on-first-retry",
  },
  webServer: [
    {
      command: "cargo run -p remuda-hub --example hub_e2e --locked",
      cwd: "..",
      url: `${hubUrl}/healthz`,
      reuseExistingServer: process.env.HUB_E2E_EXTERNAL === "1",
      timeout: 300_000,
      stdout: "pipe",
      stderr: "pipe",
      env: {
        ...process.env,
        HUB_E2E_LISTEN: new URL(hubUrl).host,
        HUB_E2E_ORIGINS: `${origin},http://localhost:${webPort}`,
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
      },
    },
  ],
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        ...(process.env.CI ? {} : { channel: "chrome" as const }),
      },
    },
  ],
});
