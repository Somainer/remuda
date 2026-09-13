import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence");

function row(page: Page, text: string) {
  return page.getByTestId("session-row").filter({ hasText: text }).first();
}

async function shot(page: Page, name: string) {
  await mkdir(evidence, { recursive: true });
  await page.screenshot({ path: path.join(evidence, name), animations: "disabled" });
}

async function shotComposer(page: Page, name: string) {
  await mkdir(evidence, { recursive: true });
  const composer = page.getByTestId("composer");
  const menu = page.getByTestId("effort-menu");
  await expect(menu).toBeVisible();
  const a = await composer.boundingBox();
  const b = await menu.boundingBox();
  expect(a).toBeTruthy();
  expect(b).toBeTruthy();
  const viewport = page.viewportSize() ?? { width: 1440, height: 900 };
  const x = Math.max(0, Math.floor(Math.min(a!.x, b!.x)));
  const y = Math.max(0, Math.floor(Math.min(a!.y, b!.y)));
  const right = Math.min(viewport.width, Math.ceil(Math.max(a!.x + a!.width, b!.x + b!.width)));
  const bottom = Math.min(viewport.height, Math.ceil(Math.max(a!.y + a!.height, b!.y + b!.height)));
  await page.screenshot({
    path: path.join(evidence, name),
    animations: "disabled",
    clip: { x, y, width: Math.max(1, right - x), height: Math.max(1, bottom - y) },
  });
}

function noOverlap(a: { x: number; y: number; width: number; height: number }, b: { x: number; y: number; width: number; height: number }) {
  return a.x + a.width <= b.x || b.x + b.width <= a.x || a.y + a.height <= b.y || b.y + b.height <= a.y;
}

async function openEffort(page: Page) {
  await page.getByTestId("model-effort-chip").click();
  await expect(page.getByTestId("effort-slider")).toBeVisible();
}

async function dragSlider(page: Page, at: "start" | "end") {
  const slider = page.getByTestId("effort-slider");
  const box = await slider.boundingBox();
  expect(box).toBeTruthy();
  const x = at === "end" ? box!.x + box!.width - 4 : box!.x + 4;
  const y = box!.y + box!.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x, y, { steps: 2 });
  await page.mouse.up();
}

/** The pill is a thick rounded bar with a large white knob, not a hairline track. */
async function assertPillGeometry(page: Page) {
  const pill = await page.getByTestId("effort-track").boundingBox();
  const knob = await page.getByTestId("effort-knob").boundingBox();
  expect(pill).toBeTruthy();
  expect(knob).toBeTruthy();
  expect(pill!.height).toBeGreaterThanOrEqual(40);
  expect(knob!.width).toBeGreaterThanOrEqual(34);
  // The knob stays inside the pill at both ends.
  expect(knob!.x).toBeGreaterThanOrEqual(pill!.x - 1);
  expect(knob!.x + knob!.width).toBeLessThanOrEqual(pill!.x + pill!.width + 1);
}

async function assertSingleLine(chip: Locator) {
  const metrics = await chip.evaluate((el) => {
    const style = getComputedStyle(el);
    const rect = el.getBoundingClientRect();
    return {
      whiteSpace: style.whiteSpace,
      writingMode: style.writingMode,
      height: rect.height,
      width: rect.width,
    };
  });
  expect(metrics.whiteSpace).toMatch(/nowrap/);
  expect(metrics.writingMode).toMatch(/horizontal|lr/i);
  expect(metrics.height).toBeLessThan(56);
}

