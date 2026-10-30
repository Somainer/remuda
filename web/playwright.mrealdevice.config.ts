import { defineConfig, devices } from "@playwright/test";

/**
 * c-mfix real-device defects: the three owner-reported iPhone failures can
 * only be reproduced on a WebKit engine with a phone device descriptor
 * (soft-keyboard viewport behaviour + WebGL/canvas differences). The shared
 * playwright.hub.config.ts pins a chromium project, so it cannot run this
 * spec; this config mirrors it (same fake-node Hub + Vite servers) with one
 * mobile-webkit/iPhone project.
 *
 * Local host note: Playwright does not ship WebKit for ubuntu20.04; run on a
 * jammy+ host or the mcr.microsoft.com/playwright:v1.63.0-jammy image.
 */
const listen = process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880";
const hubUrl = process.env.VITE_HUB_URL ?? `http://${listen}`;
const webPort = process.env.HUB_E2E_WEB_PORT ?? "58889";
const origin = `http://127.0.0.1:${webPort}`;
const upstream = process.env.HUB_E2E_UPSTREAM_LISTEN ?? "127.0.0.1:58881";
process.env.REMUDA_E2E_BACKEND = "hub";

export default defineConfig({
  testDir: "./tests/e2e",
  testMatch: /m-realdevice\.hub\.spec\.ts/,
  fullyParallel: false,
  workers: 1,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 1 : 0,
  timeout: 120_000,
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
      // The webkit runs happen in a playright-jammy container against Hub/Vite
      // already started on the host; reuse them like the hub server above.
      reuseExistingServer: process.env.HUB_E2E_EXTERNAL === "1",
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
      name: "webkit-iphone",
      use: { ...devices["iPhone 13"] },
    },
  ],
});
