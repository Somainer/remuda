// Bisect WHY a service-worker-restored page loses same-origin loopback access
// under the Vite harness but not under a plain same-origin Node server.
// Records CDP clientSecurityState.initiatorIPAddressSpace for the post-restore
// WebSocket plus the document's resourceIPAddressSpace, and the exact net
// error (PNA block vs reachable-then-closed).
import { chromium } from "/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/web/node_modules/.pnpm/playwright-core@1.63.0/node_modules/playwright-core/index.mjs";

const target = process.argv[2];
// server                = origin serves /sw.js
// route                 = Playwright fulfills /sw.js with a string body
// routePassthrough      = intercept /sw.js but re-serve the server's response
// routeHeaders          = fulfill string body WITH the server's exact headers
// routeNoOffline        = route, but skip the offline/restore cycle
// routeUnroute          = route-fulfilled SW, then unroute + online update it
const swMode = process.argv[3] ?? "server";
const label = process.argv[4] ?? "probe";
const exe = "/home/wangruming.nana/.cache/ms-playwright/chromium-1243/chrome-linux64/chrome";

const SW = `self.addEventListener("install", (e) => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (event) => {
  if (event.request.method !== "GET") return;
  event.respondWith((async () => {
    const cache = await caches.open("pna-bisect");
    try {
      const res = await fetch(event.request);
      if (res.ok) cache.put(event.request, res.clone());
      return res;
    } catch {
      const hit = await cache.match(event.request, { ignoreSearch: true });
      if (hit) return hit;
      throw new Error("offline");
    }
  })());
});`;

const browser = await chromium.launch({
  executablePath: exe,
  args: ["--headless=new", "--no-sandbox", "--disable-dev-shm-usage"],
  ignoreDefaultArgs: ["--headless=new"],
  headless: true,
});
const ctx = await browser.newContext();
if (swMode === "route" || swMode === "routeNoOffline" || swMode === "routeUnroute") {
  await ctx.route("**/sw.js", (route) =>
    route.fulfill({ contentType: "application/javascript; charset=utf-8", body: SW }),
  );
} else if (swMode === "routePassthrough") {
  await ctx.route("**/sw.js", async (route) => {
    const res = await route.fetch();
    await route.fulfill({ response: res });
  });
} else if (swMode === "routeHeaders") {
  await ctx.route("**/sw.js", (route) =>
    route.fulfill({
      status: 200,
      headers: {
        "Content-Type": "application/javascript; charset=utf-8",
        "Service-Worker-Allowed": "/",
        "Content-Length": String(Buffer.byteLength(SW)),
      },
      body: SW,
    }),
  );
}
const page = await ctx.newPage();
const client = await ctx.newCDPSession(page);
await client.send("Network.enable");
const security = [];
client.on("Network.requestWillBeSent", (m) => {
  if (/ws|sw\.js|\/($|\?)/.test(m.request.url)) {
    security.push({
      phase: "before-offline",
      url: m.request.url.slice(0, 70),
      initiatorSpace: m.clientSecurityState?.initiatorIPAddressSpace,
      policy: m.clientSecurityState?.privateNetworkRequestPolicy,
    });
  }
});
const errs = [];
page.on("console", (m) => {
  const t = m.text();
  if (/ERR_|WebSocket/i.test(t)) errs.push(t.replace(/^.*failed: /, "").slice(0, 100));
});

await page.goto(target, { waitUntil: "domcontentloaded" });

if (swMode === "routeUnroute") {
  // Let the initial install come from the interception once, then stop
  // intercepting so a later update fetch reaches the real origin.
  await page.evaluate(async () => {
    await navigator.serviceWorker.register("/sw.js", { updateViaCache: "none" });
    await navigator.serviceWorker.ready;
  });
  await ctx.unroute("**/sw.js");
} else {
  await page.evaluate(async () => {
    await navigator.serviceWorker.register("/sw.js", { updateViaCache: "none" });
    await navigator.serviceWorker.ready;
    if (!navigator.serviceWorker.controller) {
      await new Promise((r) =>
        navigator.serviceWorker.addEventListener("controllerchange", r, { once: true }),
      );
    }
  });
}
await page.goto(target, { waitUntil: "domcontentloaded" });

const post = [];
client.on("Network.requestWillBeSent", (m) => {
  if (/ping-|ws|sw\.js/.test(m.request.url)) {
    post.push({
      url: m.request.url.slice(0, 70),
      initiatorSpace: m.clientSecurityState?.initiatorIPAddressSpace,
      policy: m.clientSecurityState?.privateNetworkRequestPolicy,
    });
  }
});

const skipOffline = swMode === "routeNoOffline";
if (!skipOffline) {
  await ctx.setOffline(true);
  await page.waitForTimeout(300);
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForTimeout(300);
  await ctx.setOffline(false);
  await page.waitForTimeout(800);
}

const restoredControlled = await page.evaluate(() => Boolean(navigator.serviceWorker.controller));

// HTTP same-origin fetch from the controlled page (goes through the SW).
const httpResult = await page.evaluate(async () => {
  try {
    const res = await fetch("/ping-" + Date.now(), { cache: "no-store" });
    return "HTTP:" + res.status;
  } catch (e) {
    return "HTTP_ERR:" + String(e).slice(0, 60);
  }
});
// clientSecurityState observed for that fetch.
const httpEvt = post.find((e) => e.url.includes("ping-")) ?? null;

const wsResult = await page.evaluate(() =>
  new Promise((resolve) => {
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${location.host}/ws`);
    const t = setTimeout(() => resolve("TIMEOUT"), 4000);
    ws.addEventListener("open", () => {
      clearTimeout(t);
      resolve("OPEN");
    });
    ws.addEventListener("close", (e) => {
      clearTimeout(t);
      resolve("CLOSE:" + e.code);
    });
    ws.addEventListener("error", () => resolve("ERROR"));
  }),
);

// Does the taint follow the CONTROLLED page only? Open a same-origin page in
// a scope the SW does not claim (registration scope is /sw.js's directory /,
// so use an uncontrolled CONTEXT via hard-reload bypass trick is impossible;
// instead unregister in a fresh page and test the WS there).
let afterUnregister = "n/a";
{
  const page2 = await ctx.newPage();
  await page2.goto(target + "/other-" + Date.now(), { waitUntil: "domcontentloaded" });
  await page2.evaluate(async () => {
    const regs = await navigator.serviceWorker.getRegistrations();
    for (const r of regs) await r.unregister();
  });
  await page2.goto(target + "/other2-" + Date.now(), { waitUntil: "domcontentloaded" });
  afterUnregister = await page2.evaluate(() =>
    new Promise((resolve) => {
      const ws = new WebSocket("ws://127.0.0.1:8099/ws");
      const t = setTimeout(() => resolve("TIMEOUT"), 4000);
      ws.addEventListener("open", () => {
        clearTimeout(t);
        resolve("OPEN");
      });
      ws.addEventListener("close", (e) => {
        clearTimeout(t);
        resolve("CLOSE:" + e.code);
      });
      ws.addEventListener("error", () => resolve("ERROR"));
    }),
  );
  await page2.close();
}
await page.waitForTimeout(500);

console.log("RESULT " + JSON.stringify({ label, swMode, skipOffline, restoredControlled, httpResult, httpEvt, wsResult, afterUnregister, post, errs: errs.slice(-6) }));
await browser.close();
