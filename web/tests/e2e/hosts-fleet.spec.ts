import { expect, test } from "@playwright/test";

test.describe("hosts + fleet", () => {
  test("lists three transports and an offline host", async ({ page }) => {
    await page.goto("/hosts");
    await expect(page.getByTestId("hosts-page")).toBeVisible();
    await expect(page.locator('[data-testid=host-row][data-transport=outbound-wss]').first()).toBeVisible();
    await expect(page.locator('[data-testid=host-row][data-transport=ssh-stdio]').first()).toBeVisible();
    await expect(page.locator('[data-testid=host-row][data-transport=local]').first()).toBeVisible();
    await page.getByTestId("hosts-show-stale").click();
    await expect(page.locator('[data-testid=host-row][data-online="0"]').first()).toBeVisible();
  });

  test("host detail shows CLI inventory", async ({ page }) => {
    await page.goto("/hosts");
    await page.locator('[data-testid=host-row][data-label=devbox-sg]').click();
    await expect(page.getByTestId("host-detail")).toBeVisible();
    await expect(page.getByTestId("host-cli").first()).toContainText("claude");
    await expect(page.getByTestId("host-cli").first()).toContainText("/home/");
    await expect(page.getByTestId("host-max-instances")).toBeVisible();
  });

  test("add host form asks for an SSH target", async ({ page }) => {
    await page.goto("/hosts");
    await page.getByTestId("hosts-add").click();
    await expect(page.getByTestId("add-host-target")).toBeVisible();
    await page.getByTestId("add-host-target").fill("lyre-devbox");
    await expect(page.getByTestId("add-host-submit")).toBeEnabled();
    await page.getByTestId("add-host-submit").click();
    await expect(page.getByRole("alert")).toContainText(/演示模式|SSH|失败/);
  });

  test("fleet aggregates and broadcasts cancel", async ({ page }) => {
    await page.goto("/fleet");
    await expect(page.getByTestId("fleet-page")).toBeVisible();
    await expect(page.getByTestId("placement-picker")).toBeVisible();
    await expect(page.getByTestId("fleet-card").first()).toBeVisible();
    await page.getByTestId("fleet-cancel").first().click();
    await expect(page.locator('[data-testid=fleet-member][data-status=cancelled]').first()).toBeVisible();
  });
});
