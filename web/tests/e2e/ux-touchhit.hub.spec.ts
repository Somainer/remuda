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
 * visual resize (ui-spec.md §3.4, D-039). Geometry — not getComputedStyle —
 * is the contract: the control's box must be inside the viewport and
 * elementFromPoint at the four hit-zone corners must land on the control. A
 * computed ::after size cannot catch a deleted centring transform, a covered
 * pseudo, or a target parked off-screen.
 *
 * A default Claude session on the fake Node launches on shell-pty, so it has
 * both the 终端 / 结构 ViewSwitch and the composer effort controls.
 */
test.describe.configure({ mode: "serial" });

const TOUCH = 44;
const HALF = TOUCH / 2;

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

type Box = { x: number; y: number; width: number; height: number };

function boxInViewport(box: Box, vw: number, vh: number) {
  return box.x >= -0.5 && box.y >= -0.5 && box.x + box.width <= vw + 0.5 && box.y + box.height <= vh + 0.5;
}

/** Owner marker elementFromPoint should resolve to at a probed point. */
async function ownerAt(page: Page, x: number, y: number): Promise<string> {
  return page.evaluate(
    ({ x, y }) =>
      document
        .elementFromPoint(x, y)
        ?.closest("[data-touchhit-owner]")
        ?.getAttribute("data-touchhit-owner") ?? "none",
    { x, y },
  );
}

/**
 * A real tap target: visible box fully inside the viewport, and the four
 * corners of its 44×44 hot zone (centre ±21px) resolve to the control itself
 * — catching a missing transform, a covering sibling, and off-screen parking.
 *
 * The rect read and all four elementFromPoint probes happen in ONE evaluate:
 * AnchoredPopover re-measures on animation frames (including the pill→list
 * flip), so separate round-trips could observe a moving panel. A raw corner
 * outside the viewport fails outright — it is never clamped back onto the
 * visual box, which would certify a target with no hot zone at all.
 */
async function assertTapTarget(page: Page, target: Locator, owner: string): Promise<Box> {
  await expect(target).toBeVisible();
  return page.evaluate(({ owner: ownerName, half }) => {
    const el = document.querySelector<HTMLElement>(
      `[data-touchhit-owner="${ownerName}"]`,
    );
    if (!el) throw new Error(`${ownerName}: ownership marker missing`);
    const rect = el.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) {
      throw new Error(`${ownerName}: zero-size box`);
    }
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const inside = (x: number, y: number) =>
      x >= 0 && y >= 0 && x <= vw && y <= vh;
    if (!inside(rect.left, rect.top) || !inside(rect.right, rect.bottom)) {
      throw new Error(
        `${ownerName}: visual box [${rect.left.toFixed(1)},${rect.top.toFixed(1)}–${rect.right.toFixed(1)},${rect.bottom.toFixed(1)}] not inside ${vw}x${vh}`,
      );
    }

    const cx = rect.left + rect.width / 2;
    const cy = rect.top + rect.height / 2;
    // 0.5px inset keeps the probe inside the pseudo at sub-pixel rounding.
    const corners = [
      { name: "tl", x: cx - half + 0.5, y: cy - half + 0.5 },
      { name: "tr", x: cx + half - 0.5, y: cy - half + 0.5 },
      { name: "bl", x: cx - half + 0.5, y: cy + half - 0.5 },
      { name: "br", x: cx + half - 0.5, y: cy + half - 0.5 },
    ];
    for (const corner of corners) {
      if (!inside(corner.x, corner.y)) {
        throw new Error(
          `${ownerName}: ${corner.name} hot-zone corner (${corner.x.toFixed(1)},${corner.y.toFixed(1)}) is outside ${vw}x${vh}`,
        );
      }
      const hit = document
        .elementFromPoint(corner.x, corner.y)
        ?.closest("[data-touchhit-owner]")
        ?.getAttribute("data-touchhit-owner");
      if (hit !== ownerName) {
        throw new Error(
          `${ownerName}: ${corner.name} corner (${corner.x.toFixed(1)},${corner.y.toFixed(1)}) resolved to "${hit ?? "none"}"`,
        );
      }
    }
    const box = { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
    return box;
  }, { owner, half: HALF });
  console.log(
    `TOUCHHIT ${owner} visual ${Math.round(box.width)}x${Math.round(box.height)} at (${Math.round(box.x)},${Math.round(box.y)}) corners all self`,
  );
  return box;
}

