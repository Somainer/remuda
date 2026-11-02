import { devices, expect, test, type Page } from "@playwright/test";
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

test.afterEach(async ({ page }) => {
  const ids = created.splice(0);
  for (const id of ids) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

test("a finished turn shows in ⑤ and the count, and 打开 clears it", async ({ page }) => {
  await login(page);
  test.skip(!(await needsFakeNode(page)), "needs the in-process fake node");
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
  await page.goto("/sessions");
  await expect(page.getByTestId("home-just-done")).toHaveCount(0, { timeout: 20_000 });
  await shot(page, "seenweb-opened.png");
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
