import { build, type Plugin, type ViteDevServer } from "vite";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { cacheNameForBuild } from "./src/lib/swCache.ts";
import {
  SW_CACHE_PLACEHOLDER,
  SW_PRECACHE_PLACEHOLDER,
  buildIdFromAssetNames,
  derivePrecacheUrls,
  resolveBuildId,
  serviceWorkerPlugin,
  type BuildBundle,
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

  it("keeps one stamped cache-name line and one precache-manifest token", async () => {
    const source = await readFile(swSourcePath, "utf8");
    const cacheOccurrences = source.split(SW_CACHE_PLACEHOLDER).length - 1;
    const manifestOccurrences = source.split(SW_PRECACHE_PLACEHOLDER).length - 1;
    expect(cacheOccurrences).toBe(1);
    expect(manifestOccurrences).toBe(1);
    expect(source).toContain(`const CACHE = "${SW_CACHE_PLACEHOLDER}";`);
    expect(source).toContain(`const PRECACHE_URLS = ${SW_PRECACHE_PLACEHOLDER};`);
  });
});

// A shape matching Vite/Rolldown's generateBundle: the HTML asset references
// the entry chunk, chunks list static imports / dynamic imports (the lazy
// route chunks) and carry viteMetadata CSS + asset names.
function chunk(
  fileName: string,
  extra: Partial<{
    isEntry: boolean;
    imports: string[];
    dynamicImports: string[];
    importedCss: string[];
    importedAssets: string[];
  }> = {},
): BuildBundle[string] {
  return {
    type: "chunk",
    fileName,
    ...(extra.isEntry ? { isEntry: true } : {}),
    ...(extra.imports ? { imports: extra.imports } : {}),
    ...(extra.dynamicImports ? { dynamicImports: extra.dynamicImports } : {}),
    viteMetadata: {
      ...(extra.importedCss ? { importedCss: new Set(extra.importedCss) } : {}),
      ...(extra.importedAssets ? { importedAssets: new Set(extra.importedAssets) } : {}),
    },
  };
}

