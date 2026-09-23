import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";

/**
 * P1-3 acceptance for the settings page (exploration §5, §7.2):
 *
 * - anchored groups: deep link to a group; the back control returns to origin;
 * - every explicit save shows 保存中 → 已保存 / 失败, and a failed field rolls
 *   back to the last valid value;
 * - both themes, 200 % browser zoom and the 390 / 768 / 1440 breakpoints;
 * - 44 px hit targets measured from real bounding boxes.
 *
 * Mock mode (VITE_MOCK=1 from playwright.config.ts): the synthetic in-browser
 * fixture, no Hub and no real model.
 */

const WIDTHS = [390, 768, 1440] as const;

/**
 * Evidence capture (REMUDA_EVIDENCE=1 only; a normal run writes nothing).
 *
 * Frames are clipped to the settings element itself, so no Shell panel
 * inventory can reach the PNG; the data shown is the browser default fixture
 * (device "this-device"), which is synthetic.
 */
const evidenceDir = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence");

async function shot(page: Page, name: string) {
  const target = page.getByTestId("settings-page");
  await page.evaluate(() => document.fonts.ready);
  await expect(target).toContainText("设置");
  await mkdir(evidenceDir, { recursive: true });
  const png = await target.screenshot({ path: path.join(evidenceDir, `${name}.png`), animations: "disabled" });
  expect(png.byteLength, `${name} must stay below 300 KB`).toBeLessThanOrEqual(300_000);
}

async function setTheme(page: Page, theme: "night" | "ledger") {
  await setMode(page, theme);
}

test("evidence: settings groups, both themes, three widths", async ({ browser, browserName }) => {
  test.skip(browserName !== "chromium", "evidence frames are chromium-only");
  test.skip(process.env.REMUDA_EVIDENCE !== "1", "set REMUDA_EVIDENCE=1 to refresh committed evidence PNGs");

  const page = await browser.newPage();
  await page.emulateMedia({ reducedMotion: "reduce" });

  // 1440, night: grouped page with the section rail.
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/settings");
  await expect(page.getByTestId("settings-page")).toBeVisible();
  await setTheme(page, "night");
  await shot(page, "workbench-f-settings-1-groups-1440-night");

  // 1440, ledger: the light theme with the completed immediate-save status.
  await page.getByTestId("settings-appearance-light").click();
  await expect(page.getByTestId("settings-appearance-status")).toHaveAttribute("data-phase", "saved");
  await shot(page, "workbench-f-settings-1-saved-1440-ledger");

  // Failure frame: the theme write is denied; the rejected choice rolls back
  // to night and the persistent 失败 line stays in the group.
  const broken = await browser.newPage();
  await broken.emulateMedia({ reducedMotion: "reduce" });
  await broken.setViewportSize({ width: 1440, height: 900 });
  await broken.addInitScript(() => {
    // Same browser context may remember ledger from the frame above; start
    // the failure scenario from the night default.
    localStorage.clear();
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function (this: Storage, key: string, value: string) {
      if (key === "runtime.theme.v1") throw new DOMException("denied", "QuotaExceededError");
      return original.call(this, key, value);
    };
  });
  await broken.goto("/settings");
  await broken.getByTestId("settings-appearance-light").click();
  await expect(broken.getByTestId("settings-appearance-status")).toHaveAttribute("data-phase", "error");
  await shot(broken, "workbench-f-settings-1-failed-1440-night");
  await broken.close();

  // 768: the tablet rail.
  await setTheme(page, "night");
  await page.setViewportSize({ width: 768, height: 1024 });
  await page.getByTestId("settings-nav-notifications").click();
  await expect(page).toHaveURL(/#notifications$/);
  // Wait for the smooth anchor scroll to settle so the frame shows the group.
  await page.waitForFunction(() => {
    const element = document.getElementById("notifications");
    return element !== null && element.getBoundingClientRect().top <= 120;
  });
  await shot(page, "workbench-f-settings-1-anchor-768-night");

  // 390: the chip strip in both themes.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/settings");
  await expect(page.getByTestId("settings-page")).toBeVisible();
  await setTheme(page, "night");
  await shot(page, "workbench-f-settings-1-nav-390-night");
  await setTheme(page, "ledger");
  await shot(page, "workbench-f-settings-1-nav-390-ledger");
  await page.close();
});

async function denyStorageKey(page: Page, key: string) {
  await page.addInitScript((target) => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function (this: Storage, k: string, value: string) {
      if (k === target) throw new DOMException("denied for test", "QuotaExceededError");
      return original.call(this, k, value);
    };
  }, key);
}

async function expectMinTarget(locator: ReturnType<Page["locator"]>, name: string) {
  await expect(locator, name).toBeVisible();
  const box = await locator.boundingBox();
  expect(box, name).toBeTruthy();
  expect(box!.height, `${name} height`).toBeGreaterThanOrEqual(44);
  expect(box!.width, `${name} width`).toBeGreaterThanOrEqual(44);
}

