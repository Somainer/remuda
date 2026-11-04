import { devices, expect, test, type Browser, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * home-main-extras (H8b, home.md §4.1 item 1, §6.3, §6.4, §1 ②/⑩).
 *
 * Against the in-process fake node, with a real address-owner seat:
 *  - ⑩: a wake-up Main's lineage holds paints 「下次 …」 on the Main card
 *    (/m and 1280) and the 「今天」 card at ≥1440; the sheet lists it and a
 *    confirmed cancel issues the Hub's DELETE, after which the line disappears;
 *  - ⑨: the quick-ask chips appear only while the 问 Main input has focus,
 *    and a tap fills the draft without sending;
 *  - 交给 Main: a worker's QUESTION card offers it in its ⋯ menu, the quote
 *    reaches Main's journal, and the interaction stays pending;
 *  - ②: the host strip renders for the seat's host (★ itself is unit-covered:
 *    the fake node registers ONE host, so the strip collapses to its summary
 *    and paints no chip at all).
 *
 * It seats the Hub's one address-owner Main, so it runs only on request:
 * HUB_E2E_MAIN_UI=1 (the same gate main-ui.hub.spec.ts uses). No millisecond
 * thresholds; every wait is Playwright's eventual condition.
 */

test.describe.configure({ mode: "serial" });

test.skip(
  process.env.HUB_E2E_MAIN_UI !== "1",
  "set HUB_E2E_MAIN_UI=1: seats the Hub's one address-owner Main on the fake node",
);

const created: string[] = [];
const workers: string[] = [];

async function fakeHost(page: Page): Promise<string> {
  const response = await page.request.get("/v1/hosts");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { items?: { id?: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node");
  expect(host?.id, "fake node host").toBeTruthy();
  return host!.id!;
}

/** Seat an address-owner Main through the public create route. */
async function seatMain(page: Page, prompt: string): Promise<string> {
  const hostId = await fakeHost(page);
  const response = await page.request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      prompt,
      title: "Main e2e",
      role: "top-coordinator",
      grants: ["address-owner", "land"],
    },
  });
  expect(response.ok(), `seat Main: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId: string } };
  created.push(body.instance.instanceId);
  return body.instance.instanceId;
}

/** The lineage a seat's wake-ups hang under (an instance is its own lineage). */
async function lineageOf(page: Page, instanceId: string): Promise<string> {
  const response = await page.request.get(`/v1/instances/${instanceId}`);
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { lineageId?: string | null };
  return body.lineageId ?? instanceId;
}

async function addWake(page: Page, lineageId: string, name: string): Promise<string> {
  const response = await page.request.post(`/v1/lineages/${lineageId}/wake-ups`, {
    data: {
      name,
      // An hour out: it can never fire inside this spec.
      text: "hextras e2e wake",
      trigger: { kind: "at", afterSecs: 3600 },
      notify: "never",
    },
  });
  expect(response.ok(), `add wake: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { wakeUp?: { wakeId?: string } };
  expect(body.wakeUp?.wakeId).toBeTruthy();
  return body.wakeUp!.wakeId!;
}

async function wakeNames(page: Page, lineageId: string): Promise<string[]> {
  const response = await page.request.get(`/v1/lineages/${lineageId}/wake-ups`);
  if (!response.ok()) return [];
  const body = (await response.json()) as { items?: { name?: string; state?: string }[] };
  return (body.items ?? []).filter((row) => row.state === "active").map((row) => row.name ?? "");
}

/**
 * Release the address-owner seat: since ma-fence a close is a PAUSE, and the
 * Hub refuses DELETE while the lineage is not paused. Newest chapter first.
 */
