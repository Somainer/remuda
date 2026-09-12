import { expect, test } from "@playwright/test";

test.describe("new session sheet", () => {
  test("mobile 30s path: focus prompt, type, start", async ({ page }) => {
    await page.goto("/sessions/new");
    const prompt = page.getByTestId("new-session-prompt");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await expect(prompt).toBeFocused();
    await prompt.fill("查 bolt TaskManager spill");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByTestId("session-page")).toBeVisible();
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "starting");
    await expect(page.getByTestId("session-page").getByTestId("message")).toContainText("查 bolt TaskManager spill");
  });
});
