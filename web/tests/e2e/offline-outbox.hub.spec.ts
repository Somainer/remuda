import { expect, request as apiRequest, test, type APIRequestContext, type Browser, type Page } from "@playwright/test";
import { readdir, readFile, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
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

/** Lifecycles that no longer hold a host placement slot. */
const TERMINAL_LIFECYCLES = new Set(["exited", "failed"]);

/**
 * Delete every instance this spec created, through a FRESH authenticated
 * browser context: the driving contexts may still be emulated-offline or have
 * Hub routes installed (the lost-response test leaves its command routes in
 * place), and Playwright's standalone `request` fixture carries NO device
 * cookie (auth is the httpOnly remuda_device cookie), so a delete issued from
 * it 401s and — when swallowed — leaks every session onto the shared gate
 * hub until its 8 live-instance cap makes later specs' creates 422.
 *
 * Every DELETE is response-checked (2xx/404 only), and a final GET
 * /v1/instances must show none of the created ids still in a live lifecycle.
 */
async function deleteCreatedInstances(browser: Browser) {
  // Copy, do NOT splice up front: ids leave the shared list only AFTER the
  // response checks and the live-slot verification pass, so a failed cleanup
  // leaves them for the other hook to retry.
  const ids = [...created];
  if (!ids.length) return;
  const cleanup = await browser.newPage();
  try {
    await login(cleanup, "e2e-offline-outbox");
    for (const id of ids) {
      const res = await cleanup.request.delete(`/v1/instances/${id}?force=1`);
      expect([200, 202, 204, 404], `DELETE instance ${id} -> HTTP ${res.status()}`).toContain(
        res.status(),
      );
    }
    // The fake node settles the stop asynchronously: poll until none of our
    // ids still holds a placement slot (absent rows pass — DELETE won).
    await expect
      .poll(
        async () => {
          const res = await cleanup.request.get("/v1/instances");
          expect(res.status(), `GET instances -> HTTP ${res.status()}`).toBe(200);
          const body = (await res.json()) as {
            items?: { instanceId?: string; lifecycle?: string }[];
          };
          return (body.items ?? [])
            .filter((it) => ids.includes(it.instanceId ?? ""))
            .filter((it) => !TERMINAL_LIFECYCLES.has(it.lifecycle ?? ""))
            .map((it) => it.instanceId);
        },
        { timeout: 30_000 },
      )
      .toEqual([]);
    created.splice(0, created.length, ...created.filter((id) => !ids.includes(id)));
  } finally {
    await cleanup.close();
  }
}

// Clean up after EACH test so the three instances never pile up within the
// run, and again in afterAll as a safety net when an afterEach could not run
// its own cleanup (it only ever sees ids left behind).
test.afterEach(async ({ browser }) => {
  await deleteCreatedInstances(browser);
});

test.afterAll(async ({ browser }) => {
  await deleteCreatedInstances(browser);
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

/**
 * c-hubfakeack round 2: DETERMINISTIC proof that a Hub->Node RPC buffered
 * behind an outstanding journal.append ack is never swallowed.
 *
 * The `__ackbarrier__:hold` command makes the fake Node park its append-ack
 * wait until a FURTHER Hub->Node RPC is actually on the socket; it records
 * that RPC in a marker file ("<count> <method>"). The mounted session page's
 * ~2 s interaction.list poll supplies that RPC regardless of whether the Hub
 * pipelines command forwards (it serialises a second instance.send behind the
 * first turn, so the poll — the exact frame the original discard swallowed —
 * is the reliable trigger). The held command replies `accepted:true`, so the
 * Hub clears it from the RPC REPLY itself (state accepted / resolution clear),
 * not via later journal reconcile. A normal follow-up send then proves the
 * queued RPC was drained and the Node stayed healthy.
 *
 * Fails on the pre-fix fake: no barrier arm (it replies the non-accepted
 * `{ok:true}`, so the synchronous row is not accepted), no marker, and its
 * 2 s `ws.next()` discard swallows the intervening poll RPC.
 */
test("an RPC buffered behind an append ack is queued, answered accepted and journaled (ack barrier)", async ({
  page,
}) => {
  const instanceId = await createSession(page, "ack barrier seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  const api = await hubApi(page);

  // Marker files are scoped by the effective HUB listen port (the fake derives
  // it from HUB_E2E_LISTEN, NOT the Vite origin port), like playwright.hub.
  const effectiveHubUrl =
    process.env.VITE_HUB_URL ?? `http://${process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880"}`;
  const hubPort = new URL(effectiveHubUrl).port;
  const markerPrefix = `remuda-e2e-ackbarrier-${hubPort}-`;
  const sweepMarkers = async () => {
    const entries = await readdir(os.tmpdir()).catch(() => [] as string[]);
    await Promise.all(
      entries
        .filter((e) => e.startsWith(markerPrefix))
        .map((e) => rm(path.join(os.tmpdir(), e), { force: true }).catch(() => undefined)),
    );
  };
  await sweepMarkers();

  const post = (prompt: string) =>
    api
      .post(`/v1/instances/${instanceId}/commands`, {
        data: { operation: "instance.send", payload: { prompt } },
      })
      .then(async (res) => {
        const body = (await res.json().catch(() => ({}))) as {
          command?: { commandId?: string; state?: string; resolution?: string };
        };
        return {
          status: res.status(),
          commandId: body.command?.commandId ?? null,
          state: body.command?.state ?? null,
          resolution: body.command?.resolution ?? null,
        };
      })
      .catch((err: unknown) => ({
        status: -1,
        commandId: null,
        state: null,
        resolution: null,
        error: String(err),
      }));

  // The held send parks its ack until another Hub RPC is buffered, then is
  // answered accepted straight from the RPC reply.
  const hold = await post("__ackbarrier__:hold");

  // The fake wrote a marker only after a further Hub->Node RPC was actually
  // buffered behind the hold's outstanding ack. Content is "<count> <method>"
  // (e.g. "1 interaction.list"). The old fake never writes it.
  let markerContent = "";
  await expect
    .poll(
      async () => {
        const entries = await readdir(os.tmpdir()).catch(() => [] as string[]);
        const marker = entries.find((e) => e.startsWith(markerPrefix));
        if (!marker) return "";
        markerContent = await readFile(path.join(os.tmpdir(), marker), "utf8").catch(() => "");
        return markerContent;
      },
      { timeout: 20_000 },
    )
    .not.toBe("");
  const [, interveningCount, interveningMethod] =
    /^(\d+)\s+(\S+)$/.exec(markerContent.trim()) ?? [];
  expect(Number(interveningCount), `intervening RPC count in "${markerContent}"`).toBeGreaterThanOrEqual(1);
  expect(interveningMethod, `RPC method in "${markerContent}"`).toContain(".");

  // The held command was cleared by the RPC REPLY itself, synchronously — a
  // swallowed-but-appended RPC could only converge this row via the journal.
  expect(hold.status, `unexpected status ${JSON.stringify(hold)}`).toBe(200);
  expect(hold.commandId, `missing commandId ${JSON.stringify(hold)}`).toBeTruthy();
  expect(hold.state, `row not accepted from the RPC reply: ${JSON.stringify(hold)}`).toBe("accepted");
  expect(hold.resolution).toBe("clear");
  await expect
    .poll(() => hubJournalMessageCount(api, instanceId, hold.commandId!), { timeout: 30_000 })
    .toBe(1);

  // After the barrier drained, an ordinary send runs and converges normally —
  // the generic fake arm answers `{ok:true}`, so the hub returns this row as
  // queued/unknown synchronously and settles it from the journal (exactly like
  // every normal composer send). Its delivery proves the queued intervening
  // RPC did not wedge the single-writer node.
  const after = await post("ack barrier follow up");
  expect(after.status).toBe(200);
  expect(after.commandId, `missing follow-up commandId ${JSON.stringify(after)}`).toBeTruthy();
  await expect
    .poll(() => hubJournalMessageCount(api, instanceId, after.commandId!), { timeout: 30_000 })
    .toBe(1);
  await expectDelivered(page, after.commandId);

  // One ledger row per id.
  const rows = await hubCommands(api, instanceId);
  for (const cid of [hold.commandId, after.commandId]) {
    expect(rows.filter((c) => c.operation === "instance.send" && c.id === cid)).toHaveLength(1);
  }
  await sweepMarkers();
});
