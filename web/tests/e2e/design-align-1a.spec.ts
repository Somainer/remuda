import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), "__screenshots__");

async function shot(page: Page, name: string, size: { width: number; height: number }) {
  await page.setViewportSize(size);
  await page.emulateMedia({ reducedMotion: "reduce" });
  await expect(page.locator("[data-compact]")).toHaveAttribute("data-compact", size.width <= 767 ? "1" : "0");
  await mkdir(DIR, { recursive: true });
  const file = path.join(DIR, `${name}-${size.width}.png`);
  await page.screenshot({ path: file, animations: "disabled" });
}

test.describe("design align 1a screenshots", () => {
  test("sessions / new / approvals at 1440 and 390", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "one screenshot set");
    await page.goto("/sessions");
    await expect(page.getByTestId("session-list").first()).toBeVisible();
    await shot(page, "1a-sessions", { width: 1440, height: 900 });
    await shot(page, "1a-sessions", { width: 390, height: 844 });

    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await shot(page, "1d-new-session", { width: 1440, height: 900 });
    await shot(page, "1d-new-session", { width: 390, height: 844 });

    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/approvals");
    await expect(page.getByTestId("approvals-page")).toBeVisible();
    await shot(page, "1e-approvals", { width: 1440, height: 900 });
    await shot(page, "1e-approvals", { width: 390, height: 844 });
  });
});
