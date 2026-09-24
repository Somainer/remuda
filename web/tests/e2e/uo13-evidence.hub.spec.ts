import { expect, test, type Page } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * UO-13 evidence: the management surfaces (§4.10) — hosts + detail, fleet,
 * projects, providers + detail, bots + detail, and the login/pair card — in
 * both modes at 390 and 1440. A default run skips everything;
 * REMUDA_EVIDENCE=1 writes
 * docs/design/evidence/ui-overhaul/UO-13-<surface>-<mode>-<width>.png.
 */

test.skip(!process.env.REMUDA_EVIDENCE, "set REMUDA_EVIDENCE=1 to capture the committed screenshots");
test.describe.configure({ mode: "serial" });

const shotDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/ui-overhaul");
const MODES = ["dark", "light"] as const;
const WIDTHS = [390, 1440] as const;
type Mode = (typeof MODES)[number];

async function shoot(page: Page, surface: string, mode: Mode, width: number): Promise<void> {
  await setMode(page, mode);
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.waitForTimeout(300);
  await writeFile(path.join(shotDir, `UO-13-${surface}-${mode}-${width}.png`), await page.screenshot({ animations: "disabled" }));
}

test.beforeAll(async () => {
  await mkdir(shotDir, { recursive: true });
});

test.beforeEach(async ({ page }) => {
  await login(page, "e2e-uo13-evidence");
});

for (const width of WIDTHS) {
  test(`UO-13 management surfaces at ${width}`, async ({ page }) => {
    test.setTimeout(180_000);
    await page.setViewportSize({ width, height: width < 768 ? 844 : 900 });
    await page.emulateMedia({ reducedMotion: "reduce" });

    await page.goto("/hosts");
    await expect(page.getByTestId("hosts-page")).toBeVisible();
    await page.waitForTimeout(300);
    for (const mode of MODES) await shoot(page, "hosts", mode, width);

    const firstHost = page.getByTestId("host-row").first();
    if (await firstHost.isVisible().catch(() => false)) {
      await firstHost.click();
      await expect(page.getByTestId("host-detail")).toBeVisible();
      for (const mode of MODES) await shoot(page, "host-detail", mode, width);
    }

    await page.goto("/fleet");
    await expect(page.getByTestId("fleet-page")).toBeVisible();
    for (const mode of MODES) await shoot(page, "fleet", mode, width);

    await page.goto("/projects");
    await expect(page.getByTestId("projects-page")).toBeVisible();
    await page.waitForTimeout(300);
    for (const mode of MODES) await shoot(page, "projects", mode, width);

    await page.goto("/providers");
    await expect(page.getByTestId("providers-page")).toBeVisible();
    for (const mode of MODES) await shoot(page, "providers", mode, width);

    const gateway = page.locator('[data-testid=provider-row][data-delegation="gateway"]').first();
    if (await gateway.isVisible().catch(() => false)) {
      await gateway.click();
      await expect(page.getByTestId("provider-detail")).toBeVisible();
      for (const mode of MODES) await shoot(page, "provider-detail", mode, width);
    }

    await page.goto("/bots");
    await expect(page.getByTestId("bots-page")).toBeVisible();
    for (const mode of MODES) await shoot(page, "bots", mode, width);

    await page.locator('[data-testid=bot-row][data-channel="feishu"]').click();
    await expect(page.getByTestId("bot-detail")).toBeVisible();
    for (const mode of MODES) await shoot(page, "bot-detail", mode, width);
  });

  test(`UO-13 login and pair card at ${width}`, async ({ page: probe }) => {
    test.setTimeout(120_000);
    // The card only renders while unauthenticated: a fresh context that never
    // logs in renders both /login and /pair. Clear its storage so the mock's
    // auto-bootstrap (dev only) cannot turn it back into an authed session.
    const browser = probe.context().browser();
    if (!browser) throw new Error("no browser for the login/pair evidence");
    const context = await browser.newContext({
      viewport: { width, height: width < 768 ? 844 : 900 },
      reducedMotion: "reduce",
    });
    const page = await context.newPage();
    await page.addInitScript(() => {
      localStorage.clear();
      localStorage.setItem("runtime.logged-out", "1");
    });
    try {
      await page.goto("/login");
      await expect(page.getByTestId("login-page")).toBeVisible();
      for (const mode of MODES) await shoot(page, "login", mode, width);

      await page.goto("/pair");
      await expect(page.getByTestId("login-page")).toHaveAttribute("data-mode", "pair");
      for (const mode of MODES) await shoot(page, "pair", mode, width);
    } finally {
      await context.close();
    }
  });
}
