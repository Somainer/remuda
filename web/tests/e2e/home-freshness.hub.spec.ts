import {
  devices,
  expect,
  test,
  type Browser,
  type BrowserContext,
  type Page,
  type Route,
} from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * home-freshness (home.md §8 新鲜度) on the two home surfaces.
 *
 * Asserted here, against the in-process fake Node:
 *  - before the first successful list read both surfaces paint SKELETON rows,
 *    never the designed empty state (a cold start must not flash it). The
 *    state is produced the supported way — an offline reload, where the list
 *    reads fail and the cached-session bootstrap publishes ready without any
 *    successful snapshot;
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
 * Put the document into the real pre-snapshot state — the supported offline
 * reload. After login the list reads FAIL, so bootstrap takes its
 * cached-session path and publishes `ready:true, authed:true` with no
 * successful list read (`lastOkAt === null`); AuthGate mounts the route and
 * the lists paint their skeletons.
 *
 * Deliberately NOT a held read: a held bootstrap leaves AuthGate on
 * 「加载中…」 until the 10 s REST deadline aborts the fetch, and only then does
 * the same cached-session path run — a slow, accidental version of this state.
 */
async function failListReads(page: Page): Promise<void> {
  const fail = (route: Route) => route.abort("failed").catch(() => undefined);
  await page.route("**/v1/instances", fail);
  await page.route("**/v1/interactions", fail);
}

async function allowListReads(page: Page): Promise<void> {
  await page.unroute("**/v1/instances");
  await page.unroute("**/v1/interactions");
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
      await page.goto("/m");
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");
      // Reload with the list reads failing: the cached-session bootstrap
      // publishes ready with no successful snapshot, which is the state the
      // skeleton exists for.
      await failListReads(page);
      await page.goto("/m");
      const skeleton = page.getByTestId("home-skeleton");
      await expect(skeleton).toBeVisible({ timeout: 20_000 });
      // The gated route is mounted (not AuthGate's 加载中…), and no designed
      // empty state may stand in for the missing snapshot.
      await expect(page.getByTestId("auth-loading")).toHaveCount(0);
      await expect(page.getByTestId("home-list")).toBeVisible();
      await expect(page.getByTestId("home-empty")).toHaveCount(0);
      await expect(page.getByText("还没有会话")).toHaveCount(0);
      await expect(page.getByText("暂无会话")).toHaveCount(0);
      await shot(page, "home-freshness-skeleton-390.png");
      // Let the next poll succeed: the skeleton gives way.
      await allowListReads(page);
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

      await failListReads(page);
      await page.goto("/sessions");
      const skeleton = page.getByTestId("session-skeleton");
      await expect(skeleton).toBeVisible({ timeout: 20_000 });
      await expect(page.getByTestId("auth-loading")).toHaveCount(0);
      await expect(page.getByText("还没有会话")).toHaveCount(0);
      // /sessions must not paint its own "no hosts" placeholder off a failed
      // bootstrap either (the offline-reload path has ready:true, hosts []).
      await expect(page.getByText("无主机。请添加主机。")).toHaveCount(0);
      await shot(page, "home-freshness-skeleton-1280.png");
      await allowListReads(page);
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
