import { devices, expect, test, type Locator, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * seen-web (home.md §7, §9).
 *
 * A finished turn — the fake node settles a normal send back to idle — shows
 * in ⑤ and in the ◉ count; 打开 clears it; a long-press at 390 opens the row
 * menu below the sticky head; 关闭… names the host.
 *
 * Self-skipping: needs the in-process fake Node (`e2e-fake-node`). Evidence
 * screenshots only when REMUDA_EVIDENCE=1. No millisecond thresholds.
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

const created: string[] = [];
const createdTasks: string[] = [];

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

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

async function createSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  await hostPicker.selectOption(hostId!);
  const workspacePicker = page.getByTestId("new-session-workspace");
  await expect(workspacePicker.locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await workspacePicker.selectOption("wsp_e2e");
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

/** A normal send settles the fake node's turn back to idle, which is a ◉ row. */
async function finishTurn(page: Page, id: string): Promise<void> {
  const result = await page.request.post(`/v1/instances/${id}/commands`, {
    data: { operation: "instance.send", payload: { prompt: "settle the turn" } },
  });
  expect(result.ok(), `instance.send: ${result.status()}`).toBe(true);
}

/**
 * An owner-blocked task with no session: a 任务待你 row in the phone task layer
 * (§4.6). Archived at the end — a leftover blocked task changes the next
 * spec's exact counts.
 */
async function blockedTask(page: Page): Promise<string> {
  const suffix = Date.now().toString(36);
  const project = await page.request.post("/v1/projects", { data: { name: `seenweb ${suffix}` } });
  expect(project.ok(), `POST /v1/projects: ${project.status()}`).toBe(true);
  const projectId = ((await project.json()) as { id: string }).id;
  const createdTask = await page.request.post("/v1/tasks", {
    data: { projectId, title: `seenweb blocked ${suffix}`, intent: `e2e seen order ${suffix}` },
  });
  expect(createdTask.ok(), `POST /v1/tasks: ${createdTask.status()}`).toBe(true);
  const taskId = ((await createdTask.json()) as { id: string }).id;
  createdTasks.push(taskId);
  const patched = await page.request.patch(`/v1/tasks/${taskId}`, {
    data: { state: "failed", reason: `等你确认验收范围 ${suffix}` },
  });
  expect(patched.ok(), `PATCH /v1/tasks/${taskId}: ${patched.status()}`).toBe(true);
  return taskId;
}

test.afterEach(async ({ page }) => {
  const ids = created.splice(0);
  for (const id of ids) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
  for (const id of createdTasks.splice(0)) {
    await page.request.post(`/v1/tasks/${id}/archive`, { data: {} }).catch(() => undefined);
  }
});

test("a finished turn shows in ⑤ and the count, and 打开 clears it", async ({ page }) => {
  await login(page);
  test.skip(!(await needsFakeNode(page)), "needs the in-process fake node");
  // Record which page issued each seen write: the 打开 click itself must post
  // from /sessions, or the assertion would only prove that visiting the
  // session page clears the ◉.
  await page.addInitScript(() => {
    const w = window as unknown as { __seenPosts?: string[] };
    w.__seenPosts = [];
    const original = window.fetch.bind(window);
    window.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
      const method = (init?.method ?? (input instanceof Request ? input.method : "GET")).toUpperCase();
      if (method === "POST" && new URL(url, location.href).pathname.endsWith("/seen")) {
        w.__seenPosts!.push(location.pathname);
      }
      return original(input, init);
    };
  });
  const prompt = `seenweb finished ${Date.now().toString(36)}`;
  const id = await createSession(page, prompt);
  await finishTurn(page, id);

  await page.goto("/sessions");
  const section = page.getByTestId("home-just-done");
  await expect(section).toBeVisible({ timeout: 20_000 });
  await expect(section).toContainText(prompt);
  await expect(page.getByTestId("glance-done-unseen")).toBeVisible();

  await section.getByTestId("just-done-open").first().click();
  await expect(page).toHaveURL(new RegExp(`/s/${id}`));
  // The click's own handler posted seen while the list was still the page;
  // the session page's reporter would have recorded /s/<id> instead.
  await expect
    .poll(() => page.evaluate(() => (window as unknown as { __seenPosts?: string[] }).__seenPosts ?? []))
    .toContain("/sessions");
  await page.goto("/sessions");
  await expect(page.getByTestId("home-just-done")).toHaveCount(0, { timeout: 20_000 });
  await shot(page, "seenweb-opened.png");
});

test("390: ⑤ paints below the whole queue, including 任务待你", async ({ browser }) => {
  const context = await browser.newContext({ ...devices["iPhone 13"] });
  const page = await context.newPage();
  try {
    await login(page, "e2e-seenweb-order");
    test.skip(!(await needsFakeNode(page)), "needs the in-process fake node");
    await blockedTask(page);
    const id = await createSession(page, `seenweb order ${Date.now().toString(36)}`);
    await finishTurn(page, id);

    await page.goto("/m");
    const tasks = page.getByTestId("home-tasks");
    const done = page.getByTestId("home-just-done");
    await expect(tasks).toBeVisible({ timeout: 20_000 });
    await expect(done).toBeVisible({ timeout: 20_000 });
    const tasksBox = await tasks.boundingBox();
    const doneBox = await done.boundingBox();
    expect(tasksBox).not.toBeNull();
    expect(doneBox).not.toBeNull();
    // ⑤ after the entire queue: the task layer's painted box ends above it.
    expect(tasksBox!.y + tasksBox!.height).toBeLessThanOrEqual(doneBox!.y + 1);
    await shot(page, "seenweb-order-390.png");
  } finally {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
    for (const id of createdTasks.splice(0)) {
      await page.request.post(`/v1/tasks/${id}/archive`, { data: {} }).catch(() => undefined);
    }
    await context.close();
  }
});

test("1440: hovering a ⑦ and a ⑥ row reveals the ⋯, and each menu opens", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await login(page);
  test.skip(!(await needsFakeNode(page)), "needs the in-process fake node");
  // A normal create parks the fake turn on an approval; the `hold-working`
  // send models an agent still inside a long turn, so the row ranks 工作中.
  const prompt = `seenweb hover ${Date.now().toString(36)}`;
  const id = await createSession(page, prompt);
  const held = await page.request.post(`/v1/instances/${id}/commands`, {
    data: { operation: "instance.send", payload: { prompt: "hold-working keep the turn" } },
  });
  expect(held.ok(), `instance.send: ${held.status()}`).toBe(true);
  await page.goto("/sessions");

  const opacityOf = (locator: Locator) => locator.evaluate((el) => getComputedStyle(el).opacity);

  // ⑦ 台账 row in the main column.
  const card = page.locator('[data-testid="board-card"]').filter({ hasText: prompt }).first();
  await expect(card).toBeVisible({ timeout: 20_000 });
  const more = card.getByTestId("row-actions-more");
  expect(await opacityOf(more)).toBe("0");
  await card.hover();
  await expect.poll(() => opacityOf(more)).toBe("1");
  await more.click();
  await expect(page.getByTestId("row-actions-menu")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("row-actions-menu")).toHaveCount(0);

  // ⑥ 工作中 in the ≥1440 aside.
  const working = page.locator('[data-testid="home-working-row"]').filter({ hasText: prompt }).first();
  await expect(working).toBeVisible({ timeout: 20_000 });
  const moreWorking = working.getByTestId("row-actions-more");
  expect(await opacityOf(moreWorking)).toBe("0");
  await working.hover();
  await expect.poll(() => opacityOf(moreWorking)).toBe("1");
  await moreWorking.click();
  await expect(page.getByTestId("row-actions-menu")).toBeVisible();
  await expect(page.getByTestId("row-action-interrupt")).toBeVisible();
  await shot(page, "seenweb-hover-1440.png");
});

