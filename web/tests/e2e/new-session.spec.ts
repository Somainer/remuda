import { expect, test, type Page } from "@playwright/test";
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
  : path.join(here, "../../test-results/new-session");

/** The effort card only, so no personal path or hostname can land in a shot. */
async function shotEffort(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  const card = page.getByTestId("new-session-effort");
  await card.scrollIntoViewIfNeeded();
  const box = await card.boundingBox();
  expect(box).toBeTruthy();
  const viewport = page.viewportSize() ?? { width: 1440, height: 900 };
  const x = Math.max(0, Math.floor(box!.x) - 8);
  const y = Math.max(0, Math.floor(box!.y) - 8);
  await page.screenshot({
    path: path.join(shotDir, name),
    animations: "disabled",
    clip: {
      x,
      y,
      width: Math.max(1, Math.min(viewport.width - x, Math.ceil(box!.width) + 16)),
      height: Math.max(1, Math.min(viewport.height - y, Math.ceil(box!.height) + 16)),
    },
  });
}

/** The pill's proportions, shared with the composer: ~40px track, ~36px knob. */
async function assertPill(page: Page) {
  const pill = await page.getByTestId("new-session-effort-track").boundingBox();
  const knob = await page.getByTestId("new-session-effort-knob").boundingBox();
  const fill = await page.getByTestId("new-session-effort-fill").boundingBox();
  expect(pill).toBeTruthy();
  expect(knob).toBeTruthy();
  expect(fill).toBeTruthy();
  expect(pill!.height).toBeGreaterThanOrEqual(36);
  expect(pill!.height).toBeLessThanOrEqual(42);
  expect(knob!.width).toBeGreaterThanOrEqual(32);
  expect(knob!.width).toBeLessThanOrEqual(38);
  // The fill runs under the knob to its far edge — no bare track beside the thumb.
  expect(fill!.x + fill!.width).toBeGreaterThanOrEqual(knob!.x + knob!.width - 1);
  expect(fill!.x).toBeLessThanOrEqual(pill!.x + 1);
}

