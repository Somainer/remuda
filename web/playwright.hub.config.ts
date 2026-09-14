import { defineConfig, devices } from "@playwright/test";

const listen = process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880";
const hubUrl = process.env.VITE_HUB_URL ?? `http://${listen}`;
const webPort = process.env.HUB_E2E_WEB_PORT ?? "58889";
const origin = `http://127.0.0.1:${webPort}`;
/** Fake Anthropic-Messages gateway the provider-discovery spec probes. */
const upstream = process.env.HUB_E2E_UPSTREAM_LISTEN ?? "127.0.0.1:58881";

export default defineConfig({
  testDir: "./tests/e2e",
  testMatch: /(?:hub-live|spaces-hub-live|pairing|providers-discovery|passkey-hub-live)\.spec\.ts/,
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
        HUB_E2E_UPSTREAM_LISTEN: upstream,
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
        VITE_E2E_UPSTREAM: `http://${upstream}`,
      },
    },
  ],
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
