import { existsSync, type PathLike } from "node:fs";
import { mkdir, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { expect, test, type Page, type Response } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-mobilenew: a phone must always be able to create a session, and bulk
 * screen reads must never starve control RPCs.
 *
 * Requires the additive hub_e2e knobs:
 *   HUB_E2E_EXITED_INSTANCES=14   seed 14 exited shell-pty rows
 * The screen saturation switch is a gate file under TMPDIR (same TMPDIR the
 * fake Node inherited): while it exists, tty.screen for the seeded rows
 * parks, which is how the second case saturates the read budget without
 * touching any other spec.
 */
const seededCount = Number(process.env.HUB_E2E_EXITED_INSTANCES ?? "0");
test.skip(seededCount < 14, "set HUB_E2E_EXITED_INSTANCES=14 for ux-mobile-new");

const listen = process.env.HUB_E2E_LISTEN ?? "127.0.0.1:58880";
const gatePort = new URL(`http://${listen}`).port;
const gateFile: PathLike = path.join(os.tmpdir(), `remuda-e2e-screen-gate-${gatePort}`);

const evidence = process.env.REMUDA_EVIDENCE === "1";
const evidenceDir = path.join(path.dirname(new URL(import.meta.url).pathname), "../../../docs/design/evidence");

interface InstanceRow {
  instanceId?: string;
  id?: string;
  title?: string | null;
  lifecycle?: string;
}

async function seededExitedIds(page: Page): Promise<string[]> {
  const res = await page.request.get("/v1/instances");
  expect(res.ok()).toBeTruthy();
  const body = (await res.json()) as { items?: InstanceRow[] };
  return (body.items ?? [])
    .filter((row) => row.title?.startsWith("e2e-mobile-exited"))
    .map((row) => row.instanceId ?? row.id ?? "")
    .filter(Boolean);
}

/** The phone bottom bar actions. */
function newFromBottomBar(page: Page) {
  return page.locator("nav[aria-label='手机底栏'] button[aria-label='新建']").click();
}

function sessionsFromBottomBar(page: Page) {
  return page.locator("nav[aria-label='手机底栏'] a").filter({ hasText: "会话" }).click();
}

/**
 * Switch to the remuda-e2e space. On mobile this auto-opens the space's first
 * tab (an exited seeded row); return to the board via the bottom bar, which is
 * exactly how a phone gets back to the list.
 */
async function openSeededSpaceBoard(page: Page, exitedIds: string[]) {
  await page.getByTestId("space-chip").filter({ hasText: "remuda-e2e" }).first().click();
  // Mobile auto-opens the space's first tab: it must be one of the exited
  // seeded rows, never a new/foreign session.
  await expect(page).toHaveURL(/\/s\/ins_/);
  const landed = new URL(page.url()).pathname.split("/s/")[1];
  expect(exitedIds).toContain(landed);
  // Exited session pages never pull /screen for anything; let the pollers run.
  await page.waitForTimeout(3_000);
  await sessionsFromBottomBar(page);
  await expect(page).toHaveURL(/\/sessions/);
  await expect(
    page.locator('[data-testid="board-card"][data-lifecycle="exited"]'),
  ).toHaveCount(exitedIds.length);
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
  expect(response.ok()).toBeTruthy();
  const body = (await response.json()) as { instance?: { instanceId?: string } };
  const instanceId = body.instance?.instanceId;
  expect(instanceId).toBeTruthy();
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  await expect(page.getByTestId("session-page")).toBeVisible();
  return instanceId!;
}

/** Statuses of every /screen response the browser observed during a run. */
function collectScreenResponses(page: Page) {
  const statuses = new Map<number, number>();
  const fiveHundreds: string[] = [];
  const onResponse = (response: Response) => {
    const url = new URL(response.url());
    if (!url.pathname.endsWith("/screen")) return;
    statuses.set(response.status(), (statuses.get(response.status()) ?? 0) + 1);
    if (response.status() === 500) fiveHundreds.push(`${response.status()} ${url.pathname}`);
  };
  page.on("response", onResponse);
  return { statuses, fiveHundreds };
}

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(evidenceDir, { recursive: true });
  await page.screenshot({ path: path.join(evidenceDir, name), animations: "disabled" });
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

  test("14 exited rows never cost a /screen 500; create opens the new session", async ({ page }) => {
    await login(page);
    const collected = collectScreenResponses(page);

    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    const exitedIds = await seededExitedIds(page);
    expect(exitedIds.length).toBeGreaterThanOrEqual(14);
    await openSeededSpaceBoard(page, exitedIds);
    await shot(page, "mobile-new-session-1-list-390.png");

    // Let several 2.5s list poll cycles elapse: exited rows must not be read.
    await page.waitForTimeout(6_000);
    const screenPaths = [...collected.statuses.values()].reduce((sum, n) => sum + n, 0);
    expect(screenPaths, "no bulk screen reads are issued for exited rows").toBe(0);
    expect(collected.fiveHundreds).toEqual([]);

    // Create from the list (bottom-bar 新建), landing on the session page.
    await newFromBottomBar(page);
    const firstId = await createSessionViaSheet(page, "mobile new session repro one");
    created.push(firstId);
    await shot(page, "mobile-new-session-1-session-390.png");

    // From the open session page, 新建 again: the dimmed list behind the
    // sheet must not starve the create with per-row reads.
    await newFromBottomBar(page);
    const secondId = await createSessionViaSheet(page, "mobile new session repro two");
    created.push(secondId);

    expect(collected.fiveHundreds, `no /screen 500: ${collected.fiveHundreds.join(", ")}`).toEqual([]);
  });

  test("NODE_BUSY on create shows the Hub code and message inline and keeps the form", async ({ page }) => {
    await login(page);
    await page.route("**/v1/instances", async (route) => {
      if (route.request().method() !== "POST") {
        await route.continue();
        return;
      }
      await route.fulfill({
        status: 503,
        contentType: "application/json",
        body: JSON.stringify({
          code: "NODE_BUSY",
          error: "node busy: too many in-flight node rpcs; retry after 2500 ms",
          retryAfterMs: 2500,
          retryable: true,
        }),
      });
    });
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await page.getByTestId("new-session-prompt").fill("mobile new session busy");
    await page.getByTestId("new-session-start").click();
    const errorBox = page.getByTestId("new-session-error");
    await expect(errorBox).toBeVisible();
    await expect(errorBox).toContainText("NODE_BUSY");
    await expect(errorBox).toContainText("too many in-flight");
    // Form kept, prompt intact, primary action usable again.
    await expect(page).toHaveURL(/\/sessions\/new/);
    await expect(page.getByTestId("new-session-prompt")).toHaveValue("mobile new session busy");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await shot(page, "mobile-new-session-1-node-busy-390.png");
  });

  test("saturated screen reads still let instance.create through", async ({ page }) => {
    test.setTimeout(120_000);
    await login(page);
    const collected = collectScreenResponses(page);
    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    const exitedIds = await seededExitedIds(page);
    expect(exitedIds.length).toBeGreaterThanOrEqual(14);
    await openSeededSpaceBoard(page, exitedIds);

    // Park every seeded screen read on the Node side.
    await mkdir(path.dirname(gateFile.toString()), { recursive: true });
    await writeFile(gateFile, "block");
    expect(existsSync(gateFile)).toBe(true);

    // Fan out far past the Hub's 32-RPC link budget, cycling the 14 rows.
    // page.request carries the login cookie; the Hub caps at 16 bulk reads,
    // admits control independently, and answers excess reads 503 NODE_BUSY.
    const requests = Array.from({ length: 40 }, (_, index) => {
      const id = exitedIds[index % exitedIds.length];
      return page.request.get(`/v1/instances/${id}/screen?lines=80`);
    });
    // Let the admissible reads park before creating.
    await page.waitForTimeout(800);

    await newFromBottomBar(page);
    const instanceId = await createSessionViaSheet(page, "mobile new session under saturation");
    created.push(instanceId);

    const settled = await Promise.allSettled(requests);
    const statuses = settled.map((entry) =>
      entry.status === "fulfilled" ? entry.value.status() : -1,
    );
    const fiveHundreds = statuses.filter((status) => status === 500);
    const busy = statuses.filter((status) => status === 503);

    // Reads saturate: the retryable refusal is observed, never INTERNAL.
    expect(busy.length, "excess reads get NODE_BUSY/503").toBeGreaterThan(0);
    expect(fiveHundreds, "no bulk read may surface as 500 INTERNAL").toHaveLength(0);
    expect(
      collected.fiveHundreds,
      `browser saw no /screen 500: ${collected.fiveHundreds.join(", ")}`,
    ).toEqual([]);

    // Release the parked reads so their late replies drain before teardown.
    await rm(gateFile, { force: true });
  });
});
