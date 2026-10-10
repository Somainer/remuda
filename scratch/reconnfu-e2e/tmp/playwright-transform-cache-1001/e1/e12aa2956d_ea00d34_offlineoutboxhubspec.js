// 629ddb22b5e97ef7734ea2f174d88f567277c8f6
import { expect, request as apiRequest, test } from "@playwright/test";
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
test.describe.configure({
  mode: "serial"
});
const created = [];
async function clearApprovals(page, instanceId) {
  // The fake node raises a launch approval on create; answer it via the API so
  // the turn completes and the composer returns to its idle primary button.
  const pendingCount = () => page.evaluate(async id => {
    var _body$items;
    const res = await fetch("/v1/interactions", {
      credentials: "include"
    });
    const body = await res.json();
    return ((_body$items = body.items) !== null && _body$items !== void 0 ? _body$items : []).filter(i => i.instanceId === id && i.state === "pending").length;
  }, instanceId);
  const seen = await expect.poll(pendingCount, {
    timeout: 20000
  }).toBeGreaterThan(0).then(() => true).catch(() => false);
  if (!seen) return; // prompt scripted no approval
  await expect.poll(async () => page.evaluate(async id => {
    var _body$items2;
    const res = await fetch("/v1/interactions", {
      credentials: "include"
    });
    const body = await res.json();
    const mine = ((_body$items2 = body.items) !== null && _body$items2 !== void 0 ? _body$items2 : []).filter(i => i.instanceId === id && i.state === "pending");
    for (const item of mine) {
      var _item$request, _item$request$inputDi, _item$request2;
      const optionId = (_item$request = item.request) === null || _item$request === void 0 || (_item$request = _item$request.options) === null || _item$request === void 0 || (_item$request = _item$request[0]) === null || _item$request === void 0 ? void 0 : _item$request.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: {
          "content-type": "application/json"
        },
        body: JSON.stringify({
          answer: {
            kind: "approval",
            optionId,
            inputDigest: (_item$request$inputDi = (_item$request2 = item.request) === null || _item$request2 === void 0 ? void 0 : _item$request2.inputDigest) !== null && _item$request$inputDi !== void 0 ? _item$request$inputDi : ""
          }
        })
      });
    }
    return mine.length;
  }, instanceId), {
    timeout: 20000
  }).toBe(0);
}
async function createSession(page, prompt) {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", {
    timeout: 20000
  });
  const hostId = await hostPicker.locator("option").filter({
    hasText: "e2e-fake-node"
  }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId);
  await page.getByTestId("new-session-kind-claude").click();
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20000
  });
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, {
    timeout: 20000
  });
  const id = new URL(page.url()).pathname.split("/").pop();
  created.push(id);
  await clearApprovals(page, id);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
    timeout: 20000
  });
  return id;
}

/**
 * An authenticated API client independent of the browser context. Playwright's
 * `request.newContext` is the STATIC APIRequest factory (the per-test `request`
 * fixture is an already-built context and has no newContext). Cookies are
 * copied explicitly (httpOnly cookies are not in document.cookie).
 */
async function hubApi(page) {
  const cookies = await page.context().cookies();
  const origin = new URL(page.url()).origin;
  return apiRequest.newContext({
    baseURL: origin,
    extraHTTPHeaders: {
      Cookie: cookies.map(c => `${c.name}=${c.value}`).join("; "),
      Origin: origin
    }
  });
}

/** Commands for one instance via the independent client; a non-200 fails the test. */
async function hubCommands(api, instanceId) {
  var _body$commands;
  const res = await api.get(`/v1/instances/${instanceId}/commands?limit=100`);
  // A failed Hub read must never be swallowed into an empty (== delivered) list.
  expect(res.status(), `GET commands HTTP ${res.status()}`).toBe(200);
  const body = await res.json();
  return ((_body$commands = body.commands) !== null && _body$commands !== void 0 ? _body$commands : []).map(c => {
    var _ref, _c$id;
    return {
      ...c,
      id: (_ref = (_c$id = c.id) !== null && _c$id !== void 0 ? _c$id : c.commandId) !== null && _ref !== void 0 ? _ref : ""
    };
  });
}

