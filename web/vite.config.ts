/// <reference types="vitest/config" />
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import { serviceWorkerPlugin } from "./sw-build.ts";

const hub = process.env.VITE_HUB_URL;
// Gate / CI runs (VITE_NO_WATCH=1) serve a fixed tree and never edit it, so
// the file watcher is pure cost — and on hosts with a small inotify
// `max_user_watches` it fails the whole e2e run with ENOSPC before the first
// request. `watch: null` disables it entirely (Vite ≥ 5).
const noWatch = process.env.VITE_NO_WATCH === "1";

export default defineConfig(({ mode }) => ({
  // c-unboundresume r4 item 1: compile-time gate for the e2e test handle.
  // Literal false in every build except the dedicated `vite build --mode
  // e2e` artifact (web/dist-e2e), so the normal bundle's installation branch
  // is dead-code eliminated. No runtime switch exists.
  define: {
    __REMUDA_TEST_HOOKS__: JSON.stringify(mode === "e2e"),
  },
  plugins: [react(), serviceWorkerPlugin()],
  server: {
    ...(hub
      ? {
          proxy: {
            "/v1": { target: hub, ws: true, changeOrigin: true },
            "/healthz": { target: hub, changeOrigin: true },
          },
        }
      : {}),
    ...(noWatch ? { watch: null } : {}),
  },
  test: {
    environment: "jsdom",
    setupFiles: "./src/test/setup.ts",
    exclude: ["**/node_modules/**", "**/dist/**", "**/tests/e2e/**"],
  },
}));
