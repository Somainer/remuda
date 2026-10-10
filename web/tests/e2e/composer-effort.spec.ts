import { expect, test, type Locator, type Page } from "@playwright/test";
import { setMode } from "./appearanceHelper";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
/**
 * A default run must not rewrite tracked files, so shots land in the
 * gitignored `test-results/`. Re-capture the committed evidence with
 * REMUDA_EVIDENCE=1.
 */
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/composer-effort");

function row(page: Page, text: string) {
  return page.getByTestId("session-row").filter({ hasText: text }).first();
}

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function shotComposer(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
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
    path: path.join(shotDir, name),
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

/** Requested selections cannot stand in for native read-back in these mock sessions. */
async function assertRequestedEffortUnknown(page: Page, name: string) {
  await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", name);
  const chip = page.getByTestId("model-effort-chip");
  // D-042: the mobile collapsed trigger prefixes the permission word
  // (`询问 · Select effort, …`); match the suffix so the helper works under
  // both chromium and mobile-webkit projects.
  await expect(chip).toHaveAttribute(
    "aria-label",
    new RegExp(`Select effort, ${name}; effective unknown$`),
  );
  await expect(chip).toHaveAttribute("data-effort-effective", "unknown");
  await expect(chip).toHaveAttribute("data-effort-source", "unknown");
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("?");
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

/** The pill is a compact rounded bar with a large white knob, not a hairline track. */
async function assertPillGeometry(page: Page) {
  const pill = await page.getByTestId("effort-track").boundingBox();
  const knob = await page.getByTestId("effort-knob").boundingBox();
  expect(pill).toBeTruthy();
  expect(knob).toBeTruthy();
  // Reference proportions: ~40px track, ~36px knob. Shrunk from slider 2's 44/40.
  expect(pill!.height).toBeGreaterThanOrEqual(36);
  expect(pill!.height).toBeLessThanOrEqual(42);
  expect(knob!.width).toBeGreaterThanOrEqual(32);
  expect(knob!.width).toBeLessThanOrEqual(38);
  // The knob stays inside the pill at both ends.
  expect(knob!.x).toBeGreaterThanOrEqual(pill!.x - 1);
  expect(knob!.x + knob!.width).toBeLessThanOrEqual(pill!.x + pill!.width + 1);
}

/**
 * The brand fill must reach the knob's far edge, so no dark track shows to the
 * left of or under the thumb — the defect this pass fixes. Checked by pixel,
 * at the pill's vertical middle, just inside the knob's leading edge.
 */
async function assertFillReachesKnob(page: Page) {
  const pill = await page.getByTestId("effort-track").boundingBox();
  const knob = await page.getByTestId("effort-knob").boundingBox();
  expect(pill).toBeTruthy();
  expect(knob).toBeTruthy();
  const fill = await page.getByTestId("effort-fill").boundingBox();
  expect(fill).toBeTruthy();
  // The fill's right edge is at or past the knob's right edge (within a rounding px).
  expect(fill!.x + fill!.width).toBeGreaterThanOrEqual(knob!.x + knob!.width - 1);
  // ...and it starts at the pill's left edge, so the run is unbroken.
  expect(fill!.x).toBeLessThanOrEqual(pill!.x + 1);
}

/** Touch targets are >= 44px through hit area even though the pill is 40px tall. */
async function assertTouchTargets(page: Page) {
  const hit = await page.getByTestId("effort-slider").boundingBox();
  expect(hit).toBeTruthy();
  expect(hit!.height).toBeGreaterThanOrEqual(44);
  for (const id of ["effort-reset", "effort-open-list"] as const) {
    const target = page.getByTestId(id);
    const reach = await target.evaluate((el) => {
      const rect = el.getBoundingClientRect();
      const after = getComputedStyle(el, "::after");
      const w = parseFloat(after.width);
      const h = parseFloat(after.height);
      return {
        width: Math.max(rect.width, Number.isFinite(w) ? w : 0),
        height: Math.max(rect.height, Number.isFinite(h) ? h : 0),
      };
    });
    expect(reach.width).toBeGreaterThanOrEqual(44);
    expect(reach.height).toBeGreaterThanOrEqual(44);
  }
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
  test("chips render collapsed with requested effort and unknown native read-back", async ({ page }) => {
    const mobile = test.info().project.name === "mobile-webkit";
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await expect(page.getByTestId("composer-bar")).toBeVisible();
    if (mobile) {
      // D-042: on phones the harness/context/permission controls ride inside
      // the options sheet; the collapsed trigger keeps the permission word +
      // effort tier only.
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-permission", "manual");
      await expect(page.getByTestId("context-chip")).toHaveCount(0);
      await expect(page.getByTestId("harness-chip")).toHaveCount(0);
      await page.getByTestId("model-effort-chip").click();
      await expect(page.getByTestId("composer-options-sheet")).toBeVisible();
      await expect(page.getByTestId("harness-chip")).toContainText(/Claude/);
      await expect(page.getByTestId("context-chip")).toBeVisible();
      await expect(page.getByTestId("composer-trigger-permission")).toContainText(/询问|可改|全自动|绕过/);
      await expect(page.getByTestId("permission-option-manual")).toBeVisible();
    } else {
      await expect(page.getByTestId("harness-chip")).toContainText(/Claude/);
      await assertRequestedEffortUnknown(page, "high");
      await expect(page.getByTestId("model-effort-chip")).not.toContainText(/opus|sonnet/);
      await expect(page.getByTestId("context-chip")).toBeVisible();
      await expect(page.getByTestId("permission-chip")).toContainText(/询问|可改|全自动|绕过/);
      await expect(page.getByTestId("effort-menu")).toHaveCount(0);
    }
    if (test.info().project.name === "chromium") {
      await shot(page, "composer-1-bar.png");
    }
  });

  test("effort popover is a snapping five-stop slider with an orthogonal ultracode switch", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await openEffort(page);
    const menu = page.getByTestId("effort-menu");
    await expect(menu).toBeVisible();
    const slider = page.getByTestId("effort-slider");
    await expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max");
    await expect(slider).toHaveAttribute("aria-valuemax", "4");
    await expect(slider).toHaveAttribute("data-name", /high/);
    await expect(page.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");
    await expect(page.getByTestId("effort-title")).toBeVisible();
    await expect(page.getByTestId("effort-model")).toBeVisible();
    // Compact card: no tier/model rows until the chevron is tapped.
    await expect(page.getByTestId("effort-tier-max")).toHaveCount(0);
    await expect(page.getByTestId("model-option-opus")).toHaveCount(0);
    // The orthogonal switch is a row BELOW the pill, never a sixth tick.
    await expect(page.getByTestId("effort-ultracode")).toBeVisible();
    await expect(slider).toHaveAttribute("data-tiers", /^(?!.*ultracode)/);
    // Five short tick labels under five dots.
    const ticks = page.locator(
      "[data-testid='effort-slider-panel'] [class*='effortTickShort']",
    );
    await expect(ticks).toHaveText(["low", "med", "high", "xhigh", "max"]);
    await assertPillGeometry(page);
    const card = await menu.boundingBox();
    expect(card).toBeTruthy();
    if (test.info().project.name === "chromium") {
      expect(card!.width).toBeLessThanOrEqual(301);
      await shot(page, "composer-1-effort-menu.png");
    } else {
      await expect(menu).toHaveAttribute("data-in-sheet", "1");
      expect(card!.width).toBeGreaterThan(301);
    }
    await assertFillReachesKnob(page);
    // Dragging to the far-right stop selects MAX (a plain tier — no ember).
    await dragSlider(page, "end");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "max");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-ultracode", "0");
    await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "0");
    await expect(slider).toHaveAttribute("data-name", "max");
    await expect(slider).toHaveAttribute("data-index", "4");
    await expect(slider).toHaveAttribute("data-effort-look", "top");
    await expect(slider).toHaveAttribute("data-ember", "0");

    // Flipping the switch turns ultracode on AT MAX without moving the slider.
    await page.getByTestId("effort-ultracode-switch").click();
    await expect(page.getByTestId("effort-ultracode-switch")).toHaveAttribute("aria-checked", "true");
    await expect(slider).toHaveAttribute("data-name", "max");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "max");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-ultracode", "1");
    await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
    // Switching it off returns the ember to rest, slider still on max.
    await page.getByTestId("effort-ultracode-switch").click();
    await expect(page.getByTestId("composer")).toHaveAttribute("data-ultracode", "0");
    await expect(slider).toHaveAttribute("data-name", "max");
  });

  test("the five-stop slider is orthogonal to the ultracode switch", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await openEffort(page);
    const slider = page.getByTestId("effort-slider");
    await slider.focus();
    // Walk the whole tier track: low → max (five stops, no sixth).
    await page.keyboard.press("Home");
    for (const [index, name] of [
      ["1", "medium"],
      ["2", "high"],
      ["3", "xhigh"],
      ["4", "max"],
    ] as const) {
      await page.keyboard.press("ArrowRight");
      await expect(slider).toHaveAttribute("data-index", index);
      await expect(slider).toHaveAttribute("data-name", name);
    }
    await expect(slider).toHaveAttribute("data-ultracode", "0");
    await expect(slider).toHaveAttribute("data-effort-look", "top");
    // The ember field never appears on the pill; it belongs to the switch.
    await expect(page.getByTestId("effort-embers")).toHaveCount(0);
    // Switch on at the END of the track (max): the slider stays put.
    await page.getByTestId("effort-ultracode-switch").click();
    await expect(slider).toHaveAttribute("data-index", "4");
    await expect(slider).toHaveAttribute("data-name", "max");
    await expect(page.getByTestId("effort-ultracode-switch")).toHaveAttribute("aria-checked", "true");
    await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
    // Home moves the tier to low while the flag stays on (orthogonal).
    await slider.focus();
    await page.keyboard.press("Home");
    await expect(slider).toHaveAttribute("data-name", "low");
    await expect(page.getByTestId("effort-ultracode-switch")).toHaveAttribute("aria-checked", "true");
  });

  test("the tier name opens the tier and model list, then returns to the pill", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await openEffort(page);
    await page.getByTestId("effort-open-list").click();
    const panel = page.getByTestId("effort-slider-panel");
    await expect(panel).toHaveAttribute("data-view", "list");
    await expect(page.getByTestId("effort-slider")).toHaveCount(0);
    // Five tiers in the list; max is the restrained top accent and there is
    // no ultracode tier row (it is the switch on the pill view).
    await expect(page.getByTestId("effort-tier-max")).toHaveAttribute("data-effort-look", "top");
    await expect(page.getByTestId("effort-tier-ultracode")).toHaveCount(0);
    await expect(page.getByTestId("effort-list")).toContainText(/最省|均衡|综合|跨文件|最高档/);
    await expect(page.getByTestId("effort-menu")).toContainText("切换只影响后续回合，不重写已发出的 prompt");
    await page.getByTestId("effort-list-back").click();
    await expect(panel).toHaveAttribute("data-view", "slider");
    await page.getByTestId("effort-open-list").click();
    await page.getByTestId("effort-tier-xhigh").click();
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "xhigh");
    await expect(panel).toHaveAttribute("data-view", "slider");
    // Plain xhigh carries the restrained top accent but not the ember field.
    await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-effort-look", "top");
    await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-ember", "0");
  });

  test("grok session lists the native grok effort table", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "Grok 会话").click();
    await page.getByTestId("view-switch-structured").click();
    await expect(page.getByTestId("composer")).toHaveAttribute("data-harness", "grok");
    await openEffort(page);
    await expect(page.getByTestId("effort-slider")).toHaveAttribute(
      "data-tiers",
      "low,medium,high,xhigh",
    );
    await expect(page.getByTestId("effort-slider")).not.toHaveAttribute("data-tiers", /quick|standard/);
    // Its top row is the static accent only, never an ember.
    await page.getByTestId("effort-slider").focus();
    await page.keyboard.press("End");
    await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-name", "xhigh");
    await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-effort-look", "top");
    await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-ember", "0");
    await expect(page.getByTestId("effort-embers")).toHaveCount(0);
  });

  test("codex New Session sheet enumerates the verified low..ultra vocabulary 1:1", async ({ page }) => {
    await page.goto("/sessions/new");
    await page.getByTestId("new-session-kind-codex").click();
    const slider = page.getByTestId("new-session-effort-slider");
    await expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max,ultra");
    await expect(slider).not.toHaveAttribute("data-tiers", /minimal/);
    await expect(slider).toHaveAttribute("aria-valuemax", "5");
    // Switching from the device's Claude high default preserves the nearest
    // slider position: 2/4 maps to Codex xhigh at 3/5.
    await expect(slider).toHaveAttribute("data-name", "xhigh");
    await expect(slider).toHaveAttribute("data-index", "3");
    // Max shares Claude max's accent; only Ultra gets the full ember field.
    await slider.press("Home");
    const words = ["low", "medium", "high", "xhigh", "max", "ultra"] as const;
    const labels = ["Low", "Medium", "High", "Extra high", "Max", "Ultra"] as const;
    for (const [i, word] of words.entries()) {
      await expect(slider).toHaveAttribute("data-name", word);
      await expect(slider).toHaveAttribute("aria-valuetext", labels[i]);
      await expect(page.getByTestId("new-session-effort-title")).toHaveText(labels[i]);
      await expect(slider).toHaveAttribute("data-effort-look", i === 5 ? "ultracode" : i === 4 ? "top" : "plain");
      await expect(slider).toHaveAttribute("data-ember", i === 5 ? "1" : "0");
      await expect(slider).toHaveAttribute("data-ultracode", "0");
      await expect(page.getByTestId("new-session-effort-embers")).toHaveCount(i === 5 ? 1 : 0);
      if (i < words.length - 1) await slider.press("ArrowRight");
    }
    await expect(page.getByTestId("new-session-effort")).toHaveAttribute("data-effort", "ultra");
  });

  test("an existing session shows the harness as a label, not a menu", async ({ page }) => {
    const mobile = test.info().project.name === "mobile-webkit";
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    // D-042: the read-only harness chip rides inside the options sheet on
    // phones; open it before the chip assertions.
    if (mobile) await page.getByTestId("model-effort-chip").click();
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
      // The LAUNCH fieldset offers all six CLI modes (incl. plan/dontAsk);
      // every one stays single-line at both widths.
      await expect(chips).toHaveCount(6);
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
      // effort is the frameless inline slider spanning the sheet's column.
      await expect(page.getByTestId("new-session-effort-slider")).toBeVisible();
      await expect(page.getByTestId("new-session-effort-title")).toHaveText("high");
      const panel = await page.getByTestId("new-session-effort-slider-panel").boundingBox();
      expect(panel).toBeTruthy();
      expect(panel!.width).toBeLessThanOrEqual(width);
      // The orthogonal ultracode switch is a row under the pill (not a tick).
      await expect(page.getByTestId("new-session-effort-ultracode")).toBeVisible();
      // Five tier stops; full names on the wide sheet, shorts on mobile.
      const tickKind = width === 390 ? "effortTickShort" : "effortTickFull";
      await expect(
        page.locator(`[data-testid='new-session-effort-slider-panel'] [class*='${tickKind}']`),
      ).toHaveText(
        width === 390
          ? ["low", "med", "high", "xhigh", "max"]
          : ["low", "medium", "high", "xhigh", "max"],
      );
      if (test.info().project.name === "chromium") {
        await shot(page, `composer-1-new-session-${width}.png`);
      }
    }
  });

  test("Settings default effort flows into New Session", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByTestId("settings-effort")).toBeVisible();
    await page.getByTestId("settings-effort-max").click();
    await expect(page.getByTestId("settings-effort-max")).toHaveAttribute("data-selected", "1");
    await page.goto("/sessions/new");
    const slider = page.getByTestId("new-session-effort-slider");
    await expect(slider).toHaveAttribute("data-name", "max");
    await expect(slider).toHaveAttribute("data-index", "4");
    await expect(slider).toHaveAttribute("data-effort-look", "top");
    await expect(slider).toHaveAttribute("data-ember", "0");
    // Switching the harness re-snaps onto the new native table, top to top.
    await page.getByTestId("new-session-kind-grok").click();
    await expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh");
    await expect(slider).toHaveAttribute("data-name", "xhigh");
    await expect(slider).toHaveAttribute("data-effort-look", "top");
    await expect(slider).toHaveAttribute("data-ember", "0");
    await expect(page.getByTestId("new-session-effort-title")).toHaveText("xhigh");
  });

  test("effort selection persists after reload, incl. the ultracode switch", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await openEffort(page);
    const slider = page.getByTestId("effort-slider");
    await slider.focus();
    // End is the top TIER (max, five stops); then flip the orthogonal switch.
    await page.keyboard.press("End");
    await expect(slider).toHaveAttribute("data-name", "max");
    await page.getByTestId("effort-ultracode-switch").click();
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "max");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-ultracode", "1");
    await page.reload();
    // The level and the flag both survive reload from the durable record.
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "max");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-ultracode", "1");
    await openEffort(page);
    await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-name", "max");
    await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-index", "4");
    await expect(page.getByTestId("effort-ultracode-switch")).toHaveAttribute(
      "aria-checked",
      "true",
    );
  });

  test("structured Grok session keeps a READ-ONLY native permission chip (no Claude-id menu)", async ({ page }) => {
    const mobile = test.info().project.name === "mobile-webkit";
    await page.goto("/sessions");
    await row(page, "Grok 会话").click();
    await expect(page.getByTestId("session-page")).toBeVisible();
    const structured = page.getByTestId("view-switch-structured");
    await expect(structured).toBeVisible();
    await structured.click();
    await expect(page.getByTestId("composer-bar")).toBeVisible();
    await expect(page.getByTestId("model-effort-chip")).toBeVisible();
    // c-effortui r3 item 7: a non-Claude live session never gets the Claude
    // shift+tab wheel (the Node maps acceptEdits/auto to different native
    // modes for grok); the chip is read-only and opens no Claude-id menu.
    if (mobile) {
      // D-042: the harness chip, context chip and the read-only permission
      // chip live in the options sheet; the fused trigger is the anchor.
      const trigger = page.getByTestId("model-effort-chip");
      await expect(trigger).toHaveAttribute("aria-expanded", "false");
      await trigger.click();
      await expect(trigger).toHaveAttribute("aria-expanded", "true");
      await expect(page.getByTestId("harness-chip")).toHaveAttribute("data-readonly", "1");
      await expect(page.getByTestId("context-chip")).toBeVisible();
      const permission = page.getByTestId("permission-chip");
      await expect(permission).toHaveAttribute("data-readonly", "1");
      await expect(page.getByTestId("permission-menu")).toHaveCount(0);
    } else {
      await expect(page.getByTestId("harness-chip")).toHaveAttribute("data-readonly", "1");
      await expect(page.getByTestId("context-chip")).toBeVisible();
      const permission = page.getByTestId("permission-chip");
      await expect(permission).toHaveAttribute("data-readonly", "1");
      // It is a static chip, not a button: no wheel ever opens.
      await expect(page.getByTestId("permission-menu")).toHaveCount(0);
    }
  });

  test("the fill reaches the knob at every stop; the ember field exists on ultracode alone", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await openEffort(page);
    const slider = page.getByTestId("effort-slider");
    await slider.focus();
    // Walk every stop from the first, where the fill is at its shortest.
    await page.keyboard.press("Home");
    await expect(slider).toHaveAttribute("data-index", "0");
    await assertFillReachesKnob(page);
    await expect(page.getByTestId("effort-embers")).toHaveCount(0);
    // Walk medium → xhigh: plain tiers carry no accent at all.
    for (const index of ["1", "2"] as const) {
      await page.keyboard.press("ArrowRight");
      await expect(slider).toHaveAttribute("data-index", index);
      await expect(slider).toHaveAttribute("data-effort-look", "plain");
      await expect(slider).toHaveAttribute("data-ember", "0");
      await assertFillReachesKnob(page);
    }
    // xhigh (stop 3): restrained top accent, still no ember field.
    await page.keyboard.press("ArrowRight");
    await expect(slider).toHaveAttribute("data-index", "3");
    await expect(slider).toHaveAttribute("data-effort-look", "top");
    await expect(slider).toHaveAttribute("data-ember", "0");
    await assertFillReachesKnob(page);
    // max (stop 4): the SAME static top accent — visibly not the ember.
    await page.keyboard.press("ArrowRight");
    await expect(slider).toHaveAttribute("data-index", "4");
    await assertFillReachesKnob(page);
    await expect(slider).toHaveAttribute("data-effort-look", "top");
    await expect(slider).toHaveAttribute("data-ember", "0");
    await expect(page.getByTestId("effort-embers")).toHaveCount(0);

    // max is the LAST tier stop (five stops, index 4). Ultracode is the
    // orthogonal switch row under the pill, NOT a sixth tick.
    await page.keyboard.press("End");
    await expect(slider).toHaveAttribute("data-index", "4");
    await expect(slider).toHaveAttribute("data-name", "max");
    await assertFillReachesKnob(page);
    await expect(slider).toHaveAttribute("data-effort-look", "top");
    await expect(slider).toHaveAttribute("data-ember", "0");
    await expect(page.getByTestId("effort-embers")).toHaveCount(0);
    // ArrowRight at the end cannot advance to a sixth stop.
    await page.keyboard.press("ArrowRight");
    await expect(slider).toHaveAttribute("data-index", "4");

    // Flipping the switch ON at max carries the ember: the strongest field is
    // the COLLAPSED TRIGGER's sparks (the pill itself keeps the restrained top
    // accent — Claude ember lives on the switch/trigger, never the tier pill).
    await page.getByTestId("effort-ultracode-switch").click();
    await expect(slider).toHaveAttribute("data-index", "4");
    await expect(slider).toHaveAttribute("data-name", "max");
    await assertFillReachesKnob(page);
    // The pill shows the restrained top accent and mounts NO in-pill embers.
    await expect(slider).toHaveAttribute("data-ember", "0");
    await expect(page.getByTestId("effort-embers")).toHaveCount(0);
    // …but the collapsed trigger chip carries the ember sparks (3 layers).
    const chip = page.getByTestId("model-effort-chip");
    await expect(chip).toHaveAttribute("data-ember", "1");
    const sparks = chip.locator("[class*='emberSpark']");
    await expect(sparks).toHaveCount(3);
    // Motion is transform/opacity only — nothing here animates layout.
    const motion = await sparks.evaluateAll((nodes) =>
      nodes.map((node) => {
        const style = getComputedStyle(node);
        return { duration: style.animationDuration, name: style.animationName };
      }),
    );
    for (const m of motion) expect(m.name).not.toMatch(/width|height|left|top|margin/);
    await expect(page.getByTestId("effort-knob")).toBeVisible();
  });

  test("touch targets stay >= 44px while the pill stays 40px", async ({ page }) => {
    test.skip(test.info().project.name !== "mobile-webkit", "touch sizing is the mobile breakpoint");
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await openEffort(page);
    await assertPillGeometry(page);
    await assertTouchTargets(page);
  });

  test("reduced motion freezes the ultracode ember but keeps the strongest static state", async ({ page }) => {
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await openEffort(page);
    const slider = page.getByTestId("effort-slider");
    await slider.focus();
    await page.keyboard.press("End");
    await expect(slider).toHaveAttribute("data-name", "max");
    // Ultracode is the orthogonal switch, not a sixth tick; flip it at max.
    await page.getByTestId("effort-ultracode-switch").click();
    await expect(slider).toHaveAttribute("data-name", "max");
    // Ember rides the collapsed trigger chip, never the pill.
    const chip = page.getByTestId("model-effort-chip");
    await expect(chip).toHaveAttribute("data-ember", "1");
    await expect(slider).toHaveAttribute("data-ember", "0");
    // Reduced motion freezes the trigger sparks.
    const sparkNames = await chip
      .locator("[class*='emberSpark']")
      .evaluateAll((nodes) => nodes.map((n) => getComputedStyle(n).animationName));
    expect(sparkNames.length).toBeGreaterThan(0);
    for (const name of sparkNames) expect(name).toBe("none");
    // Switching the flag off leaves max on its static top accent with the
    // ember gone.
    await page.getByTestId("effort-ultracode-switch").click();
    await expect(slider).toHaveAttribute("data-name", "max");
    await expect(slider).toHaveAttribute("data-effort-look", "top");
    await expect(chip).toHaveAttribute("data-ember", "0");
    await expect(page.getByTestId("effort-embers")).toHaveCount(0);
  });

  test("effort ladder evidence: night/ledger at 1440, 768 and 390", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "evidence shots from chromium only");
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.goto("/sessions");
    await row(page, "空闲会话").click();
    await expect(page.getByTestId("composer-bar")).toBeVisible();
    for (const theme of ["night", "ledger"] as const) {
      await setMode(page, theme);
      for (const [width, height, tag] of [
        [1440, 900, "1440"],
        [768, 900, "768"],
        [390, 844, "390"],
      ] as const) {
        await page.setViewportSize({ width, height });
        // plain = ordinary high; top = restrained max accent; ultra = max
        // with the orthogonal switch ON (ember on the collapsed trigger).
        // Static frames.
        for (const [state, keys, name, look, ember] of [
          ["plain", ["Home", "ArrowRight", "ArrowRight"], "high", "plain", "0"],
          ["top", ["End"], "max", "top", "0"],
          ["ultra", ["End"], "max", "top", "1"],
        ] as const) {
          await page.keyboard.press("Escape");
          await openEffort(page);
          await page.getByTestId("effort-slider").focus();
          for (const key of keys) await page.keyboard.press(key);
          const slider = page.getByTestId("effort-slider");
          // The switch is an independent axis that persists across frames,
          // so sync it explicitly for each captured state.
          const sw = page.getByTestId("effort-ultracode-switch");
          const on = (await sw.getAttribute("aria-checked")) === "true";
          if (state === "ultra" && !on) await sw.click();
          if (state !== "ultra" && on) await sw.click();
          await expect(slider).toHaveAttribute("data-name", name);
          await expect(slider).toHaveAttribute("data-effort-look", look);
          await expect(slider).toHaveAttribute("data-ember", "0");
          await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", ember);
          await expect(page.getByTestId("effort-knob")).toBeVisible();
          await assertPillGeometry(page);
          const box = await page.getByTestId("effort-menu").boundingBox();
          expect(box).toBeTruthy();
          if (width < 768) {
            // D-042: at compact width the effort card is the full-width
            // frameless node inside the options sheet.
            await expect(page.getByTestId("effort-menu")).toHaveAttribute("data-in-sheet", "1");
            expect(box!.width).toBeGreaterThan(301);
            expect(box!.width).toBeLessThanOrEqual(width);
          } else {
            // Desktop/tablet: compact anchored card capped at 300px.
            await expect(page.getByTestId("effort-menu")).not.toHaveAttribute("data-in-sheet", "1");
            expect(box!.width).toBeLessThanOrEqual(Math.min(width, 301));
          }
          await assertFillReachesKnob(page);
          await shotComposer(page, `composer-slider-5-${state}-${theme}-${tag}.png`);
        }
      }
    }
    await page.setViewportSize({ width: 390, height: 844 });
    await page.keyboard.press("Escape");
    await openEffort(page);
    const menu = await page.getByTestId("effort-menu").boundingBox();
    expect(menu).toBeTruthy();
    expect(menu!.width).toBeLessThanOrEqual(390);
    await assertPillGeometry(page);
    await assertFillReachesKnob(page);
  });
});
