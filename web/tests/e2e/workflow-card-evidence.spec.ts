import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Evidence screenshots for the Workflow timeline card (workbench batch W).
 *
 * Synthetic payloads only (tests/evidence/wf-card-harness.tsx): no real models,
 * no deployment, and none of the approved mock's names. Captured at 1440 /
 * 768 / 390 against the real component and its CSS module.
 */
const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence");

async function shot(page: Page, section: string, width: number, height = 900) {
  await page.setViewportSize({ width, height });
  await page.emulateMedia({ reducedMotion: "reduce" });
  await expect(page.locator(`[data-evidence="${section}"]`)).toBeVisible();
  await mkdir(evidence, { recursive: true });
  const file = path.join(evidence, `workbench-w-card-${section}-${width}.png`);
  await page.locator(`[data-evidence="${section}"]`).screenshot({ path: file, animations: "disabled" });
}

test.describe("workflow card evidence", () => {
  test("1440 / 768 / 390 captures", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "one screenshot set");
    await page.goto("/evidence-workflow.html");
    await expect(page.getByTestId("workflow-card").first()).toBeVisible();

    // Expand the failed card and its phase once (terminal states auto-collapse);
    // state persists across viewport changes in the same page session.
    const failedCard = page.locator('[data-evidence="failed"] [data-testid="workflow-card"]').first();
    await failedCard.locator("button").first().click();
    await page
      .locator('[data-evidence="failed"] [data-testid="workflow-phase"] button')
      .first()
      .click();
    // Same for the 36-agent running section: open Validate so the second grid shows.
    const phaseHeads = page.locator('[data-evidence="fold"] [data-testid="workflow-phase"] button');
    await phaseHeads.nth(1).click();

    for (const width of [1440, 768, 390]) {
      await shot(page, "running", width, 760);
      await shot(page, "done", width, 200);
      await shot(page, "fold", width, 760);
      // Expand the failed phase for captures beyond the first viewport width.
      await shot(page, "failed", width, 1000);
      await shot(page, "flat", width, 160);
    }
  });
});
