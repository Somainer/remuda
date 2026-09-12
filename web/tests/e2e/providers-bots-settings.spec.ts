import { expect, test } from "@playwright/test";

test.describe("providers bots settings", () => {
  test("providers show none/gateway/direct without a vendor gateway name", async ({ page }) => {
    await page.goto("/providers");
    await expect(page.getByTestId("providers-page")).toBeVisible();
    await expect(page.locator('[data-testid=provider-row][data-delegation=none]')).toBeVisible();
    await expect(page.locator('[data-testid=provider-row][data-delegation=gateway]')).toBeVisible();
    await expect(page.locator('[data-testid=provider-row][data-delegation=direct]')).toBeVisible();
    await expect(page.locator("body")).not.toContainText(/astergate/i);
    await page.locator('[data-testid=provider-row][data-delegation=gateway]').click();
    await expect(page.getByTestId("provider-detail")).toBeVisible();
    await expect(page.getByTestId("provider-health")).toContainText("健康 200");
    await expect(page.getByTestId("provider-secret")).toContainText("••••34ef");
    await expect(page.getByTestId("provider-secret")).toContainText("last4");
  });

  test("create dummy gateway and test reports unreachable", async ({ page }) => {
    await page.goto("/providers");
    await page.getByTestId("provider-add").click();
    await expect(page.getByTestId("provider-form")).toBeVisible();
    await page.getByTestId("provider-name").fill("dummy-gateway");
    await page.getByTestId("provider-base-url").fill("http://127.0.0.1:1");
    await page.getByTestId("provider-token").fill("sk-dummy-token-zzzz");
    await page.getByTestId("provider-models").fill("passthrough/auto");
    await page.getByTestId("provider-save").click();
    await expect(page.getByTestId("provider-form")).toHaveCount(0);
    await expect(page.getByText("dummy-gateway")).toBeVisible();
    await page.getByText("dummy-gateway").click();
    await page.getByTestId("provider-test").click();
    await expect(page.getByTestId("provider-test-result")).toContainText("unreachable");
  });

  test("bots show Feishu binding fields and deliveries", async ({ page }) => {
    await page.goto("/bots");
    await expect(page.getByTestId("bots-page")).toBeVisible();
    await page.locator('[data-testid=bot-row][data-channel=feishu]').click();
    await expect(page.getByTestId("bot-detail")).toBeVisible();
    await expect(page.getByTestId("bot-owners")).toContainText("owner_open_ids");
    await expect(page.getByTestId("bot-allowlist")).toContainText("chat_allowlist");
    await expect(page.getByTestId("bot-session-key")).toContainText("feishu:{chat_id}:{thread_id||root_id||main}");
    await expect(page.getByTestId("bot-defaults")).toContainText("claude-print");
    await expect(page.getByTestId("bot-ttl")).toContainText("12 min");
    await expect(page.getByTestId("bot-group-policy")).toContainText("仅 @bot");
    await expect(page.getByTestId("bot-deliveries")).toBeVisible();
    await expect(page.getByTestId("bot-delivery").first()).toContainText("accepted");
  });

  test("settings device push theme permission and autoRevealTty off", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByTestId("settings-page")).toBeVisible();
    await expect(page.getByTestId("settings-device-name")).toHaveValue("this-device");
    await expect(page.getByTestId("settings-ios-hint")).toContainText("主屏幕");
    await expect(page.getByTestId("settings-theme")).toContainText("Night Corral");
    await expect(page.getByTestId("settings-auto-reveal-tty")).not.toBeChecked();
    await expect(page.getByTestId("settings-perm-manual")).toBeVisible();
    await page.getByTestId("settings-push").click();
  });
});
