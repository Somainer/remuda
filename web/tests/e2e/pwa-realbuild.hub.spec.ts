import { expect, test, type Page } from "@playwright/test";
import { build } from "vite";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm } from "node:fs/promises";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

/**
 * Real-production-build offline first visit (c-perffu r9 item 1).
 *
 * pwa-shell.hub.spec.ts proves the precache/offline contract with a SYNTHETIC
 * dist, a hand-supplied 2-URL list and a toy `window.__loadRoute`; it could
 * never catch a hole in the WORKER's build-derived manifest. This spec runs
 * the real thing instead:
 *
 *   1. `vite build` the actual app (vite.config.ts + serviceWorkerPlugin) into
 *      a temp outDir — no synthetic worker, no hand-written manifest;
 *   2. serve that dist unchanged through the same `serve` Hub example;
 *   3. log in, load one real route (/sessions) and let the production sw.js
 *      install and take control, asserting a never-visited lazy route's chunk
 *      (the /board page's `Board-*.js`) has not been requested by the page;
 *   4. go fully offline and navigate through the app's own router to /board;
 *      the real Board page renders and its chunk response is
 *      fromServiceWorker() — i.e. it came from the install-time precache.
 *
 * Build timing on the devbox: vite build ~1.5 s, full `pnpm build` ~17 s — far
 * under the 90 s threshold, so the test stays in the gate set with no
 * REMUDA_PWA_BUILD opt-in (one evidence run recorded in c-perffu r9).
 *
 * The test creates no instances and removes its temp dir and stops its server
 * in finally / afterAll.
 */
const specDir = path.dirname(fileURLToPath(import.meta.url));
const webRoot = path.resolve(specDir, "../..");
const repoRoot = path.resolve(webRoot, "..");
const target = path.resolve(repoRoot, process.env.CARGO_TARGET_DIR ?? "target");
const serveBin = process.env.HUB_E2E_SERVE_BIN ?? path.join(target, "debug/examples/serve");

/** Matches hub-auth's default so the real login form accepts it. */
const BOOTSTRAP_TOKEN = "e2e-bootstrap-token";
/** The lazy module lazyRoutes.ts loads for /board: import("../features/tasks/Board"). */
const BOARD_CHUNK_RE = /^\/assets\/Board-[A-Za-z0-9_-]+\.js$/;

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

/** The build-derived precache manifest stamped into the real dist/sw.js. */
async function readManifest(outDir: string): Promise<string[]> {
  const worker = await readFile(path.join(outDir, "sw.js"), "utf8");
  const match = worker.match(/const PRECACHE_URLS = (\[.*?\]);/s);
  if (!match) throw new Error("real dist/sw.js carries no precache manifest");
  return JSON.parse(match[1]!) as string[];
}

/** Log into the real Hub on `origin` via the production login page. */
async function loginOnOrigin(page: Page, origin: string): Promise<void> {
  await page.goto(`${origin}/login`);
  await expect(page.getByTestId("login-page")).toBeVisible({ timeout: 20_000 });
  const toggle = page.getByTestId("login-use-code");
  if (await toggle.isVisible().catch(() => false)) await toggle.click();
  await page.getByTestId("login-tab-bootstrap").click();
  await page.getByTestId("login-device-name").fill("e2e-pwa-realbuild");
  await page.getByTestId("login-bootstrap-token").fill(BOOTSTRAP_TOKEN);
  await page.getByTestId("login-submit").click();
  // Desktop Chromium lands on the SessionList; the serve Hub has no fake node
  // so the empty state is what renders.
  await expect(page.getByTestId("session-list")).toBeVisible({ timeout: 30_000 });
}

let dir = "";
let outDir = "";

test.beforeAll(async () => {
  if (!process.env.HUB_E2E_SERVE_BIN) {
    await promisify(execFile)("cargo", ["build", "-p", "remuda-hub", "--example", "serve", "--locked"], {
      cwd: repoRoot,
      env: process.env,
      timeout: 570_000,
      maxBuffer: 8 * 1024 * 1024,
    });
  }

  dir = await mkdtemp(path.join(target, "pwa-realbuild-"));
  outDir = path.join(dir, "dist");
  const startedAt = Date.now();
  // The actual production build: same config the app ships with, redirected to
  // a temp dir so the working tree's gitignored dist/ is never touched.
  await build({
    root: webRoot,
    configFile: path.join(webRoot, "vite.config.ts"),
    logLevel: "error",
    build: { outDir, emptyOutDir: true },
  });
  const buildMs = Date.now() - startedAt;
  // Visible in the gate log as the evidence-run timing (devbox: ~1.5 s).
  console.log(`[pwa-realbuild] vite build finished in ${buildMs} ms`);

  const manifest = await readManifest(outDir);
  const boardChunk = manifest.find((url) => BOARD_CHUNK_RE.test(url));
  if (!boardChunk) {
    throw new Error(
      `real precache manifest has no Board route chunk; manifest was ${JSON.stringify(manifest)}`,
    );
  }
}, 240_000);

