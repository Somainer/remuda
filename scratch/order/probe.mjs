import { chromium } from "/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/web/node_modules/.pnpm/playwright-core@1.63.0/node_modules/playwright-core/index.mjs";
const browser = await chromium.launch({ executablePath: "/home/wangruming.nana/.cache/ms-playwright/chromium-1243/chrome-linux64/chrome", args: ["--headless=new","--no-sandbox"], ignoreDefaultArgs: ["--headless=new"], headless: true });
const ctx = await browser.newContext();
const collected = [];
for (const sw of [false]) {
  const page = await ctx.newPage();
  page.on("console", (m) => collected.push(m.text()));
  await page.goto("http://127.0.0.1:8096/page.html");
  // Inject a SW-less simple reload via JS
  await page.evaluate(() => sessionStorage.removeItem("x"));
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForTimeout(300);
}
console.log("done", JSON.stringify(collected));
await browser.close();
