import { type PathLike } from "node:fs";
import { mkdir, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { expect, request, test, type APIRequestContext, type Page, type Response } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-mobilenew: a phone must always be able to create a session, and bulk
 * screen reads must never starve control RPCs.
 *
 * The spec builds every precondition itself against the stock fake Hub/Node —
 * no env knob, no config edit. The fake Node supports one runtime switch:
 * while the gate file below exists, it parks tty.screen (bulk-read half of the
 * Hub's per-link budget) and worktree.list (control half) for EVERY instance,
 * in the frame queue without replying; removing the file releases them. The
 * name is port-scoped (same derivation as hub_e2e's fake Node), so it works on
 * whatever HUB_E2E_LISTEN the config assigned without assuming a shared
 * temp-dir identity. Absent by default, the gate changes nothing otherwise.
 */
const hubListen = process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880";
const gatePort = hubListen.split(":")[1] ?? "58880";
const gateFile: PathLike = path.join(os.tmpdir(), `remuda-e2e-rpc-gate-${gatePort}`);

const evidence = process.env.REMUDA_EVIDENCE === "1";
const evidenceDir = path.join(path.dirname(new URL(import.meta.url).pathname), "../../../docs/design/evidence");
const NAME_PREFIX = "e2e-mobile-new";

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(evidenceDir, { recursive: true });
  await page.screenshot({ path: path.join(evidenceDir, name), animations: "disabled" });
}

async function fakeHost(page: Page) {
  const hosts = (await (await page.request.get("/v1/hosts")).json()) as {
    items: { hostId: string; label: string }[];
  };
  const host = hosts.items.find((item) => item.label === "e2e-fake-node");
  expect(host).toBeTruthy();
  const snapshot = (await (
    await page.request.get(`/v1/hosts/${host!.hostId}/workspaces`)
  ).json()) as { workspaces: { workspaceId: string; root: string }[] };
  const workspace = snapshot.workspaces.find((item) => item.workspaceId === "wsp_e2e");
  expect(workspace).toBeTruthy();
  return { hostId: host!.hostId, workspaceId: workspace!.workspaceId, root: workspace!.root };
}

/** Create a claude-pty instance and stop it, leaving a real `exited` row. */
async function createStoppedInstance(
  page: Page,
  hostId: string,
  workspaceId: string,
  root: string,
  name: string,
): Promise<string> {
  const headers = { Origin: new URL(page.url()).origin };
  const create = await page.request.post("/v1/instances", {
    headers,
    data: {
      hostId,
      workspaceId,
      cwd: root,
      kind: "claude",
      driver: "claude-pty",
      name,
    },
  });
  expect(create.ok(), await create.text()).toBe(true);
  const id = ((await create.json()) as { instance: { instanceId: string } }).instance.instanceId;
  // The fake Node projects the close to a real exited lifecycle journal event.
  const close = await page.request.post(`/v1/instances/${id}/commands`, {
    headers,
    data: { operation: "instance.close", payload: {} },
  });
  expect(close.ok(), await close.text()).toBe(true);
  await expect
    .poll(
      async () => {
        const res = await page.request.get(`/v1/instances/${id}`);
        expect(res.ok()).toBeTruthy();
        return ((await res.json()) as { lifecycle: string }).lifecycle;
      },
      { timeout: 20_000 },
    )
    .toBe("exited");
  return id;
}

function newFromBottomBar(page: Page) {
  return page.locator("nav[aria-label='手机底栏'] button[aria-label='新建']").click();
}

/**
 * D-049: a compact /s/:id route has no app bottom bar, so starting another
 * session goes header-back to the phone home (/m) then its 新建. The shared
 * /sessions/new route still mounts the dimmed SessionsPage behind the sheet
 * under the desktop Shell — the no-fan-out surface this spec watches.
 */
async function newSessionFromSessionRoute(page: Page) {
  await page.getByRole("link", { name: "返回" }).click();
  await expect(page).toHaveURL(/\/m$/);
  await page.getByTestId("phone-nav-new").click();
}

async function createSessionViaSheet(page: Page, prompt: string) {
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await page.getByTestId("new-session-prompt").fill(prompt);
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  const posting = page.waitForResponse(
    (response) =>
      response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const response = await posting;
  expect(response.ok(), `create HTTP ${response.status()}`).toBe(true);
  const body = (await response.json()) as { instance?: { instanceId?: string } };
  const instanceId = body.instance?.instanceId;
  expect(instanceId).toBeTruthy();
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  await expect(page.getByTestId("session-page")).toBeVisible();
  return instanceId!;
}

/** Per-id /screen response log, scoped to the ids this run created. */
function collectScreenResponses(page: Page, watchedIds: Set<string>) {
  const fiveHundreds: string[] = [];
  const watched = new Map<number, number>();
  const onResponse = (response: Response) => {
    const url = new URL(response.url());
    const match = url.pathname.match(/^\/v1\/instances\/([^/]+)\/screen$/);
    if (!match) return;
    if (watchedIds.has(match[1])) {
      watched.set(response.status(), (watched.get(response.status()) ?? 0) + 1);
    }
    if (response.status() === 500) fiveHundreds.push(`${response.status()} ${url.pathname}`);
  };
  page.on("response", onResponse);
  return { watched, fiveHundreds };
}

/**
 * A single Playwright APIRequestContext shares one browser HTTP/1.1 connection
 * pool (~6 sockets), so one context cannot hold 32 calls parked server-side.
 * Build several contexts, each its own pool with the login cookie.
 */
async function pressureContexts(page: Page, origin: string, count: number) {
  const cookie = (await page.context().cookies()).find((item) => item.name === "remuda_device");
  expect(cookie).toBeTruthy();
  const headers = {
    Origin: origin,
    Cookie: `${cookie!.name}=${cookie!.value}`,
  };
  const contexts: APIRequestContext[] = [];
  for (let i = 0; i < count; i += 1) {
    contexts.push(await request.newContext({ extraHTTPHeaders: headers }));
  }
  return contexts;
}

/** Park 40 control calls across separate request contexts so all 32 Hub slots
 *  are held for the full 60 s node timeout; the excess 8 settle 503 at once. */
async function parkControlCalls(page: Page, origin: string, hostId: string) {
  // 8 contexts × 5 calls stays under each pool's socket cap.
  const contexts = await pressureContexts(page, origin, 8);
  const calls = contexts.flatMap((ctx) =>
    Array.from({ length: 5 }, () => ctx.get(`/v1/worktrees?hostId=${hostId}`)),
  );
  return {
    settled: Promise.allSettled(calls),
    dispose: async () => {
      await Promise.all(contexts.map((ctx) => ctx.dispose().catch(() => undefined)));
    },
  };
}

/**
 * Prove all 32 slots are held. Under the gate an admitted worktree call parks
 * for its full 60 s node timeout and a saturated one is refused 503
 * synchronously — so: race the GET against a short window. Fast 503 = full.
 * Pending at the deadline = admitted and now parked server-side; KEEP that
 * context and promise (disposing would abort the call and free the slot) and
 * probe again. The returned dispose releases the extra probe waiters only
 * after the create has been attempted.
 */
async function waitForLinkSaturation(page: Page, origin: string, hostId: string) {
  const cookie = (await page.context().cookies()).find((item) => item.name === "remuda_device");
  expect(cookie).toBeTruthy();
  const parkedProbes: { ctx: APIRequestContext; done: Promise<unknown> }[] = [];
  const deadline = Date.now() + 30_000;
  for (;;) {
    const ctx = await request.newContext({
      extraHTTPHeaders: {
        Origin: origin,
        Cookie: `${cookie!.name}=${cookie!.value}`,
      },
    });
    const done = ctx.get(`/v1/worktrees?hostId=${hostId}`);
    const result = await Promise.race([
      done.then((response) => ({ status: response.status() })),
      new Promise<{ status: number }>((resolve) => setTimeout(() => resolve({ status: -1 }), 2_000)),
    ]);
    if (result.status === 503) {
      // Refused without parking: release just this probe.
      await ctx.dispose().catch(() => undefined);
      break;
    }
    // -1 (pending) or a fast 200 (gate briefly missing): hold the context so a
    // genuinely parked waiter keeps its slot, and keep probing.
    parkedProbes.push({ ctx, done: done.catch(() => undefined) });
    if (Date.now() > deadline) throw new Error("control link never refused a probe with 503");
  }
  return {
    dispose: async () => {
      for (const probe of parkedProbes) {
        await probe.ctx.dispose().catch(() => undefined);
      }
    },
  };
}


/**
 * Screen pressure that stays in flight across SEPARATE request contexts (each
 * context has its own ~6-socket HTTP pool; one context could not hold the 16
 * parked reads the bulk half needs). 16 reads park behind the gate, the rest
 * are refused 503; a refill re-issues well inside the 5 s read timeout so the
 * parked half stays full while a create runs.
 */
async function startScreenPressure(page: Page, origin: string, ids: string[]) {
  const contexts = await pressureContexts(page, origin, 4);
  let inFlight = 0;
  let fiveOhThree = 0;
  let fiveHundred = 0;
  const inflightWaiters: (() => void)[] = [];
  const notifyDrain = () => {
    if (inFlight === 0) {
      for (const wake of inflightWaiters.splice(0)) wake();
    }
  };
  const fanOut = (perContext: number) => {
    for (const ctx of contexts) {
      for (let n = 0; n < perContext; n += 1) {
        const id = ids[Math.floor(Math.random() * ids.length)];
        inFlight += 1;
        void ctx
          .get(`/v1/instances/${id}/screen?lines=80`)
          .then((response) => {
            if (response.status() === 503) fiveOhThree += 1;
            if (response.status() === 500) fiveHundred += 1;
          })
          .catch(() => undefined)
          .finally(() => {
            inFlight -= 1;
            notifyDrain();
          });
      }
    }
  };
  // Re-issue well inside the 5 s read timeout so the parked half never drains.
  const refill = setInterval(() => fanOut(6), 1_000);
  return {
    fanOut,
    inFlight: () => inFlight,
    fiveOhThree: () => fiveOhThree,
    fiveHundred: () => fiveHundred,
    stop: async () => {
      clearInterval(refill);
      // Wait for every in-flight read to settle before disposing its pool;
      // parked reads answer quickly once the gate is removed.
      await new Promise<void>((resolve) => {
        if (inFlight === 0) return resolve();
        inflightWaiters.push(resolve);
      });
      await Promise.all(contexts.map((ctx) => ctx.dispose().catch(() => undefined)));
    },
  };
}

test.describe("mobile new session (390px)", () => {
  const created: string[] = [];

  test.beforeEach(async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
    await rm(gateFile, { force: true }).catch(() => undefined);
  });

  test("exited rows cost no /screen 500 and two creates open their sessions", async ({ page }) => {
    await login(page);
    const { hostId, workspaceId, root } = await fakeHost(page);

    // Build the repro preconditions ourselves: 14 real exited rows on the Node.
    const exitedIds = new Set<string>();
    for (let n = 0; n < 14; n += 1) {
      const id = await createStoppedInstance(
        page,
        hostId,
        workspaceId,
        root,
        `${NAME_PREFIX}-exited-${n}`,
      );
      exitedIds.add(id);
      // Restore the shared board in afterEach even though the row is exited.
      created.push(id);
    }
    const collected = collectScreenResponses(page, exitedIds);

    await page.goto("/sessions");
    // Compact /sessions redirects to the /m phone home (D-049), which renders
    // HomeList rather than the desktop SessionList.
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("home-list")).toBeVisible();
    await page.getByTestId("space-chip").filter({ hasText: "remuda-e2e" }).first().click();
    // Mobile auto-opens the space's first tab (an id owned by whichever suite
    // populated this shared hub first — do not assert which one). Navigate to a
    // session this test created explicitly.
    await expect(page).toHaveURL(/\/s\/ins_/);
    const [ownId] = [...exitedIds].sort();
    await page.goto(`/s/${ownId}`);
    await expect(page.getByTestId("session-page")).toBeVisible();
    // Exited session pages never pull /screen for anything.
    await page.waitForTimeout(3_000);
    // D-049: the compact /s/:id route renders no app bottom bar; leaving the
    // session is the header back link, which lands on /sessions and bounces
    // to the phone home /m (HomeList renders there, not the desktop list).
    await page.getByRole("link", { name: "返回" }).click();
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("home-list")).toBeVisible();
    // This space also holds other suites' rows; assert at least all 14 of
    // ours are rendered, scoped by the ids this test created. The phone home
    // row carries no instance-id attribute, so match the row link's href.
    // evaluateAll serialises args as JSON, so pass an array (no Set across
    // the boundary), and poll via expect.poll: a cached one-shot promise
    // would not requery.
    await expect
      .poll(
        () =>
          page
            .locator('[data-testid="home-row"][data-status="exited"]')
            .evaluateAll(
              (rows, ids) =>
                rows.filter((row) => {
                  const href =
                    (row.querySelector("a[href]") as HTMLAnchorElement | null)?.getAttribute("href") ??
                    "";
                  return ids.includes(href.replace("/s/", ""));
                }).length,
              [...exitedIds],
            ),
        { timeout: 10_000 },
      )
      .toBeGreaterThanOrEqual(exitedIds.size);
    await shot(page, "mobile-new-session-1-list-390.png");

    // Several 2.5 s home poll cycles: zero /screen reads for the exited rows
    // (the phone home polls screens for tty-attachable rows only, and the
    // store skips exited/failed lifecycles).
    await page.waitForTimeout(6_000);
    const screenCalls = [...collected.watched.values()].reduce((sum, n) => sum + n, 0);
    expect(screenCalls, "no bulk screen reads for exited rows").toBe(0);
    expect(collected.fiveHundreds).toEqual([]);

    await newFromBottomBar(page);
    const firstId = await createSessionViaSheet(page, "mobile new session repro one");
    created.push(firstId);
    await shot(page, "mobile-new-session-1-session-390.png");

    // The dimmed list stays mounted behind this sheet; it must not fan out.
    // We're on /s/:firstId here, which has no app bottom bar in compact:
    // back to /m, then the phone shell's 新建 (c-msessionfold D-049).
    await newSessionFromSessionRoute(page);
    const secondId = await createSessionViaSheet(page, "mobile new session repro two");
    created.push(secondId);

    expect(collected.fiveHundreds).toEqual([]);
  });

  test("NODE_BUSY on a saturated link rejects the real create with 503 inline", async ({ page }) => {
    test.setTimeout(120_000);
    await login(page);
    const { hostId, workspaceId, root } = await fakeHost(page);
    const exitedIds = new Set<string>();
    for (let n = 0; n < 14; n += 1) {
      const id = await createStoppedInstance(page, hostId, workspaceId, root, `${NAME_PREFIX}-busy-${n}`);
      exitedIds.add(id);
      created.push(id);
    }

    // Saturate with CONTROL calls only: gated worktree.list frames park in the
    // fake Node for the full 60 s Hub timeout, so the 32 slots stay occupied
    // for the whole test — no 5 s screen-read expiry can reopen them.
    await mkdir(path.dirname(gateFile.toString()), { recursive: true });
    await writeFile(gateFile, "block");
    const origin = new URL(page.url()).origin;
    const parked = await parkControlCalls(page, origin, hostId);
    const probes = await waitForLinkSaturation(page, origin, hostId);

    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await page.getByTestId("new-session-prompt").fill("mobile new session busy");
    const posting = page.waitForResponse(
      (response) =>
        response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
    );
    await page.getByTestId("new-session-start").click();
    const response = await posting;
    expect(response.status()).toBe(503);
    const body = (await response.json()) as { code?: string; retryAfterMs?: number };
    expect(body.code).toBe("NODE_BUSY");
    expect(body.retryAfterMs).toBeGreaterThan(0);

    const errorBox = page.getByTestId("new-session-error");
    await expect(errorBox).toBeVisible();
    await expect(errorBox).toContainText("NODE_BUSY");
    await expect(page).toHaveURL(/\/sessions\/new/);
    await expect(page.getByTestId("new-session-prompt")).toHaveValue("mobile new session busy");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await shot(page, "mobile-new-session-1-node-busy-390.png");

    // Release parked calls so the node link drains.
    await rm(gateFile, { force: true });
    await parked.settled;
    await probes.dispose();
    await parked.dispose();
  });

  test("saturated screen reads still let instance.create through", async ({ page }) => {
    test.setTimeout(120_000);
    await login(page);
    const { hostId, workspaceId, root } = await fakeHost(page);
    const exitedIds = [] as string[];
    for (let n = 0; n < 14; n += 1) {
      const id = await createStoppedInstance(page, hostId, workspaceId, root, `${NAME_PREFIX}-sat-${n}`);
      exitedIds.push(id);
      created.push(id);
    }
    const collected = collectScreenResponses(page, new Set(exitedIds));

    // Start pressure and wait until 16 reads are actually parked in flight
    // (rather than firing and hoping): the refill keeps the bulk half full for
    // the whole 5 s read-timeout window while the create runs.
    await mkdir(path.dirname(gateFile.toString()), { recursive: true });
    await writeFile(gateFile, "block");
    const origin = new URL(page.url()).origin;
    const pressure = await startScreenPressure(page, origin, exitedIds);
    pressure.fanOut(10);
    await expect
      .poll(() => pressure.inFlight(), { timeout: 10_000 })
      .toBeGreaterThanOrEqual(16);

    await page.goto("/sessions");
    // Compact /sessions redirects to the /m phone home (D-049).
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("home-list")).toBeVisible();
    await newFromBottomBar(page);
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await page.getByTestId("new-session-prompt").fill("mobile new session under saturation");

    // Re-issue reads in the same tick as the create: their 503 refusals land
    // while the POST is pending, proving refusal traffic does not block it.
    const posting = page.waitForResponse(
      (response) =>
        response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
    );
    const refusedDuringCreate = page
      .getByTestId("new-session-start")
      .click()
      .then(() => {
        pressure.fanOut(6);
      });
    const response = await posting;
    await refusedDuringCreate;
    expect(response.status(), await response.text()).toBe(200);
    const instanceId = ((await response.json()) as { instance?: { instanceId?: string } }).instance
      ?.instanceId;
    expect(instanceId).toBeTruthy();
    await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
    created.push(instanceId!);

    // The parked bulk half was genuinely in flight at create resolution — the
    // create used the reserved control half, not slots that leaked open.
    expect(pressure.inFlight(), "screen reads parked while create posted").toBeGreaterThanOrEqual(16);

    await rm(gateFile, { force: true });
    await pressure.stop();
    expect(pressure.fiveOhThree(), "reads get NODE_BUSY/503, never pass silently").toBeGreaterThan(0);
    expect(pressure.fiveHundred(), "no read may surface as 500 INTERNAL").toBe(0);
    expect(collected.fiveHundreds).toEqual([]);
  });
});
