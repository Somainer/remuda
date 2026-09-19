import { expect, test, type Page } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

const TTY_LAB = "ins_01993ab0-0000-7000-8000-00000000aa01";
const dir = path.join(path.dirname(fileURLToPath(import.meta.url)), "__screenshots__");

async function shot(page: Page, name: string) {
  await page.screenshot({ path: path.join(dir, name), animations: "disabled", fullPage: false });
}

test.describe("ui screenshots 1440 / 390", () => {
  test.describe.configure({ mode: "serial" });

  test("gallery of every primary route", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "golden screenshots from chromium only");
    await page.emulateMedia({ reducedMotion: "reduce" });

    for (const [w, h, tag] of [
      [1440, 900, "1440"],
      [390, 844, "390"],
    ] as const) {
      await page.setViewportSize({ width: w, height: h });

      await page.goto("/sessions");
      await expect(page.getByTestId("session-list").first()).toBeVisible();
      await shot(page, `sessions-${tag}.png`);

      await page.getByTestId("session-row").filter({ hasText: "看 TaskManager spill" }).first().click();
      await expect(page.getByTestId("session-page")).toBeVisible();
      await expect(page.getByTestId("composer")).toBeVisible();
      await shot(page, `session-${tag}.png`);

      await page.goto(`/s/${TTY_LAB}/tty`);
      await expect(page.locator("[data-tty-lab='1']")).toBeVisible();
      await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 15_000 });
      await shot(page, `tty-${tag}.png`);

      await page.goto("/sessions/new");
      await expect(page.getByTestId("new-session-sheet")).toBeVisible();
      await shot(page, `new-${tag}.png`);

      await page.goto("/approvals");
      // c-minbox: at 390 the /approvals entry redirects to the phone inbox
      // /m/inbox (D-049); the desktop approvals centre is 1440-only.
      if (w < 768) {
        await expect(page).toHaveURL(/\/m\/inbox/);
        await expect(page.getByTestId("m-inbox")).toBeVisible();
      } else {
        await expect(page.getByTestId("approvals-page")).toBeVisible();
      }
      await shot(page, `approvals-${tag}.png`);

      await page.goto("/hosts");
      await expect(page.getByTestId("hosts-page")).toBeVisible();
      await shot(page, `hosts-${tag}.png`);

      await page.goto("/providers");
      await expect(page.getByTestId("providers-page")).toBeVisible();
      await shot(page, `providers-${tag}.png`);

      await page.goto("/bots");
      await expect(page.getByTestId("bots-page")).toBeVisible();
      await shot(page, `bots-${tag}.png`);

      await page.goto("/settings");
      await expect(page.getByTestId("settings-page")).toBeVisible();
      await shot(page, `settings-${tag}.png`);
    }

    await page.addInitScript(() => {
      sessionStorage.setItem("remuda.install-banner", "1");
    });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/sessions");
    await expect(page.getByTestId("install-bar")).toBeVisible();
    await shot(page, "install-390.png");
  });
});
