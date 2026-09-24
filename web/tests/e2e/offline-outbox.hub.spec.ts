import { expect, request as apiRequest, test, type APIRequestContext, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * D-055 client auto-reconnect + offline outbox, end to end against the fake
 * Hub/Node.
 *
 *  - two messages sent while the Hub is unreachable are durably queued
 *    (待发送（离线）, zero commands at the Hub) and, once reachable again,
 *    delivered with the same commandIds exactly once;
 *  - a queued message survives a full page reload WHILE the Hub is still
 *    unreachable: the restored session renders from the persisted instance
 *    projection, then one delivery happens after reconnect;
 *  - a POST the Hub COMMITTED but whose browser response was lost is retried
 *    with the same commandId; the Hub answers the retry replayed:true and the
 *    command executes exactly once (one command row, one journal message).
 *
 * Hub state is always read through Playwright's INDEPENDENT request fixture
 * (never the browser's fetch, never a swallowed failure): every read requires
 * HTTP 200, so an empty list can never masquerade as "delivered".
 */
test.describe.configure({ mode: "serial" });

const created: string[] = [];

async function clearApprovals(page: Page, instanceId: string) {
  // The fake node raises a launch approval on create; answer it via the API so
  // the turn completes and the composer returns to its idle primary button.
  const pendingCount = () =>
    page.evaluate(async (id) => {
      const res = await fetch("/v1/interactions", { credentials: "include" });
      const body = (await res.json()) as {
        items?: { id: string; instanceId?: string; state?: string }[];
      };
      return (body.items ?? []).filter((i) => i.instanceId === id && i.state === "pending").length;
    }, instanceId);
  const seen = await expect
    .poll(pendingCount, { timeout: 20_000 })
    .toBeGreaterThan(0)
    .then(() => true)
    .catch(() => false);
  if (!seen) return; // prompt scripted no approval
  await expect
    .poll(
      async () =>
        page.evaluate(async (id) => {
          const res = await fetch("/v1/interactions", { credentials: "include" });
          const body = (await res.json()) as {
            items?: {
              id: string;
              instanceId?: string;
              state?: string;
              request?: { inputDigest?: string; options?: { id: string }[] };
            }[];
          };
          const mine = (body.items ?? []).filter((i) => i.instanceId === id && i.state === "pending");
          for (const item of mine) {
            const optionId = item.request?.options?.[0]?.id;
            if (!optionId) continue;
            await fetch(`/v1/interactions/${item.id}/answer`, {
              method: "POST",
              credentials: "include",
              headers: { "content-type": "application/json" },
              body: JSON.stringify({
                answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" },
              }),
            });
          }
          return mine.length;
        }, instanceId),
      { timeout: 20_000 },
    )
    .toBe(0);
}

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
  await clearApprovals(page, id);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 20_000,
  });
  return id;
}

/**
 * An authenticated API client independent of the browser context. Playwright's
 * `request.newContext` is the STATIC APIRequest factory (the per-test `request`
 * fixture is an already-built context and has no newContext). Cookies are
 * copied explicitly (httpOnly cookies are not in document.cookie).
 */
async function hubApi(page: Page): Promise<APIRequestContext> {
  const cookies = await page.context().cookies();
  const origin = new URL(page.url()).origin;
  return apiRequest.newContext({
    baseURL: origin,
    extraHTTPHeaders: {
      Cookie: cookies.map((c) => `${c.name}=${c.value}`).join("; "),
      Origin: origin,
    },
  });
}

/** Commands for one instance via the independent client; a non-200 fails the test. */
async function hubCommands(api: APIRequestContext, instanceId: string) {
  const res = await api.get(`/v1/instances/${instanceId}/commands?limit=100`);
  // A failed Hub read must never be swallowed into an empty (== delivered) list.
  expect(res.status(), `GET commands HTTP ${res.status()}`).toBe(200);
  const body = (await res.json()) as {
    commands?: { id?: string; commandId?: string; state: string; operation: string }[];
  };
  return (body.commands ?? []).map((c) => ({ ...c, id: c.id ?? c.commandId ?? "" }));
}

/** Journal user messages for one commandId via the independent client (200 required). */
async function hubJournalMessageCount(api: APIRequestContext, instanceId: string, commandId: string) {
  const res = await api.get(`/v1/instances/${instanceId}/journal?limit=2000`);
  expect(res.status(), `GET journal HTTP ${res.status()}`).toBe(200);
  const body = (await res.json()) as {
    events?: {
      kind?: string;
      event?: { kind?: string; payload?: { commandId?: string } };
      payload?: { commandId?: string };
    }[];
  };
  return (body.events ?? []).filter((raw) => {
    const e = (raw.event ?? raw) as { kind?: string; payload?: { commandId?: string } };
    return e.kind === "message" && e.payload?.commandId === commandId;
  }).length;
}