async function mark(locator: Locator, owner: string) {
  await locator.evaluate(
    (el, value) => el.setAttribute("data-touchhit-owner", value),
    owner,
  );
}

async function markHeader(page: Page) {
  // Stop is deliberately not marked here: an exited session swaps Stop for
  // resume controls, and tests that never probe Stop would time out waiting
  // for it. Mark Stop explicitly in the two tests that hit-test it.
  await mark(page.getByRole("link", { name: "返回" }), "back");
  await mark(page.getByTestId("view-switch-tty"), "seg-tty");
  await mark(page.getByTestId("view-switch-structured"), "seg-structured");
}

async function markStop(page: Page) {
  await mark(page.getByRole("button", { name: "Stop" }), "stop");
}

async function metaFontSize(page: Page): Promise<number> {
  return page.getByTestId("session-meta").evaluate((el) =>
    parseFloat(getComputedStyle(el).fontSize),
  );
}

test("header controls that are on screen own their full 44px corners at 390px with touch", async ({
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
    await markHeader(page);

    expect(await metaFontSize(page)).toBeGreaterThanOrEqual(12);

    // Back: a ~20px arrow glyph (line-height makes its box 20.3px), 44px reach.
    const backBox = await assertTapTarget(page, page.getByRole("link", { name: "返回" }), "back");
    expect(backBox.width).toBeLessThanOrEqual(21);
    expect(backBox.height).toBeLessThan(26);

    // Both segments of the 终端 / 结构 switch keep radiogroup semantics and a
    // 30px visual height while owning the 44px hit area.
    const switchBox = page.getByTestId("view-switch");
    await expect(switchBox).toHaveAttribute("role", "radiogroup");
    for (const [id, owner] of [
      ["tty", "seg-tty"],
      ["structured", "seg-structured"],
    ] as const) {
      const seg = page.getByTestId(`view-switch-${id}`);
      await expect(seg).toHaveAttribute("role", "radio");
      const box = await assertTapTarget(page, seg, owner);
      expect(box.height).toBeLessThan(44);
    }

    // Clean header shot before opening any popover.
    await shot(page, "ux2026-touchhit-1-390.png");

    // The two 26px effort glyph buttons (reset on the pill; list-back on the
    // tier list) keep their glyph box and own their ::after corners.
    await page.getByTestId("model-effort-chip").click();
    await expect(page.getByTestId("effort-menu")).toBeVisible();
    await mark(page.getByTestId("effort-reset"), "effort-reset");
    const resetBox = await assertTapTarget(page, page.getByTestId("effort-reset"), "effort-reset");
    expect(resetBox.width).toBeLessThan(27);
    expect(resetBox.height).toBeLessThan(27);

    await page.getByTestId("effort-open-list").click();
    await expect(page.getByTestId("effort-slider-panel")).toBeVisible();
    await mark(page.getByTestId("effort-list-back"), "effort-list-back");
    const listBackBox = await assertTapTarget(
      page,
      page.getByTestId("effort-list-back"),
      "effort-list-back",
    );
    expect(listBackBox.width).toBeLessThan(27);
    expect(listBackBox.height).toBeLessThan(27);
  } finally {
    await context.close();
  }
});

