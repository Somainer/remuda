import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { seedMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * UO-1 evidence: the foundation layer (tokens, base controls, appearance,
 * terminal palette) on the five reference surfaces, in both modes at 390 and
 * 1440. A default run skips everything and writes nothing; REMUDA_EVIDENCE=1
 * refreshes docs/design/evidence/ui-overhaul/UO-1-<surface>-<mode>-<width>.png.
 *
 * Fake Node content:
 *  - a claude-print session left on its create approval (the amber pending
 *    card), then a fenced code reply ("show me code"), a table echo and a
 *    settled tool card ("toolfold settle");
 *  - a claude-pty session for the terminal surface.
 */

test.describe.configure({ mode: "serial" });

const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/ui-overhaul");

const MODES = ["dark", "light"] as const;
const WIDTHS = [390, 1440] as const;
type Mode = (typeof MODES)[number];

let structuredId = "";
let terminalId = "";

async function createRest(page: Page, body: Record<string, unknown>): Promise<string> {
  return page.evaluate(async (extra) => {
    const hosts = (await (await fetch("/v1/hosts", { credentials: "include" })).json()) as {
      items?: { hostId?: string; label?: string }[];
    };
    const hostId = hosts.items?.find((host) => host.label === "e2e-fake-node")?.hostId ?? hosts.items?.[0]?.hostId;
    const response = await fetch("/v1/instances", {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ hostId, workspaceId: "/tmp", kind: "claude", ...extra }),
    });
    if (!response.ok) throw new Error(`create ${response.status}`);
    return ((await response.json()) as { instance: { instanceId: string } }).instance.instanceId;
  }, body);
}

async function send(page: Page, instanceId: string, prompt: string): Promise<void> {
  const status = await page.evaluate(
    async ({ id, text }) => {
      const response = await fetch(`/v1/instances/${id}/commands`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ operation: "instance.send", payload: { prompt: text } }),
      });
      return response.status;
    },
    { id: instanceId, text: prompt },
  );
  expect(status, `send ${prompt.slice(0, 24)}`).toBeLessThan(300);
}

async function frame(page: Page, mode: Mode, width: number): Promise<void> {
  await page.setViewportSize({ width, height: width <= 767 ? 844 : 900 });
  await page.emulateMedia({ colorScheme: mode, reducedMotion: "reduce" });
}

async function shoot(page: Page, surface: string, mode: Mode, width: number): Promise<void> {
  await expect(page.locator("html")).toHaveAttribute("data-appearance", mode);
  // Web fonts in, then one settled frame.
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.waitForTimeout(300);
  await page.screenshot({
    path: path.join(shotDir, `UO-1-${surface}-${mode}-${width}.png`),
    animations: "disabled",
  });
}

test.beforeAll(async ({ browser }) => {
  if (!evidence) return;
  const page = await browser.newPage();
  try {
    await login(page);
    structuredId = await createRest(page, { driver: "claude-print", prompt: "UO-1 evidence session" });
    await send(page, structuredId, "show me code");
    await send(
      page,
      structuredId,
      "compare the two tiers\n\n| tier | budget | note |\n| --- | ---: | --- |\n| high | 32k | default |\n| max | 64k | slower |",
    );
    await send(page, structuredId, "toolfold settle");
    terminalId = await createRest(page, { prompt: "UO-1 evidence terminal" });
  } finally {
    await page.close();
  }
  await mkdir(shotDir, { recursive: true });
});

test.afterAll(async ({ browser }) => {
  if (!evidence) return;
  const page = await browser.newPage();
  try {
    await login(page);
    for (const id of [structuredId, terminalId].filter(Boolean)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  } finally {
    await page.close();
  }
});

test.beforeEach(async ({ page }) => {
  test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture the committed screenshots");
  await login(page);
});

for (const mode of MODES) {
  for (const width of WIDTHS) {
    test(`UO-1 surfaces in ${mode} at ${width}`, async ({ page }) => {
      test.setTimeout(120_000);
      await seedMode(page, mode);
      await frame(page, mode, width);

      await page.goto(width <= 767 ? "/m" : "/sessions");
      await expect(page.getByTestId(width <= 767 ? "home-list" : "session-list")).toBeVisible({ timeout: 20_000 });
      await shoot(page, "sessions", mode, width);

      await page.goto(`/s/${structuredId}/structured`);
      const transcript = page.getByTestId("transcript");
      await expect(transcript.locator("pre").first()).toBeVisible({ timeout: 30_000 });
      await expect(transcript.locator("table").first()).toBeAttached({ timeout: 30_000 });
      await shoot(page, "structured", mode, width);

      await page.goto(width <= 767 ? "/m/inbox" : "/approvals");
      await page.waitForLoadState("networkidle").catch(() => undefined);
      await shoot(page, "inbox", mode, width);

      await page.goto("/settings");
      await expect(page.getByTestId(`settings-appearance-${mode}`)).toBeVisible();
      await page.getByTestId(`settings-appearance-${mode}`).scrollIntoViewIfNeeded();
      await shoot(page, "settings", mode, width);

      await page.goto(`/s/${terminalId}/tty`);
      await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 20_000 });
      await expect(page.locator(".xterm")).toBeVisible();
      await shoot(page, "terminal", mode, width);
    });
  }
}
