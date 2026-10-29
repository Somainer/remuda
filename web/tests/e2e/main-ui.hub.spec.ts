import { devices, expect, test, type Browser, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * ma-main-ui (D-057 §9, H8a): the Main entry against the in-process fake node.
 *
 * Main is the address-owner lineage; on the home it is a card plus a 问 Main
 * input (D-061, home.md §6) — no pinned list row. The spec seats one through the public
 * create route (a Human device may grant `address-owner`), so it owns the
 * Hub's single address-owner seat while it runs: every case deletes its seat
 * (force) before the next one seats another. Fake-node behaviour used:
 *  - a plain create raises an approval and journals `echo: <prompt>`; every
 *    send journals `echo: <text>` after the user message;
 *  - the `mhome-exit` create sentinel reports a native session id, then ends
 *    the process — an ended chapter the card offers to resume, and the resume
 *    continues the lineage in chapter 2.
 *
 * It seats a Hub-wide Main, so it runs only when asked: HUB_E2E_MAIN_UI=1.
 * Evidence screenshots stay behind REMUDA_EVIDENCE and land in scratch/.
 */
test.describe.configure({ mode: "serial" });

test.skip(
  process.env.HUB_E2E_MAIN_UI !== "1",
  "set HUB_E2E_MAIN_UI=1: seats the Hub's one address-owner Main on the fake node",
);

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = path.join(here, "../../../scratch/evidence/main-ui");

const created: string[] = [];

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function fakeHost(page: Page): Promise<string> {
  const response = await page.request.get("/v1/hosts");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { items?: { id?: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node");
  expect(host?.id, "fake node host").toBeTruthy();
  return host!.id!;
}

/** Seat an address-owner Main on the fake node through the public route. */
async function seatMain(page: Page, prompt: string): Promise<string> {
  const hostId = await fakeHost(page);
  const response = await page.request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      // The fake node's generic claude path (where its create sentinels run);
      // it advertises no claude-sdk carrier.
      driver: "claude-print",
      prompt,
      title: "Main e2e",
      role: "top-coordinator",
      grants: ["address-owner"],
    },
  });
  expect(response.ok(), `seat Main: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId: string } };
  created.push(body.instance.instanceId);
  return body.instance.instanceId;
}

async function cleanup(page: Page) {
  // Newest first: deleting a lineage's current chapter removes every chapter.
  for (const id of created.splice(0).reverse()) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
}

async function phonePage(browser: Browser) {
  const context = await browser.newContext({ ...devices["iPhone 13"] });
  const page = await context.newPage();
  return { context, page };
}

test.describe("desktop Main entry", () => {
  test.afterEach(async ({ page }) => {
    await cleanup(page);
  });

  test("1280: the card and its input head the main column, and a send echoes then shows the reply", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await login(page);
    const mainId = await seatMain(page, "main-ui seat 1280");
    await page.goto("/sessions");
    const card = page.getByTestId("main-card");
    await expect(card).toBeVisible({ timeout: 20_000 });
    await expect(card).toHaveAttribute("data-main-id", mainId);
    await expect(page.getByTestId("sessions-aside")).toHaveCount(0);
    // The card heads the list column: above the session list header.
    const cardBox = await card.boundingBox();
    const listBox = await page.getByTestId("session-list").boundingBox();
    expect(cardBox && listBox && cardBox.y + cardBox.height <= listBox.y + 1).toBe(true);
    await expect(page.getByTestId("main-row")).toHaveCount(0);
    await expect(card.getByTestId("main-card-chapter")).toHaveText("第 1 章");
    // Line 1 names a live state of the seat (the fake turn may still be open).
    await expect(card.getByTestId("main-card-status")).toHaveText(/^(在问你|空闲|工作中|正在启动)/, { timeout: 20_000 });

    const text = `main-ui ping ${Date.now()}`;
    await card.getByTestId("composer-input").fill(text);
    await card.getByTestId("composer-send").click();
    await expect(card.getByTestId("main-card-reply")).toHaveText(`echo: ${text}`, { timeout: 30_000 });
    await shot(page, "main-ui-1280.png");
  });

  test("1440: the card heads the right column, 工作中 follows it", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await login(page);
    const mainId = await seatMain(page, "main-ui seat 1440");
    await page.goto("/sessions");
    const aside = page.getByTestId("sessions-aside");
    await expect(aside).toBeVisible({ timeout: 20_000 });
    await expect(aside.getByTestId("main-card")).toHaveAttribute("data-main-id", mainId);
    await expect(aside.getByTestId("main-card").getByTestId("composer-input")).toBeVisible();
    await expect(aside.getByTestId("home-working")).toBeVisible();
    await shot(page, "main-ui-1440.png");
  });

  test("an ended Main reads 已停止, 恢复 continues the lineage in chapter 2 with a divider", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await login(page);
    const firstId = await seatMain(page, "main-ui mhome-exit chapter one");
    await page.goto(`/s/${firstId}`);
    const bar = page.getByTestId("main-session-bar");
    await expect(bar).toBeVisible({ timeout: 20_000 });
    await expect(bar.getByTestId("main-status-chapter")).toHaveText("第 1 章");
    await expect(bar.getByTestId("main-resume")).toBeVisible({ timeout: 20_000 });
    await expect(bar.getByTestId("main-pause")).toHaveCount(0);

    await page.goto("/sessions");
    const card = page.getByTestId("main-card");
    await expect(card.getByTestId("main-card-status")).toHaveText(/^已停止/, { timeout: 20_000 });
    await expect(card.getByTestId("composer-send")).toHaveCount(0);
    await card.getByTestId("main-ask-resume").click();
    // The resolution moves to the successor chapter; the input is 发送 again.
    await expect(card.getByTestId("main-card-chapter")).toHaveText("第 2 章", { timeout: 30_000 });
    await expect(card.getByTestId("composer-send")).toBeVisible();
    const secondId = await card.getAttribute("data-main-id");
    expect(secondId).toBeTruthy();
    expect(secondId).not.toBe(firstId);
    created.push(secondId!);

    await page.goto(`/s/${secondId}`);
    await expect(bar.getByTestId("main-status-chapter")).toHaveText("第 2 章", { timeout: 20_000 });
    await expect(bar.getByTestId("main-pause")).toBeVisible();
    await bar.getByTestId("main-chapters-toggle").click();
    await expect(bar.getByTestId("main-chapter-divider")).toHaveCount(2);
    await expect(bar.getByTestId("main-chapter-divider").last()).toContainText("你恢复");
    // An owner resume is not a restart (r2 item 5).
    await expect(bar.getByTestId("main-status-restarts")).toHaveText("24 小时内无重启");
    await shot(page, "main-ui-chapters.png");

    // r2 item 4: the predecessor's page offers no resume, only the current Main.
    await page.goto(`/s/${firstId}`);
    await expect(page.getByTestId("ended-bar")).toBeVisible({ timeout: 20_000 });
    await expect(page.getByTestId("ended-open-current-main")).toHaveAttribute("href", `/s/${secondId}`);
    await expect(page.getByTestId("resume-control")).toHaveCount(0);
    await expect(page.getByTestId("main-resume")).toHaveCount(0);
  });
});

test.describe("phone Main card", () => {
  test("/m: the card sits under the head, the input docks above PhoneNav, and a send echoes the reply", async ({ browser }) => {
    const { context, page } = await phonePage(browser);
    try {
      await login(page);
      const mainId = await seatMain(page, "main-ui seat phone");
      await page.goto("/m");
      const card = page.getByTestId("main-card");
      await expect(card).toBeVisible({ timeout: 20_000 });
      await expect(card).toHaveAttribute("data-placement", "phone");
      await expect(card).toHaveAttribute("data-main-id", mainId);
      // One bottom band: the dock sits directly on PhoneNav.
      const dock = page.getByTestId("main-ask-dock");
      await expect(dock).toBeVisible();
      await expect(card.getByTestId("composer-input")).toHaveCount(0);
      const dockBox = await dock.boundingBox();
      const navBox = await page.getByTestId("phone-nav").boundingBox();
      expect(dockBox && navBox && Math.abs(dockBox.y + dockBox.height - navBox.y) <= 1).toBe(true);
      const text = `main-ui phone ${Date.now()}`;
      await dock.getByTestId("composer-input").tap();
      await dock.getByTestId("composer-input").fill(text);
      await dock.getByTestId("composer-send").tap();
      await expect(card.getByTestId("main-card-reply")).toHaveText(`echo: ${text}`, { timeout: 30_000 });
      // The home never goes blank around the send.
      await expect(page.getByTestId("home-list")).toBeVisible();
      await shot(page, "main-ui-390.png");
    } finally {
      await cleanup(page);
      await context.close();
    }
  });
});