test("Stop is a reachable 44px target on the 390px viewport (c-sessionchrome landed, D-040)", async ({
  browser,
}) => {
  const context = await browser.newContext({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
  });
  const page = await context.newPage();
  try {
    await login(page);
    const instanceId = await createClaudeSession(page);
    await clearApprovals(page, instanceId);
    await markHeader(page);
    await markStop(page);

    // Hard regression guard for flex:none: the visible Stop square keeps its
    // 32px even on the crowded 390px row (a flex-shrunk square would let the
    // centred hot zone swallow the neighbour).
    const stopBox = await page.getByRole("button", { name: "Stop" }).boundingBox();
    expect(stopBox).toBeTruthy();
    expect(stopBox!.width).toBeGreaterThanOrEqual(32);
    expect(stopBox!.height).toBeGreaterThanOrEqual(32);

    // c-sessionchrome reclaimed the header (chips fold into one chip, the
    // secondary toggles move into ⋯), so Stop is back inside the viewport and
    // owns its full 44px corners — D-040 keeps it in the header permanently.
    expect(stopBox!.x, "Stop stays inside the 390px viewport").toBeGreaterThanOrEqual(-0.5);
    expect(stopBox!.x + stopBox!.width, "Stop never parks past the right edge").toBeLessThanOrEqual(390.5);
    console.log(
      `TOUCHHIT stop visual ${Math.round(stopBox!.width)}x${Math.round(stopBox!.height)} at x=${Math.round(stopBox!.x)} (viewport 390)`,
    );
    await assertTapTarget(page, page.getByRole("button", { name: "Stop" }), "stop");
  } finally {
    await context.close();
  }
});

test("Stop hot zone never claims its neighbour (D-039), and narrow width without touch keeps the zone", async ({
  page,
}) => {
  // At a compact width wide enough for the whole headRow to fit (still
  // <=767px, so the mobile hot-zone rules apply), hit-test the border between
  // Stop and its left neighbour, the 原始事件 toggle. With Stop held at
  // flex:none the 44px ::after spills 6px into the 10px gap; the neighbour's
  // edge must still resolve to the neighbour, not Stop.
  await page.setViewportSize({ width: 767, height: 900 });
  const instanceId = await createClaudeSession(page);
  await clearApprovals(page, instanceId);
  await markHeader(page);
  await markStop(page);
  await mark(page.getByTestId("events-toggle"), "neighbour");

  const stop = page.getByRole("button", { name: "Stop" });
  const neighbour = page.getByTestId("events-toggle");
  const stopBox = await stop.boundingBox();
  const neighbourBox = await neighbour.boundingBox();
  expect(stopBox).toBeTruthy();
  expect(neighbourBox).toBeTruthy();
  expect(boxInViewport(stopBox!, 767, 900)).toBe(true);
  expect(boxInViewport(neighbourBox!, 767, 900)).toBe(true);

  // Geometry first: the 44px zone centred on the 32px square extends 6px
  // sideways; it must not reach the neighbour's visible box.
  const stopHotLeft = stopBox!.x - (TOUCH - stopBox!.width) / 2;
  console.log(
    `TOUCHHIT border neighbour right=${Math.round(neighbourBox!.x + neighbourBox!.width)} stopHotLeft=${Math.round(stopHotLeft)} stopVisualLeft=${Math.round(stopBox!.x)}`,
  );
  expect(stopHotLeft).toBeGreaterThan(neighbourBox!.x + neighbourBox!.width - 0.5);

  // Hit-testing across the border: the neighbour's right-edge column belongs
  // to the neighbour; one pixel left of the Stop zone is gap; the zone edge
  // and the visible square belong to Stop.
  const cy = stopBox!.y + stopBox!.height / 2;
  expect(await ownerAt(page, neighbourBox!.x + neighbourBox!.width - 1, cy)).toBe("neighbour");
  expect(await ownerAt(page, stopHotLeft - 1, cy)).not.toBe("stop");
  expect(await ownerAt(page, stopHotLeft + 0.5, cy)).toBe("stop");
  expect(await ownerAt(page, stopBox!.x + 1, cy)).toBe("stop");

  // Same non-touch page, narrowed to 390: the hot zone is a width question,
  // so dropping touch (compact/coarsePointer distinction) must not shrink
  // it. The session survives the viewport resize; the segment keeps owning
  // its corners.
  await page.setViewportSize({ width: 390, height: 844 });
  await mark(page.getByTestId("view-switch-structured"), "seg-structured");
  const segBox = await assertTapTarget(
    page,
    page.getByTestId("view-switch-structured"),
    "seg-structured",
  );
  expect(segBox.height).toBeLessThan(44);
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