test.describe("composer control bar and effort", () => {
  test("chips render collapsed with native values", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await expect(page.getByTestId("composer-bar")).toBeVisible();
    await expect(page.getByTestId("harness-chip")).toContainText(/Claude/);
    await expect(page.getByTestId("model-effort-chip")).toContainText(/think|default/);
    await expect(page.getByTestId("model-effort-chip")).not.toContainText(/opus|sonnet/);
    await expect(page.getByTestId("context-chip")).toBeVisible();
    await expect(page.getByTestId("permission-chip")).toContainText(/询问|可改|全自动|绕过/);
    await expect(page.getByTestId("effort-menu")).toHaveCount(0);
    if (test.info().project.name === "chromium") {
      await shot(page, "composer-1-bar.png");
    }
  });

  test("effort popover is a snapping slider and selecting a tier updates the chip", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await openEffort(page);
    const menu = page.getByTestId("effort-menu");
    await expect(menu).toBeVisible();
    const slider = page.getByTestId("effort-slider");
    await expect(slider).toHaveAttribute("data-tiers", "default,think,think-hard,ultracode");
    await expect(slider).toHaveAttribute("data-name", /think|default/);
    await expect(page.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");
    await expect(page.getByTestId("effort-title")).toBeVisible();
    await expect(page.getByTestId("effort-model")).toBeVisible();
    // Compact card: no tier list and no model list until the chevron is tapped.
    await expect(page.getByTestId("effort-tier-ultracode")).toHaveCount(0);
    await expect(page.getByTestId("model-option-opus")).toHaveCount(0);
    await assertPillGeometry(page);
    const card = await menu.boundingBox();
    expect(card).toBeTruthy();
    expect(card!.width).toBeLessThanOrEqual(401);
    if (test.info().project.name === "chromium") {
      await shot(page, "composer-1-effort-menu.png");
    }
    await dragSlider(page, "end");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "ultracode");
    await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
    await expect(page.getByTestId("model-effort-chip")).toContainText("ultracode");
    await expect(slider).toHaveAttribute("data-ember", "1");
  });

  test("the tier name opens the tier and model list, then returns to the pill", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await openEffort(page);
    await page.getByTestId("effort-open-list").click();
    const panel = page.getByTestId("effort-slider-panel");
    await expect(panel).toHaveAttribute("data-view", "list");
    await expect(page.getByTestId("effort-slider")).toHaveCount(0);
    await expect(page.getByTestId("effort-tier-ultracode")).toHaveAttribute("data-ember", "1");
    await expect(page.getByTestId("effort-list")).toContainText(/默认档|不额外思考|跨文件|最高档/);
    await expect(page.getByTestId("effort-menu")).toContainText("切换只影响后续回合，不重写已发出的 prompt");
    await page.getByTestId("effort-list-back").click();
    await expect(panel).toHaveAttribute("data-view", "slider");
    await page.getByTestId("effort-open-list").click();
    await page.getByTestId("effort-tier-ultracode").click();
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "ultracode");
    await expect(panel).toHaveAttribute("data-view", "slider");
    await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-ember", "1");
  });

  test("grok session lists the native grok effort table", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "Grok 会话").click();
    await page.getByTestId("view-switch-structured").click();
    await expect(page.getByTestId("composer")).toHaveAttribute("data-harness", "grok");
    await openEffort(page);
    await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-tiers", "quick,standard,max");
    await expect(page.getByTestId("effort-slider")).not.toHaveAttribute("data-tiers", /think/);
  });

  test("an existing session shows the harness as a label, not a menu", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    const chip = page.getByTestId("harness-chip");
    await expect(chip).toHaveAttribute("data-readonly", "1");
    await expect(chip).toContainText(/Claude/);
    await chip.click();
    await expect(page.getByTestId("harness-menu")).toHaveCount(0);
    await expect(page.getByTestId("harness-option-codex")).toHaveCount(0);
    await expect(page.getByTestId("harness-option-terminal")).toHaveCount(0);
    // New Session still picks the harness.
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-kind-codex")).toBeVisible();
    await expect(page.getByTestId("new-session-kind-terminal")).toBeVisible();
  });

  test("approval card and expanded effort menu do not overlap", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/sessions");
    await row(page, "清一下 /tmp/coord-media").click();
    await expect(page.getByTestId("approval-card")).toBeVisible();
    await expect(page.getByTestId("composer-bar")).toBeVisible();
    await page.getByTestId("model-effort-chip").click();
    const menu = page.getByTestId("effort-menu");
    await expect(menu).toBeVisible();
    await expect(menu).toHaveAttribute("data-placement", /down|up/);
    const approval = await page.getByTestId("approval-card").boundingBox();
    const panel = await menu.boundingBox();
    expect(approval).toBeTruthy();
    expect(panel).toBeTruthy();
    expect(noOverlap(approval!, panel!)).toBeTruthy();
    const allow = page.getByRole("button", { name: "允许一次" });
    const allowBox = await allow.boundingBox();
    expect(allowBox).toBeTruthy();
    expect(noOverlap(allowBox!, panel!)).toBeTruthy();
    if (test.info().project.name === "chromium") {
      await shot(page, "composer-1-approval-menu.png");
    }
  });

  test("New Session permission chips stay single-line at 390 and 1440", async ({ page }) => {
    for (const width of [390, 1440] as const) {
      await page.setViewportSize({ width, height: width === 390 ? 844 : 900 });
      await page.goto("/sessions/new");
      await expect(page.getByTestId("new-session-sheet")).toBeVisible();
      const rowEl = page.getByTestId("new-session-perm-row");
      await expect(rowEl).toBeVisible();
      const chips = rowEl.getByRole("button");
      await expect(chips).toHaveCount(4);
      const count = await chips.count();
      for (let i = 0; i < count; i++) {
        await assertSingleLine(chips.nth(i));
      }
      await page.getByTestId("new-session-perm-bypassPermissions").click();
      const yolo = page.getByTestId("new-session-yolo-hint");
      await expect(yolo).toBeVisible();
      await yolo.scrollIntoViewIfNeeded();
      await page.getByTestId("new-session-effort").scrollIntoViewIfNeeded();
      const ack = page.getByTestId("new-session-yolo-ack");
      const yoloBox = await yolo.boundingBox();
      const ackBox = await ack.boundingBox();
      expect(yoloBox).toBeTruthy();
      expect(ackBox).toBeTruthy();
      expect(ackBox!.y).toBeGreaterThanOrEqual(yoloBox!.y - 2);
      expect(ackBox!.y + ackBox!.height).toBeLessThanOrEqual(yoloBox!.y + yoloBox!.height + 2);
      await expect(page.getByTestId("new-session-effort-think")).toBeVisible();
      await expect(page.getByTestId("new-session-effort-ultracode")).toHaveAttribute("data-ember", "1");
      if (test.info().project.name === "chromium") {
        await shot(page, `composer-1-new-session-${width}.png`);
      }
    }
  });

  test("Settings default effort flows into New Session", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByTestId("settings-effort")).toBeVisible();
    await page.getByTestId("settings-effort-ultracode").click();
    await expect(page.getByTestId("settings-effort-ultracode")).toHaveAttribute("data-selected", "1");
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-effort-ultracode")).toHaveAttribute("data-selected", "1");
    await page.getByTestId("new-session-kind-grok").click();
    await expect(page.getByTestId("new-session-effort-max")).toHaveAttribute("data-selected", "1");
    await expect(page.getByTestId("new-session-effort-max")).toHaveAttribute("data-ember", "1");
  });

  test("effort selection persists after reload", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await openEffort(page);
    await page.getByTestId("effort-slider").focus();
    await page.keyboard.press("End");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "ultracode");
    await page.reload();
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "ultracode");
    await expect(page.getByTestId("model-effort-chip")).toContainText("ultracode");
  });

  test("grok pty shows four chips including a read-only yolo permission", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "Grok 会话").click();
    await expect(page.getByTestId("session-page")).toBeVisible();
    const structured = page.getByTestId("view-switch-structured");
    await expect(structured).toBeVisible();
    await structured.click();
    await expect(page.getByTestId("composer-bar")).toBeVisible();
    await expect(page.getByTestId("harness-chip")).toHaveAttribute("data-readonly", "1");
    await expect(page.getByTestId("model-effort-chip")).toBeVisible();
    await expect(page.getByTestId("context-chip")).toBeVisible();
    await expect(page.getByTestId("permission-chip")).toHaveAttribute("data-readonly", "1");
    await expect(page.getByTestId("permission-chip")).toContainText(/always-approve/);
  });

  test("effort slider evidence: night/ledger at 1440 and 390", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "evidence shots from chromium only");
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await expect(page.getByTestId("composer-bar")).toBeVisible();
    for (const theme of ["night", "ledger"] as const) {
      await page.evaluate((next) => {
        document.documentElement.setAttribute("data-theme", next);
      }, theme);
      for (const [width, height, tag] of [
        [1440, 900, "1440"],
        [390, 844, "390"],
      ] as const) {
        await page.setViewportSize({ width, height });
        // mid = a middle non-top tier, top = the ember tier.
        for (const [state, keys] of [
          ["mid", ["Home", "ArrowRight", "ArrowRight"]],
          ["top", ["End"]],
        ] as const) {
          await page.keyboard.press("Escape");
          await openEffort(page);
          await page.getByTestId("effort-slider").focus();
          for (const key of keys) await page.keyboard.press(key);
          const slider = page.getByTestId("effort-slider");
          await expect(slider).toHaveAttribute("data-ember", state === "top" ? "1" : "0");
          await expect(page.getByTestId("effort-knob")).toBeVisible();
          await assertPillGeometry(page);
          const box = await page.getByTestId("effort-menu").boundingBox();
          expect(box).toBeTruthy();
          expect(box!.width).toBeLessThanOrEqual(Math.min(width, 401));
          await shotComposer(page, `composer-slider-2-${state}-${theme}-${tag}.png`);
        }
      }
    }
    await page.setViewportSize({ width: 400, height: 844 });
    await page.keyboard.press("Escape");
    await openEffort(page);
    const menu = await page.getByTestId("effort-menu").boundingBox();
    expect(menu).toBeTruthy();
    expect(menu!.width).toBeLessThanOrEqual(400);
    await assertPillGeometry(page);
  });
});