test.describe("new session sheet", () => {
  test("mobile 30s path: focus prompt, type, start", async ({ page }) => {
    await page.goto("/sessions/new");
    const prompt = page.getByTestId("new-session-prompt");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await expect(prompt).toBeFocused();
    await expect(page.getByTestId("new-session-perm-bypassPermissions")).toBeVisible();
    await expect(page.getByTestId("new-session-delegation-none")).toBeVisible();
    await page.getByTestId("new-session-perm-bypassPermissions").click();
    await expect(page.getByTestId("new-session-yolo-hint")).toBeVisible();
    await prompt.fill("查 bolt TaskManager spill");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByTestId("session-page")).toBeVisible();
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "starting");
    await expect(page.getByTestId("session-page").getByTestId("message")).toContainText("查 bolt TaskManager spill");
  });

  test("kind terminal uses shell-pty and opens the terminal tab", async ({ page }) => {
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await page.getByTestId("new-session-kind-terminal").click();
    await expect(page.getByTestId("new-session-terminal-driver")).toContainText("shell-pty");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-driver", "shell-pty");
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
    await expect(page.locator("[data-tty-lab='1']")).toBeVisible();
    await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("view-switch")).toHaveAttribute("data-view", "tty");
    await expect(page.getByTestId("view-switch-tty")).toHaveAttribute("aria-checked", "true");
    await expect(page.getByTestId("view-switch-structured")).toHaveAttribute("aria-checked", "false");
    if (test.info().project.name === "chromium") {
      await mkdir(shotDir, { recursive: true });
      await page.screenshot({ path: path.join(shotDir, "terminal-1-new-session.png"), animations: "disabled" });
    }
  });

  test("effort is the composer's slider and the keyboard value reaches the created instance", async ({ page }) => {
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    const slider = page.getByTestId("new-session-effort-slider");
    // Inline card, not a popover: the pill is on the page with no chip to open.
    await expect(page.getByTestId("new-session-effort-slider-panel")).toHaveAttribute("data-view", "slider");
    await expect(slider).toHaveAttribute("role", "slider");
    await expect(slider).toHaveAttribute("data-tiers", "default,think,think-hard,ultracode");
    await expect(slider).toHaveAttribute("data-name", "think");
    await expect(page.getByTestId("new-session-effort")).toContainText("写进 InstanceSpec，会话内可再改");
    await assertPill(page);

    // Set the tier by keyboard alone, the way a discrete slider must answer.
    await slider.focus();
    await page.keyboard.press("Home");
    await expect(slider).toHaveAttribute("data-name", "default");
    await expect(slider).toHaveAttribute("aria-valuenow", "0");
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowRight");
    await expect(slider).toHaveAttribute("data-name", "think-hard");
    await expect(page.getByTestId("new-session-effort-title")).toHaveText("think-hard");
    await expect(slider).toHaveAttribute("data-ember", "0");
    await page.keyboard.press("End");
    await expect(slider).toHaveAttribute("data-name", "ultracode");
    await expect(slider).toHaveAttribute("data-ember", "1");
    await expect(page.getByTestId("new-session-effort-embers")).toBeVisible();
    await assertPill(page);

    // Back off the top so the asserted value is not simply the End default.
    await page.keyboard.press("ArrowLeft");
    await expect(slider).toHaveAttribute("data-name", "think-hard");
    await expect(page.getByTestId("new-session-effort")).toHaveAttribute("data-effort", "think-hard");

    await page.getByTestId("new-session-prompt").fill("effort by keyboard");
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//);
    // The created instance carries the tier the slider was left on.
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort", "think-hard");
    await expect(page.getByTestId("composer")).toHaveAttribute("data-effort-index", "2");
    await expect(page.getByTestId("model-effort-chip")).toContainText("think-hard");
  });

  test("the slider re-snaps onto the harness selected above", async ({ page }) => {
    await page.goto("/sessions/new");
    const slider = page.getByTestId("new-session-effort-slider");
    await slider.focus();
    await page.keyboard.press("End");
    await expect(slider).toHaveAttribute("data-name", "ultracode");

    // grok's table is three tiers; the top tier stays the top tier.
    await page.getByTestId("new-session-kind-grok").click();
    await expect(slider).toHaveAttribute("data-tiers", "quick,standard,max");
    await expect(slider).toHaveAttribute("data-name", "max");
    await expect(slider).toHaveAttribute("aria-valuemax", "2");
    await expect(slider).toHaveAttribute("data-ember", "1");
    await slider.focus();
    await page.keyboard.press("Home");
    await expect(slider).toHaveAttribute("data-name", "quick");

    // ...and back to claude, renamed onto the four-tier table, ember dropped.
    await page.getByTestId("new-session-kind-claude").click();
    await expect(slider).toHaveAttribute("data-tiers", "default,think,think-hard,ultracode");
    await expect(slider).toHaveAttribute("data-name", "default");
    await expect(slider).toHaveAttribute("data-ember", "0");

    // terminal has no effort axis at all, so the whole card goes.
    await page.getByTestId("new-session-kind-terminal").click();
    await expect(page.getByTestId("new-session-effort")).toHaveCount(0);
  });

  test("new session slider evidence: night/ledger at 1440 and 390", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "evidence shots from chromium only");
    await page.emulateMedia({ reducedMotion: "reduce" });
    for (const theme of ["night", "ledger"] as const) {
      for (const [width, height, tag] of [
        [1440, 900, "1440"],
        [390, 844, "390"],
      ] as const) {
        await page.setViewportSize({ width, height });
        await page.goto("/sessions/new");
        await page.evaluate((next) => {
          document.documentElement.setAttribute("data-theme", next);
        }, theme);
        const slider = page.getByTestId("new-session-effort-slider");
        await expect(slider).toBeVisible();
        // mid = a middle non-top tier, top = the ember tier.
        for (const [state, keys] of [
          ["mid", ["Home", "ArrowRight", "ArrowRight"]],
          ["top", ["End"]],
        ] as const) {
          await slider.focus();
          for (const key of keys) await page.keyboard.press(key);
          await expect(slider).toHaveAttribute("data-ember", state === "top" ? "1" : "0");
          await assertPill(page);
          const card = await page.getByTestId("new-session-effort-slider-panel").boundingBox();
          expect(card).toBeTruthy();
          expect(card!.width).toBeLessThanOrEqual(Math.min(width, 401));
          await shotEffort(page, `new-session-slider-1-${state}-${theme}-${tag}.png`);
        }
      }
    }
  });
});
