import { expect, test } from "@playwright/test";
import { expectCookieSession, login } from "./hub-auth";

test.describe.configure({ mode: "serial" });

test("device login, hosts, create/send/close, follow, approvals", async ({ page }) => {
  const followUrls: string[] = [];
  page.on("websocket", (socket) => {
    // Vite authenticates HMR with its own token; restrict this check to Hub.
    if (new URL(socket.url()).pathname === "/v1/follow") followUrls.push(socket.url());
  });
  await login(page);

  await page.getByTitle("更多", { exact: true }).click();
  await page.getByRole("menuitem", { name: "主机", exact: true }).click();
  await expect(page.getByTestId("hosts-page")).toBeVisible();
  await expect(page.getByTestId("host-row").filter({ hasText: "e2e-fake-node" })).toBeVisible({
    timeout: 20_000,
  });

  await page.locator('a[href="/sessions/new"]').first().click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", { timeout: 20_000 });
  const host = await page.getByTestId("new-session-host").locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
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
  await expect.poll(() => followUrls.length).toBeGreaterThan(0);
  expect(followUrls.every((url) => !new URL(url).searchParams.has("token"))).toBe(true);
  await page.reload();
  await expectCookieSession(page);
  await expect(page.getByTestId("message").filter({ hasText: "echo: hello from web hub" })).toHaveCount(1);
  await expect(page.getByTestId("composer-bar")).toBeVisible();

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
  await expect(page.getByTestId("composer-bar")).toBeVisible();
  await expect(page.getByTestId("composer-input")).toBeEnabled();
  await page.getByTestId("composer-input").fill("second turn");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("message").filter({ hasText: "echo: second turn" })).toBeVisible({
    timeout: 20_000,
  });

  await page.getByRole("button", { name: "Stop" }).click();
  await expect(page.getByTestId("session-page")).toBeVisible();
  expect(followUrls.every((url) => !new URL(url).searchParams.has("token"))).toBe(true);
});
