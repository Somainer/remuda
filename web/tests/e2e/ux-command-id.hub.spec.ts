import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * C2: commandId correlation between the structured composer and the
 * prompt observations a Node appends (hook `UserPromptSubmit` + transcript
 * user record). Everything runs against the in-process fake node; no real
 * model is involved.
 *
 * The bug this batch fixes: a composer prompt rendered twice — Remuda's
 * optimistic bubble and the Node's user node — while a prompt typed into the
 * attached terminal had to stay a single, commandId-less user node.
 */
test.describe.configure({ mode: "serial" });

/** Every request that drives/answers native input (exploration §5 P0-3). */
const NATIVE_ACTION = /\/v1\/instances\/[^/]+\/(commands|input|interrupt)$|\/v1\/interactions\/[^/]+\/answer$/;

type Recorder = { count: () => number; urls: () => string[] };

function recordNativeActions(page: Page): Recorder {
  const urls: string[] = [];
  page.on("request", (request) => {
    if (request.method() === "POST" && NATIVE_ACTION.test(new URL(request.url()).pathname)) urls.push(request.url());
  });
  return { count: () => urls.length, urls: () => [...urls] };
}

/** Sessions created, deleted after each test (the fake node caps at 8). */
const created: string[] = [];

async function raiseCap(page: Page, to: number): Promise<{ hostId: string; previous: number } | null> {
  const hosts = await page.evaluate(async () => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    return (await list.json()) as { items?: { id?: string; label?: string; maxInstances?: number }[] };
  });
  const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
  if (!host?.id) return null;
  const previous = host.maxInstances ?? 8;
  await page.evaluate(
    async ({ id, value }) => {
      await fetch(`/v1/hosts/${id}`, {
        method: "PATCH",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ maxInstances: value }),
      });
    },
    { id: host.id, value: to },
  );
  return { hostId: host.id, previous };
}

test.beforeEach(async ({ page }) => {
  await login(page);
  if (!cap) cap = await raiseCap(page, 24);
});

let cap: { hostId: string; previous: number } | null = null;

test.afterAll(async ({ browser }) => {
  if (!cap) return;
  const page = await browser.newPage();
  try {
    await login(page);
    await page.evaluate(
      async ({ id, value }) => {
        await fetch(`/v1/hosts/${id}`, {
          method: "PATCH",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ maxInstances: value }),
        });
      },
      { id: cap.hostId, value: cap.previous },
    );
  } finally {
    await page.close();
  }
});

test.afterEach(async ({ page }) => {
  await page.unrouteAll({ behavior: "ignoreErrors" }).catch(() => {});
  const ids = created.splice(0);
  for (const id of ids) {
    await page
      .evaluate(
        async (value) =>
          fetch(`/v1/instances/${value}?force=1`, { method: "DELETE", credentials: "include" }),
        id,
      )
      .catch(() => {});
  }
});

async function createSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  const creating = page.waitForResponse(
    (response) => response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").at(-1)!;
  created.push(instanceId);
  const response = await creating;
  expect(response.ok(), `instance create failed: ${response.status()} ${await response.text()}`).toBe(true);
  return instanceId;
}

