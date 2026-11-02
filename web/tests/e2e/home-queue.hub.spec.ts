import {
  devices,
  expect,
  request as apiRequest,
  test,
  type APIRequestContext,
  type Browser,
  type BrowserContext,
  type Locator,
  type Page,
} from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * home-queue (H2, home.md §4, §5.1, §8, §9 角标与落点).
 *
 * Gated here, against cards this spec seeds and owns on the fake Node:
 *  - §4.2 card kinds on /m: a hook approval shows ONLY 拒绝 and 允许一次 (its
 *    allow-session option is never rendered); a single-field question gets
 *    chips plus 其他…; a multi-field question is 去回答; a plan review has no
 *    approve control, only 审阅计划; an owner-blocked task is 任务待你;
 *  - the header 「需要你 · N」 equals ① 「⚠N 需要你」 (one §5.1 N);
 *  - §4.4: a chip and 允许一次 commit with 已提交, then the card leaves and the
 *    Hub no longer lists it pending; a card a second device answered first
 *    shows 已在其他设备处理 (the Hub's real 409) and leaves;
 *  - §8: offline greys the buttons with 数据可能过期;
 *  - push landing: /sessions?focus=<id> rings and shows the card on /m (390)
 *    and on /sessions (1280), where the queue is the 2-column grid and hides
 *    while the list search has text.
 *
 * The suite shares one Hub: nothing here assumes a global count or order —
 * cards are found by their own ids, the queue is expanded when other specs'
 * cards fill the fold, and every session/task is released at the end.
 *
 * No millisecond thresholds: only Playwright's own waits for an eventual
 * condition. Self-skipping: needs the in-process fake Node (`e2e-fake-node`).
 * Evidence screenshots are written only when REMUDA_EVIDENCE=1.
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

/** The fake Node enrols shortly after the Hub boots; poll before skipping. */
async function needsFakeNode(page: Page): Promise<boolean> {
  return expect
    .poll(
      async () => {
        try {
          const response = await page.request.get("/v1/hosts");
          if (!response.ok()) return false;
          const body = (await response.json()) as { items?: { label?: string }[] };
          return (body.items ?? []).some((row) => row.label === "e2e-fake-node");
        } catch {
          return false;
        }
      },
      { timeout: 15_000, intervals: [250, 500, 1_000] },
    )
    .toBe(true)
    .then(
      () => true,
      () => false,
    );
}

async function apiJson<T>(page: Page, method: string, pathName: string, body?: unknown): Promise<T> {
  const response = await page.request.fetch(pathName, {
    method,
    data: body,
    headers: body ? { "content-type": "application/json" } : undefined,
  });
  const text = await response.text();
  expect(response.ok(), `${method} ${pathName} → ${response.status()}: ${text}`).toBe(true);
  return (text ? JSON.parse(text) : null) as T;
}

type HostRow = { id?: string; label?: string; maxInstances?: number };

async function fakeHost(page: Page): Promise<HostRow> {
  const body = await apiJson<{ items?: HostRow[] }>(page, "GET", "/v1/hosts");
  const host = (body.items ?? []).find((item) => item.label === "e2e-fake-node");
  expect(host?.id, "e2e-fake-node enrolled").toBeTruthy();
  return host!;
}

/** The shared fake Node seats at most 8 live instances; wait for a free slot. */
async function waitForSeat(page: Page, host: HostRow): Promise<void> {
  await expect
    .poll(
      async () => {
        const response = await page.request.get("/v1/instances");
        if (!response.ok()) return true;
        const rows = ((await response.json()) as { items?: { hostId?: string; lifecycle?: string }[] }).items ?? [];
        const live = rows.filter(
          (row) => row.hostId === host.id && !["exited", "failed", "closed"].includes(row.lifecycle ?? ""),
        ).length;
        return live < (host.maxInstances ?? 8);
      },
      { timeout: 30_000 },
    )
    .toBe(true);
}

const createdInstances: string[] = [];
const createdTasks: string[] = [];

/**
 * A session the fake Node parks on one pending card, chosen by the prompt
 * sentinel (hub_e2e.rs); `blocked-question` keeps its native status blocked.
 * Returns the instance id and its pending interaction id.
 */
async function park(page: Page, sentinel: string): Promise<{ instanceId: string; interactionId: string }> {
  const host = await fakeHost(page);
  await waitForSeat(page, host);
  const suffix = Date.now().toString(36);
  const launched = await apiJson<{ instance?: { instanceId?: string; id?: string } }>(page, "POST", "/v1/instances", {
    hostId: host.id,
    workspaceId: "wsp_e2e",
    kind: "claude",
    driver: "claude-print",
    prompt: `HQUEUE ${sentinel} blocked-question ${suffix}`,
  });
  const instanceId = launched.instance?.instanceId ?? launched.instance?.id;
  expect(instanceId).toBeTruthy();
  createdInstances.push(instanceId!);
  let interactionId = "";
  await expect
    .poll(
      async () => {
        const list = await apiJson<{ items?: { id?: string; interactionId?: string; instanceId?: string; state?: string }[] }>(
          page,
          "GET",
          `/v1/interactions?instanceId=${instanceId}`,
        );
        const row = (list.items ?? []).find((item) => item.instanceId === instanceId && item.state === "pending");
        interactionId = row?.interactionId ?? row?.id ?? "";
        return interactionId;
      },
      { timeout: 20_000 },
    )
    .not.toBe("");
  return { instanceId: instanceId!, interactionId };
}

async function pendingOnHub(page: Page, interactionId: string): Promise<boolean> {
  const list = await apiJson<{ items?: { id?: string; interactionId?: string; state?: string }[] }>(
    page,
    "GET",
    "/v1/interactions",
  );
  return (list.items ?? []).some(
    (item) => (item.interactionId ?? item.id) === interactionId && item.state === "pending",
  );
}

/** An owner-blocked task with no session: a 「任务待你」 card (§4.6). */
async function blockedTask(page: Page): Promise<string> {
  const suffix = Date.now().toString(36);
  const project = (await apiJson<{ id: string }>(page, "POST", "/v1/projects", { name: `hqueue ${suffix}` })).id;
  const task = await apiJson<{ id: string }>(page, "POST", "/v1/tasks", {
    projectId: project,
    title: `hqueue blocked ${suffix}`,
    intent: `e2e home queue ${suffix}`,
  });
  createdTasks.push(task.id);
  await apiJson(page, "PATCH", `/v1/tasks/${task.id}`, { state: "failed", reason: `等你确认验收范围 ${suffix}` });
  return task.id;
}

/**
 * Release this spec's sessions and tasks and WAIT until the Hub no longer
 * serves the sessions: the suite shares one fake Node, and a leftover blocked
 * row changes the next spec's exact group counts (m-home asserts ⚠1).
 */
async function cleanup(request: APIRequestContext) {
  for (const id of createdTasks.splice(0)) {
    await request.post(`/v1/tasks/${id}/archive`, { data: {} }).catch(() => undefined);
  }
  for (const id of createdInstances.splice(0)) {
    await request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    await expect
      .poll(async () => (await request.get(`/v1/instances/${id}`)).status(), { timeout: 15_000 })
      .toBe(404);
  }
}

/**
 * The last signed-in cookie jar, kept so afterAll can still release what a
 * test seeded when that test ran out of time before its own finally.
 */
let lastStorage: Awaited<ReturnType<BrowserContext["storageState"]>> | null = null;

test.afterAll(async () => {
  if (!lastStorage || (createdInstances.length === 0 && createdTasks.length === 0)) return;
  const request = await apiRequest.newContext({
    baseURL: test.info().project.use.baseURL,
    storageState: lastStorage,
  });
  try {
    await cleanup(request);
  } finally {
    await request.dispose();
  }
});

function card(page: Page, id: string): Locator {
  return page.locator(`[data-testid="home-queue-card"][data-card-id="${id}"]`);
}

/** Release the expansion hold (§4.1: expanded = held until collapsed). */
async function collapseQueue(page: Page) {
  const collapse = page.locator('[data-testid="home-queue-more"][aria-expanded="true"]');
  if (await collapse.count()) await collapse.click();
}

/** Other specs' cards may fill the fold: open the queue in place. */
/**
 * Wait until `id` is painted, opening the fold on each round: after a release
 * the card may be the newest painted, or still behind a fold row that only
 * appears with the poll that delivers it.
 */
async function showCard(page: Page, id: string, timeout = 30_000) {
  await expect
    .poll(
      async () => {
        if (await card(page, id).count()) return true;
        const more = page.locator('[data-testid="home-queue-more"][aria-expanded="false"]');
        if (await more.count()) await more.click();
        return (await card(page, id).count()) > 0;
      },
      { timeout },
    )
    .toBe(true);
}

async function expandQueue(page: Page) {
  const more = page.locator('[data-testid="home-queue-more"][aria-expanded="false"]');
  if (await more.count()) await more.click();
}

async function phoneContext(browser: Browser, baseURL: string, name: string): Promise<BrowserContext> {
  const context = await browser.newContext({ ...devices["iPhone 13"], baseURL });
  await login(await context.newPage(), name);
  lastStorage = await context.storageState();
  return context;
}

/** How many line boxes one element's text paints (a clamp shows fewer). */
async function renderedLines(locator: Locator): Promise<number> {
  return locator.evaluate((el) => {
    const tops = new Set<number>();
    for (const node of Array.from(el.childNodes)) {
      if (node.nodeType !== Node.TEXT_NODE) continue;
      const range = document.createRange();
      range.selectNodeContents(node);
      for (const rect of range.getClientRects()) {
        if (rect.width > 0.5 && rect.height > 0.5) tops.add(Math.round(rect.top));
      }
    }
    return tops.size;
  });
}

/** The card's top and bottom are both inside the viewport. */
async function inViewport(locator: Locator): Promise<boolean> {
  return locator.evaluate((el) => {
    const rect = el.getBoundingClientRect();
    return rect.top >= 0 && rect.bottom <= window.innerHeight;
  });
}

test.describe("home queue", () => {
  // Each test seeds several real fake-Node sessions and logs in more than one
  // device; under gate load that outgrows the suite's 90 s default.
  test.describe.configure({ timeout: 240_000 });

  test("390: card kinds, one N, the answer flow and 409", async ({ browser }) => {
    const baseURL = test.info().project.use.baseURL!;
    const context = await phoneContext(browser, baseURL, "e2e-hqueue-phone");
    const page = await context.newPage();
    try {
      await page.goto("/m");
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");

      const approval = await park(page, "hook-approval");
      const chips = await park(page, "hqueue-chips");
      const plan = await park(page, "hqueue-plan");
      const other = await park(page, "ask-question");
      const taskId = await blockedTask(page);

      await page.goto("/m");
      await expect(page.getByTestId("home-queue")).toBeVisible({ timeout: 20_000 });
      await expect(card(page, approval.interactionId).or(page.getByTestId("home-queue-more")).first()).toBeVisible({
        timeout: 20_000,
      });
      await expandQueue(page);

      // §4.2 / §4.3: the hook approval offers deny + allow-once ONLY.
      const approvalCard = card(page, approval.interactionId);
      await expect(approvalCard).toHaveAttribute("data-home-kind", "approval");
      await expect(approvalCard.getByRole("button")).toHaveText(["拒绝", "允许一次"]);
      await expect(approvalCard).not.toContainText("始终允许");

      const chipCard = card(page, chips.interactionId);
      await expect(chipCard).toHaveAttribute("data-home-kind", "simple-question");
      await expect(chipCard.getByTestId("home-card-chip")).toHaveText(["删除", "保留", "改名"]);
      await expect(chipCard.getByTestId("home-card-other")).toHaveText("其他…");

      // A plan is review-only: not one button on the card.
      const planCard = card(page, plan.interactionId);
      await expect(planCard).toHaveAttribute("data-home-kind", "plan");
      await expect(planCard.getByRole("button")).toHaveCount(0);
      await expect(planCard.getByTestId("home-card-review")).toHaveText("审阅计划");
      // §4.2 标题、正文前 2 行: the title is its own line and the body paints
      // BOTH plan lines in full (not a title+1-line clamp).
      await expect(planCard.getByTestId("home-card-plan-title")).toHaveText("Plan: split the gate");
      expect(await renderedLines(planCard.getByTestId("home-card-body"))).toBe(2);

      const otherCard = card(page, other.interactionId);
      await expect(otherCard).toHaveAttribute("data-home-kind", "question");
      await expect(otherCard.getByTestId("home-card-answer")).toHaveText("去回答");

      const taskCard = card(page, taskId);
      await expect(taskCard).toHaveAttribute("data-card-type", "task");
      await expect(taskCard).toContainText("任务待你");
      await expect(taskCard.getByTestId("home-card-task")).toHaveText("打开任务");

      // §5.1: the queue header and ① read one N.
      await expect
        .poll(async () => {
          const header = (await page.getByTestId("home-queue-title").innerText()).match(/需要你 · (\d+)/)?.[1];
          const glance = (await page.getByTestId("glance-need-you").innerText()).match(/(\d+) 需要你/)?.[1];
          return header !== undefined && header === glance;
        })
        .toBe(true);
      await shot(page, "home-queue-390-light.png");
      await setMode(page, "dark");
      await shot(page, "home-queue-390-dark.png");
      await setMode(page, "light");

      // §4.4: a chip commits with 已提交, then the card leaves.
      await chipCard.getByRole("button", { name: "保留" }).click();
      await expect(chipCard.getByTestId("home-card-receipt")).toHaveText("已提交");
      await expect(chipCard).toHaveCount(0, { timeout: 20_000 });
      expect(await pendingOnHub(page, chips.interactionId)).toBe(false);

      await approvalCard.getByRole("button", { name: "允许一次" }).click();
      await expect(approvalCard.getByTestId("home-card-receipt")).toHaveText("已提交");
      await expect(approvalCard).toHaveCount(0, { timeout: 20_000 });
      expect(await pendingOnHub(page, approval.interactionId)).toBe(false);

      // A second device answers first. Freeze this page's interaction list
      // (its next poll would simply drop the card) so the tap reaches the Hub
      // and gets the real 409.
      // `hqueue-race`: the fake Node refuses a second answer the way the real
      // Node does, so the Hub's own 409 reaches this device.
      const raced = await park(page, "mhome-blocked hqueue-race");
      // The open queue holds its order (§4.1): collapse and re-expand to take
      // in the card that arrived while it was frozen.
      await collapseQueue(page);
      await showCard(page, raced.interactionId);
      const racedCard = card(page, raced.interactionId);
      await expect(racedCard).toBeVisible();
      const frozen = await (await page.request.get("/v1/interactions")).text();
      await page.route(/\/v1\/interactions(\?.*)?$/, async (route) => {
        if (route.request().method() !== "GET") return route.fallback();
        await route.fulfill({ status: 200, contentType: "application/json", body: frozen });
      });
      const second = await phoneContext(browser, baseURL, "e2e-hqueue-other");
      try {
        const otherPage = second.pages()[0];
        const list = await apiJson<{ items?: { id?: string; interactionId?: string; request?: { inputDigest?: string } }[] }>(
          otherPage,
          "GET",
          `/v1/interactions?instanceId=${raced.instanceId}`,
        );
        const digest = (list.items ?? []).find((row) => (row.interactionId ?? row.id) === raced.interactionId)?.request
          ?.inputDigest;
        await apiJson(otherPage, "POST", `/v1/interactions/${raced.interactionId}/answer`, {
          answer: { kind: "approval", optionId: "deny", inputDigest: digest ?? "" },
        });
      } finally {
        await second.close();
      }
      await racedCard.getByRole("button", { name: "允许一次" }).click();
      await expect(racedCard.getByTestId("home-card-receipt")).toHaveText("已在其他设备处理");
      await shot(page, "home-queue-390-elsewhere.png");
      // It leaves although the frozen list still says pending.
      await expect(racedCard).toHaveCount(0, { timeout: 20_000 });
      await page.unroute(/\/v1\/interactions(\?.*)?$/);
    } finally {
      await page.unroute(/\/v1\/interactions(\?.*)?$/).catch(() => undefined);
      await cleanup(page.request);
      await context.close();
    }
  });

  test("390: offline greys the buttons with 数据可能过期", async ({ browser }) => {
    const baseURL = test.info().project.use.baseURL!;
    const context = await phoneContext(browser, baseURL, "e2e-hqueue-stale");
    const page = await context.newPage();
    try {
      await page.goto("/m");
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");
      const parked = await park(page, "mhome-blocked");
      await page.goto("/m");
      await expect(card(page, parked.interactionId).or(page.getByTestId("home-queue-more")).first()).toBeVisible({
        timeout: 20_000,
      });
      await expandQueue(page);
      const parkedCard = card(page, parked.interactionId);
      const allow = parkedCard.getByRole("button", { name: "允许一次" });
      await expect(allow).toBeEnabled();
      await context.setOffline(true);
      // §8: no successful refresh for the stale window greys every button.
      await expect(allow).toBeDisabled({ timeout: 30_000 });
      await expect(parkedCard.getByRole("button", { name: "拒绝" })).toBeDisabled();
      await expect(parkedCard.getByTestId("home-card-stale")).toHaveText("数据可能过期");
      await shot(page, "home-queue-390-stale.png");
      await context.setOffline(false);
      await expect(allow).toBeEnabled({ timeout: 30_000 });
    } finally {
      await context.setOffline(false);
      await cleanup(page.request);
      await context.close();
    }
  });

  test("390: the expanded queue holds its order while a card arrives, and re-orders on collapse", async ({
    browser,
  }) => {
    const baseURL = test.info().project.use.baseURL!;
    const context = await phoneContext(browser, baseURL, "e2e-hqueue-hold");
    const page = await context.newPage();
    try {
      await page.goto("/m");
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");
      const first: string[] = [];
      for (let index = 0; index < 4; index += 1) first.push((await park(page, "mhome-blocked")).interactionId);

      await page.goto("/m");
      await expect(page.getByTestId("home-queue").or(page.getByTestId("home-queue-more")).first()).toBeVisible({
        timeout: 20_000,
      });
      await expandQueue(page);
      await expect(page.getByTestId("home-queue")).toHaveAttribute("data-order-held", "1");
      const before = await page.getByTestId("home-queue-card").evaluateAll((nodes) =>
        nodes.map((node) => node.getAttribute("data-card-id")),
      );
      for (const id of first) expect(before).toContain(id);

      // A newer card lands while the queue is open: §3.3 freezes the order.
      // Count the page's own interaction polls so the assertion waits for the
      // new data to have been DELIVERED (no wall-clock threshold).
      let polls = 0;
      const countPoll = (response: import("@playwright/test").Response) => {
        if (response.request().method() === "GET" && /\/v1\/interactions(\?|$)/.test(response.url())) polls += 1;
      };
      page.on("response", countPoll);
      const startPolls = polls;
      const later = await park(page, "mhome-blocked");
      await expect.poll(() => pendingOnHub(page, later.interactionId), { timeout: 20_000 }).toBe(true);
      await expect.poll(() => polls, { timeout: 20_000 }).toBeGreaterThan(startPolls + 3);
      page.off("response", countPoll);
      const during = await page.getByTestId("home-queue-card").evaluateAll((nodes) =>
        nodes.map((node) => node.getAttribute("data-card-id")),
      );
      expect(during).toEqual(before);
      expect(during).not.toContain(later.interactionId);

      // Collapsing releases the hold; re-expanding paints the new card first.
      await collapseQueue(page);
      await expect(page.getByTestId("home-queue")).toHaveAttribute("data-order-held", "0");
      await expandQueue(page);
      await expect(card(page, later.interactionId)).toBeVisible({ timeout: 20_000 });
      const after = await page.getByTestId("home-queue-card").evaluateAll((nodes) =>
        nodes.map((node) => node.getAttribute("data-card-id")),
      );
      expect(after[0]).toBe(later.interactionId);
    } finally {
      await cleanup(page.request);
      await context.close();
    }
  });

  test("push landing: /sessions?focus= rings the card on /m and on /sessions", async ({ browser }) => {
    const baseURL = test.info().project.use.baseURL!;
    const phone = await phoneContext(browser, baseURL, "e2e-hqueue-land-phone");
    const page = await phone.newPage();
    const desktop = await browser.newContext({ baseURL, viewport: { width: 1280, height: 900 } });
    try {
      await page.goto("/m");
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");
      // Enough of our own cards that the target can sit behind the fold.
      const parked = [];
      for (let index = 0; index < 4; index += 1) parked.push(await park(page, "mhome-blocked"));
      const target = parked[0];

      // The push payload URL (alerts.rs) → the compact redirect keeps ?focus.
      await page.goto(`/sessions?focus=${target.interactionId}`);
      await expect(page).toHaveURL(`/m?focus=${target.interactionId}`);
      const phoneCard = card(page, target.interactionId);
      await expect(phoneCard).toHaveAttribute("data-focus", "true", { timeout: 20_000 });
      await expect.poll(() => inViewport(phoneCard)).toBe(true);
      await shot(page, "home-queue-390-focus.png");

      const desk = await desktop.newPage();
      await login(desk, "e2e-hqueue-land-desktop");
      await desk.goto(`/sessions?focus=${target.interactionId}`);
      const deskQueue = desk.getByTestId("home-queue");
      await expect(deskQueue).toHaveAttribute("data-placement", "desktop", { timeout: 20_000 });
      const deskCard = card(desk, target.interactionId);
      await expect(deskCard).toHaveAttribute("data-focus", "true", { timeout: 20_000 });
      await expect.poll(() => inViewport(deskCard)).toBe(true);
      // The 2-column grid: the first two cards share a row.
      const tops = await deskQueue
        .getByTestId("home-queue-card")
        .evaluateAll((nodes) => nodes.slice(0, 2).map((node) => Math.round(node.getBoundingClientRect().top)));
      expect(tops).toHaveLength(2);
      expect(tops[0]).toBe(tops[1]);
      await shot(desk, "home-queue-1280-light.png");
      await setMode(desk, "dark");
      await shot(desk, "home-queue-1280-dark.png");
      await setMode(desk, "light");

      // §4.1: the queue hides while the list search has text. A fresh
      // device's default Space has no sessions (no search box), so open the
      // list in global scope, as home-glance does.
      await desk.goto("/sessions?scope=all");
      await expect(desk.getByTestId("home-queue")).toBeVisible({ timeout: 20_000 });
      await expect(desk.getByTestId("session-search")).toBeVisible({ timeout: 20_000 });
      await desk.getByTestId("session-search").fill("hqueue-no-such-session");
      await expect(desk.getByTestId("home-queue")).toHaveCount(0);
      await desk.getByTestId("session-search").fill("");
      await expect(desk.getByTestId("home-queue")).toBeVisible();
    } finally {
      await cleanup(page.request);
      await desktop.close();
      await phone.close();
    }
  });
});
