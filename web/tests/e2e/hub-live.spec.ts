import { expect, test } from "@playwright/test";

test.describe.configure({ mode: "serial" });

test("device login, hosts, create/send/close, follow, approvals", async ({ page }) => {
  await page.goto("/login");
  await expect(page.getByTestId("login-page")).toBeVisible();
  await page.getByTestId("login-tab-bootstrap").click();
  await page.getByTestId("login-device-name").fill("e2e-browser");
  await page.getByTestId("login-bootstrap-token").fill("e2e-bootstrap-token");
  await page.getByTestId("login-submit").click();
  await expect(page).toHaveURL(/\/sessions/, { timeout: 20_000 });
  await expect(page.getByTestId("session-list")).toBeVisible();

  await page.goto("/hosts");
  await expect(page.getByTestId("hosts-page")).toBeVisible();
  await expect(page.getByTestId("host-row").filter({ hasText: "e2e-fake-node" })).toBeVisible({
    timeout: 20_000,
  });

  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });
  await page.getByTestId("new-session-prompt").fill("hello from web hub");
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const sessionPath = new URL(page.url()).pathname;
  await expect(page.getByTestId("session-page")).toBeVisible();
  await expect(page.getByTestId("message").filter({ hasText: /^You/ })).toContainText("hello from web hub", {
    timeout: 20_000,
  });
  await expect(page.getByTestId("message").filter({ hasText: "echo: hello from web hub" })).toHaveCount(1, {
    timeout: 20_000,
  });

  await page.getByTestId("composer").locator("textarea").fill("second turn");
  await page.getByRole("button", { name: "送出" }).click();
  await expect(page.getByTestId("message").filter({ hasText: "echo: second turn" })).toBeVisible({
    timeout: 20_000,
  });

  await page.goto("/approvals");
  await expect(page.getByTestId("approvals-page")).toBeVisible();
  const approval = page.getByTestId("approval-row").filter({ hasText: "echo e2e" });
  await expect(approval).toBeVisible({ timeout: 20_000 });
  await approval.getByRole("button", { name: "允许一次" }).click();
  await expect(page.getByTestId("approval-row").filter({ hasText: "echo e2e" })).toHaveCount(0, {
    timeout: 20_000,
  });

  await page.goto(sessionPath);
  await expect(page.getByTestId("session-page")).toBeVisible();
  await page.getByRole("button", { name: "Stop" }).click();
  await expect(page.getByTestId("session-page")).toBeVisible();
});
