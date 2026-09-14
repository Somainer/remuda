import { expect, test } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

const TTY_LAB = "ins_01993ab0-0000-7000-8000-00000000aa01";
const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence");

test.describe("mobile visual QA", () => {
  // `hasTouch` matters, not just the width: the terminal hands the keyboard to
  // the on-screen dock on a coarse pointer, so a narrow *desktop* window (no
  // touch) deliberately stays in 直连 and has no 发送 button.
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true });

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

  test("tty key bar keys are at least 44px", async ({ page }) => {
    await page.goto(`/s/${TTY_LAB}/tty`);
    await expect(page.getByTestId("tty-keybar")).toBeVisible({ timeout: 15_000 });
    for (const id of ["esc", "tab", "ctrl", "alt", "up", "down", "left", "right", "pgup", "pgdn", "ctrl-c"]) {
      const key = page.getByTestId(`tty-key-${id}`);
      await expect(key).toBeVisible();
      const box = await key.boundingBox();
      expect(box, id).toBeTruthy();
      expect(box!.height, id).toBeGreaterThanOrEqual(44);
      expect(box!.width, id).toBeGreaterThanOrEqual(44);
    }
    await page.screenshot({ path: path.join(evidence, "terminal-1-keybar.png"), animations: "disabled" });
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

  test("space chips are at least 44px", async ({ page }) => {
    await page.goto("/sessions");
    const strip = page.getByTestId("spaces-chips");
    await expect(strip).toBeVisible();
    const chips = page.getByTestId("space-chip");
    expect(await chips.count()).toBeGreaterThan(0);
    for (const chip of await chips.all()) {
      const box = await chip.boundingBox();
      expect(box).toBeTruthy();
      expect(box!.height).toBeGreaterThanOrEqual(44);
    }
  });

  test("action sheet buttons are at least 44px", async ({ page }) => {
    await page.goto("/sessions");
    // On a phone the spaces panel (and its exited-session group) lives in the
    // drawer behind the ☰ control.
    await page.getByTestId("spaces-drawer-open").click();
    const drawer = page.getByTestId("spaces-drawer");
    await expect(drawer).toBeVisible();
    const group = drawer.getByTestId("exited-toggle").first();
    await expect(group).toBeVisible();
    if ((await group.getAttribute("aria-expanded")) === "false") await group.click();
    await expect(group).toHaveAttribute("aria-expanded", "true");
    await drawer.getByTestId("exited-delete").first().click();
    const sheet = page.getByTestId("delete-session-sheet");
    await expect(sheet).toBeVisible();
    for (const button of await sheet.getByRole("button").all()) {
      const box = await button.boundingBox();
      expect(box, await button.textContent()).toBeTruthy();
      expect(box!.height, await button.textContent()).toBeGreaterThanOrEqual(44);
    }
    await page.getByTestId("delete-session-sheet-cancel").click();
    await expect(sheet).toHaveCount(0);
  });
});
