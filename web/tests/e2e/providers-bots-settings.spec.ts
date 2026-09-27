import { expect, test } from "@playwright/test";

test.describe("providers bots settings", () => {
  test("providers show none/gateway/direct without a vendor gateway name", async ({ page }) => {
    await page.goto("/providers");
    await expect(page.getByTestId("providers-page")).toBeVisible();
    await expect(page.locator('[data-testid=provider-row][data-delegation=none]')).toBeVisible();
    await expect(page.locator('[data-testid=provider-row][data-delegation=gateway]').first()).toBeVisible();
    await expect(page.locator('[data-testid=provider-row][data-delegation=direct]')).toBeVisible();
    await expect(page.locator("body")).not.toContainText(/astergate/i);
    // The mock seeds two gateway profiles (default + via-host); the row under
    // test is the default gateway, whose health/secret lines are asserted.
    await page.locator('[data-testid="provider-row"][data-delegation="gateway"][data-default="1"]').click();
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
    await page.getByTestId("provider-model-manual").fill("passthrough/auto");
    await page.getByTestId("provider-model-add").click();
    await expect(page.getByTestId("provider-model-row")).toHaveCount(1);
    await page.getByTestId("provider-save").click();
    await expect(page.getByTestId("provider-form")).toHaveCount(0);
    await expect(page.getByText("dummy-gateway")).toBeVisible();
    await page.getByText("dummy-gateway").click();
    await page.getByTestId("provider-test").click();
    await expect(page.getByTestId("provider-test-result")).toContainText("unreachable");
  });

  test("探测模型 fills a checklist; only ticked models reach the session picker", async ({ page }) => {
    await page.goto("/providers");
    await page.getByTestId("provider-add").click();
    await page.getByTestId("provider-name").fill("probe-gateway");
    // The mock serves a /v1/models catalog for any non-dummy base URL.
    await page.getByTestId("provider-base-url").fill("https://fake-upstream.test/v1");
    await page.getByTestId("provider-token").fill("sk-fake-probe-pppp");
    await expect(page.getByTestId("provider-models-empty")).toBeVisible();

    await page.getByTestId("provider-discover").click();
    await expect(page.getByTestId("provider-model-row")).toHaveCount(3);
    await expect(page.getByTestId("provider-models-count")).toContainText("3/3 已启用");
    const auto = page.locator('[data-testid=provider-model-row][data-model="passthrough/auto"]');
    await expect(auto.getByTestId("provider-model-context")).toHaveText("1m");
    // The token is never rendered back into the page.
    await expect(page.locator("body")).not.toContainText("sk-fake-probe-pppp");

    // Expose only two of the three, and make the second one the default.
    const fast = page.locator('[data-testid=provider-model-row][data-model="passthrough/fast"]');
    await fast.getByTestId("provider-model-enabled").uncheck();
    await expect(page.getByTestId("provider-models-count")).toContainText("2/3 已启用");
    const autoModel = page.locator('[data-testid=provider-model-row][data-model="passthrough/auto_model"]');
    await autoModel.getByTestId("provider-model-default").check();
    await page.getByTestId("provider-save").click();
    await expect(page.getByTestId("provider-form")).toHaveCount(0);

    await page.getByText("probe-gateway").click();
    await expect(page.getByTestId("provider-detail")).toBeVisible();
    await expect(page.getByTestId("provider-model-summary")).toContainText("2/3 已启用");
    await expect(page.getByTestId("provider-default-model")).toContainText("passthrough/auto_model");
    await expect(
      page.locator('[data-testid=provider-model-chip][data-enabled="0"]'),
    ).toContainText("passthrough/fast");

    // Edit mode pre-checks the saved list and marks nothing as new on re-probe.
    await page.getByTestId("provider-edit").click();
    await expect(page.getByTestId("provider-model-row")).toHaveCount(3);
    await expect(fast.getByTestId("provider-model-enabled")).not.toBeChecked();
    await expect(autoModel.getByTestId("provider-model-default")).toBeChecked();
    await page.getByTestId("provider-discover").click();
    await expect(page.getByTestId("provider-models-count")).toContainText("2/3 已启用");
    await expect(page.locator('[data-testid=provider-model-row][data-new="1"]')).toHaveCount(0);
    // A re-probe must not silently re-expose what the operator hid.
    await expect(fast.getByTestId("provider-model-enabled")).not.toBeChecked();
  });

  test("探测模型 reports an unreachable gateway inline without saving", async ({ page }) => {
    await page.goto("/providers");
    await page.getByTestId("provider-add").click();
    await page.getByTestId("provider-name").fill("dead-gateway");
    await page.getByTestId("provider-base-url").fill("http://127.0.0.1:1");
    await page.getByTestId("provider-discover").click();
    await expect(page.getByTestId("provider-discover-error")).toContainText("unreachable");
    await expect(page.getByTestId("provider-models-empty")).toBeVisible();
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
    await expect(page.getByTestId("settings-theme")).toContainText("终端始终为深色底");
    await expect(page.getByTestId("settings-auto-reveal-tty")).not.toBeChecked();
    await expect(page.getByTestId("settings-perm-manual")).toBeVisible();
    await page.getByTestId("settings-push").click();
  });
});
