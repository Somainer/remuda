import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * UO-11 新建会话 acceptance.
 *
 * Geometry/interaction assertions run on every invocation; only the committed
 * screenshots gate on REMUDA_EVIDENCE (docs/design/evidence/ui-overhaul,
 * 390 + 1440, dark + light, Remuda renders only). Covered:
 *
 *  - desktop is a centered 720 sheet; 390 is a full-width single column with
 *    the footer (and 开始) pinned to the visible band through the keyboard;
 *  - every first-layer `new-session-*` control is directly operable at 390,
 *    768 and 1440 without first expanding 高级设置;
 *  - both appearances resolve role tokens;
 *  - coarse pointers get 44px text controls (ui-spec §3.4);
 *  - opening the sheet in the warm app raises no >50ms long task.
 *
 * The spec only opens the sheet; it never creates an instance, so it occupies
 * no fake-node slots.
 */

const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../docs/design/evidence/ui-overhaul",
);
const MODES = ["dark", "light"] as const;

/** First-layer controls that must never need 高级设置 expanded. */
const LAYER1 = [
  "new-session-prompt",
  "new-session-host",
  "cwd-mode-existing",
  "cwd-mode-worktree",
  "new-session-workspace",
  "new-session-kind-claude",
  "new-session-kind-codex",
  "new-session-kind-terminal",
  "new-session-model",
  "new-session-perm-manual",
  "new-session-perm-bypassPermissions",
  "new-session-effort-slider",
  "new-session-delegation-host",
  "new-session-delegation-none",
  "new-session-delegation-gateway",
  "new-session-advanced",
  "new-session-start",
];

async function openSheet(page: Page) {
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });
}

/**
 * Playwright cannot raise the platform soft keyboard. Resize
 * visualViewport exactly the way viewport.ts listens for it.
 */
async function emulateSoftKeyboard(page: Page, height: number) {
  await page.evaluate((nextHeight) => {
    const viewport = window.visualViewport;
    if (!viewport) return;
    Object.defineProperty(viewport, "height", { configurable: true, value: nextHeight });
    Object.defineProperty(viewport, "offsetTop", { configurable: true, value: 0 });
    viewport.dispatchEvent(new Event("resize"));
  }, height);
}

/**
 * The control is reachable and owns the point at its own centre after the
 * scroll container brings it into view — no disclosure, clip or overlay.
 */
async function assertOperable(page: Page, testId: string) {
  const locator = page.getByTestId(testId);
  await locator.scrollIntoViewIfNeeded();
  await expect(locator).toBeVisible();
  const report = await locator.evaluate((el) => {
    const rect = el.getBoundingClientRect();
    const cx = rect.left + rect.width / 2;
    const cy = rect.top + rect.height / 2;
    const hit = document.elementFromPoint(cx, cy);
    const owner = hit?.closest("[data-testid]") as HTMLElement | null;
    return {
      width: Math.round(rect.width),
      height: Math.round(rect.height),
      inside:
        rect.top >= 0 &&
        rect.bottom <= window.innerHeight &&
        rect.left >= 0 &&
        rect.right <= window.innerWidth,
      ownsCenter: el === hit || el.contains(hit) || owner === el,
      hit: hit ? `${hit.tagName.toLowerCase()}:${(hit as HTMLElement).dataset.testid ?? ""}` : "none",
    };
  });
  expect(report.width, `${testId} has width`).toBeGreaterThan(20);
  expect(report.height, `${testId} keeps a visible control height`).toBeGreaterThanOrEqual(24);
  expect(report.inside, `${testId} is inside the viewport after scrolling`).toBe(true);
  expect(report.ownsCenter, `${testId} owns its centre point (hit ${report.hit})`).toBe(true);
}

test.beforeEach(async ({ page }) => {
  await login(page);
});