/** Answer the approval the fake node raises on every create so the composer is usable. */
async function clearPendingApprovals(page: Page, instanceId: string) {
  await page.evaluate(async (id) => {
    const list = await fetch("/v1/interactions?instanceId=" + id, { credentials: "include" });
    const body = (await list.json()) as { items?: { id?: string; request?: { inputDigest?: string; options?: { id?: string }[] } }[] };
    for (const item of body.items ?? []) {
      const optionId = item.request?.options?.[0]?.id;
      await fetch(`/v1/interactions/${item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" } }),
      });
    }
  }, instanceId);
}

async function createReadySession(page: Page, prompt: string): Promise<string> {
  const instanceId = await createSession(page, prompt);
  await clearPendingApprovals(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 30_000 });
  return instanceId;
}

/** User transcript rows whose text contains `text` (assistant echoes excluded). */
function userRows(page: Page, text: string) {
  return page
    .locator('[data-testid="transcript-row"][data-role="user"]')
    .filter({ hasText: text });
}

test("a composer submit renders one user bubble after hook + transcript round-trip", async ({ page }) => {
  // Distinct from the create-time prompt so the two journal user nodes can
  // never be confused by text.
  await createReadySession(page, "cid create prompt for send test");
  const prompt = `cid unique send ${Date.now()}`;

  await page.getByTestId("composer-input").fill(prompt);
  await page.keyboard.press("Enter");

  // The assistant echo proves the round-trip completed; at that point the
  // optimistic bubble must have been upgraded in place, not duplicated.
  await expect(page.getByTestId("transcript")).toContainText(`echo: ${prompt}`, { timeout: 30_000 });
  // Give settleBubbles a beat after the echo batch before counting user rows.
  await expect(userRows(page, prompt)).toHaveCount(1);

  // And the surviving row is the journal node carrying the delivering
  // commandId, not a commandId-less optimistic copy.
  await expect(userRows(page, prompt).first()).toHaveAttribute("data-command-id", /^cmd_/);
  await expect(page.getByTestId("optimistic-bubble")).toHaveCount(0);
});

test("a prompt typed into the attached terminal is one commandId-less user bubble", async ({ page }) => {
  const createPrompt = `cid session for typing ${Date.now()}`;
  const instanceId = await createSession(page, createPrompt);

  await page.goto(`/s/${instanceId}/tty`);
  await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", { timeout: 20_000 });

  await page.locator(".xterm-helper-textarea").first().evaluate((el) => (el as HTMLTextAreaElement).focus());
  const typed = `typed at the tty ${Date.now()}`;
  await page.keyboard.type(typed);
  await page.keyboard.press("Enter");

  // The node echoes the submitted line back to the PTY before journaling it.
  await expect(page.getByTestId("tty-ansi-preview")).toContainText("typed at the tty", { timeout: 20_000 });

  await page.getByTestId("view-switch-structured").click();
  await expect(page.getByTestId("transcript")).toContainText(`typed echo: ${typed}`, { timeout: 20_000 });
  await expect(userRows(page, typed)).toHaveCount(1);
  // No Remuda command delivered it: the node must not have stamped one.
  await expect(userRows(page, typed).first()).not.toHaveAttribute("data-command-id", /.+/);
});

test("a delayed POST shows 等待发送, never a duplicate, then upgrades in place", async ({ page }) => {
  const recorder = recordNativeActions(page);
  await createReadySession(page, "cid create prompt for slow test");
  const setupActions = recorder.count();
  const prompt = `cid slow post ${Date.now()}`;

  // Hold the command response for 1.2 s; the handler answers itself so the
  // teardown unroute cannot race it.
  await page.route("**/v1/instances/*/commands", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 1200));
    await route.fetch();
  });

  await page.getByTestId("composer-input").fill(prompt);
  await page.keyboard.press("Enter");

  // During the hold: exactly one optimistic bubble, queued wording.
  await expect(page.getByTestId("session-status-label")).toContainText("等待发送");
  await expect(userRows(page, prompt)).toHaveCount(1);
  // Exactly one native action in flight after the POST — approval answers
  // during setup are not counted — and the bubble is never resent.
  await expect.poll(() => recorder.count()).toBe(setupActions + 1);

  await expect(page.getByTestId("transcript")).toContainText(`echo: ${prompt}`, { timeout: 30_000 });
  await expect(userRows(page, prompt)).toHaveCount(1);
  // No replay: still exactly one more native action than after setup.
  expect(recorder.count()).toBe(setupActions + 1);
});

test("a 5xx POST projects 状态待确认 and reload creates no native action or duplicate", async ({ page }) => {
  const recorder = recordNativeActions(page);
  await createReadySession(page, "cid create prompt for fail test");
  const setupActions = recorder.count();
  const prompt = `cid failed post ${Date.now()}`;

  await page.route("**/v1/instances/*/commands", async (route) => {
    await route.fulfill({ status: 502, contentType: "application/json", body: JSON.stringify({ error: { code: "BAD_GATEWAY", message: "node unreachable" } }) });
  });

  await page.getByTestId("composer-input").fill(prompt);
  await page.keyboard.press("Enter");

  // Bubble and header speak the unconfirmed vocabulary; nothing retries.
  await expect(page.getByTestId("optimistic-bubble")).toContainText("状态待确认", { timeout: 10_000 });
  await expect(page.getByTestId("session-status-label")).toContainText("状态待确认");
  // Nothing retries the failed send.
  await page.waitForTimeout(1500);
  expect(recorder.count()).toBe(setupActions);

  // Reload: the optimistic bubble is client state and is gone; the failed
  // send is never replayed, and no native action is generated on our behalf.
  await page.unrouteAll({ behavior: "ignoreErrors" }).catch(() => {});
  await page.reload();
  await expect(page.getByTestId("session-page")).toBeVisible({ timeout: 20_000 });
  await page.waitForTimeout(1000);
  // Reload created no native action either.
  expect(recorder.count()).toBe(setupActions);
  await expect(page.getByTestId("optimistic-bubble")).toHaveCount(0);
  // The fake node journaled nothing for the failed send.
  await expect(userRows(page, prompt)).toHaveCount(0);
});
