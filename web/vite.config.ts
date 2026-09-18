/// <reference types="vitest/config" />
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";
import { defineConfig, type Plugin } from "vite";
import { cacheNameForBuild } from "./src/lib/swCache.ts";

const hub = process.env.VITE_HUB_URL;
// Gate / CI runs (VITE_NO_WATCH=1) serve a fixed tree and never edit it, so
// the file watcher is pure cost — and on hosts with a small inotify
// `max_user_watches` it fails the whole e2e run with ENOSPC before the first
// request. `watch: null` disables it entirely (Vite ≥ 5).
const noWatch = process.env.VITE_NO_WATCH === "1";

const SW_SOURCE = fileURLToPath(new URL("./sw.src.js", import.meta.url));

function gitCommit(): string | null {
  try {
    return execFileSync("git", ["rev-parse", "--short=12", "HEAD"], {
      cwd: fileURLToPath(new URL(".", import.meta.url)),
      stdio: ["ignore", "pipe", "ignore"],
    })
      .toString()
      .trim();
  } catch {
    return null;
  }
}

// Emit sw.js through the build (instead of Vite's verbatim public-dir copy) so
// its bytes carry the build identity: the cache name folds in the git commit
// when the build env has one, else a hash over the emitted asset file names.
// A new worker on every deploy is what lets the browser's byte-compare update
// check fire and the worker's activate sweep reclaim the stale shell.
function serviceWorker(): Plugin {
  return {
    name: "remuda-service-worker",
    generateBundle(_options, bundle) {
      const commit = gitCommit();
      const buildId =
        commit ??
        createHash("sha256").update(Object.keys(bundle).sort().join("\n")).digest("hex").slice(0, 12);
      const source = readFileSync(SW_SOURCE, "utf8").replaceAll(
        "__CACHE_NAME__",
        cacheNameForBuild(buildId),
      );
      this.emitFile({ type: "asset", fileName: "sw.js", source });
    },
  };
}

export default defineConfig({
  plugins: [react(), serviceWorker()],
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
});