/** Journal user messages for one commandId via the independent client (200 required). */
async function hubJournalMessageCount(api, instanceId, commandId) {
  var _body$events;
  const res = await api.get(`/v1/instances/${instanceId}/journal?limit=2000`);
  expect(res.status(), `GET journal HTTP ${res.status()}`).toBe(200);
  const body = await res.json();
  return ((_body$events = body.events) !== null && _body$events !== void 0 ? _body$events : []).filter(raw => {
    var _raw$event, _e$payload;
    const e = (_raw$event = raw.event) !== null && _raw$event !== void 0 ? _raw$event : raw;
    return e.kind === "message" && ((_e$payload = e.payload) === null || _e$payload === void 0 ? void 0 : _e$payload.commandId) === commandId;
  }).length;
}
async function sendMessage(page, text) {
  await expect(page.getByTestId("composer-input")).toBeEnabled({
    timeout: 20000
  });
  await page.getByTestId("composer-input").fill(text);
  const send = page.getByTestId("composer-send");
  const queue = page.getByTestId("composer-queue");
  if (await queue.isVisible().catch(() => false)) await queue.click();else await send.click();
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
async function blockHub(context) {
  await context.route(V1, route => route.abort("failed"));
  await context.setOffline(true);
}

/**
 * Lift network emulation while KEEPING the Hub route active: use this before
 * reloading offline so the Vite document reloads but Hub REST/follow stay
 * down exactly as a Hub-only outage looks to the restored page.
 */
async function keepHubBlockedByRoutes(context) {
  await context.setOffline(false);
}
async function unblockHub(context) {
  await context.setOffline(false);
  await context.unroute(V1);
}
test.beforeEach(async ({
  page
}) => {
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
async function deleteCreatedInstances(browser) {
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
      expect([200, 202, 204, 404], `DELETE instance ${id} -> HTTP ${res.status()}`).toContain(res.status());
    }
    // The fake node settles the stop asynchronously: poll until none of our
    // ids still holds a placement slot (absent rows pass — DELETE won).
    await expect.poll(async () => {
      var _body$items3;
      const res = await cleanup.request.get("/v1/instances");
      expect(res.status(), `GET instances -> HTTP ${res.status()}`).toBe(200);
      const body = await res.json();
      return ((_body$items3 = body.items) !== null && _body$items3 !== void 0 ? _body$items3 : []).filter(it => {
        var _it$instanceId;
        return ids.includes((_it$instanceId = it.instanceId) !== null && _it$instanceId !== void 0 ? _it$instanceId : "");
      }).filter(it => {
        var _it$lifecycle;
        return !TERMINAL_LIFECYCLES.has((_it$lifecycle = it.lifecycle) !== null && _it$lifecycle !== void 0 ? _it$lifecycle : "");
      }).map(it => it.instanceId);
    }, {
      timeout: 30000
    }).toEqual([]);
    created.splice(0, created.length, ...created.filter(id => !ids.includes(id)));
  } finally {
    await cleanup.close();
  }
}

// Clean up after EACH test so the three instances never pile up within the
// run, and again in afterAll as a safety net when an afterEach could not run
// its own cleanup (it only ever sees ids left behind).
test.afterEach(async ({
  browser
}) => {
  await deleteCreatedInstances(browser);
});
test.afterAll(async ({
  browser
}) => {
  await deleteCreatedInstances(browser);
});

/**
 * Wait for the optimistic bubble to be replaced by the authoritative journal
 * transcript row carrying the SAME commandId (what "delivered" renders as once
 * the journal joins), and assert no waiting/unconfirmed chip is left behind.
 */
async function expectDelivered(page, commandId) {
  const authoritative = page.locator(`[data-testid="transcript-row"][data-role="user"][data-command-id="${commandId}"]`);
  await expect(authoritative).toHaveCount(1, {
    timeout: 30000
  });
  const bubble = page.locator(`[data-testid="optimistic-bubble"][data-command-id="${commandId}"]`);
  // The optimistic chip for THIS row is gone (the authoritative row carries
  // no waiting/unconfirmed label; other scripted commands on the page may
  // independently show their own status and are not asserted here).
  await expect(bubble).toHaveCount(0);
}
test("offline sends are queued and delivered exactly once after reconnect", async ({
  page
}) => {
  const instanceId = await createSession(page, "offline outbox seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({
    timeout: 20000
  });
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
  const commandIds = await pending.evaluateAll(nodes => nodes.map(n => n.getAttribute("data-command-id")));
  expect(commandIds).toHaveLength(2);
  expect(commandIds.every(id => id === null || id === void 0 ? void 0 : id.startsWith("cmd_"))).toBe(true);

  // The Hub has nothing yet (read independently, require 200).
  const seedSends = (await hubCommands(api, instanceId)).filter(c => c.operation === "instance.send").length;
  expect(seedSends).toBe(0);
  await unblockHub(page.context());
  await expect(banner).toHaveCount(0, {
    timeout: 20000
  });

  // Exactly one Hub command row per id …
  await expect.poll(async () => (await hubCommands(api, instanceId)).filter(c => c.operation === "instance.send").length).toBe(2);
  // … and exactly one journal execution each.
  for (const cid of commandIds) {
    await expect.poll(() => hubJournalMessageCount(api, instanceId, cid)).toBe(1);
    // The delivered row becomes the authoritative journal message (same id);
    // the optimistic chip is gone and no 状态待确认 is shown.
    await expectDelivered(page, cid);
  }
});
test("an offline-queued message survives a reload while the Hub is still off and sends once after", async ({
  page
}) => {
  const instanceId = await createSession(page, "offline reload seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({
    timeout: 20000
  });
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
  await page.reload({
    waitUntil: "domcontentloaded"
  });
  // The session renders from the persisted instance projection (no
  // 会话不存在 / cast stub), and the durable row restores and still waits
  // offline; nothing has been POSTed.
  await expect(page.getByTestId("session-page")).toBeVisible({
    timeout: 20000
  });
  const restored = page.locator(`[data-testid="optimistic-bubble"][data-command-id="${commandId}"]`);
  // The durable row restores and renders with the composer available (the
  // offline seed never loaded events, but restored bubbles unblock the
  // transcript — no "会话不存在", no stuck "加载 snapshot…").
  await expect(restored).toBeVisible();
  expect(restored).toContainText("offline across reload");
  await expect(page.getByTestId("composer-input")).toBeEnabled({
    timeout: 20000
  });
  // It is not a terminal/rejected row while the Hub has never received it.
  // Scope to the bubble's own subtree (the session page can independently
  // show 状态待确认 for the scripted create command).
  await expect(restored).not.toContainText("未送达");
  // And nothing reached the Hub yet (independent read, 200 required).
  expect((await hubCommands(api, instanceId)).filter(c => c.operation === "instance.send")).toHaveLength(0);

  // Reconnect: the restored row delivers exactly once under the same id.
  await unblockHub(page.context());
  await expect(page.getByTestId("journal-banner")).toHaveCount(0, {
    timeout: 20000
  });
  await expect.poll(() => hubJournalMessageCount(api, instanceId, commandId), {
    timeout: 30000
  }).toBe(1);
  await expect.poll(async () => (await hubCommands(api, instanceId)).filter(c => c.operation === "instance.send" && c.id === commandId).length, {
    timeout: 30000
  }).toBe(1);
  await expectDelivered(page, commandId);
});
test("a committed POST whose browser response is lost retries with replayed:true and runs once", async ({
  page
}) => {
  var _releaseRetry;
  const instanceId = await createSession(page, "lost response seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({
    timeout: 20000
  });
  const api = await hubApi(page);
  const commandsPattern = /\/v1\/instances\/[^/]+\/commands$/;
  const statusPattern = /\/v1\/instances\/[^/]+\/commands\/cmd_/;
  const replayResults = [];
  let firstPostLost = false;
  // Gate the retry so the row's intermediate state is observable instead of
  // the local harness completing the re-POST within milliseconds.
  let releaseRetry = null;
  const retryGate = new Promise(resolve => {
    releaseRetry = resolve;
  });
  await page.context().route(commandsPattern, async route => {
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
    const body = await retry.json();
    replayResults.push(body.replayed === true);
    return route.fulfill({
      response: retry
    });
  });
  // The post-loss GET reconciliation must also fail, so the client keeps the
  // row pending and re-POSTs (rather than settling on the GET verdict).
  await page.context().route(statusPattern, route => route.request().method() === "GET" ? route.abort("failed") : route.continue());
  await sendMessage(page, "lost response message");
  const bubble = page.locator('[data-testid="optimistic-bubble"]').first();
  await expect(bubble).toBeVisible();
  const commandId = await bubble.getAttribute("data-command-id");
  expect(commandId).toBeTruthy();

  // While the parked retry is in flight the row says 已发送，等待确认
  // (it reached the Hub), never 状态待确认 and never a terminal rejection.
  await expect(bubble).toContainText("已发送，等待确认", {
    timeout: 15000
  });
  await expect(bubble).not.toContainText("状态待确认");

  // Release the retry: the Hub dedupes and answers replayed:true.
  (_releaseRetry = releaseRetry) === null || _releaseRetry === void 0 || _releaseRetry();
  await expect.poll(() => replayResults.length).toBeGreaterThan(0);
  expect(replayResults[0]).toBe(true);

  // One command row …
  await expect.poll(async () => (await hubCommands(api, instanceId)).filter(c => c.operation === "instance.send" && c.id === commandId).length, {
    timeout: 30000
  }).toBe(1);
  // … and one journal message for the id (executed exactly once).
  await expect.poll(() => hubJournalMessageCount(api, instanceId, commandId)).toBe(1);
  await expectDelivered(page, commandId);
});
test("an online send labels the row 等待发送 then 已发送，等待确认/已受理 as it delivers", async ({
  page
}) => {
  var _releasePost;
  const instanceId = await createSession(page, "label progression seed");
  await expect(page.getByTestId("composer-input")).toBeEnabled({
    timeout: 20000
  });
  const api = await hubApi(page);

  // Hold this instance's single-deliverer Web Lock from inside the page: with
  // no flush possible the queued bubble must sit at its honest online label
  // (等待发送 — NOT 待发送（离线）, the link is live the whole time).
  await page.evaluate(iid => {
    const w = window;
    const lock = new Promise(resolve => {
      w.__releaseLock = resolve;
    });
    void navigator.locks.request(`remuda-outbox-${iid}`, () => lock);
  }, instanceId);

  // Once the flush acquires the lock it reaches the POST; park that so the
  // in-flight label is observable too.
  const commandsPattern = /\/v1\/instances\/[^/]+\/commands$/;
  let releasePost = null;
  const postGate = new Promise(resolve => {
    releasePost = resolve;
  });
  await page.context().route(commandsPattern, async route => {
    if (route.request().method() !== "POST") return route.continue();
    await postGate;
    const res = await route.fetch();
    return route.fulfill({
      response: res
    });
  });
  await sendMessage(page, "watch the labels");
  const bubble = page.locator('[data-testid="optimistic-bubble"]').first();
  await expect(bubble).toBeVisible();
  const commandId = await bubble.getAttribute("data-command-id");
  expect(commandId).toBeTruthy();

  // Queued behind the lock, link live: 等待发送, never the offline wording.
  await expect(bubble).toContainText("等待发送");
  await expect(bubble).not.toContainText("离线");

  // Release the lock: the flush takes it and the parked POST shows the row
  // reached the Hub (in-flight), never 状态待确认.
  await page.evaluate(() => {
    var _releaseLock, _ref2;
    return (_releaseLock = (_ref2 = window).__releaseLock) === null || _releaseLock === void 0 ? void 0 : _releaseLock.call(_ref2);
  });
  await expect(bubble).toContainText("已发送，等待确认", {
    timeout: 15000
  });
  await expect(bubble).not.toContainText("状态待确认");

  // Release the POST: the command runs once and the journal join replaces
  // the chip with the authoritative row (已受理 on the way).
  (_releasePost = releasePost) === null || _releasePost === void 0 || _releasePost();
  await expect.poll(() => hubJournalMessageCount(api, instanceId, commandId), {
    timeout: 30000
  }).toBe(1);
  await expect.poll(async () => (await hubCommands(api, instanceId)).filter(c => c.operation === "instance.send" && c.id === commandId).length, {
    timeout: 30000
  }).toBe(1);
  await expectDelivered(page, commandId);
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
    args: ["--disable-features=BlockInsecurePrivateNetworkRequests,PrivateNetworkAccessChecks,PrivateNetworkAccessForNavigations,PrivateNetworkAccessForWorkers,PrivateNetworkAccessForWebRTC,BlockInsecureLocalNetworkRequests,LocalNetworkAccessChecks,LocalNetworkAccessChecksForNavigation,LocalNetworkAccessChecksForWebRTC,LocalNetworkAccessChecksForWorkers,LocalNetworkAccessChecksWarningOnly"]
  }
});
test.describe("full offline SW restore (PNA/LNA loopback exemption for this harness case)", () => {
  test("an offline-queued message survives a reload with the browser context STILL offline and sends once after", async ({
    page
  }) => {
    const instanceId = await createSession(page, "full offline reload seed");
    await expect(page.getByTestId("composer-input")).toBeEnabled({
      timeout: 20000
    });
    const api = await hubApi(page);

    // Install the offline-shell worker and let it take control.
    await page.context().route("**/e2e-offline-sw.js", route => route.fulfill({
      contentType: "application/javascript; charset=utf-8",
      body: OFFLINE_SHELL_SW
    }));
    await page.evaluate(async () => {
      await navigator.serviceWorker.register("/e2e-offline-sw.js", {
        updateViaCache: "none"
      });
      await navigator.serviceWorker.ready;
      if (!navigator.serviceWorker.controller) {
        await new Promise(resolve => navigator.serviceWorker.addEventListener("controllerchange", resolve, {
          once: true
        }));
      }
    });

    // One more ONLINE navigation so the controlled page primes the shell cache.
    await page.goto(page.url());
    await expect(page.getByTestId("session-page")).toBeVisible({
      timeout: 20000
    });
    await expect(page.getByTestId("composer-input")).toBeEnabled({
      timeout: 20000
    });

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
    expect((await hubCommands(api, instanceId)).filter(c => c.operation === "instance.send")).toHaveLength(0);

    // Reload WHILE context offline: the service worker serves the document and
    // the whole module shell; the restored app boots from durable state.
    await page.reload({
      waitUntil: "domcontentloaded"
    });
    await expect(page.getByTestId("session-page")).toBeVisible({
      timeout: 20000
    });
    const restored = page.locator(`[data-testid="optimistic-bubble"][data-command-id="${commandId}"]`);
    await expect(restored).toBeVisible();
    expect(restored).toContainText("offline across a full offline reload");
    await expect(page.getByTestId("composer-input")).toBeEnabled({
      timeout: 20000
    });
    await expect(restored).toContainText("待发送（离线）");
    // Still nothing at the Hub through the offline reload.
    expect((await hubCommands(api, instanceId)).filter(c => c.operation === "instance.send")).toHaveLength(0);

    // Lift emulation: with this test's loopback-PNA exemption the restored
    // page can reopen its follow socket; the online event plus a foreground
    // resume kick the machine out of its offline backoff.
    await page.context().setOffline(false);
    await page.evaluate(() => {
      window.dispatchEvent(new Event("online"));
      window.dispatchEvent(new Event("focus"));
    });
    await expect.poll(async () => (await hubCommands(api, instanceId)).filter(c => c.operation === "instance.send" && c.id === commandId).length, {
      timeout: 60000
    }).toBe(1);
    await expect.poll(() => hubJournalMessageCount(api, instanceId, commandId), {
      timeout: 30000
    }).toBe(1);
    // The link banner clears after the brief 已恢复 notice (1.5 s).
    await expect(page.getByTestId("journal-banner")).toHaveCount(0, {
      timeout: 20000
    });
    await expectDelivered(page, commandId);
  });
});
//# sourceMappingURL=data:application/json;charset=utf-8;base64,eyJ2ZXJzaW9uIjozLCJuYW1lcyI6WyJleHBlY3QiLCJyZXF1ZXN0IiwiYXBpUmVxdWVzdCIsInRlc3QiLCJsb2dpbiIsImRlc2NyaWJlIiwiY29uZmlndXJlIiwibW9kZSIsImNyZWF0ZWQiLCJjbGVhckFwcHJvdmFscyIsInBhZ2UiLCJpbnN0YW5jZUlkIiwicGVuZGluZ0NvdW50IiwiZXZhbHVhdGUiLCJpZCIsIl9ib2R5JGl0ZW1zIiwicmVzIiwiZmV0Y2giLCJjcmVkZW50aWFscyIsImJvZHkiLCJqc29uIiwiaXRlbXMiLCJmaWx0ZXIiLCJpIiwic3RhdGUiLCJsZW5ndGgiLCJzZWVuIiwicG9sbCIsInRpbWVvdXQiLCJ0b0JlR3JlYXRlclRoYW4iLCJ0aGVuIiwiY2F0Y2giLCJfYm9keSRpdGVtczIiLCJtaW5lIiwiaXRlbSIsIl9pdGVtJHJlcXVlc3QiLCJfaXRlbSRyZXF1ZXN0JGlucHV0RGkiLCJfaXRlbSRyZXF1ZXN0MiIsIm9wdGlvbklkIiwib3B0aW9ucyIsIm1ldGhvZCIsImhlYWRlcnMiLCJKU09OIiwic3RyaW5naWZ5IiwiYW5zd2VyIiwia2luZCIsImlucHV0RGlnZXN0IiwidG9CZSIsImNyZWF0ZVNlc3Npb24iLCJwcm9tcHQiLCJnb3RvIiwiaG9zdFBpY2tlciIsImdldEJ5VGVzdElkIiwidG9Db250YWluVGV4dCIsImhvc3RJZCIsImxvY2F0b3IiLCJoYXNUZXh0IiwiZ2V0QXR0cmlidXRlIiwidG9CZVRydXRoeSIsInNlbGVjdE9wdGlvbiIsImNsaWNrIiwibm90IiwidG9IYXZlQ291bnQiLCJmaWxsIiwid2FpdEZvclVSTCIsIlVSTCIsInVybCIsInBhdGhuYW1lIiwic3BsaXQiLCJwb3AiLCJwdXNoIiwidG9IYXZlQXR0cmlidXRlIiwiaHViQXBpIiwiY29va2llcyIsImNvbnRleHQiLCJvcmlnaW4iLCJuZXdDb250ZXh0IiwiYmFzZVVSTCIsImV4dHJhSFRUUEhlYWRlcnMiLCJDb29raWUiLCJtYXAiLCJjIiwibmFtZSIsInZhbHVlIiwiam9pbiIsIk9yaWdpbiIsImh1YkNvbW1hbmRzIiwiYXBpIiwiX2JvZHkkY29tbWFuZHMiLCJnZXQiLCJzdGF0dXMiLCJjb21tYW5kcyIsIl9yZWYiLCJfYyRpZCIsImNvbW1hbmRJZCIsImh1YkpvdXJuYWxNZXNzYWdlQ291bnQiLCJfYm9keSRldmVudHMiLCJldmVudHMiLCJyYXciLCJfcmF3JGV2ZW50IiwiX2UkcGF5bG9hZCIsImUiLCJldmVudCIsInBheWxvYWQiLCJzZW5kTWVzc2FnZSIsInRleHQiLCJ0b0JlRW5hYmxlZCIsInNlbmQiLCJxdWV1ZSIsImlzVmlzaWJsZSIsIlYxIiwiYmxvY2tIdWIiLCJyb3V0ZSIsImFib3J0Iiwic2V0T2ZmbGluZSIsImtlZXBIdWJCbG9ja2VkQnlSb3V0ZXMiLCJ1bmJsb2NrSHViIiwidW5yb3V0ZSIsImJlZm9yZUVhY2giLCJURVJNSU5BTF9MSUZFQ1lDTEVTIiwiU2V0IiwiZGVsZXRlQ3JlYXRlZEluc3RhbmNlcyIsImJyb3dzZXIiLCJpZHMiLCJjbGVhbnVwIiwibmV3UGFnZSIsImRlbGV0ZSIsInRvQ29udGFpbiIsIl9ib2R5JGl0ZW1zMyIsIml0IiwiX2l0JGluc3RhbmNlSWQiLCJpbmNsdWRlcyIsIl9pdCRsaWZlY3ljbGUiLCJoYXMiLCJsaWZlY3ljbGUiLCJ0b0VxdWFsIiwic3BsaWNlIiwiY2xvc2UiLCJhZnRlckVhY2giLCJhZnRlckFsbCIsImV4cGVjdERlbGl2ZXJlZCIsImF1dGhvcml0YXRpdmUiLCJidWJibGUiLCJ3YWl0Rm9yVGltZW91dCIsImJhbm5lciIsInRleHRDb250ZW50IiwicGVuZGluZyIsImZpcnN0IiwiY29tbWFuZElkcyIsImV2YWx1YXRlQWxsIiwibm9kZXMiLCJuIiwidG9IYXZlTGVuZ3RoIiwiZXZlcnkiLCJzdGFydHNXaXRoIiwic2VlZFNlbmRzIiwib3BlcmF0aW9uIiwiY2lkIiwicXVldWVkIiwicmVsb2FkIiwid2FpdFVudGlsIiwidG9CZVZpc2libGUiLCJyZXN0b3JlZCIsIl9yZWxlYXNlUmV0cnkiLCJjb21tYW5kc1BhdHRlcm4iLCJzdGF0dXNQYXR0ZXJuIiwicmVwbGF5UmVzdWx0cyIsImZpcnN0UG9zdExvc3QiLCJyZWxlYXNlUmV0cnkiLCJyZXRyeUdhdGUiLCJQcm9taXNlIiwicmVzb2x2ZSIsImNvbnRpbnVlIiwic2VydmVyIiwicmV0cnkiLCJyZXBsYXllZCIsImZ1bGZpbGwiLCJyZXNwb25zZSIsIl9yZWxlYXNlUG9zdCIsImlpZCIsInciLCJ3aW5kb3ciLCJsb2NrIiwiX19yZWxlYXNlTG9jayIsIm5hdmlnYXRvciIsImxvY2tzIiwicmVsZWFzZVBvc3QiLCJwb3N0R2F0ZSIsIl9yZWxlYXNlTG9jayIsIl9yZWYyIiwiY2FsbCIsIk9GRkxJTkVfU0hFTExfU1ciLCJ1c2UiLCJsYXVuY2hPcHRpb25zIiwiYXJncyIsImNvbnRlbnRUeXBlIiwic2VydmljZVdvcmtlciIsInJlZ2lzdGVyIiwidXBkYXRlVmlhQ2FjaGUiLCJyZWFkeSIsImNvbnRyb2xsZXIiLCJhZGRFdmVudExpc3RlbmVyIiwib25jZSIsImRpc3BhdGNoRXZlbnQiLCJFdmVudCJdLCJzb3VyY2VzIjpbIm9mZmxpbmUtb3V0Ym94Lmh1Yi5zcGVjLnRzIl0sInNvdXJjZXNDb250ZW50IjpbImltcG9ydCB7IGV4cGVjdCwgcmVxdWVzdCBhcyBhcGlSZXF1ZXN0LCB0ZXN0LCB0eXBlIEFQSVJlcXVlc3RDb250ZXh0LCB0eXBlIEJyb3dzZXIsIHR5cGUgUGFnZSB9IGZyb20gXCJAcGxheXdyaWdodC90ZXN0XCI7XG5pbXBvcnQgeyBsb2dpbiB9IGZyb20gXCIuL2h1Yi1hdXRoXCI7XG5cbi8qKlxuICogRC0wNTUgY2xpZW50IGF1dG8tcmVjb25uZWN0ICsgb2ZmbGluZSBvdXRib3gsIGVuZCB0byBlbmQgYWdhaW5zdCB0aGUgZmFrZVxuICogSHViL05vZGUuXG4gKlxuICogIC0gdHdvIG1lc3NhZ2VzIHNlbnQgd2hpbGUgdGhlIEh1YiBpcyB1bnJlYWNoYWJsZSBhcmUgZHVyYWJseSBxdWV1ZWRcbiAqICAgICjlvoXlj5HpgIHvvIjnprvnur/vvIksIHplcm8gY29tbWFuZHMgYXQgdGhlIEh1YikgYW5kLCBvbmNlIHJlYWNoYWJsZSBhZ2FpbixcbiAqICAgIGRlbGl2ZXJlZCB3aXRoIHRoZSBzYW1lIGNvbW1hbmRJZHMgZXhhY3RseSBvbmNlO1xuICogIC0gYSBxdWV1ZWQgbWVzc2FnZSBzdXJ2aXZlcyBhIGZ1bGwgcGFnZSByZWxvYWQgV0hJTEUgdGhlIEh1YiBpcyBzdGlsbFxuICogICAgdW5yZWFjaGFibGU6IHRoZSByZXN0b3JlZCBzZXNzaW9uIHJlbmRlcnMgZnJvbSB0aGUgcGVyc2lzdGVkIGluc3RhbmNlXG4gKiAgICBwcm9qZWN0aW9uLCB0aGVuIG9uZSBkZWxpdmVyeSBoYXBwZW5zIGFmdGVyIHJlY29ubmVjdDtcbiAqICAtIGEgUE9TVCB0aGUgSHViIENPTU1JVFRFRCBidXQgd2hvc2UgYnJvd3NlciByZXNwb25zZSB3YXMgbG9zdCBpcyByZXRyaWVkXG4gKiAgICB3aXRoIHRoZSBzYW1lIGNvbW1hbmRJZDsgdGhlIEh1YiBhbnN3ZXJzIHRoZSByZXRyeSByZXBsYXllZDp0cnVlIGFuZCB0aGVcbiAqICAgIGNvbW1hbmQgZXhlY3V0ZXMgZXhhY3RseSBvbmNlIChvbmUgY29tbWFuZCByb3csIG9uZSBqb3VybmFsIG1lc3NhZ2UpLlxuICpcbiAqIEh1YiBzdGF0ZSBpcyBhbHdheXMgcmVhZCB0aHJvdWdoIFBsYXl3cmlnaHQncyBJTkRFUEVOREVOVCByZXF1ZXN0IGZpeHR1cmVcbiAqIChuZXZlciB0aGUgYnJvd3NlcidzIGZldGNoLCBuZXZlciBhIHN3YWxsb3dlZCBmYWlsdXJlKTogZXZlcnkgcmVhZCByZXF1aXJlc1xuICogSFRUUCAyMDAsIHNvIGFuIGVtcHR5IGxpc3QgY2FuIG5ldmVyIG1hc3F1ZXJhZGUgYXMgXCJkZWxpdmVyZWRcIi5cbiAqL1xudGVzdC5kZXNjcmliZS5jb25maWd1cmUoeyBtb2RlOiBcInNlcmlhbFwiIH0pO1xuXG5jb25zdCBjcmVhdGVkOiBzdHJpbmdbXSA9IFtdO1xuXG5hc3luYyBmdW5jdGlvbiBjbGVhckFwcHJvdmFscyhwYWdlOiBQYWdlLCBpbnN0YW5jZUlkOiBzdHJpbmcpIHtcbiAgLy8gVGhlIGZha2Ugbm9kZSByYWlzZXMgYSBsYXVuY2ggYXBwcm92YWwgb24gY3JlYXRlOyBhbnN3ZXIgaXQgdmlhIHRoZSBBUEkgc29cbiAgLy8gdGhlIHR1cm4gY29tcGxldGVzIGFuZCB0aGUgY29tcG9zZXIgcmV0dXJucyB0byBpdHMgaWRsZSBwcmltYXJ5IGJ1dHRvbi5cbiAgY29uc3QgcGVuZGluZ0NvdW50ID0gKCkgPT5cbiAgICBwYWdlLmV2YWx1YXRlKGFzeW5jIChpZCkgPT4ge1xuICAgICAgY29uc3QgcmVzID0gYXdhaXQgZmV0Y2goXCIvdjEvaW50ZXJhY3Rpb25zXCIsIHsgY3JlZGVudGlhbHM6IFwiaW5jbHVkZVwiIH0pO1xuICAgICAgY29uc3QgYm9keSA9IChhd2FpdCByZXMuanNvbigpKSBhcyB7XG4gICAgICAgIGl0ZW1zPzogeyBpZDogc3RyaW5nOyBpbnN0YW5jZUlkPzogc3RyaW5nOyBzdGF0ZT86IHN0cmluZyB9W107XG4gICAgICB9O1xuICAgICAgcmV0dXJuIChib2R5Lml0ZW1zID8/IFtdKS5maWx0ZXIoKGkpID0+IGkuaW5zdGFuY2VJZCA9PT0gaWQgJiYgaS5zdGF0ZSA9PT0gXCJwZW5kaW5nXCIpLmxlbmd0aDtcbiAgICB9LCBpbnN0YW5jZUlkKTtcbiAgY29uc3Qgc2VlbiA9IGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKHBlbmRpbmdDb3VudCwgeyB0aW1lb3V0OiAyMF8wMDAgfSlcbiAgICAudG9CZUdyZWF0ZXJUaGFuKDApXG4gICAgLnRoZW4oKCkgPT4gdHJ1ZSlcbiAgICAuY2F0Y2goKCkgPT4gZmFsc2UpO1xuICBpZiAoIXNlZW4pIHJldHVybjsgLy8gcHJvbXB0IHNjcmlwdGVkIG5vIGFwcHJvdmFsXG4gIGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKFxuICAgICAgYXN5bmMgKCkgPT5cbiAgICAgICAgcGFnZS5ldmFsdWF0ZShhc3luYyAoaWQpID0+IHtcbiAgICAgICAgICBjb25zdCByZXMgPSBhd2FpdCBmZXRjaChcIi92MS9pbnRlcmFjdGlvbnNcIiwgeyBjcmVkZW50aWFsczogXCJpbmNsdWRlXCIgfSk7XG4gICAgICAgICAgY29uc3QgYm9keSA9IChhd2FpdCByZXMuanNvbigpKSBhcyB7XG4gICAgICAgICAgICBpdGVtcz86IHtcbiAgICAgICAgICAgICAgaWQ6IHN0cmluZztcbiAgICAgICAgICAgICAgaW5zdGFuY2VJZD86IHN0cmluZztcbiAgICAgICAgICAgICAgc3RhdGU/OiBzdHJpbmc7XG4gICAgICAgICAgICAgIHJlcXVlc3Q/OiB7IGlucHV0RGlnZXN0Pzogc3RyaW5nOyBvcHRpb25zPzogeyBpZDogc3RyaW5nIH1bXSB9O1xuICAgICAgICAgICAgfVtdO1xuICAgICAgICAgIH07XG4gICAgICAgICAgY29uc3QgbWluZSA9IChib2R5Lml0ZW1zID8/IFtdKS5maWx0ZXIoKGkpID0+IGkuaW5zdGFuY2VJZCA9PT0gaWQgJiYgaS5zdGF0ZSA9PT0gXCJwZW5kaW5nXCIpO1xuICAgICAgICAgIGZvciAoY29uc3QgaXRlbSBvZiBtaW5lKSB7XG4gICAgICAgICAgICBjb25zdCBvcHRpb25JZCA9IGl0ZW0ucmVxdWVzdD8ub3B0aW9ucz8uWzBdPy5pZDtcbiAgICAgICAgICAgIGlmICghb3B0aW9uSWQpIGNvbnRpbnVlO1xuICAgICAgICAgICAgYXdhaXQgZmV0Y2goYC92MS9pbnRlcmFjdGlvbnMvJHtpdGVtLmlkfS9hbnN3ZXJgLCB7XG4gICAgICAgICAgICAgIG1ldGhvZDogXCJQT1NUXCIsXG4gICAgICAgICAgICAgIGNyZWRlbnRpYWxzOiBcImluY2x1ZGVcIixcbiAgICAgICAgICAgICAgaGVhZGVyczogeyBcImNvbnRlbnQtdHlwZVwiOiBcImFwcGxpY2F0aW9uL2pzb25cIiB9LFxuICAgICAgICAgICAgICBib2R5OiBKU09OLnN0cmluZ2lmeSh7XG4gICAgICAgICAgICAgICAgYW5zd2VyOiB7IGtpbmQ6IFwiYXBwcm92YWxcIiwgb3B0aW9uSWQsIGlucHV0RGlnZXN0OiBpdGVtLnJlcXVlc3Q/LmlucHV0RGlnZXN0ID8/IFwiXCIgfSxcbiAgICAgICAgICAgICAgfSksXG4gICAgICAgICAgICB9KTtcbiAgICAgICAgICB9XG4gICAgICAgICAgcmV0dXJuIG1pbmUubGVuZ3RoO1xuICAgICAgICB9LCBpbnN0YW5jZUlkKSxcbiAgICAgIHsgdGltZW91dDogMjBfMDAwIH0sXG4gICAgKVxuICAgIC50b0JlKDApO1xufVxuXG5hc3luYyBmdW5jdGlvbiBjcmVhdGVTZXNzaW9uKHBhZ2U6IFBhZ2UsIHByb21wdDogc3RyaW5nKTogUHJvbWlzZTxzdHJpbmc+IHtcbiAgYXdhaXQgcGFnZS5nb3RvKFwiL3Nlc3Npb25zL25ld1wiKTtcbiAgY29uc3QgaG9zdFBpY2tlciA9IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpO1xuICBhd2FpdCBleHBlY3QoaG9zdFBpY2tlcikudG9Db250YWluVGV4dChcImUyZS1mYWtlLW5vZGVcIiwgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGNvbnN0IGhvc3RJZCA9IGF3YWl0IGhvc3RQaWNrZXJcbiAgICAubG9jYXRvcihcIm9wdGlvblwiKVxuICAgIC5maWx0ZXIoeyBoYXNUZXh0OiBcImUyZS1mYWtlLW5vZGVcIiB9KVxuICAgIC5nZXRBdHRyaWJ1dGUoXCJ2YWx1ZVwiKTtcbiAgZXhwZWN0KGhvc3RJZCkudG9CZVRydXRoeSgpO1xuICBhd2FpdCBob3N0UGlja2VyLnNlbGVjdE9wdGlvbihob3N0SWQhKTtcbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLWtpbmQtY2xhdWRlXCIpLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24td29ya3NwYWNlXCIpLmxvY2F0b3IoXCJvcHRpb25cIikpLm5vdC50b0hhdmVDb3VudCgwLCB7XG4gICAgdGltZW91dDogMjBfMDAwLFxuICB9KTtcbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLXByb21wdFwiKS5maWxsKHByb21wdCk7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1zdGFydFwiKS5jbGljaygpO1xuICBhd2FpdCBwYWdlLndhaXRGb3JVUkwoL1xcL3NcXC8vLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgY29uc3QgaWQgPSBuZXcgVVJMKHBhZ2UudXJsKCkpLnBhdGhuYW1lLnNwbGl0KFwiL1wiKS5wb3AoKSE7XG4gIGNyZWF0ZWQucHVzaChpZCk7XG4gIGF3YWl0IGNsZWFyQXBwcm92YWxzKHBhZ2UsIGlkKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJzZXNzaW9uLXBhZ2VcIikpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtc3RhdHVzXCIsIFwiaWRsZVwiLCB7XG4gICAgdGltZW91dDogMjBfMDAwLFxuICB9KTtcbiAgcmV0dXJuIGlkO1xufVxuXG4vKipcbiAqIEFuIGF1dGhlbnRpY2F0ZWQgQVBJIGNsaWVudCBpbmRlcGVuZGVudCBvZiB0aGUgYnJvd3NlciBjb250ZXh0LiBQbGF5d3JpZ2h0J3NcbiAqIGByZXF1ZXN0Lm5ld0NvbnRleHRgIGlzIHRoZSBTVEFUSUMgQVBJUmVxdWVzdCBmYWN0b3J5ICh0aGUgcGVyLXRlc3QgYHJlcXVlc3RgXG4gKiBmaXh0dXJlIGlzIGFuIGFscmVhZHktYnVpbHQgY29udGV4dCBhbmQgaGFzIG5vIG5ld0NvbnRleHQpLiBDb29raWVzIGFyZVxuICogY29waWVkIGV4cGxpY2l0bHkgKGh0dHBPbmx5IGNvb2tpZXMgYXJlIG5vdCBpbiBkb2N1bWVudC5jb29raWUpLlxuICovXG5hc3luYyBmdW5jdGlvbiBodWJBcGkocGFnZTogUGFnZSk6IFByb21pc2U8QVBJUmVxdWVzdENvbnRleHQ+IHtcbiAgY29uc3QgY29va2llcyA9IGF3YWl0IHBhZ2UuY29udGV4dCgpLmNvb2tpZXMoKTtcbiAgY29uc3Qgb3JpZ2luID0gbmV3IFVSTChwYWdlLnVybCgpKS5vcmlnaW47XG4gIHJldHVybiBhcGlSZXF1ZXN0Lm5ld0NvbnRleHQoe1xuICAgIGJhc2VVUkw6IG9yaWdpbixcbiAgICBleHRyYUhUVFBIZWFkZXJzOiB7XG4gICAgICBDb29raWU6IGNvb2tpZXMubWFwKChjKSA9PiBgJHtjLm5hbWV9PSR7Yy52YWx1ZX1gKS5qb2luKFwiOyBcIiksXG4gICAgICBPcmlnaW46IG9yaWdpbixcbiAgICB9LFxuICB9KTtcbn1cblxuLyoqIENvbW1hbmRzIGZvciBvbmUgaW5zdGFuY2UgdmlhIHRoZSBpbmRlcGVuZGVudCBjbGllbnQ7IGEgbm9uLTIwMCBmYWlscyB0aGUgdGVzdC4gKi9cbmFzeW5jIGZ1bmN0aW9uIGh1YkNvbW1hbmRzKGFwaTogQVBJUmVxdWVzdENvbnRleHQsIGluc3RhbmNlSWQ6IHN0cmluZykge1xuICBjb25zdCByZXMgPSBhd2FpdCBhcGkuZ2V0KGAvdjEvaW5zdGFuY2VzLyR7aW5zdGFuY2VJZH0vY29tbWFuZHM/bGltaXQ9MTAwYCk7XG4gIC8vIEEgZmFpbGVkIEh1YiByZWFkIG11c3QgbmV2ZXIgYmUgc3dhbGxvd2VkIGludG8gYW4gZW1wdHkgKD09IGRlbGl2ZXJlZCkgbGlzdC5cbiAgZXhwZWN0KHJlcy5zdGF0dXMoKSwgYEdFVCBjb21tYW5kcyBIVFRQICR7cmVzLnN0YXR1cygpfWApLnRvQmUoMjAwKTtcbiAgY29uc3QgYm9keSA9IChhd2FpdCByZXMuanNvbigpKSBhcyB7XG4gICAgY29tbWFuZHM/OiB7IGlkPzogc3RyaW5nOyBjb21tYW5kSWQ/OiBzdHJpbmc7IHN0YXRlOiBzdHJpbmc7IG9wZXJhdGlvbjogc3RyaW5nIH1bXTtcbiAgfTtcbiAgcmV0dXJuIChib2R5LmNvbW1hbmRzID8/IFtdKS5tYXAoKGMpID0+ICh7IC4uLmMsIGlkOiBjLmlkID8/IGMuY29tbWFuZElkID8/IFwiXCIgfSkpO1xufVxuXG4vKiogSm91cm5hbCB1c2VyIG1lc3NhZ2VzIGZvciBvbmUgY29tbWFuZElkIHZpYSB0aGUgaW5kZXBlbmRlbnQgY2xpZW50ICgyMDAgcmVxdWlyZWQpLiAqL1xuYXN5bmMgZnVuY3Rpb24gaHViSm91cm5hbE1lc3NhZ2VDb3VudChhcGk6IEFQSVJlcXVlc3RDb250ZXh0LCBpbnN0YW5jZUlkOiBzdHJpbmcsIGNvbW1hbmRJZDogc3RyaW5nKSB7XG4gIGNvbnN0IHJlcyA9IGF3YWl0IGFwaS5nZXQoYC92MS9pbnN0YW5jZXMvJHtpbnN0YW5jZUlkfS9qb3VybmFsP2xpbWl0PTIwMDBgKTtcbiAgZXhwZWN0KHJlcy5zdGF0dXMoKSwgYEdFVCBqb3VybmFsIEhUVFAgJHtyZXMuc3RhdHVzKCl9YCkudG9CZSgyMDApO1xuICBjb25zdCBib2R5ID0gKGF3YWl0IHJlcy5qc29uKCkpIGFzIHtcbiAgICBldmVudHM/OiB7XG4gICAgICBraW5kPzogc3RyaW5nO1xuICAgICAgZXZlbnQ/OiB7IGtpbmQ/OiBzdHJpbmc7IHBheWxvYWQ/OiB7IGNvbW1hbmRJZD86IHN0cmluZyB9IH07XG4gICAgICBwYXlsb2FkPzogeyBjb21tYW5kSWQ/OiBzdHJpbmcgfTtcbiAgICB9W107XG4gIH07XG4gIHJldHVybiAoYm9keS5ldmVudHMgPz8gW10pLmZpbHRlcigocmF3KSA9PiB7XG4gICAgY29uc3QgZSA9IChyYXcuZXZlbnQgPz8gcmF3KSBhcyB7IGtpbmQ/OiBzdHJpbmc7IHBheWxvYWQ/OiB7IGNvbW1hbmRJZD86IHN0cmluZyB9IH07XG4gICAgcmV0dXJuIGUua2luZCA9PT0gXCJtZXNzYWdlXCIgJiYgZS5wYXlsb2FkPy5jb21tYW5kSWQgPT09IGNvbW1hbmRJZDtcbiAgfSkubGVuZ3RoO1xufVxuXG5hc3luYyBmdW5jdGlvbiBzZW5kTWVzc2FnZShwYWdlOiBQYWdlLCB0ZXh0OiBzdHJpbmcpIHtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1pbnB1dFwiKSkudG9CZUVuYWJsZWQoeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1pbnB1dFwiKS5maWxsKHRleHQpO1xuICBjb25zdCBzZW5kID0gcGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLXNlbmRcIik7XG4gIGNvbnN0IHF1ZXVlID0gcGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLXF1ZXVlXCIpO1xuICBpZiAoYXdhaXQgcXVldWUuaXNWaXNpYmxlKCkuY2F0Y2goKCkgPT4gZmFsc2UpKSBhd2FpdCBxdWV1ZS5jbGljaygpO1xuICBlbHNlIGF3YWl0IHNlbmQuY2xpY2soKTtcbn1cblxuY29uc3QgVjEgPSAvXFwvdjFcXC8vO1xuXG4vKipcbiAqIE1ha2UgdGhlIEh1YiBmdWxseSB1bnJlYWNoYWJsZSB0byB0aGUgYXBwLiBOZXR3b3JrIGVtdWxhdGlvbiBraWxscyB0aGVcbiAqIEFMUkVBRFktT1BFTiBmb2xsb3cgc29ja2V0IGltbWVkaWF0ZWx5IChhIHJvdXRlIG9ubHkgaW50ZXJjZXB0cyBmdXR1cmVcbiAqIHJlcXVlc3RzLCBpbmNsdWRpbmcgV1MgdXBncmFkZXMpOyB0aGUgL3YxIHJvdXRlIHRoZW4ga2VlcHMgYWJvcnRpbmcgSHViXG4gKiBSRVNUIGNhbGxzIGFuZCBhbnkgTkVXIGZvbGxvdyB1cGdyYWRlICh0aGUgV2ViU29ja2V0IGhhbmRzaGFrZSBpcyBhbiBIVFRQXG4gKiByZXF1ZXN0LCBzbyBhbiBhYm9ydGVkIHVwZ3JhZGUgY2xvc2VzIHRoZSBwYWdlJ3Mgc29ja2V0KS4gV2l0aCBib3RoIGluXG4gKiBwbGFjZSwgbmV0d29yayBlbXVsYXRpb24gY2FuIGxhdGVyIGJlIGxpZnRlZCBmb3IgYW4gb2ZmbGluZSBSRUxPQUQgKHRoZSBkZXZcbiAqIG9yaWdpbiBtdXN0IHN0aWxsIHNlcnZlIHRoZSBkb2N1bWVudCkgd2hpbGUgdGhlIEh1YiBzdGF5cyB1bnJlYWNoYWJsZS5cbiAqL1xuYXN5bmMgZnVuY3Rpb24gYmxvY2tIdWIoY29udGV4dDogaW1wb3J0KCdAcGxheXdyaWdodC90ZXN0JykuQnJvd3NlckNvbnRleHQpIHtcbiAgYXdhaXQgY29udGV4dC5yb3V0ZShWMSwgKHJvdXRlKSA9PiByb3V0ZS5hYm9ydChcImZhaWxlZFwiKSk7XG4gIGF3YWl0IGNvbnRleHQuc2V0T2ZmbGluZSh0cnVlKTtcbn1cblxuLyoqXG4gKiBMaWZ0IG5ldHdvcmsgZW11bGF0aW9uIHdoaWxlIEtFRVBJTkcgdGhlIEh1YiByb3V0ZSBhY3RpdmU6IHVzZSB0aGlzIGJlZm9yZVxuICogcmVsb2FkaW5nIG9mZmxpbmUgc28gdGhlIFZpdGUgZG9jdW1lbnQgcmVsb2FkcyBidXQgSHViIFJFU1QvZm9sbG93IHN0YXlcbiAqIGRvd24gZXhhY3RseSBhcyBhIEh1Yi1vbmx5IG91dGFnZSBsb29rcyB0byB0aGUgcmVzdG9yZWQgcGFnZS5cbiAqL1xuYXN5bmMgZnVuY3Rpb24ga2VlcEh1YkJsb2NrZWRCeVJvdXRlcyhjb250ZXh0OiBpbXBvcnQoJ0BwbGF5d3JpZ2h0L3Rlc3QnKS5Ccm93c2VyQ29udGV4dCkge1xuICBhd2FpdCBjb250ZXh0LnNldE9mZmxpbmUoZmFsc2UpO1xufVxuXG5hc3luYyBmdW5jdGlvbiB1bmJsb2NrSHViKGNvbnRleHQ6IGltcG9ydCgnQHBsYXl3cmlnaHQvdGVzdCcpLkJyb3dzZXJDb250ZXh0KSB7XG4gIGF3YWl0IGNvbnRleHQuc2V0T2ZmbGluZShmYWxzZSk7XG4gIGF3YWl0IGNvbnRleHQudW5yb3V0ZShWMSk7XG59XG5cbnRlc3QuYmVmb3JlRWFjaChhc3luYyAoeyBwYWdlIH0pID0+IHtcbiAgYXdhaXQgbG9naW4ocGFnZSk7XG59KTtcblxuLyoqIExpZmVjeWNsZXMgdGhhdCBubyBsb25nZXIgaG9sZCBhIGhvc3QgcGxhY2VtZW50IHNsb3QuICovXG5jb25zdCBURVJNSU5BTF9MSUZFQ1lDTEVTID0gbmV3IFNldChbXCJleGl0ZWRcIiwgXCJmYWlsZWRcIl0pO1xuXG4vKipcbiAqIERlbGV0ZSBldmVyeSBpbnN0YW5jZSB0aGlzIHNwZWMgY3JlYXRlZCwgdGhyb3VnaCBhIEZSRVNIIGF1dGhlbnRpY2F0ZWRcbiAqIGJyb3dzZXIgY29udGV4dDogdGhlIGRyaXZpbmcgY29udGV4dHMgbWF5IHN0aWxsIGJlIGVtdWxhdGVkLW9mZmxpbmUgb3IgaGF2ZVxuICogSHViIHJvdXRlcyBpbnN0YWxsZWQgKHRoZSBsb3N0LXJlc3BvbnNlIHRlc3QgbGVhdmVzIGl0cyBjb21tYW5kIHJvdXRlcyBpblxuICogcGxhY2UpLCBhbmQgUGxheXdyaWdodCdzIHN0YW5kYWxvbmUgYHJlcXVlc3RgIGZpeHR1cmUgY2FycmllcyBOTyBkZXZpY2VcbiAqIGNvb2tpZSAoYXV0aCBpcyB0aGUgaHR0cE9ubHkgcmVtdWRhX2RldmljZSBjb29raWUpLCBzbyBhIGRlbGV0ZSBpc3N1ZWQgZnJvbVxuICogaXQgNDAxcyBhbmQg4oCUIHdoZW4gc3dhbGxvd2VkIOKAlCBsZWFrcyBldmVyeSBzZXNzaW9uIG9udG8gdGhlIHNoYXJlZCBnYXRlXG4gKiBodWIgdW50aWwgaXRzIDggbGl2ZS1pbnN0YW5jZSBjYXAgbWFrZXMgbGF0ZXIgc3BlY3MnIGNyZWF0ZXMgNDIyLlxuICpcbiAqIEV2ZXJ5IERFTEVURSBpcyByZXNwb25zZS1jaGVja2VkICgyeHgvNDA0IG9ubHkpLCBhbmQgYSBmaW5hbCBHRVRcbiAqIC92MS9pbnN0YW5jZXMgbXVzdCBzaG93IG5vbmUgb2YgdGhlIGNyZWF0ZWQgaWRzIHN0aWxsIGluIGEgbGl2ZSBsaWZlY3ljbGUuXG4gKi9cbmFzeW5jIGZ1bmN0aW9uIGRlbGV0ZUNyZWF0ZWRJbnN0YW5jZXMoYnJvd3NlcjogQnJvd3Nlcikge1xuICAvLyBDb3B5LCBkbyBOT1Qgc3BsaWNlIHVwIGZyb250OiBpZHMgbGVhdmUgdGhlIHNoYXJlZCBsaXN0IG9ubHkgQUZURVIgdGhlXG4gIC8vIHJlc3BvbnNlIGNoZWNrcyBhbmQgdGhlIGxpdmUtc2xvdCB2ZXJpZmljYXRpb24gcGFzcywgc28gYSBmYWlsZWQgY2xlYW51cFxuICAvLyBsZWF2ZXMgdGhlbSBmb3IgdGhlIG90aGVyIGhvb2sgdG8gcmV0cnkuXG4gIGNvbnN0IGlkcyA9IFsuLi5jcmVhdGVkXTtcbiAgaWYgKCFpZHMubGVuZ3RoKSByZXR1cm47XG4gIGNvbnN0IGNsZWFudXAgPSBhd2FpdCBicm93c2VyLm5ld1BhZ2UoKTtcbiAgdHJ5IHtcbiAgICBhd2FpdCBsb2dpbihjbGVhbnVwLCBcImUyZS1vZmZsaW5lLW91dGJveFwiKTtcbiAgICBmb3IgKGNvbnN0IGlkIG9mIGlkcykge1xuICAgICAgY29uc3QgcmVzID0gYXdhaXQgY2xlYW51cC5yZXF1ZXN0LmRlbGV0ZShgL3YxL2luc3RhbmNlcy8ke2lkfT9mb3JjZT0xYCk7XG4gICAgICBleHBlY3QoWzIwMCwgMjAyLCAyMDQsIDQwNF0sIGBERUxFVEUgaW5zdGFuY2UgJHtpZH0gLT4gSFRUUCAke3Jlcy5zdGF0dXMoKX1gKS50b0NvbnRhaW4oXG4gICAgICAgIHJlcy5zdGF0dXMoKSxcbiAgICAgICk7XG4gICAgfVxuICAgIC8vIFRoZSBmYWtlIG5vZGUgc2V0dGxlcyB0aGUgc3RvcCBhc3luY2hyb25vdXNseTogcG9sbCB1bnRpbCBub25lIG9mIG91clxuICAgIC8vIGlkcyBzdGlsbCBob2xkcyBhIHBsYWNlbWVudCBzbG90IChhYnNlbnQgcm93cyBwYXNzIOKAlCBERUxFVEUgd29uKS5cbiAgICBhd2FpdCBleHBlY3RcbiAgICAgIC5wb2xsKFxuICAgICAgICBhc3luYyAoKSA9PiB7XG4gICAgICAgICAgY29uc3QgcmVzID0gYXdhaXQgY2xlYW51cC5yZXF1ZXN0LmdldChcIi92MS9pbnN0YW5jZXNcIik7XG4gICAgICAgICAgZXhwZWN0KHJlcy5zdGF0dXMoKSwgYEdFVCBpbnN0YW5jZXMgLT4gSFRUUCAke3Jlcy5zdGF0dXMoKX1gKS50b0JlKDIwMCk7XG4gICAgICAgICAgY29uc3QgYm9keSA9IChhd2FpdCByZXMuanNvbigpKSBhcyB7XG4gICAgICAgICAgICBpdGVtcz86IHsgaW5zdGFuY2VJZD86IHN0cmluZzsgbGlmZWN5Y2xlPzogc3RyaW5nIH1bXTtcbiAgICAgICAgICB9O1xuICAgICAgICAgIHJldHVybiAoYm9keS5pdGVtcyA/PyBbXSlcbiAgICAgICAgICAgIC5maWx0ZXIoKGl0KSA9PiBpZHMuaW5jbHVkZXMoaXQuaW5zdGFuY2VJZCA/PyBcIlwiKSlcbiAgICAgICAgICAgIC5maWx0ZXIoKGl0KSA9PiAhVEVSTUlOQUxfTElGRUNZQ0xFUy5oYXMoaXQubGlmZWN5Y2xlID8/IFwiXCIpKVxuICAgICAgICAgICAgLm1hcCgoaXQpID0+IGl0Lmluc3RhbmNlSWQpO1xuICAgICAgICB9LFxuICAgICAgICB7IHRpbWVvdXQ6IDMwXzAwMCB9LFxuICAgICAgKVxuICAgICAgLnRvRXF1YWwoW10pO1xuICAgIGNyZWF0ZWQuc3BsaWNlKDAsIGNyZWF0ZWQubGVuZ3RoLCAuLi5jcmVhdGVkLmZpbHRlcigoaWQpID0+ICFpZHMuaW5jbHVkZXMoaWQpKSk7XG4gIH0gZmluYWxseSB7XG4gICAgYXdhaXQgY2xlYW51cC5jbG9zZSgpO1xuICB9XG59XG5cbi8vIENsZWFuIHVwIGFmdGVyIEVBQ0ggdGVzdCBzbyB0aGUgdGhyZWUgaW5zdGFuY2VzIG5ldmVyIHBpbGUgdXAgd2l0aGluIHRoZVxuLy8gcnVuLCBhbmQgYWdhaW4gaW4gYWZ0ZXJBbGwgYXMgYSBzYWZldHkgbmV0IHdoZW4gYW4gYWZ0ZXJFYWNoIGNvdWxkIG5vdCBydW5cbi8vIGl0cyBvd24gY2xlYW51cCAoaXQgb25seSBldmVyIHNlZXMgaWRzIGxlZnQgYmVoaW5kKS5cbnRlc3QuYWZ0ZXJFYWNoKGFzeW5jICh7IGJyb3dzZXIgfSkgPT4ge1xuICBhd2FpdCBkZWxldGVDcmVhdGVkSW5zdGFuY2VzKGJyb3dzZXIpO1xufSk7XG5cbnRlc3QuYWZ0ZXJBbGwoYXN5bmMgKHsgYnJvd3NlciB9KSA9PiB7XG4gIGF3YWl0IGRlbGV0ZUNyZWF0ZWRJbnN0YW5jZXMoYnJvd3Nlcik7XG59KTtcblxuLyoqXG4gKiBXYWl0IGZvciB0aGUgb3B0aW1pc3RpYyBidWJibGUgdG8gYmUgcmVwbGFjZWQgYnkgdGhlIGF1dGhvcml0YXRpdmUgam91cm5hbFxuICogdHJhbnNjcmlwdCByb3cgY2FycnlpbmcgdGhlIFNBTUUgY29tbWFuZElkICh3aGF0IFwiZGVsaXZlcmVkXCIgcmVuZGVycyBhcyBvbmNlXG4gKiB0aGUgam91cm5hbCBqb2lucyksIGFuZCBhc3NlcnQgbm8gd2FpdGluZy91bmNvbmZpcm1lZCBjaGlwIGlzIGxlZnQgYmVoaW5kLlxuICovXG5hc3luYyBmdW5jdGlvbiBleHBlY3REZWxpdmVyZWQocGFnZTogUGFnZSwgY29tbWFuZElkOiBzdHJpbmcgfCBudWxsKSB7XG4gIGNvbnN0IGF1dGhvcml0YXRpdmUgPSBwYWdlLmxvY2F0b3IoXG4gICAgYFtkYXRhLXRlc3RpZD1cInRyYW5zY3JpcHQtcm93XCJdW2RhdGEtcm9sZT1cInVzZXJcIl1bZGF0YS1jb21tYW5kLWlkPVwiJHtjb21tYW5kSWR9XCJdYCxcbiAgKTtcbiAgYXdhaXQgZXhwZWN0KGF1dGhvcml0YXRpdmUpLnRvSGF2ZUNvdW50KDEsIHsgdGltZW91dDogMzBfMDAwIH0pO1xuICBjb25zdCBidWJibGUgPSBwYWdlLmxvY2F0b3IoXG4gICAgYFtkYXRhLXRlc3RpZD1cIm9wdGltaXN0aWMtYnViYmxlXCJdW2RhdGEtY29tbWFuZC1pZD1cIiR7Y29tbWFuZElkfVwiXWAsXG4gICk7XG4gIC8vIFRoZSBvcHRpbWlzdGljIGNoaXAgZm9yIFRISVMgcm93IGlzIGdvbmUgKHRoZSBhdXRob3JpdGF0aXZlIHJvdyBjYXJyaWVzXG4gIC8vIG5vIHdhaXRpbmcvdW5jb25maXJtZWQgbGFiZWw7IG90aGVyIHNjcmlwdGVkIGNvbW1hbmRzIG9uIHRoZSBwYWdlIG1heVxuICAvLyBpbmRlcGVuZGVudGx5IHNob3cgdGhlaXIgb3duIHN0YXR1cyBhbmQgYXJlIG5vdCBhc3NlcnRlZCBoZXJlKS5cbiAgYXdhaXQgZXhwZWN0KGJ1YmJsZSkudG9IYXZlQ291bnQoMCk7XG59XG5cbnRlc3QoXCJvZmZsaW5lIHNlbmRzIGFyZSBxdWV1ZWQgYW5kIGRlbGl2ZXJlZCBleGFjdGx5IG9uY2UgYWZ0ZXIgcmVjb25uZWN0XCIsIGFzeW5jICh7IHBhZ2UgfSkgPT4ge1xuICBjb25zdCBpbnN0YW5jZUlkID0gYXdhaXQgY3JlYXRlU2Vzc2lvbihwYWdlLCBcIm9mZmxpbmUgb3V0Ym94IHNlZWRcIik7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikpLnRvQmVFbmFibGVkKHsgdGltZW91dDogMjBfMDAwIH0pO1xuICBjb25zdCBhcGkgPSBhd2FpdCBodWJBcGkocGFnZSk7XG4gIGF3YWl0IHBhZ2Uud2FpdEZvclRpbWVvdXQoNTAwKTtcblxuICAvLyBCbG9jayBldmVyeXRoaW5nIEh1Yi1ib3VuZCAoUkVTVCBhbmQgdGhlIGZvbGxvdyBzb2NrZXQpLlxuICBhd2FpdCBibG9ja0h1YihwYWdlLmNvbnRleHQoKSk7XG4gIGNvbnN0IGJhbm5lciA9IHBhZ2UuZ2V0QnlUZXN0SWQoXCJqb3VybmFsLWJhbm5lclwiKTtcbiAgYXdhaXQgZXhwZWN0KGJhbm5lcikudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1zdGF0ZVwiLCBcIm9mZmxpbmVcIik7XG4gIGV4cGVjdChhd2FpdCBiYW5uZXIudGV4dENvbnRlbnQoKSkudG9Db250YWluKFwi56a757q/XCIpO1xuXG4gIGF3YWl0IHNlbmRNZXNzYWdlKHBhZ2UsIFwib2ZmbGluZSBvbmVcIik7XG4gIGF3YWl0IHNlbmRNZXNzYWdlKHBhZ2UsIFwib2ZmbGluZSB0d29cIik7XG5cbiAgY29uc3QgcGVuZGluZyA9IHBhZ2UubG9jYXRvcignW2RhdGEtdGVzdGlkPVwib3B0aW1pc3RpYy1idWJibGVcIl0nKTtcbiAgYXdhaXQgZXhwZWN0KHBlbmRpbmcpLnRvSGF2ZUNvdW50KDIpO1xuICAvLyBSb3cgbGFiZWxzIHdoaWxlIG9mZmxpbmUuXG4gIGF3YWl0IGV4cGVjdChwZW5kaW5nLmZpcnN0KCkpLnRvQ29udGFpblRleHQoXCLlvoXlj5HpgIHvvIjnprvnur/vvIlcIik7XG5cbiAgY29uc3QgY29tbWFuZElkcyA9IGF3YWl0IHBlbmRpbmcuZXZhbHVhdGVBbGwoKG5vZGVzKSA9PlxuICAgIG5vZGVzLm1hcCgobikgPT4gbi5nZXRBdHRyaWJ1dGUoXCJkYXRhLWNvbW1hbmQtaWRcIikpLFxuICApO1xuICBleHBlY3QoY29tbWFuZElkcykudG9IYXZlTGVuZ3RoKDIpO1xuICBleHBlY3QoY29tbWFuZElkcy5ldmVyeSgoaWQpID0+IGlkPy5zdGFydHNXaXRoKFwiY21kX1wiKSkpLnRvQmUodHJ1ZSk7XG5cbiAgLy8gVGhlIEh1YiBoYXMgbm90aGluZyB5ZXQgKHJlYWQgaW5kZXBlbmRlbnRseSwgcmVxdWlyZSAyMDApLlxuICBjb25zdCBzZWVkU2VuZHMgPSAoYXdhaXQgaHViQ29tbWFuZHMoYXBpLCBpbnN0YW5jZUlkKSkuZmlsdGVyKFxuICAgIChjKSA9PiBjLm9wZXJhdGlvbiA9PT0gXCJpbnN0YW5jZS5zZW5kXCIsXG4gICkubGVuZ3RoO1xuICBleHBlY3Qoc2VlZFNlbmRzKS50b0JlKDApO1xuXG4gIGF3YWl0IHVuYmxvY2tIdWIocGFnZS5jb250ZXh0KCkpO1xuICBhd2FpdCBleHBlY3QoYmFubmVyKS50b0hhdmVDb3VudCgwLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcblxuICAvLyBFeGFjdGx5IG9uZSBIdWIgY29tbWFuZCByb3cgcGVyIGlkIOKAplxuICBhd2FpdCBleHBlY3RcbiAgICAucG9sbChhc3luYyAoKSA9PiAoYXdhaXQgaHViQ29tbWFuZHMoYXBpLCBpbnN0YW5jZUlkKSkuZmlsdGVyKChjKSA9PiBjLm9wZXJhdGlvbiA9PT0gXCJpbnN0YW5jZS5zZW5kXCIpLmxlbmd0aClcbiAgICAudG9CZSgyKTtcbiAgLy8g4oCmIGFuZCBleGFjdGx5IG9uZSBqb3VybmFsIGV4ZWN1dGlvbiBlYWNoLlxuICBmb3IgKGNvbnN0IGNpZCBvZiBjb21tYW5kSWRzKSB7XG4gICAgYXdhaXQgZXhwZWN0LnBvbGwoKCkgPT4gaHViSm91cm5hbE1lc3NhZ2VDb3VudChhcGksIGluc3RhbmNlSWQsIGNpZCEpKS50b0JlKDEpO1xuICAgIC8vIFRoZSBkZWxpdmVyZWQgcm93IGJlY29tZXMgdGhlIGF1dGhvcml0YXRpdmUgam91cm5hbCBtZXNzYWdlIChzYW1lIGlkKTtcbiAgICAvLyB0aGUgb3B0aW1pc3RpYyBjaGlwIGlzIGdvbmUgYW5kIG5vIOeKtuaAgeW+heehruiupCBpcyBzaG93bi5cbiAgICBhd2FpdCBleHBlY3REZWxpdmVyZWQocGFnZSwgY2lkKTtcbiAgfVxufSk7XG5cbnRlc3QoXCJhbiBvZmZsaW5lLXF1ZXVlZCBtZXNzYWdlIHN1cnZpdmVzIGEgcmVsb2FkIHdoaWxlIHRoZSBIdWIgaXMgc3RpbGwgb2ZmIGFuZCBzZW5kcyBvbmNlIGFmdGVyXCIsIGFzeW5jICh7IHBhZ2UgfSkgPT4ge1xuICBjb25zdCBpbnN0YW5jZUlkID0gYXdhaXQgY3JlYXRlU2Vzc2lvbihwYWdlLCBcIm9mZmxpbmUgcmVsb2FkIHNlZWRcIik7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikpLnRvQmVFbmFibGVkKHsgdGltZW91dDogMjBfMDAwIH0pO1xuICBjb25zdCBhcGkgPSBhd2FpdCBodWJBcGkocGFnZSk7XG5cbiAgLy8gVGhlIEh1YiBnb2VzIGZ1bGx5IHVucmVhY2hhYmxlIChSRVNUIGFuZCBmb2xsb3cgc29ja2V0IGJvdGggZG93bikuXG4gIGF3YWl0IGJsb2NrSHViKHBhZ2UuY29udGV4dCgpKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJqb3VybmFsLWJhbm5lclwiKSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1zdGF0ZVwiLCBcIm9mZmxpbmVcIik7XG4gIGF3YWl0IHNlbmRNZXNzYWdlKHBhZ2UsIFwib2ZmbGluZSBhY3Jvc3MgcmVsb2FkXCIpO1xuICBjb25zdCBxdWV1ZWQgPSBwYWdlLmxvY2F0b3IoJ1tkYXRhLXRlc3RpZD1cIm9wdGltaXN0aWMtYnViYmxlXCJdJyk7XG4gIGF3YWl0IGV4cGVjdChxdWV1ZWQpLnRvSGF2ZUNvdW50KDEpO1xuICBjb25zdCBjb21tYW5kSWQgPSBhd2FpdCBxdWV1ZWQuZ2V0QXR0cmlidXRlKFwiZGF0YS1jb21tYW5kLWlkXCIpO1xuICBleHBlY3QoY29tbWFuZElkKS50b0JlVHJ1dGh5KCk7XG5cbiAgLy8gUmVsb2FkIFdJVEggdGhlIEh1YiBzdGlsbCB1bnJlYWNoYWJsZTogbGlmdCBuZXR3b3JrIGVtdWxhdGlvbiAoc28gdGhlIGRldlxuICAvLyBkb2N1bWVudCBsb2Fkcykgd2hpbGUgdGhlIHJvdXRlcyBrZWVwIGV2ZXJ5IEh1YiBjYWxsIGFuZCBmb2xsb3cgdXBncmFkZVxuICAvLyBmYWlsaW5nIOKAlCBhIHRydWUgb2ZmbGluZSByZWxvYWQgb2YgdGhlIGFwcCwgbm90IGEgZnJvemVuIHBhZ2UuXG4gIGF3YWl0IGtlZXBIdWJCbG9ja2VkQnlSb3V0ZXMocGFnZS5jb250ZXh0KCkpO1xuICBhd2FpdCBwYWdlLnJlbG9hZCh7IHdhaXRVbnRpbDogXCJkb21jb250ZW50bG9hZGVkXCIgfSk7XG4gIC8vIFRoZSBzZXNzaW9uIHJlbmRlcnMgZnJvbSB0aGUgcGVyc2lzdGVkIGluc3RhbmNlIHByb2plY3Rpb24gKG5vXG4gIC8vIOS8muivneS4jeWtmOWcqCAvIGNhc3Qgc3R1YiksIGFuZCB0aGUgZHVyYWJsZSByb3cgcmVzdG9yZXMgYW5kIHN0aWxsIHdhaXRzXG4gIC8vIG9mZmxpbmU7IG5vdGhpbmcgaGFzIGJlZW4gUE9TVGVkLlxuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcInNlc3Npb24tcGFnZVwiKSkudG9CZVZpc2libGUoeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGNvbnN0IHJlc3RvcmVkID0gcGFnZS5sb2NhdG9yKGBbZGF0YS10ZXN0aWQ9XCJvcHRpbWlzdGljLWJ1YmJsZVwiXVtkYXRhLWNvbW1hbmQtaWQ9XCIke2NvbW1hbmRJZH1cIl1gKTtcbiAgLy8gVGhlIGR1cmFibGUgcm93IHJlc3RvcmVzIGFuZCByZW5kZXJzIHdpdGggdGhlIGNvbXBvc2VyIGF2YWlsYWJsZSAodGhlXG4gIC8vIG9mZmxpbmUgc2VlZCBuZXZlciBsb2FkZWQgZXZlbnRzLCBidXQgcmVzdG9yZWQgYnViYmxlcyB1bmJsb2NrIHRoZVxuICAvLyB0cmFuc2NyaXB0IOKAlCBubyBcIuS8muivneS4jeWtmOWcqFwiLCBubyBzdHVjayBcIuWKoOi9vSBzbmFwc2hvdOKAplwiKS5cbiAgYXdhaXQgZXhwZWN0KHJlc3RvcmVkKS50b0JlVmlzaWJsZSgpO1xuICBleHBlY3QocmVzdG9yZWQpLnRvQ29udGFpblRleHQoXCJvZmZsaW5lIGFjcm9zcyByZWxvYWRcIik7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikpLnRvQmVFbmFibGVkKHsgdGltZW91dDogMjBfMDAwIH0pO1xuICAvLyBJdCBpcyBub3QgYSB0ZXJtaW5hbC9yZWplY3RlZCByb3cgd2hpbGUgdGhlIEh1YiBoYXMgbmV2ZXIgcmVjZWl2ZWQgaXQuXG4gIC8vIFNjb3BlIHRvIHRoZSBidWJibGUncyBvd24gc3VidHJlZSAodGhlIHNlc3Npb24gcGFnZSBjYW4gaW5kZXBlbmRlbnRseVxuICAvLyBzaG93IOeKtuaAgeW+heehruiupCBmb3IgdGhlIHNjcmlwdGVkIGNyZWF0ZSBjb21tYW5kKS5cbiAgYXdhaXQgZXhwZWN0KHJlc3RvcmVkKS5ub3QudG9Db250YWluVGV4dChcIuacqumAgei+vlwiKTtcbiAgLy8gQW5kIG5vdGhpbmcgcmVhY2hlZCB0aGUgSHViIHlldCAoaW5kZXBlbmRlbnQgcmVhZCwgMjAwIHJlcXVpcmVkKS5cbiAgZXhwZWN0KChhd2FpdCBodWJDb21tYW5kcyhhcGksIGluc3RhbmNlSWQpKS5maWx0ZXIoKGMpID0+IGMub3BlcmF0aW9uID09PSBcImluc3RhbmNlLnNlbmRcIikpLnRvSGF2ZUxlbmd0aCgwKTtcblxuICAvLyBSZWNvbm5lY3Q6IHRoZSByZXN0b3JlZCByb3cgZGVsaXZlcnMgZXhhY3RseSBvbmNlIHVuZGVyIHRoZSBzYW1lIGlkLlxuICBhd2FpdCB1bmJsb2NrSHViKHBhZ2UuY29udGV4dCgpKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJqb3VybmFsLWJhbm5lclwiKSkudG9IYXZlQ291bnQoMCwgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKCgpID0+IGh1YkpvdXJuYWxNZXNzYWdlQ291bnQoYXBpLCBpbnN0YW5jZUlkLCBjb21tYW5kSWQhKSwgeyB0aW1lb3V0OiAzMF8wMDAgfSlcbiAgICAudG9CZSgxKTtcbiAgYXdhaXQgZXhwZWN0XG4gICAgLnBvbGwoXG4gICAgICBhc3luYyAoKSA9PlxuICAgICAgICAoYXdhaXQgaHViQ29tbWFuZHMoYXBpLCBpbnN0YW5jZUlkKSkuZmlsdGVyKFxuICAgICAgICAgIChjKSA9PiBjLm9wZXJhdGlvbiA9PT0gXCJpbnN0YW5jZS5zZW5kXCIgJiYgYy5pZCA9PT0gY29tbWFuZElkLFxuICAgICAgICApLmxlbmd0aCxcbiAgICAgIHsgdGltZW91dDogMzBfMDAwIH0sXG4gICAgKVxuICAgIC50b0JlKDEpO1xuICBhd2FpdCBleHBlY3REZWxpdmVyZWQocGFnZSwgY29tbWFuZElkKTtcbn0pO1xuXG50ZXN0KFwiYSBjb21taXR0ZWQgUE9TVCB3aG9zZSBicm93c2VyIHJlc3BvbnNlIGlzIGxvc3QgcmV0cmllcyB3aXRoIHJlcGxheWVkOnRydWUgYW5kIHJ1bnMgb25jZVwiLCBhc3luYyAoeyBwYWdlIH0pID0+IHtcbiAgY29uc3QgaW5zdGFuY2VJZCA9IGF3YWl0IGNyZWF0ZVNlc3Npb24ocGFnZSwgXCJsb3N0IHJlc3BvbnNlIHNlZWRcIik7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikpLnRvQmVFbmFibGVkKHsgdGltZW91dDogMjBfMDAwIH0pO1xuICBjb25zdCBhcGkgPSBhd2FpdCBodWJBcGkocGFnZSk7XG5cbiAgY29uc3QgY29tbWFuZHNQYXR0ZXJuID0gL1xcL3YxXFwvaW5zdGFuY2VzXFwvW14vXStcXC9jb21tYW5kcyQvO1xuICBjb25zdCBzdGF0dXNQYXR0ZXJuID0gL1xcL3YxXFwvaW5zdGFuY2VzXFwvW14vXStcXC9jb21tYW5kc1xcL2NtZF8vO1xuICBjb25zdCByZXBsYXlSZXN1bHRzOiBib29sZWFuW10gPSBbXTtcbiAgbGV0IGZpcnN0UG9zdExvc3QgPSBmYWxzZTtcbiAgLy8gR2F0ZSB0aGUgcmV0cnkgc28gdGhlIHJvdydzIGludGVybWVkaWF0ZSBzdGF0ZSBpcyBvYnNlcnZhYmxlIGluc3RlYWQgb2ZcbiAgLy8gdGhlIGxvY2FsIGhhcm5lc3MgY29tcGxldGluZyB0aGUgcmUtUE9TVCB3aXRoaW4gbWlsbGlzZWNvbmRzLlxuICBsZXQgcmVsZWFzZVJldHJ5OiAoKCkgPT4gdm9pZCkgfCBudWxsID0gbnVsbDtcbiAgY29uc3QgcmV0cnlHYXRlID0gbmV3IFByb21pc2U8dm9pZD4oKHJlc29sdmUpID0+IHtcbiAgICByZWxlYXNlUmV0cnkgPSByZXNvbHZlO1xuICB9KTtcblxuICBhd2FpdCBwYWdlLmNvbnRleHQoKS5yb3V0ZShjb21tYW5kc1BhdHRlcm4sIGFzeW5jIChyb3V0ZSkgPT4ge1xuICAgIGlmIChyb3V0ZS5yZXF1ZXN0KCkubWV0aG9kKCkgIT09IFwiUE9TVFwiKSByZXR1cm4gcm91dGUuY29udGludWUoKTtcbiAgICBpZiAoIWZpcnN0UG9zdExvc3QpIHtcbiAgICAgIGZpcnN0UG9zdExvc3QgPSB0cnVlO1xuICAgICAgLy8gTGV0IHRoZSBIdWIgQ09NTUlUIHRoZSBmaXJzdCBQT1NUIGZvciByZWFsIOKAplxuICAgICAgY29uc3Qgc2VydmVyID0gYXdhaXQgcm91dGUuZmV0Y2goKTtcbiAgICAgIGV4cGVjdChzZXJ2ZXIuc3RhdHVzKCkpLnRvQmUoMjAwKTtcbiAgICAgIC8vIOKApiB0aGVuIGxvc2UgdGhlIGJyb3dzZXIncyByZXNwb25zZSAodGhlIHdpcmUgZHJvcHBlZCBhZnRlciBjb21taXQpLlxuICAgICAgcmV0dXJuIHJvdXRlLmFib3J0KFwiZmFpbGVkXCIpO1xuICAgIH1cbiAgICAvLyBUaGUgc2FtZS1pZCByZXRyeSBwYXJrcyB1bnRpbCB0aGUgdGVzdCByZWxlYXNlcyBpdCwgc28gdGhlIHJvdyBpcyBoZWxkXG4gICAgLy8gaW4tZmxpZ2h0ICjlt7Llj5HpgIHvvIznrYnlvoXnoa7orqQpIOKAlCBkZWxpdmVyZWQgdG8gdGhlIEh1YiwgbmV2ZXIg54q25oCB5b6F56Gu6K6kIOKAlFxuICAgIC8vIGJlZm9yZSB0aGUgcmVwbGF5IGFuc3dlciBzZXR0bGVzIGl0LlxuICAgIGF3YWl0IHJldHJ5R2F0ZTtcbiAgICBjb25zdCByZXRyeSA9IGF3YWl0IHJvdXRlLmZldGNoKCk7XG4gICAgZXhwZWN0KHJldHJ5LnN0YXR1cygpKS50b0JlKDIwMCk7XG4gICAgY29uc3QgYm9keSA9IChhd2FpdCByZXRyeS5qc29uKCkpIGFzIHsgY29tbWFuZD86IHsgY29tbWFuZElkPzogc3RyaW5nIH07IHJlcGxheWVkPzogYm9vbGVhbiB9O1xuICAgIHJlcGxheVJlc3VsdHMucHVzaChib2R5LnJlcGxheWVkID09PSB0cnVlKTtcbiAgICByZXR1cm4gcm91dGUuZnVsZmlsbCh7IHJlc3BvbnNlOiByZXRyeSB9KTtcbiAgfSk7XG4gIC8vIFRoZSBwb3N0LWxvc3MgR0VUIHJlY29uY2lsaWF0aW9uIG11c3QgYWxzbyBmYWlsLCBzbyB0aGUgY2xpZW50IGtlZXBzIHRoZVxuICAvLyByb3cgcGVuZGluZyBhbmQgcmUtUE9TVHMgKHJhdGhlciB0aGFuIHNldHRsaW5nIG9uIHRoZSBHRVQgdmVyZGljdCkuXG4gIGF3YWl0IHBhZ2UuY29udGV4dCgpLnJvdXRlKHN0YXR1c1BhdHRlcm4sIChyb3V0ZSkgPT5cbiAgICByb3V0ZS5yZXF1ZXN0KCkubWV0aG9kKCkgPT09IFwiR0VUXCIgPyByb3V0ZS5hYm9ydChcImZhaWxlZFwiKSA6IHJvdXRlLmNvbnRpbnVlKCksXG4gICk7XG5cbiAgYXdhaXQgc2VuZE1lc3NhZ2UocGFnZSwgXCJsb3N0IHJlc3BvbnNlIG1lc3NhZ2VcIik7XG4gIGNvbnN0IGJ1YmJsZSA9IHBhZ2UubG9jYXRvcignW2RhdGEtdGVzdGlkPVwib3B0aW1pc3RpYy1idWJibGVcIl0nKS5maXJzdCgpO1xuICBhd2FpdCBleHBlY3QoYnViYmxlKS50b0JlVmlzaWJsZSgpO1xuICBjb25zdCBjb21tYW5kSWQgPSBhd2FpdCBidWJibGUuZ2V0QXR0cmlidXRlKFwiZGF0YS1jb21tYW5kLWlkXCIpO1xuICBleHBlY3QoY29tbWFuZElkKS50b0JlVHJ1dGh5KCk7XG5cbiAgLy8gV2hpbGUgdGhlIHBhcmtlZCByZXRyeSBpcyBpbiBmbGlnaHQgdGhlIHJvdyBzYXlzIOW3suWPkemAge+8jOetieW+heehruiupFxuICAvLyAoaXQgcmVhY2hlZCB0aGUgSHViKSwgbmV2ZXIg54q25oCB5b6F56Gu6K6kIGFuZCBuZXZlciBhIHRlcm1pbmFsIHJlamVjdGlvbi5cbiAgYXdhaXQgZXhwZWN0KGJ1YmJsZSkudG9Db250YWluVGV4dChcIuW3suWPkemAge+8jOetieW+heehruiupFwiLCB7IHRpbWVvdXQ6IDE1XzAwMCB9KTtcbiAgYXdhaXQgZXhwZWN0KGJ1YmJsZSkubm90LnRvQ29udGFpblRleHQoXCLnirbmgIHlvoXnoa7orqRcIik7XG5cbiAgLy8gUmVsZWFzZSB0aGUgcmV0cnk6IHRoZSBIdWIgZGVkdXBlcyBhbmQgYW5zd2VycyByZXBsYXllZDp0cnVlLlxuICByZWxlYXNlUmV0cnk/LigpO1xuICBhd2FpdCBleHBlY3QucG9sbCgoKSA9PiByZXBsYXlSZXN1bHRzLmxlbmd0aCkudG9CZUdyZWF0ZXJUaGFuKDApO1xuICBleHBlY3QocmVwbGF5UmVzdWx0c1swXSkudG9CZSh0cnVlKTtcblxuICAvLyBPbmUgY29tbWFuZCByb3cg4oCmXG4gIGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKFxuICAgICAgYXN5bmMgKCkgPT5cbiAgICAgICAgKGF3YWl0IGh1YkNvbW1hbmRzKGFwaSwgaW5zdGFuY2VJZCkpLmZpbHRlcihcbiAgICAgICAgICAoYykgPT4gYy5vcGVyYXRpb24gPT09IFwiaW5zdGFuY2Uuc2VuZFwiICYmIGMuaWQgPT09IGNvbW1hbmRJZCxcbiAgICAgICAgKS5sZW5ndGgsXG4gICAgICB7IHRpbWVvdXQ6IDMwXzAwMCB9LFxuICAgIClcbiAgICAudG9CZSgxKTtcbiAgLy8g4oCmIGFuZCBvbmUgam91cm5hbCBtZXNzYWdlIGZvciB0aGUgaWQgKGV4ZWN1dGVkIGV4YWN0bHkgb25jZSkuXG4gIGF3YWl0IGV4cGVjdC5wb2xsKCgpID0+IGh1YkpvdXJuYWxNZXNzYWdlQ291bnQoYXBpLCBpbnN0YW5jZUlkLCBjb21tYW5kSWQhKSkudG9CZSgxKTtcbiAgYXdhaXQgZXhwZWN0RGVsaXZlcmVkKHBhZ2UsIGNvbW1hbmRJZCk7XG59KTtcblxudGVzdChcImFuIG9ubGluZSBzZW5kIGxhYmVscyB0aGUgcm93IOetieW+heWPkemAgSB0aGVuIOW3suWPkemAge+8jOetieW+heehruiupC/lt7Llj5fnkIYgYXMgaXQgZGVsaXZlcnNcIiwgYXN5bmMgKHsgcGFnZSB9KSA9PiB7XG4gIGNvbnN0IGluc3RhbmNlSWQgPSBhd2FpdCBjcmVhdGVTZXNzaW9uKHBhZ2UsIFwibGFiZWwgcHJvZ3Jlc3Npb24gc2VlZFwiKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJjb21wb3Nlci1pbnB1dFwiKSkudG9CZUVuYWJsZWQoeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGNvbnN0IGFwaSA9IGF3YWl0IGh1YkFwaShwYWdlKTtcblxuICAvLyBIb2xkIHRoaXMgaW5zdGFuY2UncyBzaW5nbGUtZGVsaXZlcmVyIFdlYiBMb2NrIGZyb20gaW5zaWRlIHRoZSBwYWdlOiB3aXRoXG4gIC8vIG5vIGZsdXNoIHBvc3NpYmxlIHRoZSBxdWV1ZWQgYnViYmxlIG11c3Qgc2l0IGF0IGl0cyBob25lc3Qgb25saW5lIGxhYmVsXG4gIC8vICjnrYnlvoXlj5HpgIEg4oCUIE5PVCDlvoXlj5HpgIHvvIjnprvnur/vvIksIHRoZSBsaW5rIGlzIGxpdmUgdGhlIHdob2xlIHRpbWUpLlxuICBhd2FpdCBwYWdlLmV2YWx1YXRlKChpaWQpID0+IHtcbiAgICBjb25zdCB3ID0gd2luZG93IGFzIHVua25vd24gYXMge1xuICAgICAgX19yZWxlYXNlTG9jaz86ICgpID0+IHZvaWQ7XG4gICAgfTtcbiAgICBjb25zdCBsb2NrID0gbmV3IFByb21pc2U8dm9pZD4oKHJlc29sdmUpID0+IHtcbiAgICAgIHcuX19yZWxlYXNlTG9jayA9IHJlc29sdmU7XG4gICAgfSk7XG4gICAgdm9pZCBuYXZpZ2F0b3IubG9ja3MucmVxdWVzdChgcmVtdWRhLW91dGJveC0ke2lpZH1gLCAoKSA9PiBsb2NrKTtcbiAgfSwgaW5zdGFuY2VJZCk7XG5cbiAgLy8gT25jZSB0aGUgZmx1c2ggYWNxdWlyZXMgdGhlIGxvY2sgaXQgcmVhY2hlcyB0aGUgUE9TVDsgcGFyayB0aGF0IHNvIHRoZVxuICAvLyBpbi1mbGlnaHQgbGFiZWwgaXMgb2JzZXJ2YWJsZSB0b28uXG4gIGNvbnN0IGNvbW1hbmRzUGF0dGVybiA9IC9cXC92MVxcL2luc3RhbmNlc1xcL1teL10rXFwvY29tbWFuZHMkLztcbiAgbGV0IHJlbGVhc2VQb3N0OiAoKCkgPT4gdm9pZCkgfCBudWxsID0gbnVsbDtcbiAgY29uc3QgcG9zdEdhdGUgPSBuZXcgUHJvbWlzZTx2b2lkPigocmVzb2x2ZSkgPT4ge1xuICAgIHJlbGVhc2VQb3N0ID0gcmVzb2x2ZTtcbiAgfSk7XG4gIGF3YWl0IHBhZ2UuY29udGV4dCgpLnJvdXRlKGNvbW1hbmRzUGF0dGVybiwgYXN5bmMgKHJvdXRlKSA9PiB7XG4gICAgaWYgKHJvdXRlLnJlcXVlc3QoKS5tZXRob2QoKSAhPT0gXCJQT1NUXCIpIHJldHVybiByb3V0ZS5jb250aW51ZSgpO1xuICAgIGF3YWl0IHBvc3RHYXRlO1xuICAgIGNvbnN0IHJlcyA9IGF3YWl0IHJvdXRlLmZldGNoKCk7XG4gICAgcmV0dXJuIHJvdXRlLmZ1bGZpbGwoeyByZXNwb25zZTogcmVzIH0pO1xuICB9KTtcblxuICBhd2FpdCBzZW5kTWVzc2FnZShwYWdlLCBcIndhdGNoIHRoZSBsYWJlbHNcIik7XG4gIGNvbnN0IGJ1YmJsZSA9IHBhZ2UubG9jYXRvcignW2RhdGEtdGVzdGlkPVwib3B0aW1pc3RpYy1idWJibGVcIl0nKS5maXJzdCgpO1xuICBhd2FpdCBleHBlY3QoYnViYmxlKS50b0JlVmlzaWJsZSgpO1xuICBjb25zdCBjb21tYW5kSWQgPSBhd2FpdCBidWJibGUuZ2V0QXR0cmlidXRlKFwiZGF0YS1jb21tYW5kLWlkXCIpO1xuICBleHBlY3QoY29tbWFuZElkKS50b0JlVHJ1dGh5KCk7XG5cbiAgLy8gUXVldWVkIGJlaGluZCB0aGUgbG9jaywgbGluayBsaXZlOiDnrYnlvoXlj5HpgIEsIG5ldmVyIHRoZSBvZmZsaW5lIHdvcmRpbmcuXG4gIGF3YWl0IGV4cGVjdChidWJibGUpLnRvQ29udGFpblRleHQoXCLnrYnlvoXlj5HpgIFcIik7XG4gIGF3YWl0IGV4cGVjdChidWJibGUpLm5vdC50b0NvbnRhaW5UZXh0KFwi56a757q/XCIpO1xuXG4gIC8vIFJlbGVhc2UgdGhlIGxvY2s6IHRoZSBmbHVzaCB0YWtlcyBpdCBhbmQgdGhlIHBhcmtlZCBQT1NUIHNob3dzIHRoZSByb3dcbiAgLy8gcmVhY2hlZCB0aGUgSHViIChpbi1mbGlnaHQpLCBuZXZlciDnirbmgIHlvoXnoa7orqQuXG4gIGF3YWl0IHBhZ2UuZXZhbHVhdGUoKCkgPT4gKHdpbmRvdyBhcyB1bmtub3duIGFzIHsgX19yZWxlYXNlTG9jaz86ICgpID0+IHZvaWQgfSkuX19yZWxlYXNlTG9jaz8uKCkpO1xuICBhd2FpdCBleHBlY3QoYnViYmxlKS50b0NvbnRhaW5UZXh0KFwi5bey5Y+R6YCB77yM562J5b6F56Gu6K6kXCIsIHsgdGltZW91dDogMTVfMDAwIH0pO1xuICBhd2FpdCBleHBlY3QoYnViYmxlKS5ub3QudG9Db250YWluVGV4dChcIueKtuaAgeW+heehruiupFwiKTtcblxuICAvLyBSZWxlYXNlIHRoZSBQT1NUOiB0aGUgY29tbWFuZCBydW5zIG9uY2UgYW5kIHRoZSBqb3VybmFsIGpvaW4gcmVwbGFjZXNcbiAgLy8gdGhlIGNoaXAgd2l0aCB0aGUgYXV0aG9yaXRhdGl2ZSByb3cgKOW3suWPl+eQhiBvbiB0aGUgd2F5KS5cbiAgcmVsZWFzZVBvc3Q/LigpO1xuICBhd2FpdCBleHBlY3RcbiAgICAucG9sbCgoKSA9PiBodWJKb3VybmFsTWVzc2FnZUNvdW50KGFwaSwgaW5zdGFuY2VJZCwgY29tbWFuZElkISksIHsgdGltZW91dDogMzBfMDAwIH0pXG4gICAgLnRvQmUoMSk7XG4gIGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKFxuICAgICAgYXN5bmMgKCkgPT5cbiAgICAgICAgKGF3YWl0IGh1YkNvbW1hbmRzKGFwaSwgaW5zdGFuY2VJZCkpLmZpbHRlcihcbiAgICAgICAgICAoYykgPT4gYy5vcGVyYXRpb24gPT09IFwiaW5zdGFuY2Uuc2VuZFwiICYmIGMuaWQgPT09IGNvbW1hbmRJZCxcbiAgICAgICAgKS5sZW5ndGgsXG4gICAgICB7IHRpbWVvdXQ6IDMwXzAwMCB9LFxuICAgIClcbiAgICAudG9CZSgxKTtcbiAgYXdhaXQgZXhwZWN0RGVsaXZlcmVkKHBhZ2UsIGNvbW1hbmRJZCk7XG59KTtcblxuLyoqXG4gKiBUZXN0LW9ubHkgc2VydmljZSB3b3JrZXI6IG5ldHdvcmstZmlyc3Qgd2l0aCBhbiBvZmZsaW5lIGNhY2hlIGZhbGxiYWNrIGZvclxuICogZXZlcnkgc2FtZS1vcmlnaW4gR0VUIEVYQ0VQVCB0aGUgSHViIEFQSSAoL3YxKSwgd2hpY2ggbXVzdCBhbHdheXMgcmVhY2ggdGhlXG4gKiBuZXR3b3JrIHNvIGFuIG9mZmxpbmUgYm9vdHN0cmFwIGZhaWxzIGhvbmVzdGx5LiBSZWdpc3RlcmVkIGV4cGxpY2l0bHkgZnJvbVxuICogdGhlIHRlc3QgKHRoZSBhcHAgcmVnaXN0ZXJzIGl0cyBTVyBvbmx5IGluIFBST0QgYnVpbGRzKSBzbyBhIEZVTEwgZW11bGF0ZWRcbiAqIG9mZmxpbmUgbmF2aWdhdGlvbiDigJQgY29udGV4dC5zZXRPZmZsaW5lKHRydWUpIHN0aWxsIGluIGVmZmVjdCBhY3Jvc3MgdGhlXG4gKiByZWxvYWQg4oCUIGNhbiBzZXJ2ZSB0aGUgZGV2LXNlcnZlciBzaGVsbCBmcm9tIHRoZSBjYWNoZS5cbiAqL1xuY29uc3QgT0ZGTElORV9TSEVMTF9TVyA9IGBcbmNvbnN0IENBQ0hFID0gXCJlMmUtb2ZmbGluZS1zaGVsbC12MVwiO1xuc2VsZi5hZGRFdmVudExpc3RlbmVyKFwiaW5zdGFsbFwiLCAoKSA9PiBzZWxmLnNraXBXYWl0aW5nKCkpO1xuc2VsZi5hZGRFdmVudExpc3RlbmVyKFwiYWN0aXZhdGVcIiwgKGV2ZW50KSA9PiBldmVudC53YWl0VW50aWwoc2VsZi5jbGllbnRzLmNsYWltKCkpKTtcbnNlbGYuYWRkRXZlbnRMaXN0ZW5lcihcImZldGNoXCIsIChldmVudCkgPT4ge1xuICBjb25zdCByZXEgPSBldmVudC5yZXF1ZXN0O1xuICBpZiAocmVxLm1ldGhvZCAhPT0gXCJHRVRcIikgcmV0dXJuO1xuICBjb25zdCB1cmwgPSBuZXcgVVJMKHJlcS51cmwpO1xuICBpZiAodXJsLm9yaWdpbiAhPT0gc2VsZi5sb2NhdGlvbi5vcmlnaW4pIHJldHVybjtcbiAgaWYgKHVybC5wYXRobmFtZS5zdGFydHNXaXRoKFwiL3YxL1wiKSkgcmV0dXJuO1xuICBldmVudC5yZXNwb25kV2l0aCgoYXN5bmMgKCkgPT4ge1xuICAgIGNvbnN0IGNhY2hlID0gYXdhaXQgY2FjaGVzLm9wZW4oQ0FDSEUpO1xuICAgIHRyeSB7XG4gICAgICBjb25zdCByZXMgPSBhd2FpdCBmZXRjaChyZXEpO1xuICAgICAgaWYgKHJlcyAmJiByZXMub2sgJiYgcmVzLnR5cGUgPT09IFwiYmFzaWNcIikge1xuICAgICAgICBjYWNoZS5wdXQocmVxLCByZXMuY2xvbmUoKSkuY2F0Y2goKCkgPT4ge30pO1xuICAgICAgfVxuICAgICAgcmV0dXJuIHJlcztcbiAgICB9IGNhdGNoIChlcnIpIHtcbiAgICAgIGNvbnN0IGhpdCA9IGF3YWl0IGNhY2hlLm1hdGNoKHJlcSwgeyBpZ25vcmVTZWFyY2g6IHRydWUgfSk7XG4gICAgICBpZiAoaGl0KSByZXR1cm4gaGl0O1xuICAgICAgdGhyb3cgZXJyO1xuICAgIH1cbiAgfSkoKSk7XG59KTtcbmA7XG5cbi8vIFRoZSBmdWxsLW9mZmxpbmUtU1ctcmVzdG9yZSB0ZXN0IG5lZWRzIHRoZSBsb29wYmFjayBQTkEvTE5BIGV4ZW1wdGlvbiAoc2VlIHRoZVxuLy8gZGVzY3JpYmUgYmVsb3cgYW5kIGRvY3MvZGVzaWduL2h1Yi1yZXNpbGllbmNlLm1kIMKnNS42KS4gUGxheXdyaWdodCBvbmx5XG4vLyBhY2NlcHRzIGxhdW5jaE9wdGlvbnMgYXQgZmlsZSBzY29wZSAoYSBkZXNjcmliZS1sZXZlbCB1c2UgZm9yY2VzIGEgbmV3XG4vLyB3b3JrZXIpLCBzbyB0aGlzIGRpc2FibGVzIHRoZSBjaGVja3MgZm9yIFRISVMgU1BFQyBGSUxFIG9ubHkg4oCUIG5vdCB0aGUgaHViXG4vLyBjb25maWcgYW5kIG5vdCB0aGUgcmVzdCBvZiB0aGUgc3VpdGUuXG50ZXN0LnVzZSh7XG4gIGxhdW5jaE9wdGlvbnM6IHtcbiAgICBhcmdzOiBbXG4gICAgICBcIi0tZGlzYWJsZS1mZWF0dXJlcz1CbG9ja0luc2VjdXJlUHJpdmF0ZU5ldHdvcmtSZXF1ZXN0cyxQcml2YXRlTmV0d29ya0FjY2Vzc0NoZWNrcyxQcml2YXRlTmV0d29ya0FjY2Vzc0Zvck5hdmlnYXRpb25zLFByaXZhdGVOZXR3b3JrQWNjZXNzRm9yV29ya2VycyxQcml2YXRlTmV0d29ya0FjY2Vzc0ZvcldlYlJUQyxCbG9ja0luc2VjdXJlTG9jYWxOZXR3b3JrUmVxdWVzdHMsTG9jYWxOZXR3b3JrQWNjZXNzQ2hlY2tzLExvY2FsTmV0d29ya0FjY2Vzc0NoZWNrc0Zvck5hdmlnYXRpb24sTG9jYWxOZXR3b3JrQWNjZXNzQ2hlY2tzRm9yV2ViUlRDLExvY2FsTmV0d29ya0FjY2Vzc0NoZWNrc0ZvcldvcmtlcnMsTG9jYWxOZXR3b3JrQWNjZXNzQ2hlY2tzV2FybmluZ09ubHlcIixcbiAgICBdLFxuICB9LFxufSk7XG5cbnRlc3QuZGVzY3JpYmUoXCJmdWxsIG9mZmxpbmUgU1cgcmVzdG9yZSAoUE5BL0xOQSBsb29wYmFjayBleGVtcHRpb24gZm9yIHRoaXMgaGFybmVzcyBjYXNlKVwiLCAoKSA9PiB7XG4gIHRlc3QoXCJhbiBvZmZsaW5lLXF1ZXVlZCBtZXNzYWdlIHN1cnZpdmVzIGEgcmVsb2FkIHdpdGggdGhlIGJyb3dzZXIgY29udGV4dCBTVElMTCBvZmZsaW5lIGFuZCBzZW5kcyBvbmNlIGFmdGVyXCIsIGFzeW5jICh7IHBhZ2UgfSkgPT4ge1xuICAgIGNvbnN0IGluc3RhbmNlSWQgPSBhd2FpdCBjcmVhdGVTZXNzaW9uKHBhZ2UsIFwiZnVsbCBvZmZsaW5lIHJlbG9hZCBzZWVkXCIpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikpLnRvQmVFbmFibGVkKHsgdGltZW91dDogMjBfMDAwIH0pO1xuICAgIGNvbnN0IGFwaSA9IGF3YWl0IGh1YkFwaShwYWdlKTtcblxuICAgIC8vIEluc3RhbGwgdGhlIG9mZmxpbmUtc2hlbGwgd29ya2VyIGFuZCBsZXQgaXQgdGFrZSBjb250cm9sLlxuICAgIGF3YWl0IHBhZ2UuY29udGV4dCgpLnJvdXRlKFwiKiovZTJlLW9mZmxpbmUtc3cuanNcIiwgKHJvdXRlKSA9PlxuICAgICAgcm91dGUuZnVsZmlsbCh7IGNvbnRlbnRUeXBlOiBcImFwcGxpY2F0aW9uL2phdmFzY3JpcHQ7IGNoYXJzZXQ9dXRmLThcIiwgYm9keTogT0ZGTElORV9TSEVMTF9TVyB9KSxcbiAgICApO1xuICAgIGF3YWl0IHBhZ2UuZXZhbHVhdGUoYXN5bmMgKCkgPT4ge1xuICAgICAgYXdhaXQgbmF2aWdhdG9yLnNlcnZpY2VXb3JrZXIucmVnaXN0ZXIoXCIvZTJlLW9mZmxpbmUtc3cuanNcIiwgeyB1cGRhdGVWaWFDYWNoZTogXCJub25lXCIgfSk7XG4gICAgICBhd2FpdCBuYXZpZ2F0b3Iuc2VydmljZVdvcmtlci5yZWFkeTtcbiAgICAgIGlmICghbmF2aWdhdG9yLnNlcnZpY2VXb3JrZXIuY29udHJvbGxlcikge1xuICAgICAgICBhd2FpdCBuZXcgUHJvbWlzZSgocmVzb2x2ZSkgPT5cbiAgICAgICAgICBuYXZpZ2F0b3Iuc2VydmljZVdvcmtlci5hZGRFdmVudExpc3RlbmVyKFwiY29udHJvbGxlcmNoYW5nZVwiLCByZXNvbHZlLCB7IG9uY2U6IHRydWUgfSksXG4gICAgICAgICk7XG4gICAgICB9XG4gICAgfSk7XG5cbiAgICAvLyBPbmUgbW9yZSBPTkxJTkUgbmF2aWdhdGlvbiBzbyB0aGUgY29udHJvbGxlZCBwYWdlIHByaW1lcyB0aGUgc2hlbGwgY2FjaGUuXG4gICAgYXdhaXQgcGFnZS5nb3RvKHBhZ2UudXJsKCkpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwic2Vzc2lvbi1wYWdlXCIpKS50b0JlVmlzaWJsZSh7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImNvbXBvc2VyLWlucHV0XCIpKS50b0JlRW5hYmxlZCh7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcblxuICAgIC8vIEdvIGZ1bGx5IG9mZmxpbmUgYXQgdGhlIEJST1dTRVIgQ09OVEVYVCBsZXZlbCAobm90IGEgSHViLW9ubHkgcm91dGUpOiB0aGVcbiAgICAvLyBuZXh0IHJlbG9hZCBoYXBwZW5zIHdpdGggZW11bGF0aW9uIHN0aWxsIGluIGVmZmVjdC4gVGhlIGNhY2hlZCBzaGVsbCBtdXN0XG4gICAgLy8gYm9vdCB3aGlsZSBldmVyeSBIdWIgY2FsbCBhbmQgdGhlIGZvbGxvdyB1cGdyYWRlIGdlbnVpbmVseSBmYWlsLlxuICAgIGF3YWl0IHBhZ2UuY29udGV4dCgpLnNldE9mZmxpbmUodHJ1ZSk7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJqb3VybmFsLWJhbm5lclwiKSkudG9IYXZlQXR0cmlidXRlKFwiZGF0YS1zdGF0ZVwiLCBcIm9mZmxpbmVcIik7XG5cbiAgICBhd2FpdCBzZW5kTWVzc2FnZShwYWdlLCBcIm9mZmxpbmUgYWNyb3NzIGEgZnVsbCBvZmZsaW5lIHJlbG9hZFwiKTtcbiAgICBjb25zdCBxdWV1ZWQgPSBwYWdlLmxvY2F0b3IoJ1tkYXRhLXRlc3RpZD1cIm9wdGltaXN0aWMtYnViYmxlXCJdJyk7XG4gICAgYXdhaXQgZXhwZWN0KHF1ZXVlZCkudG9IYXZlQ291bnQoMSk7XG4gICAgY29uc3QgY29tbWFuZElkID0gYXdhaXQgcXVldWVkLmdldEF0dHJpYnV0ZShcImRhdGEtY29tbWFuZC1pZFwiKTtcbiAgICBleHBlY3QoY29tbWFuZElkKS50b0JlVHJ1dGh5KCk7XG4gICAgYXdhaXQgZXhwZWN0KHF1ZXVlZC5maXJzdCgpKS50b0NvbnRhaW5UZXh0KFwi5b6F5Y+R6YCB77yI56a757q/77yJXCIpO1xuICAgIC8vIFRoZSBpbmRlcGVuZGVudCBBUEkgY2xpZW50IGlzIG91dHNpZGUgdGhlIGJyb3dzZXIgY29udGV4dDogbm90aGluZyBzZW50LlxuICAgIGV4cGVjdCgoYXdhaXQgaHViQ29tbWFuZHMoYXBpLCBpbnN0YW5jZUlkKSkuZmlsdGVyKChjKSA9PiBjLm9wZXJhdGlvbiA9PT0gXCJpbnN0YW5jZS5zZW5kXCIpKS50b0hhdmVMZW5ndGgoMCk7XG5cbiAgICAvLyBSZWxvYWQgV0hJTEUgY29udGV4dCBvZmZsaW5lOiB0aGUgc2VydmljZSB3b3JrZXIgc2VydmVzIHRoZSBkb2N1bWVudCBhbmRcbiAgICAvLyB0aGUgd2hvbGUgbW9kdWxlIHNoZWxsOyB0aGUgcmVzdG9yZWQgYXBwIGJvb3RzIGZyb20gZHVyYWJsZSBzdGF0ZS5cbiAgICBhd2FpdCBwYWdlLnJlbG9hZCh7IHdhaXRVbnRpbDogXCJkb21jb250ZW50bG9hZGVkXCIgfSk7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJzZXNzaW9uLXBhZ2VcIikpLnRvQmVWaXNpYmxlKHsgdGltZW91dDogMjBfMDAwIH0pO1xuICAgIGNvbnN0IHJlc3RvcmVkID0gcGFnZS5sb2NhdG9yKGBbZGF0YS10ZXN0aWQ9XCJvcHRpbWlzdGljLWJ1YmJsZVwiXVtkYXRhLWNvbW1hbmQtaWQ9XCIke2NvbW1hbmRJZH1cIl1gKTtcbiAgICBhd2FpdCBleHBlY3QocmVzdG9yZWQpLnRvQmVWaXNpYmxlKCk7XG4gICAgZXhwZWN0KHJlc3RvcmVkKS50b0NvbnRhaW5UZXh0KFwib2ZmbGluZSBhY3Jvc3MgYSBmdWxsIG9mZmxpbmUgcmVsb2FkXCIpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiY29tcG9zZXItaW5wdXRcIikpLnRvQmVFbmFibGVkKHsgdGltZW91dDogMjBfMDAwIH0pO1xuICAgIGF3YWl0IGV4cGVjdChyZXN0b3JlZCkudG9Db250YWluVGV4dChcIuW+heWPkemAge+8iOemu+e6v++8iVwiKTtcbiAgICAvLyBTdGlsbCBub3RoaW5nIGF0IHRoZSBIdWIgdGhyb3VnaCB0aGUgb2ZmbGluZSByZWxvYWQuXG4gICAgZXhwZWN0KChhd2FpdCBodWJDb21tYW5kcyhhcGksIGluc3RhbmNlSWQpKS5maWx0ZXIoKGMpID0+IGMub3BlcmF0aW9uID09PSBcImluc3RhbmNlLnNlbmRcIikpLnRvSGF2ZUxlbmd0aCgwKTtcblxuICAgIC8vIExpZnQgZW11bGF0aW9uOiB3aXRoIHRoaXMgdGVzdCdzIGxvb3BiYWNrLVBOQSBleGVtcHRpb24gdGhlIHJlc3RvcmVkXG4gICAgLy8gcGFnZSBjYW4gcmVvcGVuIGl0cyBmb2xsb3cgc29ja2V0OyB0aGUgb25saW5lIGV2ZW50IHBsdXMgYSBmb3JlZ3JvdW5kXG4gICAgLy8gcmVzdW1lIGtpY2sgdGhlIG1hY2hpbmUgb3V0IG9mIGl0cyBvZmZsaW5lIGJhY2tvZmYuXG4gICAgYXdhaXQgcGFnZS5jb250ZXh0KCkuc2V0T2ZmbGluZShmYWxzZSk7XG4gICAgYXdhaXQgcGFnZS5ldmFsdWF0ZSgoKSA9PiB7XG4gICAgICB3aW5kb3cuZGlzcGF0Y2hFdmVudChuZXcgRXZlbnQoXCJvbmxpbmVcIikpO1xuICAgICAgd2luZG93LmRpc3BhdGNoRXZlbnQobmV3IEV2ZW50KFwiZm9jdXNcIikpO1xuICAgIH0pO1xuICAgIGF3YWl0IGV4cGVjdFxuICAgICAgLnBvbGwoXG4gICAgICAgIGFzeW5jICgpID0+XG4gICAgICAgICAgKGF3YWl0IGh1YkNvbW1hbmRzKGFwaSwgaW5zdGFuY2VJZCkpLmZpbHRlcihcbiAgICAgICAgICAgIChjKSA9PiBjLm9wZXJhdGlvbiA9PT0gXCJpbnN0YW5jZS5zZW5kXCIgJiYgYy5pZCA9PT0gY29tbWFuZElkLFxuICAgICAgICAgICkubGVuZ3RoLFxuICAgICAgICB7IHRpbWVvdXQ6IDYwXzAwMCB9LFxuICAgICAgKVxuICAgICAgLnRvQmUoMSk7XG4gICAgYXdhaXQgZXhwZWN0LnBvbGwoKCkgPT4gaHViSm91cm5hbE1lc3NhZ2VDb3VudChhcGksIGluc3RhbmNlSWQsIGNvbW1hbmRJZCEpLCB7IHRpbWVvdXQ6IDMwXzAwMCB9KS50b0JlKDEpO1xuICAgIC8vIFRoZSBsaW5rIGJhbm5lciBjbGVhcnMgYWZ0ZXIgdGhlIGJyaWVmIOW3suaBouWkjSBub3RpY2UgKDEuNSBzKS5cbiAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImpvdXJuYWwtYmFubmVyXCIpKS50b0hhdmVDb3VudCgwLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgICBhd2FpdCBleHBlY3REZWxpdmVyZWQocGFnZSwgY29tbWFuZElkKTtcbiAgfSk7XG59KTtcbiJdLCJtYXBwaW5ncyI6IkFBQUEsU0FBU0EsTUFBTSxFQUFFQyxPQUFPLElBQUlDLFVBQVUsRUFBRUMsSUFBSSxRQUF5RCxrQkFBa0I7QUFDdkgsU0FBU0MsS0FBSyxRQUFRLFlBQVk7O0FBRWxDO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBRCxJQUFJLENBQUNFLFFBQVEsQ0FBQ0MsU0FBUyxDQUFDO0VBQUVDLElBQUksRUFBRTtBQUFTLENBQUMsQ0FBQztBQUUzQyxNQUFNQyxPQUFpQixHQUFHLEVBQUU7QUFFNUIsZUFBZUMsY0FBY0EsQ0FBQ0MsSUFBVSxFQUFFQyxVQUFrQixF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