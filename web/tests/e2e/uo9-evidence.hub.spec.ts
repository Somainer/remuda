import { expect, test, type Page } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * UO-9 evidence: the single inbox shell at 390 and 1440 in both modes.
 *  - 390 (fine pointer) lands on /m/inbox (the compact two-tier shell);
 *  - 1440 lands on /approvals (the desktop three-tier shell);
 * both render the SAME decision card and kind radiogroup.
 *
 * A default run skips everything; REMUDA_EVIDENCE=1 writes
 * docs/design/evidence/ui-overhaul/UO-9-inbox-<mode>-<width>.png. Only Remuda
 * itself is captured (D-053 不做什么).
 */

test.skip(!process.env.REMUDA_EVIDENCE, "set REMUDA_EVIDENCE=1 to capture the committed screenshots");
test.describe.configure({ mode: "serial" });

const shotDir = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../docs/design/evidence/ui-overhaul",
);
const MODES = ["dark", "light"] as const;
const WIDTHS = [390, 1440] as const;
type Mode = (typeof MODES)[number];

const created: string[] = [];

async function createPendingSession(page: Page): Promise<void> {
  await page.evaluate(async () => {
    const hosts = (await (await fetch("/v1/hosts", { credentials: "include" })).json()) as {
      items?: { hostId?: string; label?: string }[];
    };
    const hostId =
      hosts.items?.find((host) => host.label === "e2e-fake-node")?.hostId ?? hosts.items?.[0]?.hostId;
    const response = await fetch("/v1/instances", {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        hostId,
        workspaceId: "/tmp",
        kind: "claude",
        driver: "claude-print",
        prompt: "UO-9 evidence inbox approval",
      }),
    });
    if (!response.ok) throw new Error(`create ${response.status}`);
    const body = (await response.json()) as { instance: { instanceId: string } };
    return body.instance.instanceId;
  }).then((id) => {
    if (id) created.push(id);
  });
}

async function waitForPending(page: Page): Promise<void> {
  await expect.poll(
    async () =>
      page.evaluate(async () => {
        const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
        return (body.items ?? []).filter(
          (item: { state?: string }) => item.state === "pending",
        ).length;
      }),
    { timeout: 20_000, intervals: [500, 1000] },
  ).toBeGreaterThan(0);
}

async function shoot(page: Page, mode: Mode, width: number): Promise<void> {
  await setMode(page, mode);
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.waitForTimeout(300);
  await writeFile(
    path.join(shotDir, `UO-9-inbox-${mode}-${width}.png`),
    await page.screenshot({ animations: "disabled" }),
  );
}

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  try {
    await login(page);
    // Two pending approvals so the queue and the 待处理 count both read clearly.
    await createPendingSession(page);
    await createPendingSession(page);
  } finally {
    await page.close();
  }
  await mkdir(shotDir, { recursive: true });
});

test.afterAll(async ({ browser }) => {
  const page = await browser.newPage();
  try {
    await login(page);
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  } finally {
    await page.close();
  }
});

for (const width of WIDTHS) {
  test(`UO-9 inbox at ${width}`, async ({ page }) => {
    test.setTimeout(120_000);
    const phone = width < 768;
    await page.setViewportSize({ width, height: phone ? 844 : 900 });
    await page.emulateMedia({ reducedMotion: "reduce" });
    await login(page);
    await waitForPending(page);

    await page.goto("/approvals");
    if (phone) {
      await expect(page).toHaveURL(/\/m\/inbox/);
      await expect(page.getByTestId("m-inbox")).toBeVisible();
      await expect(page.getByTestId("approval-row").first()).toBeVisible({ timeout: 20_000 });
    } else {
      await expect(page).toHaveURL(/\/approvals(?:\?|$)/);
      await expect(page.getByTestId("approvals-page")).toBeVisible();
      await expect(page.getByTestId("approval-row").first()).toBeVisible({ timeout: 20_000 });
    }
    await page.waitForLoadState("networkidle").catch(() => undefined);
    for (const mode of MODES) await shoot(page, mode, width);
  });
}
