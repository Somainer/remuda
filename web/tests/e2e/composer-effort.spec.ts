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

function noOverlap(a: { x: number; y: number; width: number; height: number }, b: { x: number; y: number; width: number; height: number }) {
  return a.x + a.width <= b.x || b.x + b.width <= a.x || a.y + a.height <= b.y || b.y + b.height <= a.y;
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
    await expect(page.getByTestId("model-effort-chip")).toContainText(/think|default|opus|auto/);
    await expect(page.getByTestId("context-chip")).toBeVisible();
    await expect(page.getByTestId("permission-chip")).toContainText(/询问|可改|全自动|绕过/);
    await expect(page.getByTestId("effort-menu")).toHaveCount(0);
    if (test.info().project.name === "chromium") {
      await shot(page, "composer-1-bar.png");
    }
  });

  test("effort popover lists native tables and selecting a tier updates the chip", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await page.getByTestId("model-effort-chip").click();
    const menu = page.getByTestId("effort-menu");
    await expect(menu).toBeVisible();
    await expect(menu).toContainText("EFFORT · 本回合生效，发 Command 不只改本地");
    await expect(page.getByTestId("effort-tier-default")).toBeVisible();
    await expect(page.getByTestId("effort-tier-think")).toBeVisible();
    await expect(page.getByTestId("effort-tier-think-hard")).toBeVisible();
    await expect(page.getByTestId("effort-tier-ultracode")).toHaveAttribute("data-ember", "1");
    await expect(menu).toContainText("切换只影响后续回合，不重写已发出的 prompt");
    if (test.info().project.name === "chromium") {
      await shot(page, "composer-1-effort-menu.png");
    }
    await page.getByTestId("effort-tier-ultracode").click();
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "ultracode");
    await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
    await expect(page.getByTestId("model-effort-chip")).toContainText("ultracode");
  });

  test("codex and grok popovers use native names", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await page.getByTestId("harness-chip").click();
    await expect(page.getByTestId("harness-menu")).toBeVisible();
    if (test.info().project.name === "chromium") {
      await shot(page, "composer-1-harness-menu.png");
    }
    const grok = page.getByTestId("harness-option-grok");
    if ((await grok.getAttribute("data-installed")) === "1") {
      await grok.click();
      await page.getByTestId("model-effort-chip").click();
      await expect(page.getByTestId("effort-tier-quick")).toBeVisible();
      await expect(page.getByTestId("effort-tier-standard")).toBeVisible();
      await expect(page.getByTestId("effort-tier-max")).toHaveAttribute("data-ember", "1");
      await expect(page.getByTestId("effort-tier-think")).toHaveCount(0);
    }
    await page.keyboard.press("Escape");
    await page.getByTestId("harness-chip").click();
    const codex = page.getByTestId("harness-option-codex");
    if ((await codex.getAttribute("data-installed")) === "1") {
      await codex.click();
      await page.getByTestId("model-effort-chip").click();
      await expect(page.getByTestId("effort-tier-low")).toBeVisible();
      await expect(page.getByTestId("effort-tier-medium")).toBeVisible();
      await expect(page.getByTestId("effort-tier-high")).toBeVisible();
      await expect(page.getByTestId("effort-tier-ultra")).toHaveAttribute("data-ember", "1");
    } else {
      await expect(codex).toContainText("+");
    }
  });

  test("switching harness remaps the tier by index", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await page.getByTestId("model-effort-chip").click();
    await page.getByTestId("effort-tier-ultracode").click();
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "ultracode");
    await page.getByTestId("harness-chip").click();
    const grok = page.getByTestId("harness-option-grok");
    test.skip((await grok.getAttribute("data-installed")) !== "1", "grok not installed on this host");
    await grok.click();
    await expect(page.getByTestId("composer")).toHaveAttribute("data-harness", "grok");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "max");
    await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
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
});