async function cleanup(page: Page) {
  for (const id of workers.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    await expect
      .poll(async () => (await page.request.get(`/v1/instances/${id}`)).status(), { timeout: 15_000 })
      .toBe(404);
  }
  const origin = new URL(page.url()).origin;
  for (const id of created.splice(0).reverse()) {
    await page.request
      .post(`/v1/instances/${id}/commands`, {
        headers: { Origin: origin },
        data: { operation: "instance.close", payload: {} },
      })
      .catch(() => undefined);
    const pausedOrGone = async () => {
      const record = await page.request.get(`/v1/instances/${id}`);
      if (record.status() === 404) return true;
      const body = (await record.json()) as { lineageId?: string | null };
      if (!body.lineageId) return true;
      const lineage = await page.request.get(`/v1/lineages/${body.lineageId}`);
      return !lineage.ok() || ((await lineage.json()) as { state?: string }).state === "paused";
    };
    await expect.poll(pausedOrGone, { timeout: 20_000 }).toBe(true);
    for (let attempt = 0; attempt < 5; attempt += 1) {
      const removed = await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
      if (!removed || removed.ok() || removed.status() === 404) break;
      await page.waitForTimeout(500);
    }
    await expect
      .poll(async () => (await page.request.get(`/v1/instances/${id}`)).status(), { timeout: 15_000 })
      .toBe(404);
  }
}

/**
 * Park a worker on one pending card, chosen by the fake node's prompt
 * sentinel: `hqueue-chips` is a single-field question (the ⋯ card case).
 */
async function parkQuestion(page: Page): Promise<{ instanceId: string; interactionId: string }> {
  const hostId = await fakeHost(page);
  const suffix = Date.now().toString(36);
  const response = await page.request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      prompt: `HEXTRAS hqueue-chips blocked-question ${suffix}`,
    },
  });
  expect(response.ok(), `park: ${response.status()} ${await response.text()}`).toBe(true);
  const launched = (await response.json()) as { instance?: { instanceId?: string } };
  const instanceId = launched.instance?.instanceId;
  expect(instanceId).toBeTruthy();
  workers.push(instanceId!);
  let interactionId = "";
  await expect
    .poll(
      async () => {
        const response = await page.request.get(`/v1/interactions?instanceId=${instanceId}`);
        if (!response.ok()) return "";
        const body = (await response.json()) as {
          items?: { id?: string; interactionId?: string; state?: string }[];
        };
        const row = (body.items ?? []).find((item) => item.state === "pending");
        interactionId = row?.interactionId ?? row?.id ?? "";
        return interactionId;
      },
      { timeout: 20_000 },
    )
    .not.toBe("");
  return { instanceId: instanceId!, interactionId };
}

async function pendingOnHub(page: Page, interactionId: string): Promise<boolean> {
  const response = await page.request.get("/v1/interactions");
  if (!response.ok()) return false;
  const body = (await response.json()) as { items?: { id?: string; interactionId?: string; state?: string }[] };
  return (body.items ?? []).some(
    (row) => (row.interactionId ?? row.id) === interactionId && row.state === "pending",
  );
}

/** Poll Main's journal for an event text carrying `needle`. */
async function journalHas(page: Page, instanceId: string, needle: string): Promise<boolean> {
  return page.evaluate(
    async ([id, text]) => {
      // A same-origin GET: the Vite dev server proxies /v1 to the Hub, so the
      // page's cookies ride along (a cross-origin absolute URL would be a CORS
      // failure, not a journal read).
      const response = await fetch(`/v1/instances/${id}/journal`, { credentials: "include" });
      if (!response.ok) return false;
      const body = (await response.json()) as { events?: unknown[] };
      return (body.events ?? []).some((event) => JSON.stringify(event).includes(String(text)));
    },
    [instanceId, needle] as const,
  );
}

function card(page: Page, id: string) {
  return page.locator(`[data-testid="home-queue-card"][data-card-id="${id}"]`);
}

async function expandQueue(page: Page) {
  const more = page.locator('[data-testid="home-queue-more"][aria-expanded="false"]');
  if (await more.count()) await more.click();
}

async function phonePage(browser: Browser) {
  const context = await browser.newContext({ ...devices["iPhone 13"] });
  const page = await context.newPage();
  await login(page);
  return { context, page };
}