test.afterAll(async () => {
  if (dir) await rm(dir, { recursive: true, force: true });
});

test("an offline first visit through the router loads a never-opened real route from the precache", async ({
  page,
  context,
}) => {
  test.setTimeout(180_000);
  const dataDir = path.join(dir, "data");
  await mkdir(dataDir, { recursive: true });
  const port = await freePort();
  const origin = `http://127.0.0.1:${port}`;
  let hub: ChildProcess | undefined;
  let hubLog = "";

  try {
    hub = spawn(serveBin, [], {
      cwd: dir,
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        REMUDA_LISTEN: `127.0.0.1:${port}`,
        REMUDA_DATA_DIR: dataDir,
        REMUDA_WEB_ROOT: outDir,
        REMUDA_COOKIE_SECURE: "0",
        REMUDA_BOOTSTRAP_TOKEN: BOOTSTRAP_TOKEN,
        RUST_LOG: "warn",
      },
    });
    hub.stdout?.on("data", (chunk) => (hubLog += String(chunk)));
    hub.stderr?.on("data", (chunk) => (hubLog += String(chunk)));

    await expect
      .poll(
        async () => {
          if (hub.exitCode !== null || hub.signalCode !== null) {
            throw new Error(`serve hub exited early: ${hubLog}`);
          }
          return await page.request
            .get(`${origin}/index.html`)
            .then((r) => r.status())
            .catch(() => 0);
        },
        { timeout: 30_000 },
      )
      .toBe(200);

    const manifest = await readManifest(outDir);
    const boardChunk = manifest.find((url) => BOARD_CHUNK_RE.test(url))!;

    // Record every same-origin asset request, and the Board chunk response in
    // particular. fromServiceWorker() is the proof the chunk came from the
    // install-time precache (a cache-first response is still observable as a
    // page request, so the request list alone cannot show provenance).
    const requested: string[] = [];
    let boardFromSW: boolean | null = null;
    page.on("request", (request) => {
      const url = new URL(request.url());
      if (url.origin === origin) requested.push(url.pathname);
    });
    page.on("response", (response) => {
      const url = new URL(response.url());
      if (url.origin === origin && url.pathname === boardChunk) {
        boardFromSW = response.fromServiceWorker();
      }
    });

    // 1 — online first visit to /sessions: real app boots, logs in, and the
    // production sw.js installs (addAll over the real 96-file manifest) and
    // takes control of the page.
    await loginOnOrigin(page, origin);
    await expect(page).toHaveURL(/\/sessions(?:[/?]|$)/);
    await expect
      .poll(
        async () =>
          await page.evaluate(() => Boolean(navigator.serviceWorker?.controller)),
        { timeout: 30_000 },
      )
      .toBe(true);
    // The never-visited Board route chunk is sitting in the precache, but the
    // page has not asked for it.
    await expect
      .poll(
        async () => await page.evaluate(async (url) => Boolean(await caches.match(url)), boardChunk),
        { timeout: 20_000 },
      )
      .toBe(true);
    expect(requested, `Board chunk fetched before the offline visit: ${requested.join(", ")}`).not.toContain(
      boardChunk,
    );

    // 2 — fully offline, navigate through the app's OWN router (the sidebar
    // link, not a reload) to the never-visited /board route.
    await context.setOffline(true);
    await page.getByRole("link", { name: "任务看板" }).click();
    await expect(page).toHaveURL(/\/board(?:[/?]|$)/, { timeout: 15_000 });
    // The real Board page mounts even though its /v1 projection fetch fails
    // offline (the hook keeps the empty state and still renders the shell).
    await expect(page.getByTestId("board-page")).toBeVisible({ timeout: 20_000 });

    // The chunk the page had never opened was fulfilled by the service worker
    // from its build-derived precache — not by the network (which is down).
    await expect
      .poll(() => boardFromSW, { timeout: 15_000 })
      .toBe(true);
  } finally {
    await context.setOffline(false).catch(() => undefined);
    if (hub) await stopChild(hub);
  }
});
