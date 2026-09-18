import { expect, test } from "@playwright/test";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { cacheNameForBuild } from "../../src/lib/swCache";

/**
 * PWA shell cache identity. A demo refresh that replaces web/dist wholesale
 * (new Vite asset hashes) must not strand an installed browser on a dead
 * cached shell. This spec drives its OWN Hub origin (the config baseURL is the
 * Vite dev server, and startPWA only registers the worker under PROD, so the
 * dev origin never installs one): a real hand-written service worker
 * (web/sw.src.js, the byte-for-byte source the build stamps) is served over a
 * faked dist, controls the page, then the dist is swapped for one with
 * different asset hashes. After reload the app must render from the network
 * with no 404 and no HTML served for the module script.
 */
const specDir = path.dirname(fileURLToPath(import.meta.url));
const webRoot = path.resolve(specDir, "../..");
const repoRoot = path.resolve(webRoot, "..");
const target = path.resolve(repoRoot, process.env.CARGO_TARGET_DIR ?? "target");
const serveBin = process.env.HUB_E2E_SERVE_BIN ?? path.join(target, "debug/examples/serve");

/** Real worker source; stamp it exactly the way sw-build.ts does for dist. */
async function swFor(build: string): Promise<string> {
  const source = await readFile(path.join(webRoot, "sw.src.js"), "utf8");
  return source.replaceAll("__CACHE_NAME__", cacheNameForBuild(build));
}

const PNG_1PX = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
  "base64",
);

// A faked dist whose only moving part is the hashed module name and the build
// string the module stamps into the DOM. The shell precache list in sw.src.js
// (index.html, manifest, favicon, icons) must all exist or install() rejects
// and the worker never controls the page.
async function writeDist(dir: string, build: string, assetHash: string): Promise<void> {
  const assets = path.join(dir, "assets");
  const icons = path.join(dir, "icons");
  await mkdir(assets, { recursive: true });
  await mkdir(icons, { recursive: true });
  await writeFile(
    path.join(dir, "index.html"),
    `<!doctype html><html lang="zh-CN"><head><meta charset="UTF-8"><link rel="manifest" href="/manifest.webmanifest"><link rel="icon" href="/favicon.svg"><title>runtime</title><script type="module" crossorigin src="/assets/app-${assetHash}.js"></script></head><body><div id="root"></div></body></html>\n`,
  );
  await writeFile(
    path.join(assets, `app-${assetHash}.js`),
    `document.documentElement.dataset.appBuild=${JSON.stringify(build)};` +
      `document.getElementById("root").textContent=${JSON.stringify(`build:${build}`)};` +
      `if("serviceWorker" in navigator){navigator.serviceWorker.register("/sw.js",{updateViaCache:"none"});}\n`,
  );
  await writeFile(path.join(dir, "sw.js"), await swFor(build));
  await writeFile(path.join(dir, "manifest.webmanifest"), JSON.stringify({ name: "runtime" }));
  await writeFile(path.join(dir, "favicon.svg"), "<svg xmlns='http://www.w3.org/2000/svg'/>");
  await writeFile(path.join(icons, "icon-192.png"), PNG_1PX);
  await writeFile(path.join(icons, "icon-512.png"), PNG_1PX);
}

async function freePort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const port = (srv.address() as net.AddressInfo).port;
      srv.close(() => resolve(port));
    });
  });
}

