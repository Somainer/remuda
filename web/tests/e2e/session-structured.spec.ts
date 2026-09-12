import { expect, test } from "@playwright/test";

function row(page: import("@playwright/test").Page, text: string) {
  return page.getByTestId("session-row").filter({ hasText: text }).first();
}

test.describe("structured session M0-13", () => {
  test("session list pins pending and projects status", async ({ page }) => {
    await page.goto("/sessions");
    await expect(page.getByTestId("session-list").first()).toBeVisible();
    const groups = page.locator("[data-testid^='session-group-']").first();
    await expect(page.getByTestId("session-group-blocked").first()).toBeVisible();
    await expect(groups).toHaveAttribute("data-testid", "session-group-blocked");
    await expect(row(page, "清一下 /tmp/coord-media")).toHaveAttribute("data-status", "blocked");
    await expect(row(page, "看 TaskManager spill")).toHaveAttribute("data-status", "working");
    await expect(row(page, "空闲会话")).toHaveAttribute("data-status", "idle");
    await expect(row(page, "失败会话")).toHaveAttribute("data-status", "exited");
    await expect(row(page, "正在启动")).toHaveAttribute("data-status", "starting");
    await expect(page.getByRole("navigation", { name: /主导航|手机底栏/ })).toBeVisible();
  });

  test("working session shows compact fold, tool cards, usage, opaque", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "看 TaskManager spill").click();
    await expect(page.getByTestId("session-page")).toBeVisible();
    await expect(page.getByTestId("transcript")).toBeVisible();
    await expect(page.getByText("You", { exact: false }).first()).toBeVisible();
    await expect(page.getByTestId("compact-fold")).toContainText("次工具");
    await page.getByTestId("compact-fold").click();
    await expect(page.getByText("Bash", { exact: true }).first()).toBeVisible();
    await expect(page.getByText("已写入").first()).toBeVisible();
    await expect(page.getByTestId("usage-row")).toContainText("usage");
    await expect(page.getByText("未识别事件")).toBeVisible();
    await expect(page.getByTestId("composer")).toBeVisible();
  });

  test("blocked session pins approval on composer", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "清一下 /tmp/coord-media").click();
    await expect(page.getByTestId("approval-card")).toBeVisible();
    await expect(page.getByText("多台设备同时点")).toBeVisible();
    await expect(page.getByTestId("composer")).toBeVisible();
    await expect(page.getByTestId("composer-input")).toBeDisabled();
    await page.getByRole("button", { name: "允许一次" }).click();
    await expect(page.getByTestId("approval-card")).toHaveCount(0);
  });

  test("AskUserQuestion form takes over composer", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "spill 从哪改").click();
    await expect(page.getByTestId("question-form")).toBeVisible();
    await expect(page.getByTestId("composer")).toBeVisible();
    await expect(page.getByTestId("composer-input")).toBeDisabled();
    await page.getByText("src/exec.cc").click();
    await page.getByRole("button", { name: "提交" }).click();
  });

  test("composer send on idle session", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await expect(page.getByTestId("composer")).toBeVisible();
    await page.getByTestId("composer-input").fill("补一条");
    await page.getByRole("button", { name: "送出" }).click();
    await expect(page.getByText("补一条").first()).toBeVisible();
  });
});
