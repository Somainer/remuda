import { defineConfig, devices } from "@playwright/test";

// Production-shaped bundle WITH the e2e test handle compiled in
// (`pnpm build:e2e` → web/dist-e2e, vite --mode e2e). Served through the Hub's
// real response headers without Vite. Only handle-dependent hub-live tests run
// here; ordinary production-build checks (CSP/headers) stay in
// playwright.security.config.ts against the normal web/dist.
const listen = process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58890";
const origin = `http://${listen}`;

export default defineConfig({
  testDir: "./tests/e2e",
  testMatch: /hub-live\.spec\.ts/,
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
      REMUDA_WEB_ROOT: "web/dist-e2e",
    },
  },
  projects: [{
    name: "chromium",
    use: { ...devices["Desktop Chrome"], ...(process.env.CI ? {} : { channel: "chrome" as const }) },
  }],
});
