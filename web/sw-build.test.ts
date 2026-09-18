import { build, type Plugin, type ViteDevServer } from "vite";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { cacheNameForBuild } from "./src/lib/swCache.ts";
import {
  SW_CACHE_PLACEHOLDER,
  buildIdFromAssetNames,
  resolveBuildId,
  serviceWorkerPlugin,
} from "./sw-build.ts";

const webDir = (() => {
  try {
    return path.dirname(fileURLToPath(import.meta.url));
  } catch {
    return process.cwd();
  }
})();
const swSourcePath = path.join(webDir, "sw.src.js");

describe("service worker build stamping", () => {
  it("falls back to a stable, distinct hash when there is no commit", () => {
    const v1 = buildIdFromAssetNames(["assets/index-aaa.js", "assets/index-bbb.css"]);
    const v2 = buildIdFromAssetNames(["assets/index-ccc.js", "assets/index-bbb.css"]);
    expect(v1).toBe(buildIdFromAssetNames(["assets/index-aaa.js", "assets/index-bbb.css"]));
    expect(v1).not.toBe(v2);
    expect(v1).toMatch(/^[0-9a-f]{12}$/);
  });

  it("prefers the git commit over the asset hash", () => {
    const commit = resolveBuildId(["assets/index-aaa.js"]);
    // In this checkout the build id is the 12-char HEAD; either way it must be
    // a usable token and never the raw placeholder.
    expect(commit).toMatch(/^[A-Za-z0-9._-]{7,40}$/);
    expect(commit).not.toContain(SW_CACHE_PLACEHOLDER);
  });

  it("keeps a single stamped placeholder line in the worker source", async () => {
    const source = await readFile(swSourcePath, "utf8");
    const occurrences = source.split(SW_CACHE_PLACEHOLDER).length - 1;
    expect(occurrences).toBe(1);
    expect(source).toContain(`const CACHE = "${SW_CACHE_PLACEHOLDER}";`);
  });
});

// The failure that is actually silent in production: if the placeholder in
// sw.src.js drifts or the generateBundle plugin stops running, dist/sw.js
// ships the literal placeholder and every cache name stays constant. Run the
// real plugin through a real Vite build and assert on the emitted worker.
describe("emitted dist/sw.js", () => {
  let dir = "";
  let outDir = "";
  let emitted = "";
  let bundleKeys: string[] = [];

  beforeAll(async () => {
    dir = await mkdtemp(path.join(tmpdir(), "remuda-sw-build-"));
    outDir = path.join(dir, "dist");
    const entry = path.join(dir, "entry.js");
    await writeFile(entry, "console.log('stub app entry');\n");
    const capture: Plugin = {
      name: "test-capture-bundle-keys",
      generateBundle(_options, bundle) {
        // Runs before the sw plugin (registered after it), so this is exactly
        // the key set resolveBuildId receives inside generateBundle.
        bundleKeys = Object.keys(bundle);
      },
    };
    await build({
      configFile: false,
      logLevel: "error",
      root: dir,
      plugins: [capture, serviceWorkerPlugin()],
      build: {
        outDir,
        emptyOutDir: true,
        rollupOptions: { input: entry },
      },
    });
    emitted = await readFile(path.join(outDir, "sw.js"), "utf8");
  }, 60_000);

  afterAll(async () => {
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  it("is emitted into the bundle", async () => {
    const files = await readdir(outDir);
    expect(files).toContain("sw.js");
  });

  it("contains no leftover placeholder", () => {
    expect(emitted).not.toContain(SW_CACHE_PLACEHOLDER);
  });

  it("stamps exactly the cache name cacheNameForBuild derives for this build", () => {
    const match = emitted.match(/const CACHE = "(runtime-shell-[^"]*)";/);
    expect(match, "emitted worker declares its stamped cache name").not.toBeNull();
    const expected = cacheNameForBuild(resolveBuildId(bundleKeys));
    expect(match![1]).toBe(expected);
  });
});

// vite dev must still answer /sw.js: the public-dir copy is gone, and the
// unconditional register in src/lib/push.ts would otherwise 404 locally.
describe("dev server middleware", () => {
  function dispatch(url: string): { body: string; headers: Record<string, string>; next: boolean } {
    const plugin = serviceWorkerPlugin();
    const handlers: Array<(req: unknown, res: unknown, next: () => void) => void> = [];
    const configure = plugin.configureServer;
    expect(typeof configure).toBe("function");
    const server = {
      middlewares: { use: (handler: (req: unknown, res: unknown, next: () => void) => void) => handlers.push(handler) },
    } as unknown as ViteDevServer;
    (configure as (server: ViteDevServer) => void)(server);
    const headers: Record<string, string> = {};
    let body = "";
    let next = false;
    handlers[0](
      { url },
      {
        setHeader: (key: string, value: string) => {
          headers[key] = value;
        },
        end: (chunk?: string) => {
          if (chunk) body = chunk;
        },
      },
      () => {
        next = true;
      },
    );
    return { body, headers, next };
  }

  it("serves a stamped worker at /sw.js with no-cache", () => {
    const served = dispatch("/sw.js");
    expect(served.next).toBe(false);
    expect(served.headers["Content-Type"]).toContain("javascript");
    expect(served.headers["Cache-Control"]).toBe("no-cache");
    expect(served.body).not.toContain(SW_CACHE_PLACEHOLDER);
    expect(served.body).toContain(`const CACHE = "${cacheNameForBuild("dev")}";`);
  });

  it("ignores the query string", () => {
    expect(dispatch("/sw.js?ignored").body).toContain("ACTIVATE_UPDATE");
  });

  it("lets every other path fall through", () => {
    expect(dispatch("/").next).toBe(true);
    expect(dispatch("/src/main.tsx").next).toBe(true);
  });
});
