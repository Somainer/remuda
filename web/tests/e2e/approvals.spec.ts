import { expect, test } from "@playwright/test";

test.describe("approvals center", () => {
  test("lists pending, filters, focus, allow once, and UI states", async ({ page }) => {
    await page.goto("/approvals");
    await expect(page.getByTestId("approvals-page")).toBeVisible();
    await expect(page.getByTestId("approval-row").filter({ hasText: "rm -rf /tmp/coord-media" })).toBeVisible();
    await expect(page.getByTestId("approval-row").filter({ hasText: "AskUserQuestion" })).toBeVisible();
    await expect(page.getByTestId("approval-row").filter({ hasText: "过期" }).first()).toBeVisible();
    await expect(page.getByTestId("approval-row").filter({ hasText: "其它设备" })).toBeVisible();
    await expect(page.getByTestId("approval-row").filter({ hasText: "主机离线" })).toBeVisible();
    await expect(page.getByTestId("approval-row").filter({ hasText: "实施计划" })).toBeVisible();

    await page.getByRole("button", { name: "提问" }).click();
    await expect(page.getByTestId("approval-row").filter({ hasText: "AskUserQuestion" })).toBeVisible();
    await expect(page.getByTestId("approval-row").filter({ hasText: "rm -rf" })).toHaveCount(0);

    await page.getByRole("button", { name: "全部" }).click();
    const bash = page.getByTestId("approval-row").filter({ hasText: "rm -rf /tmp/coord-media" });
    await bash.getByRole("button", { name: "允许一次" }).click();
    await expect(bash).toHaveCount(0);

    await page.getByTestId("approval-row").filter({ hasText: "AskUserQuestion" }).getByRole("link", { name: "去回答" }).click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByTestId("question-form")).toBeVisible();
  });

  test("focus query highlights a row", async ({ page }) => {
    await page.goto("/approvals");
    await expect(page.getByTestId("approvals-page")).toBeVisible();
    const paused = page.getByTestId("approval-row").filter({ hasText: "host offline" });
    await expect(paused).toBeVisible();
    await expect(paused.getByRole("button", { name: "允许一次" })).toBeDisabled();
  });
});
