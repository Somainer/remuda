import { expect, request as apiRequest, test, type APIRequestContext, type Browser, type Page } from "@playwright/test";
import { rm, readdir, writeFile } from "node:fs/promises";
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

/**
 * Test-controlled journal release (c-reconnfu rounds 4–6): the fake
 * node's `__gate_journal__` fixture accepts the POST immediately but
 * parks its mirrored journal observation OFF the node RPC loop until
 * the spec creates the release file — replacing the autonomous 8 s
 * sleep of `__hold_journal__` with a deterministic handshake.
 *
 * Handshake (files in os.tmpdir(), scoped by hub port + commandId
 * so parallel hubs never cross):
 *   ...-release-<port>-<cid>  spec writes → node appends, then writes
 *   ...-cancel-<port>-<cid>   spec writes → node ACKs without appending
 *   ...-ack-<port>-<cid>      node writes once the parked append is
 *                              journaled ("journaled") or the gate is
 *                              cancelled ("cancelled").
 * Registration is kept until the node ACKs, so a failure immediately
 * after release still has a provable teardown — no fixed sleeps.
 *
 * The port is derived from the SAME effective Hub URL
 * playwright.hub.config.ts resolves (VITE_HUB_URL wins, then
 * HUB_E2E_LISTEN, then the 58880 default).
 */
const effectiveHubUrl =
  process.env.VITE_HUB_URL ?? `http://${process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880"}`;
const hubPort = new URL(effectiveHubUrl).port;
const journalReleasePrefix = `remuda-e2e-journal-release-${hubPort}-`;
const journalCancelPrefix = `remuda-e2e-journal-cancel-${hubPort}-`;
const journalAckPrefix = `remuda-e2e-journal-ack-${hubPort}-`;

/** Stays registered until the node writes the ACK file. */
const pendingJournalGates = new Set<string>();
function journalReleasePath(commandId: string): string {
  return path.join(os.tmpdir(), `${journalReleasePrefix}${commandId}`);
}
function journalCancelPath(commandId: string): string {
  return path.join(os.tmpdir(), `${journalCancelPrefix}${commandId}`);
}
function journalAckPath(commandId: string): string {
  return path.join(os.tmpdir(), `${journalAckPrefix}${commandId}`);
}
/** Register the gate the moment its commandId is known. */
function registerJournalGate(commandId: string): void {
  pendingJournalGates.add(commandId);
}
async function journalGateFileExists(file: string): Promise<boolean> {
  // access/stat adds an import; readdir is already used by the sweep.
  const entries = await readdir(os.tmpdir()).catch(() => [] as string[]);
  return entries.includes(path.basename(file));
}
/**
 * Bounded wait for the node's ACK. The node's poller ticks every
 * 100 ms and the append round-trip is local, so this is generous
 * without resorting to a blind sleep.
 */
async function awaitJournalAck(commandId: string, timeoutMs = 20_000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await journalGateFileExists(journalAckPath(commandId))) return true;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  return journalGateFileExists(journalAckPath(commandId));
}
/** Spec-driven happy-path release; registration is kept until ACK. */
async function releaseJournal(commandId: string): Promise<void> {
  await writeFile(journalReleasePath(commandId), "release\n");
}
/**
 * Release EVERY gate still registered when the test body ends —
 * including a body that failed before calling releaseJournal(), or one
 * that released and failed before the node ACKed. Each gate is
 * released (if not already), then awaited with a bounded poll on the
 * node's ACK file: the parked append must land BEFORE instances are
 * * deleted. On timeout the gate is cancelled and teardown fails
 * loudly (a swallowed stall would leak a 60 s poller into later
 * specs). All gate files are swept afterwards.
 */
async function teardownJournalGates(): Promise<void> {
  const stuck: string[] = [];
  for (const commandId of pendingJournalGates) {
    let acked = await journalGateFileExists(journalAckPath(commandId));
    if (!acked) {
      // Release unless the node already observed a release (happy path
      // that crashed before ACK); either way, then wait for ACK.
      await writeFile(journalReleasePath(commandId), "teardown-release\n").catch(() => undefined);
      acked = await awaitJournalAck(commandId);
      if (!acked) {
        // Tell the poller to ACK without appending so it cannot
        // linger; still report the stall as a test failure.
        await writeFile(journalCancelPath(commandId), "teardown-cancel\n").catch(() => undefined);
        await awaitJournalAck(commandId, 2_000);
        stuck.push(commandId);
      }
    }
  }
  pendingJournalGates.clear();
  await sweepJournalGateFiles();
  if (stuck.length > 0) {
    throw new Error(`journal gate never ACKed by the fake node: ${stuck.join(", ")}`);
  }
}
async function sweepJournalGateFiles(): Promise<void> {
  const entries = await readdir(os.tmpdir()).catch(() => [] as string[]);
  await Promise.all(
    entries
      .filter(
        (entry) =>
          entry.startsWith(journalReleasePrefix) ||
          entry.startsWith(journalCancelPrefix) ||
          entry.startsWith(journalAckPrefix),
      )
      .map((entry) => rm(path.join(os.tmpdir(), entry), { force: true }).catch(() => undefined)),
  );
}

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

