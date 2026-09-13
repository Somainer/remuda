import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./tests/e2e",
  testMatch: /pty-toolcalls(-before)?\.spec\.ts/,
  use: {
    baseURL: process.env.PTY_TOOLCALLS_BASE_URL ?? "http://127.0.0.1:61680",
    trace: "off",
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"], channel: "chrome" } },
  ],
});
