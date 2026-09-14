import { expect, test } from "@playwright/test";
import { randomUUID } from "node:crypto";
import { bootstrapToken, expectCookieSession, login, logout, useAccessCode } from "./hub-auth";

test.describe("device pairing", () => {
  test.beforeEach(async ({ page }) => { await login(page); });

  test("logout redirects protected routes to /login", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByTestId("settings-page")).toBeVisible();
    await logout(page);
    await expect(page).toHaveURL(/\/login/);
    await expect(page.getByTestId("login-page")).toBeVisible();
    await page.reload();
    await expect(page.getByTestId("login-page")).toBeVisible();
    await page.goto("/approvals");
    await expect(page).toHaveURL(/\/login/);
    await expect(page.getByTestId("login-page")).toBeVisible();
  });

  test("bootstrap token logs a device in after logout", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByTestId("settings-page")).toBeVisible();
    await logout(page);
    await expect(page.getByTestId("login-page")).toBeVisible();
    await useAccessCode(page);
    await page.getByTestId("login-tab-bootstrap").click();
    await page.getByTestId("login-bootstrap-token").fill(bootstrapToken);
    await page.getByTestId("login-device-name").fill("desk");
    await page.getByTestId("login-submit").click();
    await expect(page.getByTestId("login-page")).toHaveCount(0);
    await page.goto("/sessions");
    await expect(page.getByTestId("session-list").first()).toBeVisible();
    await expectCookieSession(page);
  });

  test("wrong bootstrap token stays on login", async ({ page }) => {
    await page.goto("/settings");
    await logout(page);
    await page.getByTestId("login-tab-bootstrap").click();
    await page.getByTestId("login-bootstrap-token").fill("nope");
    await page.getByTestId("login-submit").click();
    await expect(page.getByTestId("login-error")).toBeVisible();
    await expect(page).toHaveURL(/\/login/);
  });

  test("mobile flow: issue pair code, logout, redeem on login", async ({ page }) => {
    const deviceName = `phone-${randomUUID()}`;
    const deviceRow = page.getByTestId("settings-device-row").filter({ hasText: deviceName });
    await page.goto("/settings");
    await expect(page.getByTestId("settings-devices")).toBeVisible();
    await page.getByTestId("settings-pair-code").click();
    await expect(page.getByTestId("settings-pair-code-value")).toBeVisible();
    const code = (await page.getByTestId("settings-pair-code-value").innerText()).trim();
    expect(code.length).toBe(8);
    await logout(page);
    await expect(page.getByTestId("login-page")).toBeVisible();
    await useAccessCode(page);
    await page.getByTestId("login-tab-pair").click();
    await page.getByTestId("login-pair-code").fill(code);
    await page.getByTestId("login-device-name").fill(deviceName);
    await page.getByTestId("login-submit").click();
    await expect(page.getByTestId("login-page")).toHaveCount(0);
    await page.goto("/settings");
    await expect(deviceRow).toHaveCount(1);
    await expect(deviceRow).toBeVisible();
    await expectCookieSession(page);
    await page.reload();
    await expect(deviceRow).toHaveCount(1);
    await expect(deviceRow).toBeVisible();
    await logout(page);
  });
});

test("API clients still authenticate with a device bearer without cookies", async ({ request }) => {
  const response = await request.post("/v1/login", { data: { bootstrapToken, deviceName: "api-client" } });
  expect(response.ok()).toBe(true);
  const { token } = await response.json();
  expect((await request.get("/v1/devices", { headers: { Cookie: "" } })).status()).toBe(401);
  expect((await request.get("/v1/devices", { headers: { Cookie: "", Authorization: `Bearer ${token}` } })).ok()).toBe(true);
});
