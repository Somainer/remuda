import { devices, expect, test, type Browser, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * routed-to (H9, home.md §5.1/§10, D-061 `routedTo`).
 *
 * Against the in-process fake node, with a real address-owner seat and a
 * worker it dispatched (the `routedto-question` sentinel journals one
 * single-select question, as a real Node does):
 *  - the Hub routes the question to the running Main (D-057 §8.1), so
 *    `GET /v1/interactions` reads `routedTo: "main"`; it is absent from ④, ①
 *    and the OS badge stay where they were, and the Main card says
 *    「正在处理 1 个 worker 提问」;
 *  - /approvals keeps it, labelled 「已转给 Main」;
 *  - Main is paused: the lift reads `owner` again, the card joins ④ and the
 *    badge goes up by one.
 *
 * It seats the Hub's one address-owner Main and needs the D-051 switch on in
 * the Hub's environment (the fake Hub inherits Playwright's), so it runs only
 * on request: HUB_E2E_MAIN_UI=1 REMUDA_DELEGATED_DECISIONS=1. No millisecond
 * thresholds; every wait is Playwright's eventual condition.
 */

test.describe.configure({ mode: "serial" });

test.skip(
  process.env.HUB_E2E_MAIN_UI !== "1" || process.env.REMUDA_DELEGATED_DECISIONS !== "1",
  "set HUB_E2E_MAIN_UI=1 and REMUDA_DELEGATED_DECISIONS=1: seats the Hub's one Main and routes to it",
);

const seats: string[] = [];
const workers: string[] = [];

async function fakeHost(page: Page): Promise<string> {
  const response = await page.request.get("/v1/hosts");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { items?: { id?: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node");
  expect(host?.id, "fake node host").toBeTruthy();
  return host!.id!;
}

/** Seat a Main that may dispatch, and wait until its lineage runs. */
async function seatMain(page: Page): Promise<string> {
  const hostId = await fakeHost(page);
  const response = await page.request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      prompt: `routedto-seat ${Date.now().toString(36)}`,
      title: "Main e2e",
      role: "top-coordinator",
      grants: ["address-owner", "dispatch"],
    },
  });
  expect(response.ok(), `seat Main: ${response.status()} ${await response.text()}`).toBe(true);
  const id = ((await response.json()) as { instance: { instanceId: string } }).instance.instanceId;
  seats.push(id);
  await expect.poll(async () => lineageState(page, id), { timeout: 30_000 }).toBe("running");
  return id;
}

async function lineageState(page: Page, id: string): Promise<string> {
  const record = await page.request.get(`/v1/instances/${id}`);
  if (!record.ok()) return "";
  const lineage = ((await record.json()) as { lineageId?: string | null }).lineageId ?? id;
  const response = await page.request.get(`/v1/lineages/${lineage}`);
  if (!response.ok()) return "";
  return ((await response.json()) as { state?: string }).state ?? "";
}

/** Main dispatches a worker (its credential narrowed to Main) that asks one question. */
async function dispatchWorker(page: Page, mainId: string): Promise<{ instanceId: string; interactionId: string }> {
  const hostId = await fakeHost(page);
  const origin = new URL(page.url()).origin;
  const response = await page.request.post("/v1/instances", {
    headers: { Origin: origin, "x-remuda-instance-id": mainId },
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      prompt: `ROUTEDTO routedto-question ${Date.now().toString(36)}`,
    },
  });
  expect(response.ok(), `dispatch: ${response.status()} ${await response.text()}`).toBe(true);
  const instanceId = ((await response.json()) as { instance: { instanceId: string } }).instance.instanceId;
  workers.push(instanceId);
  let interactionId = "";
  await expect
    .poll(
      async () => {
        const row = (await items(page)).find((item) => item.instanceId === instanceId && item.state === "pending");
        interactionId = row?.interactionId ?? row?.id ?? "";
        return interactionId;
      },
      { timeout: 20_000 },
    )
    .not.toBe("");
  return { instanceId, interactionId };
}

type Item = { id?: string; interactionId?: string; instanceId?: string; state?: string; routedTo?: string };

async function items(page: Page): Promise<Item[]> {
  const response = await page.request.get("/v1/interactions");
  if (!response.ok()) return [];
  return ((await response.json()) as { items?: Item[] }).items ?? [];
}

async function routedTo(page: Page, interactionId: string): Promise<string> {
  const row = (await items(page)).find((item) => (item.interactionId ?? item.id) === interactionId);
  return row?.routedTo ?? "";
}

async function closeChapter(page: Page, id: string) {
  const origin = new URL(page.url()).origin;
  await page.request
    .post(`/v1/instances/${id}/commands`, {
      headers: { Origin: origin },
      data: { operation: "instance.close", payload: {} },
    })
    .catch(() => undefined);
}

