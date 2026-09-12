import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), "__screenshots__");

function row(page: Page, text: string) {
  return page.getByTestId("session-row").filter({ hasText: text }).first();
}

async function shot(page: Page, name: string, size: { width: number; height: number }) {
  await page.setViewportSize(size);
  await page.emulateMedia({ reducedMotion: "reduce" });
  await expect(page.locator("[data-compact]")).toHaveAttribute("data-compact", size.width <= 767 ? "1" : "0");
  const scroller = page.getByTestId("transcript-scroller");
  if (await scroller.count()) {
    await scroller.evaluate((el) => {
      el.scrollTop = 0;
    });
  }
  await mkdir(DIR, { recursive: true });
  await page.screenshot({ path: path.join(DIR, `${name}-${size.width}.png`), animations: "disabled" });
}

test.describe("design align 1b screenshots", () => {
  test("structured session at 1440 and 390", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "one screenshot set");

    await page.goto("/sessions");
    await expect(page.getByTestId("session-list").first()).toBeVisible();
    await row(page, "看 TaskManager spill").click();
    await expect(page.getByTestId("session-page")).toBeVisible();
    await expect(page.getByTestId("transcript")).toBeVisible();
    const density = page.getByTestId("density-toggle");
    if ((await density.getAttribute("data-mode")) === "compact") await density.click();
    await expect(page.getByTestId("tool-card").first()).toBeVisible();
    await shot(page, "1b-session", { width: 1440, height: 900 });
    await shot(page, "1b-session", { width: 390, height: 844 });

    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/sessions");
    await row(page, "清一下 /tmp/coord-media").click();
    await expect(page.getByTestId("approval-card")).toBeVisible();
    await shot(page, "1b-approval", { width: 1440, height: 900 });
    await shot(page, "1b-approval", { width: 390, height: 844 });

    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/sessions");
    await row(page, "spill 从哪改").click();
    await expect(page.getByTestId("question-form")).toBeVisible();
    await shot(page, "1b-question", { width: 390, height: 844 });
  });
});
