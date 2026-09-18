import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import type { Plugin } from "vite";
import { cacheNameForBuild } from "./src/lib/swCache.ts";

// Under the Vite/Vitest module runner import.meta.url is not always a file: URL,
// so fall back to the working directory (the web package root in every build
// and test invocation).
const here = (() => {
  try {
    return path.dirname(fileURLToPath(import.meta.url));
  } catch {
    return process.cwd();
  }
})();

const SW_SOURCE = path.join(here, "sw.src.js");
/** The single token sw.src.js carries; it must survive nowhere in dist/sw.js. */
export const SW_CACHE_PLACEHOLDER = "__CACHE_NAME__";
const DEV_BUILD_ID = "dev";

function stampWorker(cacheName: string): string {
  return readFileSync(SW_SOURCE, "utf8").replaceAll(SW_CACHE_PLACEHOLDER, cacheName);
}

export function readGitCommit(): string | null {
  try {
    return execFileSync("git", ["rev-parse", "--short=12", "HEAD"], {
      cwd: here,
      stdio: ["ignore", "pipe", "ignore"],
    })
      .toString()
      .trim();
  } catch {
    return null;
  }
}

/** Fallback build identity when the build env exposes no git commit. */
export function buildIdFromAssetNames(bundleKeys: readonly string[]): string {
  return createHash("sha256")
    .update([...bundleKeys].sort().join("\n"))
    .digest("hex")
    .slice(0, 12);
}

/** Git commit when available, else a hash over the emitted asset file names. */
export function resolveBuildId(bundleKeys: readonly string[]): string {
  return readGitCommit() ?? buildIdFromAssetNames(bundleKeys);
}

// Emit sw.js through the build (instead of Vite's verbatim public-dir copy) so
// its bytes carry the build identity: the cache name folds in the git commit
// when the build env has one, else a hash over the emitted asset file names.
// A new worker on every deploy is what lets the browser's byte-compare update
// check fire and the worker's activate sweep reclaim the stale shell. The dev
// middleware serves the same stamped worker at /sw.js so the unconditional
// registration in push.ts keeps working under `vite dev` (the public-dir copy
// is gone, so nothing else serves that path in development).
export function serviceWorkerPlugin(): Plugin {
  return {
    name: "remuda-service-worker",
    generateBundle(_options, bundle) {
      const cacheName = cacheNameForBuild(resolveBuildId(Object.keys(bundle)));
      this.emitFile({ type: "asset", fileName: "sw.js", source: stampWorker(cacheName) });
    },
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        if (req.url?.split("?", 1)[0] !== "/sw.js") {
          next();
          return;
        }
        res.setHeader("Content-Type", "text/javascript; charset=utf-8");
        res.setHeader("Cache-Control", "no-cache");
        res.end(stampWorker(cacheNameForBuild(DEV_BUILD_ID)));
      });
    },
  };
}