/** Workers first (force delete), then the seat: a close is a PAUSE and DELETE needs paused. */
async function cleanup(page: Page) {
  for (const id of workers.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    await expect
      .poll(async () => (await page.request.get(`/v1/instances/${id}`)).status(), { timeout: 15_000 })
      .toBe(404);
  }
  for (const id of seats.splice(0).reverse()) {
    await closeChapter(page, id);
    await expect
      .poll(async () => ["paused", ""].includes(await lineageState(page, id)), { timeout: 20_000 })
      .toBe(true);
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

/** Record every OS badge write the page makes (the Badging API is stubbed). */
async function recordBadge(page: Page) {
  await page.addInitScript(() => {
    const record = window as unknown as { __badges: number[] };
    record.__badges = [];
    Object.defineProperty(navigator, "setAppBadge", {
      configurable: true,
      value: (count: number) => {
        record.__badges.push(count);
        return Promise.resolve();
      },
    });
    Object.defineProperty(navigator, "clearAppBadge", {
      configurable: true,
      value: () => {
        record.__badges.push(0);
        return Promise.resolve();
      },
    });
  });
}

async function lastBadge(page: Page): Promise<number | null> {
  return page.evaluate(() => {
    const badges = (window as unknown as { __badges?: number[] }).__badges ?? [];
    return badges.length ? badges[badges.length - 1] : null;
  });
}

/** ① 需要你 as a number; 0 when the segment is hidden (a known zero). */
async function needYou(page: Page): Promise<number> {
  const segment = page.getByTestId("glance-need-you");
  if ((await segment.count()) === 0) return 0;
  const match = /(\d+)\s*需要你/.exec((await segment.textContent()) ?? "");
  return match ? Number(match[1]) : -1;
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
  await recordBadge(page);
  await login(page);
  return { context, page };
}

test.describe("routed-to", () => {
  test.describe.configure({ timeout: 240_000 });

  test("/m 390: a routed worker question leaves ④ and the badge, the Main card counts it, the lift brings it back", async ({
    browser,
  }) => {
    const { context, page } = await phonePage(browser);
    try {
      const mainId = await seatMain(page);
      await page.goto("/m");
      const mainCard = page.getByTestId("main-card");
      await expect(mainCard).toHaveAttribute("data-main-id", mainId, { timeout: 20_000 });
      await expect(page.getByTestId("main-card-routed")).toHaveCount(0);
      // The owner's N before the worker asks: ① and the OS badge agree.
      await expect.poll(async () => lastBadge(page), { timeout: 20_000 }).not.toBeNull();
      const before = await needYou(page);
      await expect.poll(async () => lastBadge(page), { timeout: 20_000 }).toBe(before);

      const worker = await dispatchWorker(page, mainId);
      await expect.poll(async () => routedTo(page, worker.interactionId), { timeout: 20_000 }).toBe("main");

      // Main is handling it: the card says so, ④ does not hold it, ① and the
      // badge did not move.
      const routedLine = page.getByTestId("main-card-routed");
      await expect(routedLine).toHaveText("正在处理 1 个 worker 提问", { timeout: 20_000 });
      await expandQueue(page);
      await expect(card(page, worker.interactionId)).toHaveCount(0);
      expect(await needYou(page)).toBe(before);
      expect(await lastBadge(page)).toBe(before);

      // Pause Main: the lift hands the question back to the owner.
      await closeChapter(page, mainId);
      await expect.poll(async () => lineageState(page, mainId), { timeout: 20_000 }).toBe("paused");
      await expect.poll(async () => routedTo(page, worker.interactionId), { timeout: 20_000 }).toBe("owner");
      await expandQueue(page);
      await expect(card(page, worker.interactionId)).toBeVisible({ timeout: 20_000 });
      await expect.poll(async () => needYou(page), { timeout: 20_000 }).toBe(before + 1);
      await expect.poll(async () => lastBadge(page), { timeout: 20_000 }).toBe(before + 1);
      await expect(page.getByTestId("main-card-routed")).toHaveCount(0);
    } finally {
      await cleanup(page);
      await context.close();
    }
  });

  test("1440: /approvals keeps the routed question, labelled 「已转给 Main」", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await login(page);
    try {
      const mainId = await seatMain(page);
      const worker = await dispatchWorker(page, mainId);
      await expect.poll(async () => routedTo(page, worker.interactionId), { timeout: 20_000 }).toBe("main");
      await page.goto("/sessions");
      await expect(page.getByTestId("main-card-routed")).toHaveText("正在处理 1 个 worker 提问", {
        timeout: 20_000,
      });
      await page.goto(`/approvals?focus=${worker.interactionId}`);
      const row = page.locator(`[data-testid="approval-row"][data-interaction-id="${worker.interactionId}"]`);
      await expect(row).toBeVisible({ timeout: 20_000 });
      await expect(row.getByTestId("approval-routed")).toHaveText("已转给 Main");
    } finally {
      await cleanup(page);
    }
  });
});
