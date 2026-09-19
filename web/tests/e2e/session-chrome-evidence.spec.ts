import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
/**
 * A default run must not rewrite the tracked goldens, so shots land in the
 * gitignored `test-results/`. Re-capture the committed evidence with
 * REMUDA_EVIDENCE=1.
 */
const shotDir =
  process.env.REMUDA_EVIDENCE === "1"
    ? path.join(here, "../../../docs/design/evidence")
    : path.join(here, "../../test-results/session-chrome");

function row(page: Page, text: string) {
  return page.getByTestId("session-row").filter({ hasText: text }).first();
}

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

test.describe("session chrome evidence", () => {
  test.describe.configure({ mode: "serial" });

  test("desktop and phone captures for the switch, harness label, and files route", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "golden screenshots from chromium only");
    await page.emulateMedia({ reducedMotion: "reduce" });

    for (const [w, h, tag] of [
      [1440, 900, "1440"],
      [400, 844, "400"],
    ] as const) {
      await page.setViewportSize({ width: w, height: h });

      // Segmented switch on a terminal-capable session, both states. Start from a
      // clean slate so the previous viewport's remembered choice does not leak in.
      await page.goto("/sessions");
      await page.evaluate(() => {
        for (const key of Object.keys(localStorage)) {
          if (key.startsWith("runtime.session-view.")) localStorage.removeItem(key);
        }
      });
      await row(page, "Grok 会话").click();
      await expect(page.getByTestId("view-switch")).toBeVisible();
      // Wait for the session route/view to settle before the tty shot: right
      // after a row click resolvedView can still read "structured" (the
      // previous width's remembered choice), which made the tty and structured
      // goldens byte-identical. Click 终端 and assert the documented state.
      await page.getByTestId("view-switch-tty").click();
      await expect(page).toHaveURL(/\/tty$/);
      await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
      await expect(page.getByTestId("view-switch-tty")).toHaveAttribute("aria-checked", "true");
      await shot(page, `session-chrome-1-switch-tty-${tag}.png`);
      await page.getByTestId("view-switch-structured").click();
      await expect(page).toHaveURL(/\/structured$/);
      await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
      await expect(page.getByTestId("view-switch-structured")).toHaveAttribute("aria-checked", "true");
      await shot(page, `session-chrome-1-switch-structured-${tag}.png`);

      // No switch at all for a structured-only driver; harness is a static label.
      await page.goto("/sessions");
      await row(page, "空闲会话").click();
      await expect(page.getByTestId("view-switch")).toHaveCount(0);
      // Gate the shot on the SESSION page + composer bar (view-switch count 0
      // is already true on the sessions list, so it alone does not prove the
      // row navigation finished); otherwise the golden is a list capture. A
      // structured-only driver lands on the bare /s/{id} route (no suffix).
      await expect(page).toHaveURL(/\/s\/[^/]+$/);
      await expect(page.getByTestId("composer-bar")).toBeVisible();
      // The golden shows the composer bar without a modal scrim, so take the
      // harness-label screenshot with the bar in its normal state.
      await shot(page, `session-chrome-1-harness-label-${tag}.png`);
      // D-042 (c-composer): at compact widths the read-only harness chip
      // rides inside the composer options sheet; assert it there after the
      // screenshot, then close the sheet (without regenerating the golden).
      const compact = w < 768;
      if (compact) {
        await expect(page.getByTestId("harness-chip")).toHaveCount(0);
        await page.getByTestId("model-effort-chip").click();
        await expect(page.getByTestId("composer-options-sheet")).toBeVisible();
        await expect(page.getByTestId("harness-chip")).toHaveAttribute("data-readonly", "1");
        await page.getByTestId("composer-options-close").click();
        await expect(page.getByTestId("composer-options-sheet")).toHaveCount(0);
      } else {
        await expect(page.getByTestId("harness-chip")).toHaveAttribute("data-readonly", "1");
      }

      // Files route: header kept, 文件 active, back affordance present.
      const id = new URL(page.url()).pathname.split("/")[2];
      await page.goto(`/s/${id}/files`);
      await expect(page.getByTestId("files-pane")).toBeVisible();
      await shot(page, `session-chrome-1-files-${tag}.png`);
      await page.getByTestId("files-back").click();
      await expect(page.getByTestId("transcript")).toBeVisible();
    }

    // New Session keeps the harness picker.
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-kind-codex")).toBeVisible();
    await shot(page, "session-chrome-1-new-session-1440.png");
  });
});
