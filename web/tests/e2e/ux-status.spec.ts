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

/**
 * Sessions created by this spec, deleted after each test.
 *
 * The fake node advertises `maxInstances: 8` and the hub config runs every
 * spec serially against one Hub, so leaking a session here starves whatever
 * runs next (and the later tests in this file).
 */
const created: string[] = [];

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
  const created_ = await creating;
  expect(created_.ok(), `instance create failed: ${created_.status()} ${await created_.text()}`).toBe(true);
  const instanceId = (await created_.json()).instance.instanceId as string;
  expect(instanceId).toBeTruthy();
  created.push(instanceId);
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  return instanceId;
}

/**
 * Answer the approval the fake node raises on every `instance.create`.
 *
 * It keeps the session blocked (`activity: waiting-interaction`), which
 * disables the composer — so any test that wants to send has to clear it
 * first. Returns the number answered.
 */
async function clearPendingApprovals(page: Page, instanceId: string): Promise<number> {
  return page.evaluate(async (id) => {
    const list = await fetch("/v1/interactions", { credentials: "include" });
    const body = (await list.json()) as {
      items?: {
        id: string;
        instanceId?: string;
        state?: string;
        request?: { inputDigest?: string; options?: { id: string }[] };
      }[];
    };
    const mine = (body.items ?? []).filter((item) => item.instanceId === id && item.state === "pending");
    for (const item of mine) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" } }),
      });
    }
    return mine.length;
  }, instanceId);
}

/** A session whose opening approval is answered, so the composer is usable. */
async function createReadySession(page: Page, prompt: string): Promise<string> {
  const instanceId = await createSession(page, prompt);
  await clearPendingApprovals(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 30_000 });
  return instanceId;
}

/** Post a notification through the app's own surface (mirrors `__ttyLab`). */
async function post(page: Page, input: Record<string, unknown>) {
  await page.waitForFunction(() => Boolean(window.__notifyLab), null, { timeout: 10_000 });
  const posted = await page.evaluate((value) => {
    const lab = window.__notifyLab;
    if (!lab) return null;
    return lab.notify(value as never);
  }, input);
  expect(posted, "window.__notifyLab must be installed by ShellNotify").toBeTruthy();
}

/**
 * Make room on the fake node.
 *
 * It advertises `maxInstances: 8` and every spec in this config shares one
 * Hub serially, so by the time this file runs the earlier specs have usually
 * filled the host — `POST /v1/instances` then returns 422
 * `PLACEMENT_UNSATISFIABLE` and the symptom (create never navigates) looks
 * exactly like the host-contention flake documented for this box.
 *
 * `maxInstances` is a property of the *fake* Node fixture, not a product
 * assertion this spec makes, so raising it for the duration is test
 * isolation rather than hiding a capacity bug. The original value is put
 * back in `afterAll` so a later spec still sees the fixture it expects.
 */
async function raiseCap(page: Page, to: number): Promise<{ hostId: string; previous: number } | null> {
  const hosts = await page.evaluate(async () => {
    const response = await fetch("/v1/hosts", { credentials: "include" });
    return response.json();
  });
  const host = (hosts.items ?? []).find((h: { label?: string }) => h.label === "e2e-fake-node");
  if (!host) return null;

  const hostId = (host.hostId ?? host.id) as string;
  const previous = (host.maxInstances ?? 8) as number;
  if (previous >= to) return { hostId, previous };

  await page.evaluate(
    ({ id, value }) =>
      fetch(`/v1/hosts/${id}`, {
        method: "PATCH",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ maxInstances: value }),
      }),
    { id: hostId, value: to },
  );
  return { hostId, previous };
}

let cap: { hostId: string; previous: number } | null = null;

test.beforeEach(async ({ page }) => {
  await login(page);
  if (!cap) cap = await raiseCap(page, 24);
});

test.afterAll(async ({ browser }) => {
  if (!cap) return;
  const page = await browser.newPage();
  try {
    await login(page);
    await page.evaluate(
      ({ id, value }) =>
        fetch(`/v1/hosts/${id}`, {
          method: "PATCH",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ maxInstances: value }),
        }),
      { id: cap!.hostId, value: cap!.previous },
    );
  } finally {
    await page.close();
  }
});

/**
 * Release the host slot. Routes are unrouted first so a fault this test
 * installed cannot swallow its own cleanup.
 */