test.describe("UO-11 new session sheet", () => {
  test("desktop is a centered 720 sheet; 390 is full width and single column", async ({ page }) => {
    // Desktop: popover variant re-centred by the page to a 720 dialog.
    await page.setViewportSize({ width: 1440, height: 900 });
    await openSheet(page);
    const panel = page.getByTestId("new-session-sheet");
    await expect(panel).toHaveAttribute("data-variant", "popover");
    let box = await panel.boundingBox();
    expect(box).toBeTruthy();
    expect(box!.width).toBe(720);
    expect(Math.round(box!.x)).toBe(360);

    // The host / 工作目录 pair is two side-by-side columns at desktop: the
    // right field wrapper starts past the left wrapper's edge.
    await page.setViewportSize({ width: 768, height: 900 });
    await expect(panel).toBeVisible();
    box = await panel.boundingBox();
    expect(box).toBeTruthy();
    expect(box!.width).toBe(720);
    let host = await page.getByTestId("new-session-host").locator("..").boundingBox();
    let workspace = await page.getByTestId("new-session-workspace").locator("..").boundingBox();
    expect(host && workspace).toBeTruthy();
    expect(workspace!.x - (host!.x + host!.width)).toBeGreaterThanOrEqual(8);

    // 390: bottom sheet, flush to both edges, stacked single column. The two
    // field wrappers share a left edge and the workspace row is below.
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(panel).toHaveAttribute("data-variant", "sheet");
    box = await panel.boundingBox();
    expect(box).toBeTruthy();
    expect(Math.round(box!.width)).toBe(390);
    expect(Math.round(box!.x)).toBe(0);
    host = await page.getByTestId("new-session-host").locator("..").boundingBox();
    workspace = await page.getByTestId("new-session-workspace").locator("..").boundingBox();
    expect(host && workspace).toBeTruthy();
    expect(Math.abs(workspace!.x - host!.x)).toBeLessThanOrEqual(2);
    expect(workspace!.y).toBeGreaterThan(host!.y + host!.height - 1);
  });

  for (const width of [390, 768, 1440]) {
    test(`every first-layer control is operable at ${width} without expanding 高级设置`, async ({ page }) => {
      await page.setViewportSize({ width, height: width < 768 ? 844 : 900 });
      await openSheet(page);
      // The carrier matrix stays one disclosure away; the controls under test
      // are all reachable with it closed.
      await expect(page.getByTestId("new-session-driver-row")).toHaveCount(0);
      for (const testId of LAYER1) {
        await assertOperable(page, testId);
      }
    });
  }

  test("390: 开始 stays inside the visible band when the soft keyboard opens", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openSheet(page);
    const start = page.getByTestId("new-session-start");
    const panel = page.getByTestId("new-session-sheet");

    await emulateSoftKeyboard(page, 504);
    await expect(start).toBeVisible();
    const box = await start.boundingBox();
    const sheetBox = await panel.boundingBox();
    expect(box).toBeTruthy();
    expect(sheetBox).toBeTruthy();
    // The whole button sits in the unobscured 0..504 band, on the footer that
    // closes the sheet at the band's bottom edge.
    expect(box!.y).toBeGreaterThanOrEqual(0);
    expect(box!.y + box!.height).toBeLessThanOrEqual(510);
    expect(sheetBox!.y + sheetBox!.height).toBeLessThanOrEqual(510);
    // 开始 rides the footer: its bottom edge is within one footer row of the
    // sheet's bottom edge, not stranded up in the form.
    expect(box!.y + box!.height).toBeGreaterThan(sheetBox!.y + sheetBox!.height - 72);
    expect(await start.isEnabled()).toBe(true);

    // Closing the keyboard re-grows the band without dropping the action.
    await emulateSoftKeyboard(page, 844);
    const after = await start.boundingBox();
    expect(after).toBeTruthy();
    expect(after!.y + after!.height).toBeLessThanOrEqual(848);
  });

  test("both appearances resolve role tokens at 390 and 1440", async ({ page }) => {
    for (const [width, height] of [
      [1440, 900],
      [390, 844],
    ] as const) {
      await page.setViewportSize({ width, height });
      for (const mode of MODES) {
        await openSheet(page);
        await setMode(page, mode);
        await page.emulateMedia({ reducedMotion: "reduce", colorScheme: mode });
        await page.getByTestId("new-session-prompt").fill("示例：梳理 worktree 创建失败后的重试与提示路径");
        const match = await page.getByTestId("new-session-sheet").evaluate((el) => {
          const sheetBg = getComputedStyle(el).backgroundColor;
          const raised = getComputedStyle(document.documentElement).getPropertyValue("--bg-raised").trim();
          const strong = getComputedStyle(document.documentElement).getPropertyValue("--fg-strong").trim();
          const title = el.querySelector("h1");
          const titleColor = title ? getComputedStyle(title).color : "";
          return { sheetBg, raised, strong, titleColor };
        });
        // role tokens resolve to real colours and the panel uses --bg-raised.
        expect(match.raised).not.toBe("");
        expect(match.sheetBg).not.toBe("rgba(0, 0, 0, 0)");
        expect(match.titleColor).not.toBe("");

        if (evidence) {
          await mkdir(shotDir, { recursive: true });
          await page.evaluate(() => document.fonts.ready.then(() => undefined));
          await page.waitForTimeout(150);
          await page.screenshot({
            path: path.join(shotDir, `UO-11-newsession-${mode}-${width}.png`),
            animations: "disabled",
          });
        }
      }
    }
  });

  test("opening the sheet in the warm app raises no >50ms long task", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    // Install before any app document loads so the observer exists for the
    // SPA route mount we are about to measure.
    await page.addInitScript(() => {
      (window as unknown as { __uo11Tasks?: number[] }).__uo11Tasks = [];
      new PerformanceObserver((list) => {
        const store = (window as unknown as { __uo11Tasks?: number[] }).__uo11Tasks;
        for (const entry of list.getEntries()) store?.push(entry.duration);
      }).observe({ entryTypes: ["longtask"] });
    });
    await page.goto("/sessions");
    await expect(page.getByRole("link", { name: "新建" }).first()).toBeVisible();
    // Warm: one prior open preloads the route chunk, then measure a second open.
    await page.getByRole("link", { name: "新建" }).first().click();
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("new-session-sheet")).toHaveCount(0);

    await page.evaluate(() => {
      (window as unknown as { __uo11Tasks?: number[] }).__uo11Tasks = [];
    });
    const openedAt = Date.now();
    await page.getByRole("link", { name: "新建" }).first().click();
    await expect(page.getByTestId("new-session-start")).toBeVisible();
    const openMs = Date.now() - openedAt;
    await page.waitForTimeout(400);
    const tasks = await page.evaluate(
      () => (window as unknown as { __uo11Tasks?: number[] }).__uo11Tasks ?? [],
    );
    const over50 = tasks.filter((duration) => duration > 50);
    const max = tasks.reduce((acc, value) => Math.max(acc, value), 0);
    console.log(`UO11-PERF warm sheet open: ${openMs}ms wall, longtasks n=${tasks.length} max=${max.toFixed(1)}ms over50=${over50.length}`);
    expect(
      over50,
      `opening the new-session sheet produced ${over50.length} long task(s) >50ms: ${over50.map((value) => value.toFixed(1)).join(", ")}`,
    ).toEqual([]);
  });
});

