// Airtight same-origin PNA reproduction. Asserts the reload genuinely served
// offline (fetch fails and only the SW cache can answer), retries the
// same-origin WebSocket several times while offline (like the connection
// machine), then returns to online and tries the WS again. No PNA/LNA flags.
import { chromium } from "/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/web/node_modules/.pnpm/playwright-core@1.63.0/node_modules/playwright-core/index.mjs";

const target = process.argv[2];
const trust = process.argv[3] === "trust";
const label = process.argv[4] ?? "probe";
const exe = "/home/wangruming.nana/.cache/ms-playwright/chromium-1243/chrome-linux64/chrome";
const args = ["--headless=new", "--no-sandbox", "--disable-dev-shm-usage"];
if (trust) args.push(`--unsafely-treat-insecure-origin-as-secure=${target}`);
if ((process.argv[5] === "tls" || process.argv[5] === "1"))
  args.push("--ignore-certificate-errors", "--ignore-certificate-errors-spki-list=");

console.error("LAUNCH_ARGS", JSON.stringify(args)); const browser = await chromium.launch({ executablePath: exe, args, ignoreDefaultArgs: ["--headless=new"], headless: true });
const ctx = await browser.newContext({
  ignoreHTTPSErrors: (process.argv[5] === "tls" || process.argv[5] === "1"),
});
// Expose a one-shot WS attempt to every document before any page script runs.
await ctx.addInitScript(() => {
  window.__ws = () =>
    new Promise((resolve) => {
      const ws = new WebSocket(
        `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`,
      );
      let settled = false;
      const finish = (v) => {
        if (settled) return;
        settled = true;
        clearTimeout(t);
        try {
          ws.close();
        } catch {}
        resolve(v);
      };
      const t = setTimeout(() => finish("TIMEOUT"), 4000);
      ws.addEventListener("open", () => finish("OPEN"));
      ws.addEventListener("close", () => finish("CLOSE"));
      ws.addEventListener("error", () => finish("ERROR"));
    });
});
const page = await ctx.newPage();
const errs = [];
page.on("console", (m) => {
  const t = m.text();
  if (/WebSocket|net::|ERR_/i.test(t)) errs.push(t.replace(/^.*failed: /, "").slice(0, 80));
});

await page.goto(target, { waitUntil: "networkidle" });
await page.evaluate(async () => {
  await navigator.serviceWorker.register("/sw.js", { updateViaCache: "none" });
  await navigator.serviceWorker.ready;
  if (!navigator.serviceWorker.controller) {
    await new Promise((r) =>
      navigator.serviceWorker.addEventListener("controllerchange", r, { once: true }),
    );
  }
});
await page.goto(target, { waitUntil: "networkidle" });
const controlled = await page.evaluate(() => Boolean(navigator.serviceWorker.controller));

const onlineWs = await page.evaluate(() => window.__ws());

await ctx.setOffline(true);
await page.waitForTimeout(400);
// Prove genuine offline: a non-cached network-only fetch must fail.
const offlineFetch = await page.evaluate(async () => {
  try {
    await fetch("/never-cached-" + Date.now(), { cache: "no-store" });
    return "UNEXPECTED_OK";
  } catch {
    return "FAILED_AS_EXPECTED";
  }
});
// Several same-origin WS attempts while offline, like the machine's retries.
const offlineWs = [];
for (let i = 0; i < 3; i += 1) offlineWs.push(await page.evaluate(() => window.__ws()));
// Reload while offline (SW serves the shell).
await page.reload({ waitUntil: "domcontentloaded" });
await page.waitForSelector("#out");
const restoredControlled = await page.evaluate(() => Boolean(navigator.serviceWorker.controller));
// More offline retries from the restored document.
const restoredOfflineWs = await page.evaluate(() => window.__ws());

await ctx.setOffline(false);
await page.waitForTimeout(1000);
const postWs1 = await page.evaluate(() => window.__ws());
await page.waitForTimeout(2000);
const postWs2 = await page.evaluate(() => window.__ws());

console.log(
  "RESULT " +
    JSON.stringify({
      label,
      controlled,
      restoredControlled,
      onlineWs,
      offlineFetch,
      offlineWs,
      restoredOfflineWs,
      postWs1,
      postWs2,
      errs: errs.slice(-10),
    }),
);
await browser.close();
