import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * c-sessionchrome (P0-1 / P0-5, D-040): compact session chrome and the
 * per-device 运行详情 disclosure.
 *
 * Geometry is the contract: at 390 the full spaces strip folds into one
 * current-space chip, the secondary toggles move into ⋯ (never the view
 * switch, never Stop), and every main-row control owns its 44px hot-zone
 * corners with no overlaps (D-039). At 1440 the diagnostics sit behind a
 * collapsed disclosure while host + cost + switch + Stop keep the main row.
 *
 * A default Claude session on the fake Node launches on shell-pty, so it has
 * the 终端 / 结构 ViewSwitch.
 */
test.describe.configure({ mode: "serial" });

const TOUCH = 44;
const HALF = TOUCH / 2;
// session-body top on this fixture BEFORE the reclaim (measured 2026-09-19):
// chips 56 + tabs 47 + two-row header 121 + gaps → 226.17px. The plan
// requires the compact fold to give the transcript at least 56px back.
const BASELINE_BODY_TOP_390 = 226.17;
const MIN_GAIN = 56;

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

async function resolveHost(page: Page): Promise<string> {
  const response = await page.request.get("/v1/hosts");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { items?: { id: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  expect(host, "fake node host").toBeTruthy();
  return host!.id;
}

/**
 * Create through the API, not the New-Session sheet: under a loaded serial
 * run the sheet's client-side create→navigate occasionally settles just past
 * the UI navigation wait (the instance lands fine, the page just has not
 * moved yet). Geometry under test is the session header, which is identical
 * either way — same pattern as ux-files.hub.spec.ts.
 */
async function createClaudeSession(page: Page): Promise<string> {
  const hostId = await resolveHost(page);
  const response = await page.request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "shell-pty",
      prompt: "session chrome geometry",
    },
  });
  expect(response.ok(), `create instance: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId: string } };
  const id = body.instance.instanceId;
  created.push(id);
  await page.goto(`/s/${id}/structured`);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-lifecycle", "running", {
    timeout: 20_000,
  });
  await expect(page.getByTestId("composer")).toBeVisible();
  return id;
}

async function clearApprovals(page: Page, instanceId: string) {
  await page.evaluate(async (id) => {
    const listPending = async () => {
      const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
      return (body.items ?? []).filter(
        (item: { instanceId?: string; state?: string }) =>
          item.instanceId === id && item.state === "pending",
      );
    };
    for (const item of await listPending()) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.interactionId ?? item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" },
        }),
      });
    }
  }, instanceId);
}

async function forceDeleteAllInstances(page: Page) {
  await page.evaluate(async () => {
    const body = (await (await fetch("/v1/instances", { credentials: "include" })).json()) as {
      items?: { id: string }[];
    };
    await Promise.all(
      (body.items ?? []).map((instance) =>
        fetch(`/v1/instances/${instance.id}?force=1`, {
          method: "DELETE",
          credentials: "include",
        }).catch(() => undefined),
      ),
    );
  });
}

test.beforeEach(async ({ page }) => {
  await login(page);
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

/**
 * This file runs ~60 specs deep in the serial hub run; earlier specs can
 * leave the fake node's 8-instance cap saturated, so a placement 422 would
 * reject our create even though we never touched those instances. Raise the
 * cap for this file (same established pattern as ux-comment.hub.spec.ts) and
 * put it back in afterAll.
 */
let previousMax = 8;

async function patchMaxInstances(page: Page, value: number): Promise<number | undefined> {
  const response = await page.request.get("/v1/hosts");
  const body = (await response.json()) as {
    items?: { id?: string; maxInstances?: number; label?: string }[];
  };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  if (!host?.id) return undefined;
  const previous = host.maxInstances;
  await page.request.patch(`/v1/hosts/${host.id}`, { data: { maxInstances: value } });
  return previous;
}

test.beforeAll(async ({ browser }) => {
  const setup = await browser.newPage();
  await login(setup);
  previousMax = (await patchMaxInstances(setup, 24)) ?? previousMax;
  await setup.close();
});

test.afterAll(async ({ browser }) => {
  const cleanup = await browser.newPage();
  await login(cleanup);
  await forceDeleteAllInstances(cleanup).catch(() => undefined);
  await patchMaxInstances(cleanup, previousMax).catch(() => undefined);
  await cleanup.close();
});

type Box = { x: number; y: number; width: number; height: number };

async function ownerAt(page: Page, x: number, y: number): Promise<string> {
  return page.evaluate(
    ({ x, y }) =>
      document
        .elementFromPoint(x, y)
        ?.closest("[data-chrome-owner]")
        ?.getAttribute("data-chrome-owner") ?? "none",
    { x, y },
  );
}

async function mark(locator: Locator, owner: string) {
  await locator.evaluate(
    (el, value) => el.setAttribute("data-chrome-owner", value),
    owner,
  );
}

/** Visible box inside the viewport and the four 44px corners owned by self. */
async function assertHotTarget(page: Page, target: Locator, owner: string, vw: number, vh: number): Promise<Box> {
  await expect(target).toBeVisible();
  const box = await target.boundingBox();
  expect(box, `${owner} renders`).toBeTruthy();
  expect(box!.x >= -0.5 && box!.y >= -0.5 && box!.x + box!.width <= vw + 0.5 && box!.y + box!.height <= vh + 0.5,
    `${owner} inside the ${vw}px viewport`).toBe(true);
  const cx = box!.x + box!.width / 2;
  const cy = box!.y + box!.height / 2;
  const clampX = (x: number) => Math.min(Math.max(x, 0.5), vw - 0.5);
  const clampY = (y: number) => Math.min(Math.max(y, 0.5), vh - 0.5);
  for (const [name, x, y] of [
    ["tl", clampX(cx - HALF + 0.5), clampY(cy - HALF + 0.5)],
    ["tr", clampX(cx + HALF - 0.5), clampY(cy - HALF + 0.5)],
    ["bl", clampX(cx - HALF + 0.5), clampY(cy + HALF - 0.5)],
    ["br", clampX(cx + HALF - 0.5), clampY(cy + HALF - 0.5)],
  ] as const) {
    expect(await ownerAt(page, x, y), `${owner} owns its ${name} 44px corner`).toBe(owner);
  }
  console.log(
    `CHROME ${owner} visual ${Math.round(box!.width)}x${Math.round(box!.height)} at (${Math.round(box!.x)},${Math.round(box!.y)})`,
  );
  return box!;
}

async function markHeader(page: Page) {
  await mark(page.getByRole("link", { name: "返回" }), "back");
  await mark(page.getByTestId("spaces-drawer-open"), "space-chip");
  await mark(page.getByTestId("view-switch-tty"), "seg-tty");
  await mark(page.getByTestId("view-switch-structured"), "seg-struct");
  await mark(page.getByRole("button", { name: "Stop" }), "stop");
  await mark(page.getByTestId("session-more-open"), "more");
  await mark(page.getByTestId("run-details-summary"), "details");
}

test("390px: the chips strip folds, Stop and the switch stay reachable, and every hot zone is disjoint", async ({
  browser,
}) => {
  // The fold is a LAYOUT question, so it must hold both with and without
  // touch (plan risk 5): run the same assertions in both contexts.
  for (const touch of [true, false]) {
    const context = await browser.newContext({
      viewport: { width: 390, height: 844 },
      hasTouch: touch,
      ...(touch ? { isMobile: true } : {}),
    });
    const page = await context.newPage();
    try {
      await login(page);

      // Baseline: the index route still carries the full chips strip.
      await page.goto("/sessions");
      await expect(page.getByTestId("spaces-chips").first()).toBeVisible();
      expect(await page.getByTestId("space-chip").count()).toBeGreaterThan(0);

      const instanceId = await createClaudeSession(page);
      await clearApprovals(page, instanceId);
      await markHeader(page);

      // The strip's per-space chips are gone; the single current-space chip
      // lives INSIDE the session header and opens the same drawer.
      expect(await page.getByTestId("space-chip").count()).toBe(0);
      const headerChips = page.locator("header").getByTestId("spaces-chips");
      await expect(headerChips).toBeVisible();
      await page.getByTestId("spaces-drawer-open").click();
      await expect(page.getByTestId("spaces-drawer")).toBeVisible();
      // Close through the explicit button and WAIT for the backdrop to
      // unmount: under a loaded serial run an immediate next click otherwise
      // races the teardown and lands on the backdrop instead of the header.
      await page.getByRole("button", { name: "关闭空间面板" }).click();
      await expect(page.getByTestId("spaces-drawer")).toHaveCount(0);

      // Compact / 文件 / 原始事件 move into ⋯; the switch and Stop do not.
      await expect(page.getByTestId("density-toggle")).toHaveCount(0);
      await expect(page.getByTestId("files-toggle")).toHaveCount(0);
      await expect(page.getByTestId("events-toggle")).toHaveCount(0);
      await page.getByTestId("session-more-open").click();
      const sheet = page.getByTestId("session-more-sheet");
      await expect(sheet).toBeVisible();
      await expect(sheet.getByRole("menuitem")).toHaveCount(3);
      await expect(sheet.getByTestId("density-toggle")).toBeVisible();
      await expect(sheet.getByTestId("files-toggle")).toBeVisible();
      await expect(sheet.getByTestId("events-toggle")).toBeVisible();
      expect(await sheet.getByTestId("view-switch").count()).toBe(0);
      expect(await sheet.getByRole("button", { name: "Stop" }).count()).toBe(0);

      // 原始事件 navigates through the sheet.
      await sheet.getByTestId("events-toggle").click();
      await expect(page).toHaveURL(/\/events$/);
      await page.goto(`/s/${instanceId}/structured`);
      await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
      await markHeader(page);

      // No horizontal page overflow.
      const overflow = await page.evaluate(() => document.documentElement.scrollWidth);
      expect(overflow).toBeLessThanOrEqual(390);

      // Geometry: each reachable control owns its 44px corners and the zones
      // never overlap (corner ownership proves the D-039 borders).
      await assertHotTarget(page, page.getByRole("link", { name: "返回" }), "back", 390, 844);
      await assertHotTarget(page, page.getByTestId("spaces-drawer-open"), "space-chip", 390, 844);
      await assertHotTarget(page, page.getByTestId("view-switch-tty"), "seg-tty", 390, 844);
      await assertHotTarget(page, page.getByTestId("view-switch-structured"), "seg-struct", 390, 844);
      await assertHotTarget(page, page.getByRole("button", { name: "Stop" }), "stop", 390, 844);
      await assertHotTarget(page, page.getByTestId("session-more-open"), "more", 390, 844);
      await assertHotTarget(page, page.getByTestId("run-details-summary"), "details", 390, 844);

      // The fold gave the transcript at least 56px of vertical space.
      const bodyBox = await page.getByTestId("session-body").boundingBox();
      expect(bodyBox).toBeTruthy();
      console.log(
        `CHROME body top ${touch ? "touch" : "no-touch"}=${Math.round(bodyBox!.y)} baseline=${BASELINE_BODY_TOP_390}`,
      );
      expect(bodyBox!.y, "session body top vs the pre-reclaim 226.17px baseline").toBeLessThanOrEqual(
        BASELINE_BODY_TOP_390 - MIN_GAIN,
      );

      await shot(page, "ux2026-chrome-1-390.png");
    } finally {
      await context.close();
    }
  }
});

test("1440px: diagnostics hide behind a collapsed per-device disclosure while host/cost/switch/Stop keep the main row", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  const instanceId = await createClaudeSession(page);
  await clearApprovals(page, instanceId);

  // Main-row citizens, all inside the viewport.
  const vw = 1440;
  for (const [name, target] of [
    ["host", page.getByTestId("session-host")],
    ["cost", page.getByTestId("session-cost")],
    ["switch", page.getByTestId("view-switch")],
    ["stop", page.getByRole("button", { name: "Stop" })],
    ["density", page.getByTestId("density-toggle")],
    ["files", page.getByTestId("files-toggle")],
    ["events", page.getByTestId("events-toggle")],
  ] as const) {
    const box = await target.boundingBox();
    expect(box, `${name} renders`).toBeTruthy();
    expect(box!.x + box!.width <= vw + 0.5, `${name} inside the viewport`).toBe(true);
    // Center-point ownership (no ::after on desktop).
    await mark(target, name);
    expect(
      await ownerAt(page, box!.x + box!.width / 2, box!.y + box!.height / 2),
      `${name} owns its centre`,
    ).toBe(name);
    console.log(`CHROME-1440 ${name} ${Math.round(box!.width)}x${Math.round(box!.height)} at x=${Math.round(box!.x)}`);
  }
  expect(await page.getByTestId("session-more-open").count()).toBe(0);
  await expect(page.getByTestId("session-host")).not.toHaveText("");

  // The disclosure exists, starts collapsed, and the diagnostics are hidden.
  const details = page.getByTestId("run-details");
  await expect(details).toHaveCount(1);
  expect(await details.evaluate((el) => (el as HTMLDetailsElement).open)).toBe(false);
  await expect(page.getByTestId("session-meta")).not.toBeVisible();
  await expect(page.getByTestId("session-driver")).toBeHidden();
  await shot(page, "ux2026-chrome-1-1440.png");

  // One fold reveals seq / connectivity / driver.
  await page.getByTestId("run-details-summary").click();
  await expect(page.getByTestId("session-meta")).toBeVisible();
  await expect(page.getByTestId("session-driver")).toContainText(/pty|print/);
  const meta = page.getByTestId("session-meta");
  await expect(meta).toContainText(/seq \d+/);
  await expect(meta).toContainText(/connected|offline|degraded|reconnecting/);
  await shot(page, "ux2026-chrome-1-1440-open.png");

  // Per-device persistence: a reload keeps it open.
  await page.reload();
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
  expect(await page.getByTestId("run-details").evaluate((el) => (el as HTMLDetailsElement).open)).toBe(true);

  await shot(page, "ux2026-chrome-1-1440.png");
});