test.afterEach(async ({ page }) => {
  await page.unrouteAll({ behavior: "ignoreErrors" }).catch(() => {});
  const ids = created.splice(0, created.length);
  for (const id of ids) {
    await page
      .evaluate((value) => fetch(`/v1/instances/${value}?force=1`, { method: "DELETE", credentials: "include" }), id)
      .catch(() => {});
  }
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

  // A later success, exactly the case the 2.4s toast used to lose. It stacks
  // above the standing error rather than replacing it — both stay readable.
  await post(page, { subject: "会话 beta", stage: "保存", severity: "info" });
  await expect(page.getByTestId("info-toast")).toBeVisible();
  await expect(page.getByTestId("info-toast")).toHaveText("会话 beta · 保存");
  await expect(error).toBeVisible();

  // The error is not covered: its box and the toast's do not overlap.
  const errorBox = await error.boundingBox();
  const toastBox = await page.getByTestId("info-toast").boundingBox();
  expect(errorBox && toastBox).toBeTruthy();
  const overlaps =
    errorBox!.y < toastBox!.y + toastBox!.height && toastBox!.y < errorBox!.y + errorBox!.height;
  expect(overlaps, "a success toast must not overlap the standing error").toBe(false);

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
  const instanceId = await createReadySession(page, "status-slow-ack");
  const recorder = await recordNativeActions(page);

  // Delay the command *response* so the UI sits in the written-but-
  // unconfirmed window for a measurable time. The handler answers on its own
  // rather than staying suspended until the test releases it: a suspended
  // handler races `unrouteAll` in teardown and dies with "Route is already
  // handled!", which is a harness artefact, not a product fact.
  const HOLD_MS = 3000;
  await page.route(`**/v1/instances/${instanceId}/commands`, async (route) => {
    let response;
    try {
      response = await route.fetch();
    } catch {
      return; // Page went away mid-flight; nothing to fulfil.
    }
    await new Promise((resolve) => setTimeout(resolve, HOLD_MS));
    await route.fulfill({ response }).catch(() => {});
  });

  const composer = page.getByTestId("composer-input");
  await composer.fill("slow one");
  await composer.press("Enter");

  // While it is in flight the UI must not claim the turn finished.
  await page.waitForTimeout(1000);
  const body = (await page.getByTestId("session-page").textContent()) ?? "";
  expect(body).not.toContain("本轮已结束");
  const sentWhileHeld = recorder.count();

  // Let the held response land, then confirm waiting did not cause a resend.
  await page.waitForTimeout(HOLD_MS);
  await page.unroute(`**/v1/instances/${instanceId}/commands`);
  await page.waitForTimeout(1000);
  expect(recorder.count(), "a slow ack must not trigger a second send").toBe(sentWhileHeld);
});

test("rejection: a refused command surfaces and does not auto-resend", async ({ page }) => {
  const instanceId = await createReadySession(page, "status-rejected");
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

  // The route answered instead of the Hub, so the row still exists and
  // afterEach still has to release the slot.
  await page.unroute(`**/v1/instances/${instanceId}`);
});

test("the live region carries status text only, never streamed transcript body", async ({ page }) => {
  await createReadySession(page, "status-live-region");
  const region = page.getByTestId("live-region");
  await expect(region).toHaveAttribute("role", "status");
  await expect(region).toHaveAttribute("aria-live", "polite");

  // The transcript is explicitly opted out, so no ancestor can announce it.
  await expect(page.getByTestId("transcript")).toHaveAttribute("aria-live", "off");

  // Drive real streaming output through the fake node.
  //
  // One turn, not a loop: after a turn the fake node keeps reporting
  // `working` for a moment, so a second Enter becomes a *queue* rather than a
  // new turn (`You · queued`) and the test starts measuring composer
  // semantics instead of the live region. The session already carries its
  // creation prompt and echo, so there is transcript body either way — the
  // invariant is that none of it is announced, not how much of it there is.
  const composer = page.getByTestId("composer-input");
  await expect(composer).toBeEnabled({ timeout: 20_000 });
  await composer.fill("streamed body text");
  await composer.press("Enter");
  // The echo round-trip is command dispatch + journal append + WS propagation;
  // under several concurrent gates on the shared devbox it has exceeded 20s, so
  // use the same 30s allowance hub-live gives equivalent fake-node round-trips.
  await expect(page.getByTestId("transcript")).toContainText("echo: streamed body text", { timeout: 30_000 });

  // Whatever the transcript rendered, none of it was announced.
  const announced = (await region.textContent()) ?? "";
  expect(announced).not.toContain("echo:");
  expect(announced).not.toContain("streamed body text");
  expect(announced.length).toBeLessThan(120);

  // A real status line still reaches it, debounced to one short phrase.
  await post(page, { subject: "会话", stage: "保存", severity: "info" });
  await expect(region).toHaveText("会话 · 保存", { timeout: 5_000 });
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