async function sendMessage(page: Page, text: string) {
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  await page.getByTestId("composer-input").fill(text);
  const send = page.getByTestId("composer-send");
  const queue = page.getByTestId("composer-queue");
  if (await queue.isVisible().catch(() => false)) await queue.click();
  else await send.click();
}

const V1 = /\/v1\//;

/**
 * Make the Hub fully unreachable to the app. Network emulation kills the
 * ALREADY-OPEN follow socket immediately (a route only intercepts future
 * requests, including WS upgrades); the /v1 route then keeps aborting Hub
 * REST calls and any NEW follow upgrade (the WebSocket handshake is an HTTP
 * request, so an aborted upgrade closes the page's socket). With both in
 * place, network emulation can later be lifted for an offline RELOAD (the dev
 * origin must still serve the document) while the Hub stays unreachable.
 */
async function blockHub(context: import('@playwright/test').BrowserContext) {
  await context.route(V1, (route) => route.abort("failed"));
  await context.setOffline(true);
}

/**
 * Lift network emulation while KEEPING the Hub route active: use this before
 * reloading offline so the Vite document reloads but Hub REST/follow stay
 * down exactly as a Hub-only outage looks to the restored page.
 */
async function keepHubBlockedByRoutes(context: import('@playwright/test').BrowserContext) {
  await context.setOffline(false);
}

async function unblockHub(context: import('@playwright/test').BrowserContext) {
  await context.setOffline(false);
  await context.unroute(V1);
}

test.beforeEach(async ({ page }) => {
  await login(page);
});

