import { expect, test } from "@playwright/test";

test.describe("hosts + fleet", () => {
  test("hosts header keeps title and add on one row at 390", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/hosts");
    await expect(page.getByTestId("hosts-page")).toBeVisible();
    const title = page.getByTestId("hosts-head").locator("h1");
    const add = page.getByTestId("hosts-add");
    await expect(title).toHaveText("主机");
    await expect(add).toBeVisible();
    const titleBox = await title.boundingBox();
    const addBox = await add.boundingBox();
    expect(titleBox).toBeTruthy();
    expect(addBox).toBeTruthy();
    expect(Math.abs(titleBox!.y - addBox!.y)).toBeLessThan(10);
    const metrics = await title.evaluate((el) => {
      const style = getComputedStyle(el);
      const rect = el.getBoundingClientRect();
      return { whiteSpace: style.whiteSpace, height: rect.height };
    });
    expect(metrics.whiteSpace).toMatch(/nowrap/);
    expect(metrics.height).toBeLessThan(40);
  });

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
    await expect(page.getByTestId("host-cli").first()).toContainText("/opt/claude/");
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