/**
 * The hit area reaches 44px even where the visible box is smaller: points
 * 21px above and below the centre still land on the control.
 */
async function expectHitArea(page: Page, locator: ReturnType<Page["locator"]>, name: string) {
  await expect(locator, name).toBeVisible();
  const box = await locator.boundingBox();
  expect(box, name).toBeTruthy();
  expect(box!.width, `${name} width`).toBeGreaterThanOrEqual(44);
  const cx = box!.x + box!.width / 2;
  const cy = box!.y + box!.height / 2;
  const handle = await locator.elementHandle();
  const hits = await page.evaluate(
    ([el, x, ys]) => ys.map((y) => (el as Element).contains(document.elementFromPoint(x, y))),
    [handle, cx, [cy - 21, cy + 21]] as const,
  );
  expect(hits, `${name} hit area spans 44px`).toEqual([true, true]);
}

test.describe("settings groups and deep links", () => {
  test("deep link opens the right group and browser back returns to it", async ({ page }) => {
    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    // Arrive from the workbench: real history, so 返回 must come back here.
    await page.goto("/settings");
    await expect(page.getByTestId("settings-page")).toBeVisible();
    await page.getByTestId("settings-back").click();
    await expect(page).toHaveURL(/\/sessions$/);
  });

  test("a direct deep link shows the group, and back falls back to the list", async ({ page }) => {
    await page.goto("/settings#notifications");
    await expect(page.getByTestId("settings-page")).toBeVisible();
    await expect(page.getByTestId("settings-nav-notifications")).toHaveAttribute("aria-current", "true");
    expect(await page.url()).toContain("#notifications");
    // Deep links have no in-app history; the fallback is the sessions list.
    await page.getByTestId("settings-back").click();
    await expect(page).toHaveURL(/\/sessions$/);
  });

  test("group navigation updates the anchor and the current marker", async ({ page }) => {
    await page.goto("/settings");
    for (const [id, label] of [
      ["appearance", "外观与输入"],
      ["notifications", "通知"],
      ["connection", "连接与登录"],
    ] as const) {
      await page.getByTestId(`settings-nav-${id}`).click();
      await expect(page).toHaveURL(new RegExp(`#${id}$`));
      await expect(page.getByTestId(`settings-nav-${id}`)).toHaveAttribute("aria-current", "true");
      expect(await page.getByTestId(`settings-nav-${id}`).textContent()).toContain(label);
    }
  });
});

test.describe("save states", () => {
  test("appearance change runs through 保存中 and 已保存 and persists", async ({ page }) => {
    await page.goto("/settings");
    // Appearance prefs commit immediately on selection.
    await page.getByTestId("settings-appearance-light").click();

    const status = page.getByTestId("settings-appearance-status");
    // The intermediate state has to be painted, not just visited in state.
    await expect(status).toHaveAttribute("data-phase", "saving");
    await expect(status).toContainText("保存中");
    await expect(status).toHaveAttribute("data-phase", "saved");
    await expect(status).toContainText("已保存");

    expect(await page.evaluate(() => localStorage.getItem("runtime.theme.v1"))).toBe("light");
    expect(await page.evaluate(() => document.documentElement.dataset.appearance)).toBe("light");
    // Reload keeps the choice and applies it before interaction.
    await page.reload();
    await expect(page.getByTestId("settings-page")).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.dataset.appearance)).toBe("light");
    await expect(page.getByTestId("settings-appearance-light")).toHaveAttribute("aria-checked", "true");
  });

  test("a failed save shows 失败 and rolls the field back to the last valid value", async ({ page }) => {
    await denyStorageKey(page, "runtime.theme.v1");
    await page.goto("/settings");
    // Fresh context starts at the follow-system default — that is the
    // committed "last valid value" the rejected edit must return to.
    await expect(page.getByTestId("settings-appearance-system")).toHaveAttribute("aria-checked", "true");

    await page.getByTestId("settings-appearance-light").click();
    const status = page.getByTestId("settings-appearance-status");
    await expect(status).toHaveAttribute("data-phase", "error");
    await expect(status).toContainText("失败");
    // The rejected choice is back to system and nothing was persisted.
    await expect(page.getByTestId("settings-appearance-system")).toHaveAttribute("aria-checked", "true");
    await expect(page.getByTestId("settings-appearance-light")).toHaveAttribute("aria-checked", "false");
    expect(await page.evaluate(() => document.documentElement.getAttribute("data-appearance"))).toBeNull();
    // The error is persistent — it does not time out behind a later toast.
    await expect(status).toContainText("失败");
  });

  test("an empty device name is rejected and rolled back without touching storage", async ({ page }) => {
    await page.goto("/settings");
    const name = page.getByTestId("settings-device-name");
    await name.fill("   ");
    await page.getByTestId("settings-identity-save").click();
    const status = page.getByTestId("settings-identity-status");
    await expect(status).toHaveAttribute("data-phase", "error");
    await expect(status).toContainText("设备名不能为空");
    await expect(name).toHaveValue("this-device");
  });
});

