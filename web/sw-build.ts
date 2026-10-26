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
/** The single token sw.src.js carries for the cache name. */
export const SW_CACHE_PLACEHOLDER = "__CACHE_NAME__";
/** The token replaced with the build-derived precache manifest (JSON array). */
export const SW_PRECACHE_PLACEHOLDER = "__PRECACHE_MANIFEST__";
const DEV_BUILD_ID = "dev";

// Minimal structural view of the Rollup/Rolldown bundle generateBundle
// receives. Vite augments every emitted chunk with `viteMetadata` listing the
// CSS and static assets (fonts, …) the chunk pulls in; those names are the
// build manifest we derive the precache list from — nothing is hand-maintained.
interface BundleChunk {
  type: "chunk";
  fileName: string;
  isEntry?: boolean;
  // Rolldown leaves zero-code chunks in the generateBundle graph (they only
  // carry importedCss/importedAssets) but PRUNES the empty .js at write time,
  // so such a file name must never enter the precache list (addAll would 404
  // and install would fail).
  code?: string;
  imports?: readonly string[];
  dynamicImports?: readonly string[];
  viteMetadata?: {
    importedCss?: ReadonlySet<string>;
    importedAssets?: ReadonlySet<string>;
  };
}
interface BundleAsset {
  type: "asset";
  fileName: string;
  source?: string | Uint8Array;
}
type BundleFile = BundleChunk | BundleAsset | { type: string };
export type BuildBundle = Record<string, BundleFile>;

function asChunk(file: BundleFile | undefined): BundleChunk | null {
  // A missing bundle key is "not a chunk", never a throw: index.html can
  // reference public-dir files (/favicon.svg, /manifest.webmanifest, icons)
  // that a plugin-ordering change may leave out of the generateBundle graph,
  // and a chunk's `imports` can name a key absent from this snapshot. Deref
  // with optional chaining so generateBundle still emits sw.js in that case.
  return file?.type === "chunk" ? (file as BundleChunk) : null;
}

/** Asset references (src/href, leading slash) inside the emitted HTML. */
function htmlAssetReferences(source: string | Uint8Array | undefined): string[] {
  if (source === undefined) return [];
  const text =
    typeof source === "string" ? source : new TextDecoder().decode(source);
  const out: string[] = [];
  const re = /(?:src|href)\s*=\s*["'](\/[^"']+)["']/g;
  for (const match of text.matchAll(re)) out.push(match[1]!.slice(1));
  return out;
}

/**
 * Build-derived precache list: the index.html entry chunk(s) plus the FULL
 * transitive closure of their static `imports` and `dynamicImports` (the
 * lazy route chunks), together with every chunk's Vite-attributed CSS and
 * static assets (fonts, …). Returned as root-relative URLs, sorted and
 * de-duplicated. Walking the closure means an offline first visit to a route
 * the tab never opened can still load its chunk and everything it imports.
 */
export function derivePrecacheUrls(
  bundle: BuildBundle,
  htmlFileName = "index.html",
): string[] {
  const urls = new Set<string>();
  const addBuilt = (fileName: string) => {
    if (fileName) urls.add(`/${fileName}`);
  };

  const roots = new Set<string>();
  const html = bundle[htmlFileName];
  if (html && html.type === "asset") {
    for (const ref of htmlAssetReferences((html as BundleAsset).source)) {
      // Skip non-chunk HTML refs (public-dir files, assets): they are shell
      // precache entries in sw.src.js, never roots of the JS/CSS closure.
      if (asChunk(bundle[ref])) roots.add(ref);
    }
  }
  // Fallback for builds that reach the plugin without an in-bundle HTML (unit
  // fixtures, alternate entries): trust Rollup's entry flags.
  if (roots.size === 0) {
    for (const file of Object.values(bundle)) {
      const chunk = asChunk(file);
      if (chunk?.isEntry) roots.add(chunk.fileName);
    }
  }

  const visited = new Set<string>();
  const queue = [...roots];
  while (queue.length) {
    const fileName = queue.shift()!;
    if (visited.has(fileName)) continue;
    visited.add(fileName);
    const chunk = asChunk(bundle[fileName]);
    if (!chunk) continue;
    // Walk the CSS/assets even for a zero-code chunk: its JS is pruned but
    // its stylesheet/font attribution is real and still gets emitted.
    if (chunk.code !== "") addBuilt(chunk.fileName);
    for (const css of chunk.viteMetadata?.importedCss ?? []) addBuilt(css);
    for (const asset of chunk.viteMetadata?.importedAssets ?? []) addBuilt(asset);
    for (const dep of [...(chunk.imports ?? []), ...(chunk.dynamicImports ?? [])]) {
      if (!visited.has(dep)) queue.push(dep);
    }
  }
  return [...urls].sort();
}

function stampWorker(cacheName: string, precacheUrls: readonly string[] = []): string {
  return readFileSync(SW_SOURCE, "utf8")
    .replaceAll(SW_CACHE_PLACEHOLDER, cacheName)
    .replaceAll(SW_PRECACHE_PLACEHOLDER, JSON.stringify([...precacheUrls]));
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
      const buildBundle = bundle as unknown as BuildBundle;
      const cacheName = cacheNameForBuild(resolveBuildId(Object.keys(buildBundle)));
      // The precache list is stamped at build time from the emitted graph
      // (entry closure + every lazy route chunk + their CSS/assets); the
      // worker never hard-codes a file name. dev serves an empty list because
      // no worker registers there.
      const precacheUrls = derivePrecacheUrls(buildBundle);
      this.emitFile({
        type: "asset",
        fileName: "sw.js",
        source: stampWorker(cacheName, precacheUrls),
      });
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
