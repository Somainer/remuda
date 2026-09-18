import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * c-touchhit (P0-2 / P1-8): touch hit areas and the .meta type floor.
 *
 * Visual glyph sizes stay as they were (back 20px, stop 32px, viewSeg
 * 25/30px); the 44px reach is supplied by an invisible `::after`, never by a
 * visual resize (ui-spec.md §3.4, D-039). jsdom cannot measure geometry, so
 * this spec is the binding check: it measures the real boxes in Chromium at
 * 390×844 with touch, and re-checks that the hit area is width-driven (it is
 * still there without a touch pointer) while desktop keeps the small visual
 * segment and no ::after.
 *
 * A default Claude session on the fake Node launches on shell-pty, so it has
 * both the 终端 / 结构 ViewSwitch and the composer effort controls.
 */
test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

const created: string[] = [];

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function createClaudeSession(page: Page): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await page.getByTestId("new-session-kind-claude").click();
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-prompt").fill("touch hit geometry");
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  // The effort controls live in the structured-view composer dock; a fresh
  // shell-pty session may auto-resolve to the tty view, so pin structured.
  await page.goto(`/s/${id}/structured`);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
  await expect(page.getByTestId("session-page")).toHaveAttribute(
    "data-lifecycle",
    "running",
    { timeout: 20_000 },
  );
  await expect(page.getByTestId("composer")).toBeVisible();
  return id;
}

/** Resolve any pending approval the fake harness emits, same as ux-permmode. */
async function clearApprovals(page: Page, instanceId: string) {
  await page.evaluate(async (id) => {
    const listPending = async () => {
      const body = await (
        await fetch("/v1/interactions", { credentials: "include" })
      ).json();
      return (body.items ?? []).filter(
        (item: { instanceId?: string; state?: string }) =>
          item.instanceId === id && item.state === "pending",
      );
    };
    let mine = await listPending();
    const deadline = Date.now() + 10_000;
    for (const item of mine) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.interactionId ?? item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          answer: {
            kind: "approval",
            optionId,
            inputDigest: item.request?.inputDigest ?? "",
          },
        }),
      });
    }
    while (mine.length > 0 && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 100));
      mine = await listPending();
    }
    return mine;
  }, instanceId);
}