test.describe("responsive settings", () => {
  for (const width of WIDTHS) {
    test(`settings fits ${width}px without sideways overflow`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await page.goto("/settings");
      await expect(page.getByTestId("settings-page")).toBeVisible();
      const dims = await page.evaluate(() => ({
        width: window.innerWidth,
        scroll: document.documentElement.scrollWidth,
        client: document.documentElement.clientWidth,
      }));
      expect(dims.scroll).toBeLessThanOrEqual(dims.client);
      // Every anchored group is reachable at every width.
      for (const id of ["appearance", "notifications", "connection", "host-defaults"] as const) {
        await page.getByTestId(`settings-nav-${id}`).click();
        await expect(page.getByTestId(`settings-group-${id}`)).toBeVisible();
      }
    });
  }

  test("200% zoom keeps the page usable and the group target tappable", async ({ page, browserName }) => {
    // The device-metrics emulation used to model browser zoom is CDP-only.
    test.skip(browserName !== "chromium", "zoom emulation requires the Chromium CDP session");
    // Browser zoom at 200 % halves CSS pixels and doubles DPR: the 1440
    // physical-pixel viewport becomes 720 CSS px at dsf 2 (setDeviceMetrics'
    // width is expressed in CSS pixels).
    const client = await page.context().newCDPSession(page);
    await client.send("Emulation.setDeviceMetricsOverride", {
      width: 720,
      height: 450,
      deviceScaleFactor: 2,
      mobile: false,
    });
    await page.goto("/settings");
    await expect(page.getByTestId("settings-page")).toBeVisible();
    expect(await page.evaluate(() => window.innerWidth)).toBe(720);
    await expect(page.getByTestId("settings-nav-notifications")).toBeVisible();

    // A full immediate-save cycle has to complete while zoomed.
    await page.getByTestId("settings-appearance-light").click();
    await expect(page.getByTestId("settings-appearance-status")).toHaveAttribute("data-phase", "saved");
    const dims = await page.evaluate(() => ({
      scroll: document.documentElement.scrollWidth,
      client: document.documentElement.clientWidth,
    }));
    expect(dims.scroll).toBeLessThanOrEqual(dims.client);
    await client.send("Emulation.clearDeviceMetricsOverride");
  });

  for (const theme of ["night", "ledger"] as const) {
    test(`the ${theme} theme renders every group`, async ({ page }) => {
      await page.goto("/settings");
      await setMode(page, theme);
      for (const id of ["appearance", "notifications", "connection", "host-defaults"] as const) {
        await expect(page.getByTestId(`settings-group-${id}`)).toBeVisible();
      }
      // Text stays on-theme: the ground and the ink are distinct in both.
      const colors = await page.evaluate(() => {
        const body = getComputedStyle(document.body);
        return { background: body.backgroundColor, color: body.color };
      });
      expect(colors.background).not.toBe(colors.color);
    });
  }
});

/**
 * Hit areas follow pointer: coarse, not width (visual-system.md §6.2), so
 * these run with touch emulation. Fine pointers keep desktop density at any
 * width; the responsive block above covers their layout.
 */
test.describe("44px touch targets", () => {
  test.use({ hasTouch: true });

  test("settings controls measure at least 44px at phone width", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/settings");
    await expectMinTarget(page.getByTestId("settings-back"), "back");
    for (const id of ["appearance", "notifications", "connection", "host-defaults"] as const) {
      await expectMinTarget(page.getByTestId(`settings-nav-${id}`), `nav-${id}`);
    }
    await expectMinTarget(page.getByTestId("settings-perm-manual"), "perm chip");
    await expectMinTarget(page.getByTestId("settings-effort-high"), "effort chip");
    // Segment items stay 26px visible; ::after carries the 44px hit area.
    await expectHitArea(page, page.getByTestId("settings-appearance-dark"), "appearance segment");
    await expectMinTarget(page.getByTestId("settings-identity-save"), "save");
  });

  test("settings controls keep 44px at 768 and 1440", async ({ page }) => {
    for (const width of [768, 1440] as const) {
      await page.setViewportSize({ width, height: 900 });
      await page.goto("/settings");
      for (const testId of [
        "settings-back",
        "settings-nav-appearance",
        "settings-perm-manual",
        "settings-identity-save",
      ] as const) {
        const box = await page.getByTestId(testId).boundingBox();
        expect(box, `${testId}@${width}`).toBeTruthy();
        expect(box!.height, `${testId}@${width} height`).toBeGreaterThanOrEqual(44);
      }
    }
  });
});
