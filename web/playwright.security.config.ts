import { defineConfig, devices } from "@playwright/test";

// Exercise the built app behind the Hub's real response headers, without Vite.
const listen = process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880";
const origin = `http://${listen}`;

export default defineConfig({
  testDir: "./tests/e2e",
  testMatch: /hub-live\.spec\.ts|hub-security\.spec\.ts/,
  fullyParallel: false,
  workers: 1,
  timeout: 90_000,
  use: { baseURL: origin, trace: "retain-on-failure" },
  webServer: {
    command: "cargo run -p remuda-hub --example hub_e2e --locked",
    cwd: "..",
    url: `${origin}/healthz`,
    reuseExistingServer: false,
    timeout: 180_000,
    env: {
      ...process.env,
      HUB_E2E_LISTEN: listen,
      HUB_E2E_ORIGINS: origin,
      REMUDA_WEB_ROOT: "web/dist",
    },
  },
  projects: [{
    name: "chromium",
    use: { ...devices["Desktop Chrome"], ...(process.env.CI ? {} : { channel: "chrome" as const }) },
  }],
});