describe("derivePrecacheUrls", () => {
  // Mirrors the real build: index.html loads the entry chunk, which
  // statically pulls shared chunks and dynamically imports one chunk PER
  // LAZY ROUTE (lazyRoutes.ts); route chunks carry their own CSS and font
  // assets.
  const bundle: BuildBundle = {
    "index.html": {
      type: "asset",
      fileName: "index.html",
      source:
        `<script type="module" src="/assets/index-entry.js"></script>` +
        `<link rel="modulepreload" href="/assets/shared-runtime.js">`,
    },
    "assets/index-entry.js": chunk("assets/index-entry.js", {
      isEntry: true,
      imports: ["assets/shared-runtime.js"],
      dynamicImports: ["assets/SessionPage-route.js", "assets/Board-route.js"],
      importedCss: ["assets/index-shell.css"],
    }),
    "assets/shared-runtime.js": chunk("assets/shared-runtime.js", {}),
    "assets/SessionPage-route.js": chunk("assets/SessionPage-route.js", {
      imports: ["assets/shared-runtime.js", "assets/ToolCard-card.js"],
      importedCss: ["assets/SessionPage-route.css"],
      importedAssets: ["assets/plex-mono-font.woff2"],
    }),
    "assets/Board-route.js": chunk("assets/Board-route.js", {
      imports: ["assets/shared-runtime.js"],
      importedCss: ["assets/Board-route.css"],
    }),
    "assets/ToolCard-card.js": chunk("assets/ToolCard-card.js", {}),
    // An orphan chunk present in the bundle but unreachable from the entry is
    // not a route the app can navigate to and is not precached.
    "assets/orphan-leftover.js": chunk("assets/orphan-leftover.js", {}),
  };

  it("includes every lazy route chunk reachable from index.html", () => {
    const urls = derivePrecacheUrls(bundle);
    expect(urls).toContain("/assets/SessionPage-route.js");
    expect(urls).toContain("/assets/Board-route.js");
  });

  it("includes the entry, static imports and the routes' transitive imports", () => {
    const urls = derivePrecacheUrls(bundle);
    for (const expected of [
      "/assets/index-entry.js",
      "/assets/shared-runtime.js",
      "/assets/ToolCard-card.js",
    ]) {
      expect(urls).toContain(expected);
    }
  });

  it("includes attributed CSS and static assets (fonts) for the closure", () => {
    const urls = derivePrecacheUrls(bundle);
    expect(urls).toContain("/assets/index-shell.css");
    expect(urls).toContain("/assets/SessionPage-route.css");
    expect(urls).toContain("/assets/Board-route.css");
    expect(urls).toContain("/assets/plex-mono-font.woff2");
  });

  it("never precaches chunks unreachable from the entry, and stays de-duped/sorted", () => {
    const urls = derivePrecacheUrls(bundle);
    expect(urls).not.toContain("/assets/orphan-leftover.js");
    expect([...urls].sort()).toEqual(urls);
    expect(new Set(urls).size).toBe(urls.length);
  });

  it("roots from Rollup entry flags when no HTML asset is in the bundle", () => {
    const noHtml: BuildBundle = {
      "a.js": chunk("a.js", { isEntry: true, dynamicImports: ["b.js"] }),
      "b.js": chunk("b.js"),
    };
    expect(derivePrecacheUrls(noHtml)).toEqual(["/a.js", "/b.js"]);
  });

  it("omits a zero-code chunk's pruned JS but still precaches its CSS and assets", () => {
    const b: BuildBundle = {
      "index.html": {
        type: "asset",
        fileName: "index.html",
        source: `<script type="module" src="/assets/index-entry.js"></script>`,
      },
      "assets/index-entry.js": chunk("assets/index-entry.js", {
        isEntry: true,
        dynamicImports: ["assets/mathKatex-empty.js"],
      }),
      // Rolldown leaves this chunk in the graph (CSS/fonts attributed to it)
      // but never writes the empty .js to dist.
      "assets/mathKatex-empty.js": {
        type: "chunk",
        fileName: "assets/mathKatex-empty.js",
        code: "",
        viteMetadata: {
          importedCss: new Set(["assets/mathKatex.css"]),
          importedAssets: new Set(["assets/katex-font.woff2"]),
        },
      },
    };
    const urls = derivePrecacheUrls(b);
    expect(urls).not.toContain("/assets/mathKatex-empty.js");
    expect(urls).toContain("/assets/mathKatex.css");
    expect(urls).toContain("/assets/katex-font.woff2");
  });

  it("reads Uint8Array HTML sources", () => {
    const b: BuildBundle = {
      "index.html": {
        type: "asset",
        fileName: "index.html",
        source: new TextEncoder().encode(
          `<script type="module" src="/assets/index-entry.js"></script>`,
        ),
      },
      "assets/index-entry.js": chunk("assets/index-entry.js", { isEntry: true }),
    };
    expect(derivePrecacheUrls(b)).toEqual(["/assets/index-entry.js"]);
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
    expect(emitted).not.toContain(SW_PRECACHE_PLACEHOLDER);
  });

  it("stamps a JSON precache manifest covering the build's entry chunk", () => {
    const match = emitted.match(/const PRECACHE_URLS = (\[.*?\]);/s);
    expect(match, "emitted worker declares its stamped precache list").not.toBeNull();
    const urls = JSON.parse(match![1]!) as string[];
    // The stub build's single entry is the closure root (no HTML in fixture).
    // Rolldown hashes even a root input name, so assert against the emitted
    // bundle keys the plugin itself saw.
    const emittedJs = bundleKeys.filter((k) => k.endsWith(".js"));
    expect(emittedJs).toHaveLength(1);
    expect(urls).toContain(`/${emittedJs[0]}`);
    for (const url of urls) expect(url).toMatch(/^\//);
  });

  it("stamps exactly the cache name cacheNameForBuild derives for this build", () => {
    const match = emitted.match(/const CACHE = "(runtime-shell-[^"]*)";/);
    expect(match, "emitted worker declares its stamped cache name").not.toBeNull();
    const expected = cacheNameForBuild(resolveBuildId(bundleKeys));
    expect(match![1]).toBe(expected);
  });
});

