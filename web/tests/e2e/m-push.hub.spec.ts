import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-mpush / D-049 ui-spec §4.5: the parts of the m-push work the site
 * itself can assert at 390px.
 *
 * Real push delivery (VAPID + the browser push service) and the real OS
 * badge are NOT covered here — they run on a physical device via the
 * owner checklist in docs/design/evidence/mobile-ui-8.md. This spec only
 * covers:
 *  - the /m/inbox permission banner appears with the default permission
 *    and disappears on explicit dismiss, staying dismissed per device;
 *  - granted notification permission hides the banner outright (the only
 *    asks are the banner tap and settings — nothing is requested here);
 *  - the closed-state click landing: /approvals?focus=<id> is what the
 *    service worker opens, and the compact redirect layer must land it on
 *    /m/inbox?focus=<id> with that row focused (redirect from c-minbox /
 *    m-shell; asserted here, not reimplemented).
 */
test.describe.configure({ mode: "serial" });

// chrome-headless-shell (the headless default build) does not propagate a
// granted notifications permission through to Notification.permission, so
// this file pins the full Chromium build. Browser-launch options must be
// set at file scope.
test.use({
  channel: "chromium",
  viewport: { width: 390, height: 844 },
  hasTouch: true,
  isMobile: true,
});

const created: string[] = [];

async function createSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await page.getByTestId("new-session-kind-claude").click();
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

async function pendingInteractionId(page: Page, instanceId: string): Promise<string> {
  const id = await page.evaluate(async (iid) => {
    const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
    const found = (body.items ?? []).find(
      (item: { instanceId?: string; state?: string; interactionId?: string; id?: string }) =>
        item.instanceId === iid && item.state === "pending",
    );
    return found?.interactionId ?? found?.id ?? null;
  }, instanceId);
  expect(id, "the fake node raises a pending approval for a fresh session").toBeTruthy();
  return id as string;
}

test.describe("390px m-push in-app surface", () => {
  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test("the permission banner appears, dismisses, and stays dismissed per device", async ({
    page,
  }) => {
    await login(page);
    await page.goto("/m/inbox");
    const banner = page.getByTestId("m-inbox-push-banner");
    // Default permission: the banner is the single phone entry point.
    await expect(banner).toBeVisible({ timeout: 15_000 });
    await expect(banner).toHaveAttribute("data-mode", "enable");
    await expect(page.getByTestId("m-inbox-push-enable")).toBeVisible();

    await page.getByTestId("m-inbox-push-dismiss").click();
    await expect(banner).toHaveCount(0);

    // Per-device persistence (runtime.m-inbox-push-banner-dismissed): a
    // fresh mount of the inbox must not bring it back.
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    await expect(page.getByTestId("m-inbox-push-banner")).toHaveCount(0);
  });

  test("the banner never shows once notification permission is granted", async ({ page }) => {
    // Grant at the browser-context level, simulating a device that enabled
    // notifications previously. The page only READS status on mount.
    await page.context().grantPermissions(["notifications"]);
    await login(page);
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    await expect(page.getByTestId("m-inbox-push-banner")).toHaveCount(0);
  });

  test("a closed-state notification click lands /approvals?focus= on the focused inbox row", async ({
    page,
  }) => {
    await login(page);
    const instanceId = await createSession(page, "m push focus synthetic");
    const interactionId = await pendingInteractionId(page, instanceId);

    // sw.src.js notificationclick opens data.url — for an interaction that
    // is /approvals?focus=<id>. In compact the redirect layer carries the
    // query verbatim to /m/inbox; the row renders focused and in view.
    await page.goto(`/approvals?focus=${interactionId}`);
    await expect(page).toHaveURL(`/m/inbox?focus=${interactionId}`);
    const row = page.locator(`[data-interaction-id="${interactionId}"]`);
    await expect(row).toHaveAttribute("data-focus", "true");
    const box = await row.boundingBox();
    expect(box, "focused row rendered").toBeTruthy();
    expect(box!.y).toBeGreaterThanOrEqual(0);
    expect(box!.y + box!.height).toBeLessThanOrEqual(844);
  });
});
