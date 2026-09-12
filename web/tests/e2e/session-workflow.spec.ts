import { expect, test } from "@playwright/test";

function row(page: import("@playwright/test").Page, text: string) {
  return page.getByTestId("session-row").filter({ hasText: text }).first();
}

test.describe("workflow tree, task track, events drawer", () => {
  test("working session shows workflow tree and task track", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    await expect(page.getByTestId("workflow-tree")).toBeVisible();
    await expect(page.getByTestId("workflow-phase")).toContainText("compile");
    const linked = page.getByTestId("workflow-member").filter({ has: page.locator("a") });
    await expect(linked.first()).toBeVisible();
    await expect(page.getByTestId("workflow-member").filter({ hasText: "sonnet-cold" })).toBeVisible();
    await expect(page.getByTestId("task-track")).toContainText("summarize");
    await expect(page.getByTestId("usage-row").first()).toContainText("usage");
    await expect(page.getByTestId("opaque-row")).toBeVisible();
    await page.getByTestId("opaque-row").locator("summary").click();
    await expect(page.getByTestId("opaque-json")).toContainText("rate_limit_event");
  });

  test("raw events drawer filters by kind", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    await page.getByRole("button", { name: "原始事件" }).click();
    await expect(page).toHaveURL(/\/events$/);
    await expect(page.getByTestId("raw-events")).toBeVisible();
    await page.getByRole("button", { name: "workflow.member" }).click();
    const rows = page.getByTestId("raw-event-row");
    await expect(rows.first()).toHaveAttribute("data-kind", "workflow.member");
    await rows.first().click();
    await expect(page.getByTestId("raw-event-json")).toContainText("workflowId");
  });

  test("workflow member without childInstanceId is not a link", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    const cold = page.getByTestId("workflow-member").filter({ hasText: "sonnet-cold" });
    await expect(cold).toHaveAttribute("data-child", "0");
    await expect(cold.locator("a")).toHaveCount(0);
  });
});