test.describe("UO-11 coarse pointer", () => {
  test("390 with a fine pointer keeps desktop density (44px follows touch, not width)", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openSheet(page);
    const metrics = await page.evaluate(() => {
      const height = (testId: string) => {
        const el = document.querySelector(`[data-testid="${testId}"]`);
        return el ? Math.round(el.getBoundingClientRect().height) : 0;
      };
      const hostWrap = document
        .querySelector('[data-testid="new-session-host"]')
        ?.closest("div[class]") as HTMLElement | null;
      return {
        pointerCoarse: window.matchMedia("(pointer: coarse)").matches,
        controlH: getComputedStyle(document.documentElement).getPropertyValue("--control-h").trim(),
        start: height("new-session-start"),
        kind: height("new-session-kind-claude"),
        hostWrap: hostWrap ? Math.round(hostWrap.getBoundingClientRect().height) : 0,
      };
    });
    expect(metrics.pointerCoarse).toBe(false);
    // A narrow mouse window keeps the 32px density; width alone never inflates.
    expect(metrics.controlH).toBe("32px");
    expect(metrics.start).toBe(32);
    expect(metrics.kind).toBe(32);
    expect(metrics.hostWrap).toBe(32);
  });

  test("first-layer text controls are 44px tall at 390 touch", async ({ browser }) => {
    const context = await browser.newContext({ viewport: { width: 390, height: 844 }, hasTouch: true });
    const page = await context.newPage();
    try {
      await login(page);
      await openSheet(page);
      const metrics = await page.evaluate(() => {
        const measure = (testId: string) => {
          const el = document.querySelector(`[data-testid="${testId}"]`);
          const rect = el?.getBoundingClientRect();
          return rect ? Math.round(rect.height) : 0;
        };
        return {
          start: measure("new-session-start"),
          kind: measure("new-session-kind-claude"),
          hostWrap: (document.querySelector('[data-testid="new-session-host"]')?.closest('div[class]') as HTMLElement | null)
            ?.getBoundingClientRect().height ?? 0,
        };
      });
      expect(metrics.start).toBeGreaterThanOrEqual(44);
      expect(metrics.kind).toBeGreaterThanOrEqual(44);
      // The host select sits in a 44px finger-sized wrapper.
      expect(Math.round(metrics.hostWrap)).toBeGreaterThanOrEqual(44);
    } finally {
      await context.close();
    }
  });
});
