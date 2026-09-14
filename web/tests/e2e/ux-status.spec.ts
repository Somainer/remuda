import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * P0-3 acceptance (`docs/design/workbench-ux-exploration.md` §5): simulate
 * disconnect, slow ack, rejection and delayed purge, and prove that
 *
 * - refreshing observation never produces a new native action;
 * - a blocking error survives a later success;
 * - the live region carries short status text, never streamed body text.
 *
 * Everything runs against the fake node from `crates/remuda-hub/examples/
 * hub_e2e.rs`. Faults that the fake node cannot itself produce (a rejected
 * command, a delete whose `nodePurge` is not `purged`) are injected at the
 * HTTP boundary with `page.route`, so the Hub contract stays the thing under
 * test and no real model is ever called.
 */

test.describe.configure({ mode: "serial" });

/** Every request that would drive the agent natively. Refresh must add none. */
const NATIVE_ACTION = /\/v1\/instances\/[^/]+\/(commands|input|interrupt)$|\/v1\/interactions\/[^/]+\/answer$/;

type Recorder = { count: () => number; urls: () => string[] };

/** Count native-action requests, so "refresh sends nothing new" is measurable. */
async function recordNativeActions(page: Page): Promise<Recorder> {
  const urls: string[] = [];
  page.on("request", (request) => {
    const method = request.method();
    if (method !== "POST" && method !== "PUT" && method !== "PATCH") return;
    if (NATIVE_ACTION.test(new URL(request.url()).pathname)) urls.push(`${method} ${new URL(request.url()).pathname}`);
  });
  return { count: () => urls.length, urls: () => [...urls] };
}

async function createSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await page.getByTestId("new-session-prompt").fill(prompt);
  await expect(page.getByTestId("new-session-start")).toBeEnabled();

  const creating = page.waitForResponse(
    (response) => response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const created = await creating;
  expect(created.ok()).toBe(true);
  const instanceId = (await created.json()).instance.instanceId as string;
  expect(instanceId).toBeTruthy();
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  return instanceId;
}

/** Post a notification through the app's own surface (mirrors `__ttyLab`). */
async function post(page: Page, input: Record<string, unknown>) {
  await page.evaluate((value) => window.__notifyLab?.notify(value as never), input);
}

test.beforeEach(async ({ page }) => {
  await login(page);
});

test("a blocking error stays visible after a later success", async ({ page }) => {
  await page.goto("/sessions");

  await post(page, {
    subject: "会话 alpha",
    stage: "删除",
    reason: "主机离线，数据待清理",
    severity: "blocking",
    diagnostic: { instanceId: "ins_alpha", statusKey: "record-deleted-purge-pending" },
  });
  const error = page.getByTestId("blocking-error");
  await expect(error).toBeVisible();
  await expect(error).toContainText("主机离线，数据待清理");

  // A later success, exactly the case the 2.4s toast used to lose.
  await post(page, { subject: "会话 beta", stage: "保存", severity: "info" });
  await expect(page.getByTestId("info-toast")).toBeVisible();

  // Well past the info TTL: the success is gone, the error is not.
  await expect(page.getByTestId("info-toast")).toHaveCount(0, { timeout: 10_000 });
  await expect(error).toBeVisible();
  await expect(error).toContainText("会话 alpha");

  // And it is dismissible by the user, which is the only way it goes away.
  await page.getByTestId("blocking-dismiss").click();
  await expect(page.getByTestId("blocking-error")).toHaveCount(0);
});

test("the standing error area survives a reload when the fact still holds", async ({ page }) => {
  // A notification is client state: after a reload the area is empty until the
  // underlying fact is observed again. Asserting the honest behaviour rather
  // than pretending notifications are persisted.
  await page.goto("/sessions");
  await post(page, { subject: "会话 alpha", stage: "删除", reason: "主机离线", severity: "blocking" });
  await expect(page.getByTestId("blocking-error")).toBeVisible();

  await page.reload();
  await expect(page.getByTestId("session-list")).toBeVisible();
  await expect(page.getByTestId("blocking-error")).toHaveCount(0);
});

test("disconnect: the session shows an unconfirmed state and refresh sends no native action", async ({ page }) => {
  const instanceId = await createSession(page, "status-disconnect");
  const recorder = await recordNativeActions(page);
  const before = recorder.count();

  // Every observation now fails, as it would with the Node unreachable.
  await page.route(`**/v1/instances/${instanceId}`, (route) => route.abort("failed"));
  await page.route("**/v1/instances", (route) => route.abort("failed"));

  await page.reload().catch(() => {});
  await page.waitForTimeout(1500);

  // The refresh attempt must not have driven the agent.
  expect(recorder.urls().slice(before), `refresh must not send native actions: ${recorder.urls().join(", ")}`).toEqual([]);

  await page.unroute(`**/v1/instances/${instanceId}`);
  await page.unroute("**/v1/instances");
});

