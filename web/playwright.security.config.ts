import { defineConfig, devices } from "@playwright/test";

// Exercise the ORDINARY production app (normal `pnpm build` → web/dist) behind
// the Hub's real response headers, without Vite. hub-live flows run here
// against the normal bundle EXCEPT the @e2e-handle-tagged test, which needs
// the compile-time test handle and runs against the distinct web/dist-e2e
// artifact via playwright.security-e2e.config.ts (c-unboundresume r4/r5): the
// normal production bundle must not contain the handle at all.
const listen = process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880";
const origin = `http://${listen}`;

export default defineConfig({
  testDir: "./tests/e2e",
  // Anchored: exactly hub-live.spec.ts (NOT passkey-hub-live.spec.ts, which is
  // a separate CDP-authenticator suite with its own configuration).
  testMatch: /(^|[/\\])hub-live\.spec\.ts$|(^|[/\\])hub-security\.spec\.ts$/,
  grepInvert: /@e2e-handle/,
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
