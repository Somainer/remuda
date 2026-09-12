import { expect, test } from "@playwright/test";

test.describe("device pairing", () => {
  test("logout redirects protected routes to /login", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByTestId("settings-page")).toBeVisible();
    await page.getByTestId("settings-logout").click();
    await expect(page).toHaveURL(/\/login/);
    await expect(page.getByTestId("login-page")).toBeVisible();
    await page.goto("/approvals");
    await expect(page).toHaveURL(/\/login/);
    await expect(page.getByTestId("login-page")).toBeVisible();
  });

  test("bootstrap token logs a device in after logout", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByTestId("settings-page")).toBeVisible();
    await page.getByTestId("settings-logout").click();
    await expect(page.getByTestId("login-page")).toBeVisible();
    await page.getByTestId("login-tab-bootstrap").click();
    await page.getByTestId("login-bootstrap-token").fill("dev-bootstrap");
    await page.getByTestId("login-device-name").fill("desk");
    await page.getByTestId("login-submit").click();
    await expect(page.getByTestId("login-page")).toHaveCount(0);
    await page.goto("/sessions");
    await expect(page.getByTestId("session-list").first()).toBeVisible();
  });

  test("wrong bootstrap token stays on login", async ({ page }) => {
    await page.goto("/settings");
    await page.getByTestId("settings-logout").click();
    await page.getByTestId("login-tab-bootstrap").click();
    await page.getByTestId("login-bootstrap-token").fill("nope");
    await page.getByTestId("login-submit").click();
    await expect(page.getByTestId("login-error")).toBeVisible();
    await expect(page).toHaveURL(/\/login/);
  });

  test("mobile flow: issue pair code, logout, redeem on login", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByTestId("settings-devices")).toBeVisible();
    await page.getByTestId("settings-pair-code").click();
    await expect(page.getByTestId("settings-pair-code-value")).toBeVisible();
    const code = (await page.getByTestId("settings-pair-code-value").innerText()).trim();
    expect(code.length).toBe(8);
    await page.getByTestId("settings-logout").click();
    await expect(page.getByTestId("login-page")).toBeVisible();
    await page.getByTestId("login-tab-pair").click();
    await page.getByTestId("login-pair-code").fill(code);
    await page.getByTestId("login-device-name").fill("phone");
    await page.getByTestId("login-submit").click();
    await expect(page.getByTestId("login-page")).toHaveCount(0);
    await page.goto("/settings");
    await expect(page.getByTestId("settings-device-row").filter({ hasText: "phone" })).toBeVisible();
  });
});