async function stopChild(child: ChildProcess): Promise<void> {
  if (!child.pid || child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise<void>((resolve) => child.once("exit", () => resolve()));
  child.kill("SIGTERM");
  const timer = setTimeout(() => child.kill("SIGKILL"), 10_000);
  await exited;
  clearTimeout(timer);
}

test.beforeAll(async () => {
  if (process.env.HUB_E2E_SERVE_BIN) return;
  await promisify(execFile)("cargo", ["build", "-p", "remuda-hub", "--example", "serve", "--locked"], {
    cwd: repoRoot,
    env: process.env,
    timeout: 570_000,
    maxBuffer: 8 * 1024 * 1024,
  });
});

test("a redeployed shell renders on reload with no 404 and no HTML for the module script", async ({ page }) => {
  test.setTimeout(120_000);
  const dir = await mkdtemp(path.join(target, "pwa-shell-"));
  const served = path.join(dir, "web");
  const dataDir = path.join(dir, "data");
  await mkdir(dataDir, { recursive: true });
  const port = await freePort();
  const origin = `http://127.0.0.1:${port}`;
  let hub: ChildProcess | undefined;
  let hubLog = "";

  try {
    await writeDist(served, "v1", "1111aaaa");
    hub = spawn(serveBin, [], {
      cwd: dir,
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        REMUDA_LISTEN: `127.0.0.1:${port}`,
        REMUDA_DATA_DIR: dataDir,
        REMUDA_WEB_ROOT: served,
        REMUDA_COOKIE_SECURE: "0",
        REMUDA_BOOTSTRAP_TOKEN: "pwa-shell-e2e",
        RUST_LOG: "warn",
      },
    });
    hub.stdout?.on("data", (chunk) => (hubLog += String(chunk)));
    hub.stderr?.on("data", (chunk) => (hubLog += String(chunk)));

    await expect
      .poll(async () => {
        if (hub && (hub.exitCode !== null || hub.signalCode !== null)) {
          throw new Error(`serve hub exited early: ${hubLog}`);
        }
        return await page.request
          .get(`${origin}/index.html`)
          .then((r) => r.status())
          .catch(() => 0);
      }, { timeout: 30_000 })
      .toBe(200);

    // Every same-origin response, so we can assert what the module request got.
    const responses: { url: string; status: number; type: string }[] = [];
    page.on("response", (response) => {
      const url = new URL(response.url());
      if (url.origin !== origin) return;
      responses.push({
        url: url.pathname,
        status: response.status(),
        type: response.headers()["content-type"] ?? "",
      });
    });

    // 1 — first load installs and activates the worker over the v1 shell.
    await page.goto(`${origin}/`);
    await expect(page.locator("#root")).toHaveText("build:v1");
    await expect
      .poll(async () => await page.evaluate(() => Boolean(navigator.serviceWorker.controller)), {
        timeout: 20_000,
      })
      .toBe(true);

    // Cache-Control policy the Hub applied to the shell it just served.
    const shellHeaders = await page.request.get(`${origin}/index.html`);
    expect(shellHeaders.headers()["cache-control"]).toBe("no-cache");
    const swHeaders = await page.request.get(`${origin}/sw.js`);
    expect(swHeaders.headers()["cache-control"]).toBe("no-cache");

    // 2 — the demo refresh: replace the dist wholesale with new asset hashes.
    await rm(served, { recursive: true, force: true });
    await writeDist(served, "v2", "2222bbbb");
    // The old hashed asset is gone; a request for it must 404, not serve HTML.
    const stale = await page.request.get(`${origin}/assets/app-1111aaaa.js`);
    expect(stale.status()).toBe(404);
    expect(stale.headers()["content-type"] ?? "").not.toContain("text/html");
    const fresh = await page.request.get(`${origin}/assets/app-2222bbbb.js`);
    expect(fresh.headers()["cache-control"]).toBe("public, max-age=31536000, immutable");

    responses.length = 0;

    // 3 — reload while the v1 worker still controls: network-first navigation
    // serves the v2 shell and the new module loads, so the app mounts fresh.
    await page.reload();
    await expect(page.locator("#root")).toHaveText("build:v2", { timeout: 20_000 });
    await expect(page.locator("html")).toHaveAttribute("data-app-build", "v2");

    const moduleResponse = responses.find((r) => r.url === "/assets/app-2222bbbb.js");
    expect(moduleResponse, "the v2 module was requested").toBeTruthy();
    expect(moduleResponse!.status).toBe(200);
    expect(moduleResponse!.type).toContain("javascript");

    // No 404 and no HTML masquerading as a module for any /assets request.
    for (const r of responses.filter((row) => row.url.startsWith("/assets/"))) {
      expect(r.status, `${r.url} status`).toBe(200);
      expect(r.type, `${r.url} content-type`).not.toContain("text/html");
    }
  } finally {
    if (hub) await stopChild(hub);
    await rm(dir, { recursive: true, force: true });
  }
});