test.describe("home-main-extras", () => {
  test.describe.configure({ timeout: 240_000 });

  test("/m 390: ⑩ line, sheet cancel, chips on focus, and 交给 Main", async ({ browser }) => {
    const { context, page } = await phonePage(browser);
    try {
      const mainId = await seatMain(page, "hextras-main phone");
      const hostId = await fakeHost(page);
      const lineageId = await lineageOf(page, mainId);
      const wakeName = `hextras-${Date.now().toString(36)}`;
      await addWake(page, lineageId, wakeName);

      await page.goto("/m");
      const cardBox = page.getByTestId("main-card");
      await expect(cardBox).toBeVisible({ timeout: 20_000 });
      await expect(cardBox).toHaveAttribute("data-main-id", mainId);

      // ② ★ rides Main's host chip even though the single-host strip is
      // collapsed (home.md §1 ②, r2 item 1).
      const star = page.getByTestId("host-main-star");
      await expect(star).toBeVisible({ timeout: 20_000 });
      await expect(star.locator("xpath=ancestor::*[@data-testid='host-chip']")).toHaveAttribute(
        "data-host-id",
        hostId,
      );
      await expect(page.getByTestId("host-summary")).toContainText("主机在线");

      // ⑩: the line names the next fire and the active count.
      const line = page.getByTestId("main-wake-line");
      await expect(line).toBeVisible({ timeout: 20_000 });
      await expect(line).toHaveText(new RegExp(`下次 .*${wakeName} · 共 1 个`));

      // The sheet lists name, trigger, next fire and notify.
      await line.click();
      await expect(page.getByTestId("wakes-sheet")).toBeVisible();
      const row = page.getByTestId("wakes-row");
      await expect(row).toContainText(wakeName);
      await expect(row).toContainText("一次");
      await expect(row).toContainText(/下次 \d{2}:\d{2}/);
      await expect(row).toContainText("不通知");

      // Cancel is behind a confirm and issues the Hub's DELETE.
      await row.getByTestId("wakes-cancel").click();
      await expect(page.getByTestId("wake-cancel-confirm")).toContainText(wakeName);
      await page.getByTestId("wake-cancel-yes").click();
      await expect
        .poll(async () => wakeNames(page, lineageId), { timeout: 20_000 })
        .not.toContain(wakeName);
      // The sheet stays open (now with no rows) and the line disappears once
      // no wake-up can fire.
      await expect(page.getByTestId("wakes-row")).toHaveCount(0, { timeout: 20_000 });
      await expect(page.getByTestId("main-wake-line")).toHaveCount(0, { timeout: 20_000 });
      await page.getByTestId("wakes-close").click();

      // ⑨: chips appear only on focus and a tap fills the dock's draft.
      // Observe every command POST from here on: the chip tap must produce
      // none (the hand-off later does, proving the window and the listener).
      const commandPosts: string[] = [];
      page.on("request", (request) => {
        if (request.method() !== "POST") return;
        if (!/\/v1\/instances\/[^/]+\/commands$/.test(new URL(request.url()).pathname)) return;
        commandPosts.push(request.postData() ?? "");
      });
      const dock = page.getByTestId("main-ask-dock");
      await expect(dock).toBeVisible();
      await expect(page.getByTestId("composer-suggestions")).toHaveCount(0);
      const input = dock.getByTestId("composer-input");
      await input.tap();
      await expect(page.getByTestId("composer-suggestions")).toBeVisible();
      const chip = page.locator('[data-testid="composer-suggestion"][data-suggestion-id="who-waits"]');
      await expect(chip).toContainText("谁在等我");
      await chip.click();
      await expect(input).toHaveValue("谁在等我");
      // A second tap fills nothing new: the draft only keeps what was typed.
      await expect(page.getByTestId("main-card-reply")).not.toContainText("echo: 谁在等我");

      // §6.4 交给 Main: a worker question quotes itself to Main, stays pending.
      const worker = await parkQuestion(page);
      await page.goto("/m");
      await expandQueue(page);
      // Main's own pending approval is pinned first and labelled.
      const pinned = page.locator('[data-testid="home-queue-card"][data-main-asking="1"]');
      await expect(pinned.first()).toContainText("Main 在问你");
      // r2: Main never gets its own pinned question back.
      await expect(pinned.first().getByTestId("home-card-more")).toHaveCount(0);
      const askCard = card(page, worker.interactionId);
      await expect(askCard).toBeVisible({ timeout: 20_000 });
      await expect(askCard).toHaveAttribute("data-home-kind", "simple-question");
      await askCard.getByTestId("home-card-more").click();
      await page.getByTestId("home-card-hand-main").click();
      // r2: the receipt is the command's SETTLED outcome, not the enqueue.
      await expect(askCard.getByTestId("home-card-hand-receipt")).toHaveText("已交给 Main");
      await expect(askCard.getByTestId("home-card-hand-receipt")).toHaveAttribute("data-state", "accepted");
      // The quote reached Main's journal…
      await expect.poll(async () => journalHas(page, mainId, "untrusted:"), { timeout: 30_000 }).toBe(true);
      await expect.poll(async () => journalHas(page, mainId, "旧迁移 0042 删掉还是保留？"), { timeout: 30_000 }).toBe(true);
      // …and the interaction itself is still pending.
      expect(await pendingOnHub(page, worker.interactionId)).toBe(true);
      await expect(askCard).toBeVisible();
      // r2 item 8: the chip tap never sent. The hand-off POST above proves
      // this observer was live and the window elapsed; no command carried the
      // chip's question.
      expect(commandPosts.length, "the hand-off command was observed").toBeGreaterThan(0);
      expect(commandPosts.some((body) => body.includes("谁在等我"))).toBe(false);
    } finally {
      await cleanup(page);
      await context.close();
    }
  });

  test("1440: ⑩ becomes the 「今天」 card in the right column", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await login(page);
    try {
      const mainId = await seatMain(page, "hextras-main 1440");
      const hostId = await fakeHost(page);
      const lineageId = await lineageOf(page, mainId);
      const wakeName = `hextras-today-${Date.now().toString(36)}`;
      await addWake(page, lineageId, wakeName);
      await page.goto("/sessions");
      const aside = page.getByTestId("sessions-aside");
      await expect(aside).toBeVisible({ timeout: 20_000 });
      await expect(aside.getByTestId("main-card")).toHaveAttribute("data-main-id", mainId);
      // ② ★ is visible in the all-online collapsed strip at 1440 too (r2),
      // on the host it names (r3).
      const star = page.getByTestId("host-main-star");
      await expect(star).toBeVisible({ timeout: 20_000 });
      await expect(star.locator("xpath=ancestor::*[@data-testid='host-chip']")).toHaveAttribute(
        "data-host-id",
        hostId,
      );
      // §2.2: the right column owns ⑩ here, so the card has no third line.
      await expect(page.getByTestId("main-wake-line")).toHaveCount(0);
      const today = aside.getByTestId("home-today");
      await expect(today).toBeVisible({ timeout: 20_000 });
      await expect(today.getByTestId("home-today-count")).toHaveText("共 1 个");
      await expect(today.getByTestId("home-today-row")).toContainText(wakeName);
      // A row opens the same sheet; 取消 greys while nothing is stale.
      await today.getByTestId("home-today-row").click();
      await expect(page.getByTestId("wakes-sheet")).toBeVisible();
      await expect(page.getByTestId("wakes-cancel")).toBeEnabled();
      await page.getByTestId("wakes-cancel").click();
      await page.getByTestId("wake-cancel-yes").click();
      await expect(page.getByTestId("home-today")).toHaveCount(0, { timeout: 20_000 });
    } finally {
      await cleanup(page);
    }
  });
});