/**
 * Read the instance.send commandId from the Hub command record
 * (independent APIRequestContext), polling until it is committed. Use this
 * for ungated sends whose optimistic bubble may already have been
 * journal-replaced before an attribute read (c-reconnfu r6 item 2).
 */
async function pollForCommandId(
  api: APIRequestContext,
  instanceId: string,
  timeoutMs = 15_000,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const id = (await hubCommands(api, instanceId)).find(
      (c) => c.operation === "instance.send",
    )?.id;
    if (id) return id;
    if (Date.now() >= deadline) throw new Error("no instance.send command recorded");
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
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
  // Release gates a failed test body left pending BEFORE deleting
  // instances (the parked node append must not stall close/purge).
  await teardownJournalGates();
  await deleteCreatedInstances(browser);
});

test.afterAll(async ({ browser }) => {
  await sweepJournalGateFiles();
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

  await sendMessage(page, "__hold_journal__:8000");
  const bubble = page.locator('[data-testid="optimistic-bubble"]').first();
  // The fake node acks the first POST but WITHHOLDS its mirrored journal user
  // observation for 8 s (__hold_journal__): without the hold the journal
  // confirmation retires the row to done first (the terminal-done guarantee
  // then correctly skips the replay), so the lost-response retry could never
  // be observed. The hold makes the re-POST and its replayed:true answer the
  // deterministic path; the journal replacement is asserted afterwards (the
  // withheld append lands well inside the 30 s poll).
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
  // … and one journal message for the id (executed exactly once). The fake
  // node withholds it behind the __hold_journal__ delay, so allow for that.
  await expect
    .poll(() => hubJournalMessageCount(api, instanceId, commandId!), { timeout: 30_000 })
    .toBe(1);
  await expectDelivered(page, commandId);
});

