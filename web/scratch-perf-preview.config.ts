import { defineConfig } from "vite";

/**
 * c-perffu scratch-only config: serve the PRODUCTION build in scratch/perf-dist
 * with the same same-origin /v1 proxy the dev server uses, so the production
 * bundle is measured against the real fake Hub. Untracked, never committed.
 */
const hub = process.env.VITE_HUB_URL;
const port = Number(process.env.HUB_E2E_WEB_PORT ?? "59589");

export default defineConfig({
  configFile: false,
  root: "..", // repo root: outDir is resolved relative to root
  build: { outDir: process.env.PERF_DIST ?? "scratch/perf-dist" },
  preview: {
    host: "127.0.0.1",
    port,
    strictPort: true,
    ...(hub
      ? {
          proxy: {
            "/v1": { target: hub, ws: true, changeOrigin: true },
            "/healthz": { target: hub, changeOrigin: true },
          },
        }
      : {}),
  },
});
