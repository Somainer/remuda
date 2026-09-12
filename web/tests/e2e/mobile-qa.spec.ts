import { expect, test } from "@playwright/test";

const TTY_LAB = "ins_01993ab0-0000-7000-8000-00000000aa01";

test.describe("mobile visual QA", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test("composer stays above the bottom bar", async ({ page }) => {
    await page.goto("/sessions");
    await page.getByTestId("session-row").filter({ hasText: "看 TaskManager spill" }).first().click();
    await expect(page.getByTestId("composer")).toBeVisible();
    const composer = await page.getByTestId("composer").boundingBox();
    const bar = await page.getByRole("navigation", { name: "手机底栏" }).boundingBox();
    expect(composer).toBeTruthy();
    expect(bar).toBeTruthy();
    expect(composer!.y + composer!.height).toBeLessThanOrEqual(bar!.y + 2);
  });

  test("tty dock stays above the bottom bar", async ({ page }) => {
    await page.goto(`/s/${TTY_LAB}/tty`);
    await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 15_000 });
    const send = page.getByRole("button", { name: "发送" });
    await expect(send).toBeVisible();
    const box = await send.boundingBox();
    const bar = await page.getByRole("navigation", { name: "手机底栏" }).boundingBox();
    expect(box).toBeTruthy();
    expect(bar).toBeTruthy();
    expect(box!.y + box!.height).toBeLessThanOrEqual(bar!.y + 2);
    expect(box!.height).toBeGreaterThanOrEqual(44);
  });

  test("bottom bar and more menu tap targets are at least 44px", async ({ page }) => {
    await page.goto("/sessions");
    const bar = page.getByRole("navigation", { name: "手机底栏" });
    await expect(bar).toBeVisible();
    for (const loc of await bar.locator("a, button").all()) {
      const box = await loc.boundingBox();
      expect(box).toBeTruthy();
      expect(box!.height).toBeGreaterThanOrEqual(44);
    }
    await page.getByRole("button", { name: /更多/ }).click();
    const item = page.getByRole("menuitem").first();
    await expect(item).toBeVisible();
    const menuBox = await item.boundingBox();
    expect(menuBox).toBeTruthy();
    expect(menuBox!.height).toBeGreaterThanOrEqual(44);
  });

  test("install banner is 44px and dismissible", async ({ page }) => {
    await page.addInitScript(() => {
      sessionStorage.setItem("remuda.install-banner", "1");
    });
    await page.goto("/sessions");
    const bar = page.getByTestId("install-bar");
    await expect(bar).toBeVisible();
    const install = bar.getByRole("button", { name: "安装" });
    const box = await install.boundingBox();
    expect(box).toBeTruthy();
    expect(box!.height).toBeGreaterThanOrEqual(44);
    await bar.getByRole("button", { name: "稍后" }).click();
    await expect(bar).toHaveCount(0);
  });

  test("settings permission chips are at least 44px", async ({ page }) => {
    await page.goto("/settings");
    const chip = page.getByTestId("settings-perm-manual");
    await expect(chip).toBeVisible();
    const box = await chip.boundingBox();
    expect(box).toBeTruthy();
    expect(box!.height).toBeGreaterThanOrEqual(44);
  });
});