test("an online send labels the row 等待发送 then 已发送，等待确认/已受理 as it delivers", async ({ page }) => {
  const instanceId = await createSession(page, "label progression seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  const api = await hubApi(page);

  // Hold this instance's single-deliverer Web Lock from inside the page: with
  // no flush possible the queued bubble must sit at its honest online label
  // (等待发送 — NOT 待发送（离线）, the link is live the whole time).
  await page.evaluate((iid) => {
    const w = window as unknown as {
      __releaseLock?: () => void;
    };
    const lock = new Promise<void>((resolve) => {
      w.__releaseLock = resolve;
    });
    void navigator.locks.request(`remuda-outbox-${iid}`, () => lock);
  }, instanceId);

  // Once the flush acquires the lock it reaches the POST; park that so the
  // in-flight label is observable too.
  const commandsPattern = /\/v1\/instances\/[^/]+\/commands$/;
  let releasePost: (() => void) | null = null;
  const postGate = new Promise<void>((resolve) => {
    releasePost = resolve;
  });
  await page.context().route(commandsPattern, async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    await postGate;
    const res = await route.fetch();
    return route.fulfill({ response: res });
  });

  await sendMessage(page, "__gate_journal__");
  const bubble = page.locator('[data-testid="optimistic-bubble"]').first();
  await expect(bubble).toBeVisible();
  const commandId = await bubble.getAttribute("data-command-id");
  expect(commandId).toBeTruthy();
  // Drop any stale gate from a crashed earlier worker, then register
  // THIS commandId the moment it is known so a failure below still
  // releases the gate in afterEach.
  await rm(journalReleasePath(commandId!), { force: true });
  registerJournalGate(commandId!);

  // Queued behind the lock, link live: 等待发送, never the offline wording.
  await expect(bubble).toContainText("等待发送");
  await expect(bubble).not.toContainText("离线");

  // Release the lock: the flush takes it and the parked POST shows the row
  // is in flight with NO answer yet — 已发送，等待确认， never 状态待确认.
  await page.evaluate(() => (window as unknown as { __releaseLock?: () => void }).__releaseLock?.());
  await expect(bubble).toContainText("已发送，等待确认", { timeout: 15_000 });
  await expect(bubble).not.toContainText("已受理");
  await expect(bubble).not.toContainText("状态待确认");

  // Release the POST. The fake node accepts it immediately but withholds
  // the mirrored journal user observation on the TEST-CONTROLLED gate
  // (__gate_journal__, hub_e2e.rs): once the browser has processed the
  // accepted response the STILL-VISIBLE bubble must carry the distinct
  // accepted/delivered label 已受理 — unlike inflight's
  // 已发送，等待确认 — and exactly one Hub command row is committed.
  releasePost?.();
  await expect(bubble).toContainText("已受理", { timeout: 15_000 });
  await expect(bubble).not.toContainText("已发送，等待确认");
  await expect(bubble).toBeVisible();
  await expect
    .poll(
      async () =>
        (await hubCommands(api, instanceId)).filter(
          (c) => c.operation === "instance.send" && c.id === commandId,
        ).length,
      { timeout: 15_000 },
    )
    .toBe(1);

  // The journal is still gated by the test (no autonomous sleep to race):
  // the observation has not joined and the chip stays on screen.
  await expect
    .poll(() => hubJournalMessageCount(api, instanceId, commandId!), { timeout: 2_000 })
    .toBe(0);

  // Release the journal and WAIT FOR THE NODE ACK (append journaled),
  // then assert the observation joined exactly once and the bubble was
  // replaced by the authoritative transcript row.
  await releaseJournal(commandId!);
  expect(await awaitJournalAck(commandId!)).toBe(true);
  pendingJournalGates.delete(commandId!);
  await expect
    .poll(() => hubJournalMessageCount(api, instanceId, commandId!), { timeout: 30_000 })
    .toBe(1);
  await expectDelivered(page, commandId);
});