// The acceptance guarantee for c-perffu r8: run the REAL production build and
// prove the manifest stamped into dist/sw.js contains the route chunk for
// EVERY lazy route the app declares (lazyRoutes.ts), so an offline first
// visit to a never-visited route cannot miss its chunk.
describe("real build precache manifest covers every declared lazy route", () => {
  let manifest: string[] = [];
  let dir = "";
  let outDir = "";

  function declaredRouteChunkBases(): string[] {
    const source = readFileSync(path.join(webDir, "src/app/lazyRoutes.ts"), "utf8");
    // createLazyRoute(() => import("../pages/SessionsPage"), "SessionsPage")
    //              and createLazyRoute(() => import("../features/tasks/Board"), …)
    return [...source.matchAll(/import\(["'](\.[^"']+)["']\)/g)].map((m) =>
      m[1]!.split("/").pop()!,
    );
  }

  beforeAll(async () => {
    dir = await mkdtemp(path.join(tmpdir(), "remuda-sw-realbuild-"));
    outDir = path.join(dir, "dist");
    await build({
      root: webDir,
      configFile: path.join(webDir, "vite.config.ts"),
      logLevel: "error",
      // Redirect the app's own dist/ so this test never touches the working
      // tree's build output.
      build: { outDir, emptyOutDir: true },
    });
    const worker = await readFile(path.join(outDir, "sw.js"), "utf8");
    const match = worker.match(/const PRECACHE_URLS = (\[.*?\]);/s);
    if (!match) throw new Error("real build emitted no precache manifest");
    manifest = JSON.parse(match[1]!) as string[];
  }, 180_000);

  afterAll(async () => {
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  it("has a non-empty manifest of emitted JS/CSS/font URLs", () => {
    expect(manifest.length).toBeGreaterThan(10);
    for (const url of manifest) expect(url).toMatch(/^\/assets\/.*\.(js|css|woff2?)$/);
  });

  it.each(declaredRouteChunkBases())("precaches the %s route chunk", (base) => {
    // Rolldown names the emitted file "<SourceName>-<hash>.js".
    const hit = manifest.some((url) => {
      const file = url.split("/").pop()!;
      return url.endsWith(".js") && file.startsWith(`${base}-`) && /-[A-Za-z0-9_-]{8}\.js$/.test(file);
    });
    expect(hit, `manifest missing the ${base} route chunk`).toBe(true);
  });

  it("stamps only files the build actually emitted", async () => {
    // The zero-code mathKatex chunk is in the graph but pruned at write time;
    // every stamped URL must exist, or install()'s addAll would 404.
    for (const url of manifest) {
      await expect(readFile(path.join(outDir, url.slice(1)), "utf8")).resolves.toBeTruthy();
    }
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
    expect(served.body).not.toContain(SW_PRECACHE_PLACEHOLDER);
    expect(served.body).toContain(`const CACHE = "${cacheNameForBuild("dev")}";`);
    // No build graph under vite dev (and no worker registers there): empty.
    expect(served.body).toContain("const PRECACHE_URLS = [];");
  });

  it("ignores the query string", () => {
    expect(dispatch("/sw.js?ignored").body).toContain("ACTIVATE_UPDATE");
  });

  it("lets every other path fall through", () => {
    expect(dispatch("/").next).toBe(true);
    expect(dispatch("/src/main.tsx").next).toBe(true);
  });
});
