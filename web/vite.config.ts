/// <reference types="vitest/config" />
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const hub = process.env.VITE_HUB_URL;

export default defineConfig({
  plugins: [react()],
  server: hub
    ? {
        proxy: {
          "/v1": { target: hub, ws: true, changeOrigin: true },
          "/healthz": { target: hub, changeOrigin: true },
        },
      }
    : undefined,
  test: {
    environment: "jsdom",
    setupFiles: "./src/test/setup.ts",
    exclude: ["**/node_modules/**", "**/dist/**", "**/tests/e2e/**"],
  },
});