test("a Hub-accepted send shows its delivered label on the still-visible bubble until the journal join", async ({ page }) => {
  const instanceId = await createSession(page, "label hold seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  const api = await hubApi(page);

  // The fake node accepts the POST immediately but withholds its mirrored
  // journal user observation until the TEST-CONTROLLED gate file appears
  // (__gate_journal__, hub_e2e.rs — no autonomous sleep to race): the
  // accepted/delivered phase is asserted on the STILL-VISIBLE bubble, then
  // the spec releases the journal and asserts the replacement itself.
  await sendMessage(page, "__gate_journal__");
  const bubble = page.locator('[data-testid="optimistic-bubble"]').first();
  await expect(bubble).toBeVisible();
  const commandId = await bubble.getAttribute("data-command-id");
  expect(commandId).toBeTruthy();
  // Drop any stale gate from a crashed earlier worker, then register
  // THIS commandId the moment it is known: an assertion failing below
  // still releases the gate in afterEach.
  await rm(journalReleasePath(commandId!), { force: true });
  registerJournalGate(commandId!);

  // The POST landed with a CLEAR answer and the Hub committed the command
  // (the outbox row settles to "sent"), while the journal confirmation is
  // held back. The still-visible bubble shows the accepted/delivered label
  // 已受理 — DISTINCT from inflight's 已发送，等待确认 — never 状态待确认.
  await expect(bubble).toContainText("已受理", { timeout: 15_000 });
  await expect(bubble).not.toContainText("已发送，等待确认");
  await expect(bubble).not.toContainText("状态待确认");
  await expect
    .poll(
      async () =>
        (await hubCommands(api, instanceId)).filter(
          (c) => c.operation === "instance.send" && c.id === commandId,
        ).length,
      { timeout: 15_000 },
    )
    .toBe(1);
  // The optimistic chip is still on screen: the journal confirmation is gated
  // by this test, so the authoritative (non-bubble) transcript row cannot
  // have replaced it yet (give the node a moment: it appends within 100 ms
  // of the release file appearing, and no release has happened).
  await expect(bubble).toBeVisible();
  await expect
    .poll(() => hubJournalMessageCount(api, instanceId, commandId!), { timeout: 2_000 })
    .toBe(0);

  // Only NOW release the journal, wait for the node ACK (append
  // journaled), then assert the observation joined exactly once and
  // the bubble was replaced by the transcript row.
  await releaseJournal(commandId!);
  expect(await awaitJournalAck(commandId!)).toBe(true);
  pendingJournalGates.delete(commandId!);
  await expect
    .poll(() => hubJournalMessageCount(api, instanceId, commandId!), { timeout: 30_000 })
    .toBe(1);
  await expectDelivered(page, commandId);
});

test("a parked journal gate never blocks the node RPC loop and teardown ACKs a released-but-unacked gate", async ({ page }) => {
  // c-reconnfu round 5/6 regression. The fake node parks the
  // __gate_journal__ append OFF its shared RPC read loop and
  // handshakes with an ACK file once it is journaled, so:
  //   1. while A is parked, a normal send on a DIFFERENT instance B
  //      is still answered and journaled immediately (the parked A must
  //      not stall the single fake-node read loop / later RPCs);
  //   2. a body that releases A but FAILS IMMEDIATELY (before the node
  //      ACK lands) still has a provable teardown: registration is
  //      kept until the node ACKs, teardown awaits it (bounded, no
  //      fixed sleep), and the append lands before instance deletion.
  const instanceA = await createSession(page, "gate parked A");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });

  // A parks its journal; register it and do NOT await anything here.
  await sendMessage(page, "__gate_journal__");
  const bubbleA = page.locator('[data-testid="optimistic-bubble"]').first();
  const commandA = await bubbleA.getAttribute("data-command-id");
  expect(commandA).toBeTruthy();
  await rm(journalReleasePath(commandA!), { force: true });
  await rm(journalAckPath(commandA!), { force: true });
  registerJournalGate(commandA!);
  expect(pendingJournalGates.has(commandA!)).toBe(true);
  await expect(bubbleA).toContainText("已受理", { timeout: 15_000 });

  // B is an independent instance; an ordinary send must flow through
  // the node RPC loop RIGHT NOW despite A being parked. Read B's
  // commandId from the Hub command record (c-reconnfu r6 item 2): the
  // ungated bubble may already have been journal-replaced before we
  // read its attribute.
  const instanceB = await createSession(page, "gate normal B");
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  const apiB = await hubApi(page);
  await sendMessage(page, "unblocked normal B");
  // B is ungated, so its optimistic bubble may already have been
  // journal-replaced: read the commandId from the Hub command
  // record (c-reconnfu r6 item 2), never the bubble attribute.
  const commandB = await pollForCommandId(apiB, instanceB, 15_000);
  // B's node observation joins promptly (well inside one 60 s gate
  // timeout): the parked A did not block B's RPC + journal append.
  await expect
    .poll(() => hubJournalMessageCount(apiB, instanceB, commandB), { timeout: 15_000 })
    .toBe(1);
  await expectDelivered(page, commandB);

  // Simulate a body failure IMMEDIATELY AFTER release: trigger the
  // release but do NOT wait for the node ACK, then run teardown.
  // The registration is still present; teardown must await the node's
  // bounded ACK instead of assuming a fixed sleep.
  expect(pendingJournalGates.has(commandA!)).toBe(true);
  await releaseJournal(commandA!);
  await teardownJournalGates();
  expect(pendingJournalGates.has(commandA!)).toBe(false);
  // The node wrote the ACK and A's parked append landed exactly once.
  expect(await journalGateFileExists(journalAckPath(commandA!))).toBe(false); // swept
  const apiA = await hubApi(page);
  expect(await hubJournalMessageCount(apiA, instanceA, commandA!)).toBe(1);
});

/**
 * Test-only service worker: network-first with an offline cache fallback for
 * every same-origin GET EXCEPT the Hub API (/v1), which must always reach the
 * network so an offline bootstrap fails honestly. Registered explicitly from
 * the test (the app registers its SW only in PROD builds) so a FULL emulated
 * offline navigation — context.setOffline(true) still in effect across the
 * reload — can serve the dev-server shell from the cache.
 */
const OFFLINE_SHELL_SW = `
const CACHE = "e2e-offline-shell-v1";
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname.startsWith("/v1/")) return;
  event.respondWith((async () => {
    const cache = await caches.open(CACHE);
    try {
      const res = await fetch(req);
      if (res && res.ok && res.type === "basic") {
        cache.put(req, res.clone()).catch(() => {});
      }
      return res;
    } catch (err) {
      const hit = await cache.match(req, { ignoreSearch: true });
      if (hit) return hit;
      throw err;
    }
  })());
});
`;

