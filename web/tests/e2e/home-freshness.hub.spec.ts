import { devices, expect, test, type Browser, type BrowserContext, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * home-freshness (home.md §8 新鲜度) on the two home surfaces.
 *
 * Asserted here, against the in-process fake Node:
 *  - before the first successful list read both surfaces paint SKELETON rows,
 *    never the designed empty state (a cold start must not flash it);
 *  - a search that matches nothing gets the second empty state (没有匹配的会话
 *    + 清除搜索), and 清除搜索 really clears the field;
 *  - an unreachable Hub (context.setOffline) raises the 32 px banner after its
 *    consecutive-failure debounce, with a working 重试 action, and a successful
 *    refresh takes it down again.
 *
 * The 5 s / 10 s boundaries are product constants asserted in the unit tests
 * on the injected clock; this spec only ever awaits the eventual state, never
 * a wall-clock millisecond threshold.
 *
 * Self-skipping: needs the in-process fake Node (`e2e-fake-node`). Evidence
 * screenshots are written only when REMUDA_EVIDENCE=1.
 */

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

/** The fake Node is this spec's trigger; a Hub without it skips, not fails. */
async function needsFakeNode(page: Page): Promise<boolean> {
  const response = await page.request.get("/v1/hosts");
  if (!response.ok()) return false;
  const body = (await response.json()) as { items?: { label?: string }[] };
  return (body.items ?? []).some((row) => row.label === "e2e-fake-node");
}

/**
 * Hold the first list reads so the pre-snapshot paint can be observed. The
 * routes stay installed after release, where they simply pass through.
 */
async function holdListReads(page: Page): Promise<() => void> {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const handler = async (route: Parameters<Parameters<Page["route"]>[1]>[0]) => {
    await gate;
    await route.continue().catch(() => undefined);
  };
  await page.route("**/v1/instances", handler);
  await page.route("**/v1/interactions", handler);
  return release;
}

async function phoneContext(browser: Browser, baseURL: string): Promise<BrowserContext> {
  const context = await browser.newContext({ ...devices["iPhone 13"], baseURL });
  await login(await context.newPage(), "e2e-hfresh-phone");
  return context;
}

test.describe("home freshness (phone)", () => {
  test("a cold /m paints skeleton rows and never the empty state, and a no-match search gets 清除搜索", async ({
    browser,
  }) => {
    const baseURL = test.info().project.use.baseURL;
    expect(baseURL, "project baseURL").toBeTruthy();
    const context = await phoneContext(browser, baseURL!);
    try {
      const page = await context.newPage();
      const release = await holdListReads(page);
      await page.goto("/m");
      // First paint: placeholder rows, not 「还没有会话」/「暂无会话」.
      const skeleton = page.getByTestId("home-skeleton");
      await expect(skeleton).toBeVisible({ timeout: 20_000 });
      await expect(page.getByTestId("home-empty")).toHaveCount(0);
      await expect(page.getByText("还没有会话")).toHaveCount(0);
      await expect(page.getByText("暂无会话")).toHaveCount(0);
      await shot(page, "home-freshness-skeleton-390.png");
      release();
      await expect(skeleton).toHaveCount(0, { timeout: 20_000 });

      // The second empty state, distinct from the first: a search with no
      // matches offers clearing the search, not creating a session.
      await page.getByTestId("home-search").fill("zzz-freshness-no-such-session");
      const empty = page.getByTestId("home-empty");
      await expect(empty).toContainText("没有匹配的会话");
      await expect(page.getByTestId("home-empty-clear")).toHaveText("清除搜索");
      await shot(page, "home-freshness-nomatch-390.png");
      await page.getByTestId("home-empty-clear").click();
      await expect(page.getByTestId("home-search")).toHaveValue("");
    } finally {
      await context.close();
    }
  });

  test("an unreachable Hub raises the banner, 重试 is offered, and recovery takes it down", async ({
    browser,
  }) => {
    const baseURL = test.info().project.use.baseURL;
    expect(baseURL, "project baseURL").toBeTruthy();
    const context = await phoneContext(browser, baseURL!);
    try {
      const page = await context.newPage();
      await page.goto("/m");
      await expect(page.getByTestId("home-list")).toBeVisible();
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");
      await expect(page.getByTestId("hub-banner")).toHaveCount(0);

      await context.setOffline(true);
      const banner = page.getByTestId("hub-banner");
      await expect(banner).toBeVisible({ timeout: 30_000 });
      await expect(banner).toContainText("连不上 Hub");
      await expect(banner).toContainText("上次更新");
      await expect(page.getByTestId("hub-banner-retry")).toHaveText("重试");
      await shot(page, "home-freshness-banner-390.png");

      // Any successful refresh resets the failure run and hides the banner.
      await context.setOffline(false);
      await expect(banner).toHaveCount(0, { timeout: 30_000 });
    } finally {
      await context.close();
    }
  });
});

test.describe("home freshness (desktop)", () => {
  test("a cold /sessions paints skeleton cards, then the unreachable-Hub banner appears and clears", async ({
    browser,
  }) => {
    const baseURL = test.info().project.use.baseURL;
    expect(baseURL, "project baseURL").toBeTruthy();
    const context = await browser.newContext({
      baseURL: baseURL!,
      viewport: { width: 1280, height: 900 },
    });
    try {
      const page = await context.newPage();
      await login(page, "e2e-hfresh-desktop");
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");

      const release = await holdListReads(page);
      await page.goto("/sessions");
      const skeleton = page.getByTestId("session-skeleton");
      await expect(skeleton).toBeVisible({ timeout: 20_000 });
      await expect(page.getByText("还没有会话")).toHaveCount(0);
      await shot(page, "home-freshness-skeleton-1280.png");
      release();
      await expect(skeleton).toHaveCount(0, { timeout: 20_000 });

      await context.setOffline(true);
      const banner = page.getByTestId("hub-banner");
      await expect(banner).toBeVisible({ timeout: 30_000 });
      await expect(banner).toContainText("连不上 Hub");
      await shot(page, "home-freshness-banner-1280.png");
      await context.setOffline(false);
      await expect(banner).toHaveCount(0, { timeout: 30_000 });
    } finally {
      await context.close();
    }
  });
});