test.afterAll(async ({ request }) => {
  for (const id of created) {
    await request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

/**
 * Wait for the optimistic bubble to be replaced by the authoritative journal
 * transcript row carrying the SAME commandId (what "delivered" renders as once
 * the journal joins), and assert no waiting/unconfirmed chip is left behind.
 */
async function expectDelivered(page: Page, commandId: string | null) {
  const authoritative = page.locator(
    `[data-testid="transcript-row"][data-role="user"][data-command-id="${commandId}"]`,
  );
  await expect(authoritative).toHaveCount(1, { timeout: 30_000 });
  const bubble = page.locator(
    `[data-testid="optimistic-bubble"][data-command-id="${commandId}"]`,
  );
  // The optimistic chip for THIS row is gone (the authoritative row carries
  // no waiting/unconfirmed label; other scripted commands on the page may
  // independently show their own status and are not asserted here).
  await expect(bubble).toHaveCount(0);
}

test("offline sends are queued and delivered exactly once after reconnect", async ({ page }) => {
  const instanceId = await createSession(page, "offline outbox seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  const api = await hubApi(page);
  await page.waitForTimeout(500);

  // Block everything Hub-bound (REST and the follow socket).
  await blockHub(page.context());
  const banner = page.getByTestId("journal-banner");
  await expect(banner).toHaveAttribute("data-state", "offline");
  expect(await banner.textContent()).toContain("离线");

  await sendMessage(page, "offline one");
  await sendMessage(page, "offline two");

  const pending = page.locator('[data-testid="optimistic-bubble"]');
  await expect(pending).toHaveCount(2);
  // Row labels while offline.
  await expect(pending.first()).toContainText("待发送（离线）");

  const commandIds = await pending.evaluateAll((nodes) =>
    nodes.map((n) => n.getAttribute("data-command-id")),
  );
  expect(commandIds).toHaveLength(2);
  expect(commandIds.every((id) => id?.startsWith("cmd_"))).toBe(true);

  // The Hub has nothing yet (read independently, require 200).
  const seedSends = (await hubCommands(api, instanceId)).filter(
    (c) => c.operation === "instance.send",
  ).length;
  expect(seedSends).toBe(0);

  await unblockHub(page.context());
  await expect(banner).toHaveCount(0, { timeout: 20_000 });

  // Exactly one Hub command row per id …
  await expect
    .poll(async () => (await hubCommands(api, instanceId)).filter((c) => c.operation === "instance.send").length)
    .toBe(2);
  // … and exactly one journal execution each.
  for (const cid of commandIds) {
    await expect.poll(() => hubJournalMessageCount(api, instanceId, cid!)).toBe(1);
    // The delivered row becomes the authoritative journal message (same id);
    // the optimistic chip is gone and no 状态待确认 is shown.
    await expectDelivered(page, cid);
  }
});

test("an offline-queued message survives a reload while the Hub is still off and sends once after", async ({ page }) => {
  const instanceId = await createSession(page, "offline reload seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  const api = await hubApi(page);

  // The Hub goes fully unreachable (REST and follow socket both down).
  await blockHub(page.context());
  await expect(page.getByTestId("journal-banner")).toHaveAttribute("data-state", "offline");
  await sendMessage(page, "offline across reload");
  const queued = page.locator('[data-testid="optimistic-bubble"]');
  await expect(queued).toHaveCount(1);
  const commandId = await queued.getAttribute("data-command-id");
  expect(commandId).toBeTruthy();

  // Reload WITH the Hub still unreachable: lift network emulation (so the dev
  // document loads) while the routes keep every Hub call and follow upgrade
  // failing — a true offline reload of the app, not a frozen page.
  await keepHubBlockedByRoutes(page.context());
  await page.reload({ waitUntil: "domcontentloaded" });
  // The session renders from the persisted instance projection (no
  // 会话不存在 / cast stub), and the durable row restores and still waits
  // offline; nothing has been POSTed.
  await expect(page.getByTestId("session-page")).toBeVisible({ timeout: 20_000 });
  const restored = page.locator(`[data-testid="optimistic-bubble"][data-command-id="${commandId}"]`);
  // The durable row restores and renders with the composer available (the
  // offline seed never loaded events, but restored bubbles unblock the
  // transcript — no "会话不存在", no stuck "加载 snapshot…").
  await expect(restored).toBeVisible();
  expect(restored).toContainText("offline across reload");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  // It is not a terminal/rejected row while the Hub has never received it.
  // Scope to the bubble's own subtree (the session page can independently
  // show 状态待确认 for the scripted create command).
  await expect(restored).not.toContainText("未送达");
  // And nothing reached the Hub yet (independent read, 200 required).
  expect((await hubCommands(api, instanceId)).filter((c) => c.operation === "instance.send")).toHaveLength(0);

  // Reconnect: the restored row delivers exactly once under the same id.
  await unblockHub(page.context());
  await expect(page.getByTestId("journal-banner")).toHaveCount(0, { timeout: 20_000 });
  await expect
    .poll(() => hubJournalMessageCount(api, instanceId, commandId!), { timeout: 30_000 })
    .toBe(1);
  await expect
    .poll(
      async () =>
        (await hubCommands(api, instanceId)).filter(
          (c) => c.operation === "instance.send" && c.id === commandId,
        ).length,
      { timeout: 30_000 },
    )
    .toBe(1);
  await expectDelivered(page, commandId);
});

test("a committed POST whose browser response is lost retries with replayed:true and runs once", async ({ page }) => {
  const instanceId = await createSession(page, "lost response seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  const api = await hubApi(page);

  const commandsPattern = /\/v1\/instances\/[^/]+\/commands$/;
  const statusPattern = /\/v1\/instances\/[^/]+\/commands\/cmd_/;
  const replayResults: boolean[] = [];
  let firstPostLost = false;
  // Gate the retry so the row's intermediate state is observable instead of
  // the local harness completing the re-POST within milliseconds.
  let releaseRetry: (() => void) | null = null;
  const retryGate = new Promise<void>((resolve) => {
    releaseRetry = resolve;
  });

  await page.context().route(commandsPattern, async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    if (!firstPostLost) {
      firstPostLost = true;
      // Let the Hub COMMIT the first POST for real …
      const server = await route.fetch();
      expect(server.status()).toBe(200);
      // … then lose the browser's response (the wire dropped after commit).
      return route.abort("failed");
    }
    // The same-id retry parks until the test releases it, so the row is held
    // in-flight (已发送，等待确认) — delivered to the Hub, never 状态待确认 —
    // before the replay answer settles it.
    await retryGate;
    const retry = await route.fetch();
    expect(retry.status()).toBe(200);
    const body = (await retry.json()) as { command?: { commandId?: string }; replayed?: boolean };
    replayResults.push(body.replayed === true);
    return route.fulfill({ response: retry });
  });
  // The post-loss GET reconciliation must also fail, so the client keeps the
  // row pending and re-POSTs (rather than settling on the GET verdict).
  await page.context().route(statusPattern, (route) =>
    route.request().method() === "GET" ? route.abort("failed") : route.continue(),
  );

  await sendMessage(page, "lost response message");
  const bubble = page.locator('[data-testid="optimistic-bubble"]').first();
  await expect(bubble).toBeVisible();
  const commandId = await bubble.getAttribute("data-command-id");
  expect(commandId).toBeTruthy();

  // While the parked retry is in flight the row says 已发送，等待确认
  // (it reached the Hub), never 状态待确认 and never a terminal rejection.
  await expect(bubble).toContainText("已发送，等待确认", { timeout: 15_000 });
  await expect(bubble).not.toContainText("状态待确认");

  // Release the retry: the Hub dedupes and answers replayed:true.
  releaseRetry?.();
  await expect.poll(() => replayResults.length).toBeGreaterThan(0);
  expect(replayResults[0]).toBe(true);

  // One command row …
  await expect
    .poll(
      async () =>
        (await hubCommands(api, instanceId)).filter(
          (c) => c.operation === "instance.send" && c.id === commandId,
        ).length,
      { timeout: 30_000 },
    )
    .toBe(1);
  // … and one journal message for the id (executed exactly once).
  await expect.poll(() => hubJournalMessageCount(api, instanceId, commandId!)).toBe(1);
  await expectDelivered(page, commandId);
});
