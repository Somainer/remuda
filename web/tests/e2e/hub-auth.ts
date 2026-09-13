import { expect, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";

export const bootstrapToken = process.env.HUB_E2E_ACCESS_CODE_FILE
  ? readFileSync(process.env.HUB_E2E_ACCESS_CODE_FILE, "utf8").trim()
  : process.env.HUB_E2E_ACCESS_CODE ?? process.env.HUB_E2E_TOKEN ?? "e2e-bootstrap-token";

export async function expectCookieSession(page: Page) {
  const cookie = (await page.context().cookies()).find((item) => item.name === "remuda_device");
  expect(cookie?.httpOnly).toBe(true);
  expect(cookie?.sameSite).toBe("Strict");
  const stored = await page.evaluate(() => ({
    session: localStorage.getItem("runtime.device-session"),
    access: localStorage.getItem("runtime.access-code"),
    cookie: document.cookie,
  }));
  expect(JSON.parse(stored.session!)).not.toHaveProperty("token");
  expect(stored.access).toBeNull();
  expect(stored.cookie).not.toContain("remuda_device=");
}

export async function login(page: Page, name = "e2e-browser") {
  await page.goto("/login");
  await expect(page.getByTestId("login-page")).toBeVisible();
  await page.getByTestId("login-tab-bootstrap").click();
  await page.getByTestId("login-device-name").fill(name);
  await page.getByTestId("login-bootstrap-token").fill(bootstrapToken);
  await page.getByTestId("login-submit").click();
  await expect(page).toHaveURL(/\/sessions/, { timeout: 20_000 });
  await expect(page.getByTestId("session-list")).toBeVisible();
  await expectCookieSession(page);
}

export async function logout(page: Page) {
  // Wait for the server to expire the HttpOnly cookie before starting a new
  // login or navigating away; JavaScript cannot clear this cookie itself.
  const revoked = page.waitForResponse((response) =>
    response.request().method() === "DELETE" && new URL(response.url()).pathname.startsWith("/v1/devices/"));
  await page.getByTestId("settings-logout").click();
  expect((await revoked).ok()).toBe(true);
  await expect(page.getByTestId("login-page")).toBeVisible();
  expect((await page.context().cookies()).some((item) => item.name === "remuda_device")).toBe(false);
}
