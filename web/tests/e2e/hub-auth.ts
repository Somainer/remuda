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

/** Expand the collapsed access-code form (passkey is the primary login). */
export async function useAccessCode(page: Page) {
  const toggle = page.getByTestId("login-use-code");
  if (await toggle.isVisible().catch(() => false)) await toggle.click();
}

export async function login(page: Page, name = "e2e-browser") {
  await page.goto("/login");
  // The first SPA render after goto can miss the 5 s expect default under gate
  // load; match the generous post-submit waits below.
  await expect(page.getByTestId("login-page")).toBeVisible({ timeout: 20_000 });
  await useAccessCode(page);
  await page.getByTestId("login-tab-bootstrap").click();
  await page.getByTestId("login-device-name").fill(name);
  await page.getByTestId("login-bootstrap-token").fill(bootstrapToken);
  await page.getByTestId("login-submit").click();
  // D-049: a compact viewport lands on the phone home /m (HomeList,
  // m-home); desktop lands on /sessions (SessionList). The shared SessionsPage
  // list renders only under the desktop shell. At 390 px the SPA first lands on
  // /sessions and resolveLanding (web/src/lib/mobileRoute.ts) redirects to /m
  // when the compact media query settles, so wait for either landing list first
  // — waiting on session-list while it is being unmounted is the 20 s timeout —
  // and only after that assert which of the two final routes we landed on.
  await expect(page.getByTestId("home-list").or(page.getByTestId("session-list"))).toBeVisible({
    timeout: 20_000,
  });
  await expect(page).toHaveURL(/\/(sessions|m)(?:[/?]|$)/, { timeout: 20_000 });
  await expectCookieSession(page);
}

export async function logout(page: Page) {
  // Wait for the server to expire the HttpOnly cookie before starting a new
  // login or navigating away; JavaScript cannot clear this cookie itself.
  const revoked = page.waitForResponse((response) =>
    response.request().method() === "DELETE" && new URL(response.url()).pathname.startsWith("/v1/devices/"));
  await page.getByTestId("settings-logout").click();
  expect((await revoked).ok()).toBe(true);
  await expect(page.getByTestId("login-page")).toBeVisible({ timeout: 20_000 });
  expect((await page.context().cookies()).some((item) => item.name === "remuda_device")).toBe(false);
}
