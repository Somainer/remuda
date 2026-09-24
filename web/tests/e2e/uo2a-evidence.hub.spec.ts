import { expect, test, type Page } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * UO-2a evidence: the desktop layout skeleton (sidebar, PageHeader, the
 * sessions index column, the 管理 menu, the folded sidebar) and the phone
 * bottom bar, in both modes at 390 and 1440. A default run skips everything;
 * REMUDA_EVIDENCE=1 writes
 * docs/design/evidence/ui-overhaul/UO-2a-<surface>-<mode>-<width>.png.
 */

test.skip(!process.env.REMUDA_EVIDENCE, "set REMUDA_EVIDENCE=1 to capture the committed screenshots");
test.describe.configure({ mode: "serial" });

const shotDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/ui-overhaul");
const MODES = ["dark", "light"] as const;
const WIDTHS = [390, 1440] as const;
type Mode = (typeof MODES)[number];

let sessionId = "";

async function createSession(page: Page): Promise<string> {
  return page.evaluate(async () => {
    const hosts = (await (await fetch("/v1/hosts", { credentials: "include" })).json()) as {
      items?: { hostId?: string; label?: string }[];
    };
    const hostId = hosts.items?.find((host) => host.label === "e2e-fake-node")?.hostId ?? hosts.items?.[0]?.hostId;
    const response = await fetch("/v1/instances", {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ hostId, workspaceId: "/tmp", kind: "claude", driver: "claude-print", prompt: "UO-2a evidence session" }),
    });
    if (!response.ok) throw new Error(`create ${response.status}`);
    return ((await response.json()) as { instance: { instanceId: string } }).instance.instanceId;
  });
}

async function shoot(page: Page, surface: string, mode: Mode, width: number): Promise<void> {
  await setMode(page, mode);
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.waitForTimeout(300);
  await writeFile(path.join(shotDir, `UO-2a-${surface}-${mode}-${width}.png`), await page.screenshot({ animations: "disabled" }));
}

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  try {
    await login(page);
    sessionId = await createSession(page);
  } finally {
    await page.close();
  }
  await mkdir(shotDir, { recursive: true });
});

test.afterAll(async ({ browser }) => {
  const page = await browser.newPage();
  try {
    await login(page);
    if (sessionId) await page.request.delete(`/v1/instances/${sessionId}?force=1`).catch(() => undefined);
  } finally {
    await page.close();
  }
});

test.beforeEach(async ({ page }) => {
  await login(page);
});

for (const width of WIDTHS) {
  test(`UO-2a layout at ${width}`, async ({ page }) => {
    test.setTimeout(120_000);
    const phone = width < 768;
    await page.setViewportSize({ width, height: phone ? 844 : 900 });
    await page.emulateMedia({ reducedMotion: "reduce" });
    const bottomBar = page.locator('nav[aria-label="手机底栏"]');

    // Compact /sessions redirects to the phone home /m, whose bar is PhoneShell's.
    await page.goto(phone ? "/m" : "/sessions");
    await page.waitForLoadState("networkidle").catch(() => undefined);
    if (phone) {
      await expect(page).toHaveURL(/\/m$/);
      await expect(bottomBar).toBeVisible();
    } else await expect(page.getByTestId("sidebar")).toBeVisible();
    for (const mode of MODES) await shoot(page, "sessions", mode, width);

    await page.goto(`/s/${sessionId}`);
    await expect(page.getByTestId("transcript")).toBeVisible({ timeout: 30_000 });
    // A session page on a phone gives the whole height to the conversation.
    if (phone) await expect(bottomBar).toHaveCount(0);
    for (const mode of MODES) await shoot(page, "session", mode, width);

    await page.goto("/approvals");
    await page.waitForLoadState("networkidle").catch(() => undefined);
    for (const mode of MODES) await shoot(page, "approvals", mode, width);

    if (phone) return;

    await page.goto("/sessions");
    await page.getByTestId("sidebar-admin").click();
    const fleet = page.getByRole("menu", { name: "管理" }).getByRole("menuitem", { name: "集群" });
    await expect(fleet).toHaveAttribute("href", "/fleet");
    for (const mode of MODES) await shoot(page, "admin-menu", mode, width);
    await page.keyboard.press("Escape");

    await page.getByTestId("sidebar-toggle").click();
    await expect(page.getByTestId("sidebar")).toHaveAttribute("data-collapsed", "true");
    for (const mode of MODES) await shoot(page, "collapsed", mode, width);
    await page.getByTestId("sidebar-toggle").click();
    await expect(page.getByTestId("sidebar")).toHaveAttribute("data-collapsed", "false");
  });
}