// The full-offline-SW-restore test needs the loopback PNA/LNA exemption (see the
// describe below and docs/design/hub-resilience.md §5.6). Playwright only
// accepts launchOptions at file scope (a describe-level use forces a new
// worker), so this disables the checks for THIS SPEC FILE only — not the hub
// config and not the rest of the suite.
test.use({
  launchOptions: {
    args: [
      "--disable-features=BlockInsecurePrivateNetworkRequests,PrivateNetworkAccessChecks,PrivateNetworkAccessForNavigations,PrivateNetworkAccessForWorkers,PrivateNetworkAccessForWebRTC,BlockInsecureLocalNetworkRequests,LocalNetworkAccessChecks,LocalNetworkAccessChecksForNavigation,LocalNetworkAccessChecksForWebRTC,LocalNetworkAccessChecksForWorkers,LocalNetworkAccessChecksWarningOnly",
    ],
  },
});

test.describe("full offline SW restore (PNA/LNA loopback exemption for this harness case)", () => {
  test("an offline-queued message survives a reload with the browser context STILL offline and sends once after", async ({ page }) => {
    const instanceId = await createSession(page, "full offline reload seed");
    await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
    const api = await hubApi(page);

    // Install the offline-shell worker and let it take control.
    await page.context().route("**/e2e-offline-sw.js", (route) =>
      route.fulfill({ contentType: "application/javascript; charset=utf-8", body: OFFLINE_SHELL_SW }),
    );
    await page.evaluate(async () => {
      await navigator.serviceWorker.register("/e2e-offline-sw.js", { updateViaCache: "none" });
      await navigator.serviceWorker.ready;
      if (!navigator.serviceWorker.controller) {
        await new Promise((resolve) =>
          navigator.serviceWorker.addEventListener("controllerchange", resolve, { once: true }),
        );
      }
    });

    // One more ONLINE navigation so the controlled page primes the shell cache.
    await page.goto(page.url());
    await expect(page.getByTestId("session-page")).toBeVisible({ timeout: 20_000 });
    await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });

    // Go fully offline at the BROWSER CONTEXT level (not a Hub-only route): the
    // next reload happens with emulation still in effect. The cached shell must
    // boot while every Hub call and the follow upgrade genuinely fail.
    await page.context().setOffline(true);
    await expect(page.getByTestId("journal-banner")).toHaveAttribute("data-state", "offline");

    await sendMessage(page, "offline across a full offline reload");
    const queued = page.locator('[data-testid="optimistic-bubble"]');
    await expect(queued).toHaveCount(1);
    const commandId = await queued.getAttribute("data-command-id");
    expect(commandId).toBeTruthy();
    await expect(queued.first()).toContainText("待发送（离线）");
    // The independent API client is outside the browser context: nothing sent.
    expect((await hubCommands(api, instanceId)).filter((c) => c.operation === "instance.send")).toHaveLength(0);

    // Reload WHILE context offline: the service worker serves the document and
    // the whole module shell; the restored app boots from durable state.
    await page.reload({ waitUntil: "domcontentloaded" });
    await expect(page.getByTestId("session-page")).toBeVisible({ timeout: 20_000 });
    const restored = page.locator(`[data-testid="optimistic-bubble"][data-command-id="${commandId}"]`);
    await expect(restored).toBeVisible();
    expect(restored).toContainText("offline across a full offline reload");
    await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
    await expect(restored).toContainText("待发送（离线）");
    // Still nothing at the Hub through the offline reload.
    expect((await hubCommands(api, instanceId)).filter((c) => c.operation === "instance.send")).toHaveLength(0);

    // Lift emulation: with this test's loopback-PNA exemption the restored
    // page can reopen its follow socket; the online event plus a foreground
    // resume kick the machine out of its offline backoff.
    await page.context().setOffline(false);
    await page.evaluate(() => {
      window.dispatchEvent(new Event("online"));
      window.dispatchEvent(new Event("focus"));
    });
    await expect
      .poll(
        async () =>
          (await hubCommands(api, instanceId)).filter(
            (c) => c.operation === "instance.send" && c.id === commandId,
          ).length,
        { timeout: 60_000 },
      )
      .toBe(1);
    await expect.poll(() => hubJournalMessageCount(api, instanceId, commandId!), { timeout: 30_000 }).toBe(1);
    // The link banner clears after the brief 已恢复 notice (1.5 s).
    await expect(page.getByTestId("journal-banner")).toHaveCount(0, { timeout: 20_000 });
    await expectDelivered(page, commandId);
  });
});