async function forceDeleteAllInstances(page: Page) {
  await page.evaluate(async () => {
    const list = await fetch("/v1/instances", { credentials: "include" });
    const body = (await list.json()) as { items?: { id: string }[] };
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

test.afterAll(async ({ browser }) => {
  const cleanup = await browser.newPage();
  await login(cleanup);
  await forceDeleteAllInstances(cleanup).catch(() => undefined);
  await cleanup.close();
});

/**
 * The clickable box of an element plus its invisible ::after. boundingBox()
 * is the visual box; the pseudo supplies the reach centred on it.
 */
async function touchReach(locator: Locator) {
  const box = await locator.boundingBox();
  expect(box, "target is rendered").toBeTruthy();
  const after = await locator.evaluate((el) => {
    const pseudo = getComputedStyle(el, "::after");
    return { width: parseFloat(pseudo.width), height: parseFloat(pseudo.height) };
  });
  return {
    visual: { width: box!.width, height: box!.height },
    reach: {
      width: Math.max(box!.width, Number.isFinite(after.width) ? after.width : 0),
      height: Math.max(box!.height, Number.isFinite(after.height) ? after.height : 0),
    },
  };
}

async function metaFontSize(page: Page): Promise<number> {
  return page.getByTestId("session-meta").evaluate((el) =>
    parseFloat(getComputedStyle(el).fontSize),
  );
}

async function assertTouchControls(page: Page) {
  const table: Record<string, { visual: { w: number; h: number }; reach: { w: number; h: number } }> = {};
  // Back (20px glyph) and Stop (32px square) reach 44px only via ::after.
  const back = page.getByRole("link", { name: "返回" });
  const stop = page.getByRole("button", { name: "Stop" });
  await expect(back).toBeVisible();
  await expect(stop).toBeVisible();
  for (const [name, target] of [["back", back], ["stop", stop]] as const) {
    const m = await touchReach(target);
    table[name] = {
      visual: { w: Math.round(m.visual.width), h: Math.round(m.visual.height) },
      reach: { w: Math.round(m.reach.width), h: Math.round(m.reach.height) },
    };
    expect(m.reach.width).toBeGreaterThanOrEqual(44);
    expect(m.reach.height).toBeGreaterThanOrEqual(44);
  }

  // Both segments of the 终端 / 结构 switch keep their visual height but have
  // a 44px hit area, and the radiogroup semantics are untouched.
  const switchBox = page.getByTestId("view-switch");
  await expect(switchBox).toHaveAttribute("role", "radiogroup");
  for (const id of ["tty", "structured"] as const) {
    const seg = page.getByTestId(`view-switch-${id}`);
    expect(seg).toHaveAttribute("role", "radio");
    const m = await touchReach(seg);
    table[`viewSeg-${id}`] = {
      visual: { w: Math.round(m.visual.width), h: Math.round(m.visual.height) },
      reach: { w: Math.round(m.reach.width), h: Math.round(m.reach.height) },
    };
    // The segment itself never grew to 44px…
    expect(m.visual.height).toBeLessThan(44);
    // …the invisible hit area did.
    expect(m.reach.width).toBeGreaterThanOrEqual(44);
    expect(m.reach.height).toBeGreaterThanOrEqual(44);
  }

  // The two 26px effort glyph buttons (reset on the pill; list-back on the
  // tier list) keep their glyph box and an ::after, now on var(--touch).
  await page.getByTestId("model-effort-chip").click();
  await expect(page.getByTestId("effort-menu")).toBeVisible();
  const reset = page.getByTestId("effort-reset");
  let measured = await touchReach(reset);
  table["effort-reset"] = {
    visual: { w: Math.round(measured.visual.width), h: Math.round(measured.visual.height) },
    reach: { w: Math.round(measured.reach.width), h: Math.round(measured.reach.height) },
  };
  expect(measured.visual.height).toBeLessThanOrEqual(26);
  expect(measured.reach.width).toBeGreaterThanOrEqual(44);
  expect(measured.reach.height).toBeGreaterThanOrEqual(44);

  await page.getByTestId("effort-open-list").click();
  await expect(page.getByTestId("effort-slider-panel")).toBeVisible();
  const listBack = page.getByTestId("effort-list-back");
  measured = await touchReach(listBack);
  table["effort-list-back"] = {
    visual: { w: Math.round(measured.visual.width), h: Math.round(measured.visual.height) },
    reach: { w: Math.round(measured.reach.width), h: Math.round(measured.reach.height) },
  };
  expect(measured.visual.height).toBeLessThanOrEqual(26);
  expect(measured.reach.width).toBeGreaterThanOrEqual(44);
  expect(measured.reach.height).toBeGreaterThanOrEqual(44);

  console.log(`TOUCHHIT-390 ${JSON.stringify(table)}`);
  console.log(`TOUCHHIT-META-390 ${await metaFontSize(page)}`);
}

test("header controls carry 44px hit areas at 390px with touch while visuals stay small", async ({
  browser,
}) => {
  // hasTouch is a context property, not a viewport one: emulate a phone
  // instead of shrinking a desktop window.
  const context = await browser.newContext({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
  });
  const page = await context.newPage();
  try {
    await login(page);
    const instanceId = await createClaudeSession(page);
    await clearApprovals(page, instanceId);

    expect(await metaFontSize(page)).toBeGreaterThanOrEqual(12);
    await shot(page, "ux2026-touchhit-1-390.png");
    await assertTouchControls(page);
  } finally {
    await context.close();
  }
});

test("the 44px hit area is width-driven, so a 390px window without touch keeps it", async ({
  page,
}) => {
  // Compact layout is a width question; dropping touch must not silently
  // shrink the hit area (the compact/coarse-pointer distinction).
  await page.setViewportSize({ width: 390, height: 844 });
  const instanceId = await createClaudeSession(page);
  await clearApprovals(page, instanceId);

  const seg = page.getByTestId("view-switch-structured");
  const { reach } = await touchReach(seg);
  expect(reach.width).toBeGreaterThanOrEqual(44);
  expect(reach.height).toBeGreaterThanOrEqual(44);
});

test("desktop keeps the small visual segment with no ::after and .meta at 12px", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  const instanceId = await createClaudeSession(page);
  await clearApprovals(page, instanceId);

  expect(await metaFontSize(page)).toBeGreaterThanOrEqual(12);
  // No back link is rendered on desktop, and the segment is the original
  // 25px visual with no pseudo-element reach attached.
  const seg = page.getByTestId("view-switch-tty");
  const box = await seg.boundingBox();
  expect(box).toBeTruthy();
  expect(box!.height).toBeLessThan(44);
  const afterSize = await seg.evaluate((el) => {
    const pseudo = getComputedStyle(el, "::after");
    return { width: parseFloat(pseudo.width), height: parseFloat(pseudo.height) };
  });
  expect(Number.isFinite(afterSize.width)).toBe(false);
  expect(Number.isFinite(afterSize.height)).toBe(false);
  console.log(
    `TOUCHHIT-1440 viewSeg visual ${Math.round(box!.width)}x${Math.round(box!.height)} ::after=${JSON.stringify(afterSize)}`,
  );
});