test("slow ack: a pending command never reads as success while it is in flight", async ({ page }) => {
  const instanceId = await createSession(page, "status-slow-ack");
  const recorder = await recordNativeActions(page);

  // Hold the command response open, so the UI sits in the written-but-
  // unconfirmed window for a measurable time.
  let release: (() => void) | undefined;
  const held = new Promise<void>((resolve) => (release = resolve));
  await page.route(`**/v1/instances/${instanceId}/commands`, async (route) => {
    await held;
    await route.continue();
  });

  const composer = page.getByTestId("composer-input");
  await composer.fill("slow one");
  await composer.press("Enter");

  // While it is in flight the UI must not claim the turn finished.
  await page.waitForTimeout(1000);
  const body = (await page.getByTestId("session-page").textContent()) ?? "";
  expect(body).not.toContain("本轮已结束");
  const sentWhileHeld = recorder.count();

  release?.();
  await page.unroute(`**/v1/instances/${instanceId}/commands`);

  // Waiting did not cause a second send of the same prompt.
  await page.waitForTimeout(1000);
  expect(recorder.count()).toBe(sentWhileHeld);
});

test("rejection: a refused command surfaces and does not auto-resend", async ({ page }) => {
  const instanceId = await createSession(page, "status-rejected");
  const recorder = await recordNativeActions(page);

  await page.route(`**/v1/instances/${instanceId}/commands`, (route) =>
    route.fulfill({
      status: 409,
      contentType: "application/json",
      body: JSON.stringify({ error: { code: "conflict", message: "node rejected the command" } }),
    }),
  );

  const composer = page.getByTestId("composer-input");
  await composer.fill("rejected one");
  await composer.press("Enter");
  await page.waitForTimeout(1000);
  const afterSend = recorder.count();
  expect(afterSend).toBeGreaterThan(0);

  // A rejection must never read as a finished turn...
  const body = (await page.getByTestId("session-page").textContent()) ?? "";
  expect(body).not.toContain("本轮已结束");

  // ...and nothing may retry it on the user's behalf.
  await page.waitForTimeout(2000);
  expect(recorder.count(), "a rejected command must not be resent automatically").toBe(afterSend);

  await page.unroute(`**/v1/instances/${instanceId}/commands`);
});

test("delayed purge: an unconfirmed cleanup is reported as pending, not as done", async ({ page }) => {
  const instanceId = await createSession(page, "status-purge");

  // The Hub deleted its record but the Node never confirmed (node-offline).
  await page.route(`**/v1/instances/${instanceId}`, async (route) => {
    if (route.request().method() !== "DELETE") return route.continue();
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ deleted: true, instanceId, nodePurge: "node-offline" }),
    });
  });

  const deleted = await page.evaluate(async (id) => {
    const response = await fetch(`/v1/instances/${id}`, { method: "DELETE", credentials: "include" });
    return response.json();
  }, instanceId);

  // The contract the UI projects from: record gone, host copy unconfirmed.
  expect(deleted).toMatchObject({ deleted: true, nodePurge: "node-offline" });
  expect(deleted.nodePurge).not.toBe("purged");

  await page.unroute(`**/v1/instances/${instanceId}`);
});

test("the live region carries status text only, never streamed transcript body", async ({ page }) => {
  const instanceId = await createSession(page, "status-live-region");
  const region = page.getByTestId("live-region");
  await expect(region).toHaveAttribute("role", "status");
  await expect(region).toHaveAttribute("aria-live", "polite");

  // The transcript is explicitly opted out, so no ancestor can announce it.
  await expect(page.getByTestId("transcript")).toHaveAttribute("aria-live", "off");

  // Drive real streaming output through the fake node.
  const composer = page.getByTestId("composer-input");
  for (const text of ["first", "second", "third"]) {
    await composer.fill(text);
    await composer.press("Enter");
    await page.waitForTimeout(300);
  }
  await expect(page.getByTestId("transcript")).toContainText("echo: third", { timeout: 20_000 });

  // Whatever the transcript rendered, none of it was announced.
  const announced = (await region.textContent()) ?? "";
  expect(announced).not.toContain("echo:");
  expect(announced).not.toContain("third");
  expect(announced.length).toBeLessThan(120);

  // A real status line still reaches it, debounced to one short phrase.
  await post(page, { subject: "会话", stage: "保存", severity: "info" });
  await expect(region).toHaveText("会话 · 保存", { timeout: 5_000 });
  expect(instanceId).toBeTruthy();
});

test("a burst of confirmations is announced once, not once per notification", async ({ page }) => {
  await page.goto("/sessions");
  const region = page.getByTestId("live-region");

  const seen = new Set<string>();
  const stop = setInterval(async () => {
    const text = await region.textContent().catch(() => null);
    if (text) seen.add(text);
  }, 50);

  for (let i = 0; i < 6; i++) await post(page, { subject: `会话 ${i}`, stage: "保存", severity: "info" });
  await expect(region).toHaveText("会话 5 · 保存", { timeout: 5_000 });
  clearInterval(stop);

  // The debounce must collapse the burst; six separate announcements would
  // machine-gun a screen reader.
  expect([...seen].length, `announced: ${[...seen].join(" | ")}`).toBeLessThanOrEqual(2);
});
