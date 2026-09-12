import { expect, test } from "@playwright/test";

test.describe("hosts + fleet", () => {
  test("lists three transports and an offline host", async ({ page }) => {
    await page.goto("/hosts");
    await expect(page.getByTestId("hosts-page")).toBeVisible();
    await expect(page.locator('[data-testid=host-row][data-transport=outbound-wss]').first()).toBeVisible();
    await expect(page.locator('[data-testid=host-row][data-transport=ssh-stdio]').first()).toBeVisible();
    await expect(page.locator('[data-testid=host-row][data-transport=local]').first()).toBeVisible();
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

  test("add host probe then bootstrap", async ({ page }) => {
    await page.goto("/hosts");
    await page.getByTestId("hosts-add").click();
    await page.getByTestId("add-host-alias").selectOption("lyre-devbox");
    await page.getByTestId("add-host-probe").click();
    await expect(page.getByTestId("add-host-probe-ok")).toBeVisible();
    await page.getByTestId("add-host-bootstrap").click();
    await expect(page.locator('[data-testid=host-row][data-label=lyre-devbox]').first()).toBeVisible();
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
