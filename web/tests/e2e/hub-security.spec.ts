import { expect, test } from "@playwright/test";

test("Hub production UI loads with CSP and only same-origin fonts", async ({ page }) => {
  const violations: string[] = [];
  const errors: string[] = [];
  const fonts: string[] = [];
  await page.exposeFunction("recordCspViolation", (directive: string) => violations.push(directive));
  await page.addInitScript(() => {
    document.addEventListener("securitypolicyviolation", (event) => {
      void (window as unknown as { recordCspViolation: (value: string) => Promise<void> })
        .recordCspViolation(`${event.effectiveDirective}: ${event.blockedURI}`);
    });
  });
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => {
    if (request.resourceType() === "font") fonts.push(request.url());
  });
  const response = await page.goto("/login");
  expect(response?.headers()["content-security-policy"]).toContain("frame-ancestors 'none'");
  expect(response?.headers()["x-frame-options"]).toBe("DENY");
  expect(response?.headers()["x-content-type-options"]).toBe("nosniff");
  expect(response?.headers()["referrer-policy"]).toBe("no-referrer");
  await expect(page.getByTestId("login-page")).toBeVisible();
  await page.evaluate(() => document.fonts.ready);
  expect(fonts.length).toBeGreaterThan(0);
  expect(fonts.every((url) => new URL(url).origin === new URL(page.url()).origin)).toBe(true);
  expect(violations).toEqual([]);
  expect(errors).toEqual([]);
});
