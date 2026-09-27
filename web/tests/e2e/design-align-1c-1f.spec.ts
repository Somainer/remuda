import { expect, test, type Page } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

const TTY_LAB = "ins_01993ab0-0000-7000-8000-00000000aa01";
const dir = path.join(path.dirname(fileURLToPath(import.meta.url)), "__screenshots__");

// Committed goldens: write only under the evidence opt-in so a plain gate
// run never dirties the tracked screenshot tree.
const capture = process.env.REMUDA_EVIDENCE === "1";

async function shot(page: Page, name: string) {
  if (!capture) return;
  await page.screenshot({ path: path.join(dir, name), animations: "disabled" });
}

test.describe("design align 1c / 1f", () => {
  test.describe.configure({ mode: "serial" });

  test("1440 and 390 screenshots for tty, hosts, providers", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "golden screenshots from chromium only");
    await page.emulateMedia({ reducedMotion: "reduce" });

    for (const [w, h, tag] of [
      [1440, 900, "1440"],
      [390, 844, "390"],
    ] as const) {
      await page.setViewportSize({ width: w, height: h });

      await page.goto(`/s/${TTY_LAB}/tty`);
      await expect(page.locator("[data-tty-lab='1']")).toBeVisible();
      await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 15_000 });
      await shot(page, `1c-tty-${tag}.png`);

      await page.goto("/hosts");
      await expect(page.getByTestId("hosts-page")).toBeVisible();
      await shot(page, `1f-hosts-${tag}.png`);

      const first = page.getByTestId("host-row").first();
      if (await first.count()) {
        await first.click();
        await expect(page.getByTestId("host-detail")).toBeVisible();
        await shot(page, `1f-host-detail-${tag}.png`);
      }

      await page.goto("/providers");
      await expect(page.getByTestId("providers-page")).toBeVisible();
      await shot(page, `1f-providers-${tag}.png`);
    }
  });
});