test("a long-press at 390 opens the row menu below the sticky head, and 关闭… names the host", async ({ browser }) => {
  const context = await browser.newContext({ ...devices["iPhone 13"] });
  const page = await context.newPage();
  try {
    await login(page, "e2e-seenweb-phone");
    test.skip(!(await needsFakeNode(page)), "needs the in-process fake node");
    await createSession(page, `seenweb menu ${Date.now().toString(36)}`);

    await page.goto("/m");
    const row = page.getByTestId("home-row").first();
    await expect(row).toBeVisible({ timeout: 20_000 });
    const box = await row.boundingBox();
    expect(box).not.toBeNull();
    // A press held past the long-press threshold. The steps option spaces the
    // move so it is a hold, not a flick, and the assertion waits on the menu.
    await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
    await page.mouse.down();
    await expect(page.getByTestId("row-actions-menu")).toBeVisible({ timeout: 5_000 });
    await page.mouse.up();

    const menu = page.getByTestId("row-actions-menu");
    const head = await page.getByTestId("home-head").boundingBox();
    const menuBox = await menu.boundingBox();
    expect(head).not.toBeNull();
    expect(menuBox).not.toBeNull();
    expect(menuBox!.y).toBeGreaterThanOrEqual(head!.y + head!.height);

    await page.getByTestId("row-action-close").click();
    const confirm = page.getByTestId("row-close-confirm");
    await expect(confirm).toContainText("e2e-fake-node");
    await shot(page, "seenweb-menu-390.png");
  } finally {
    // The default page never logged in, so the shared afterEach cannot delete
    // this context's session; a leftover working row changes m-home's counts.
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
    await context.close();
  }
});
