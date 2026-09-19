import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * context-usage-1: the composer's context chip ring and hover popover backed
 * by the Hub's additive usage rollup. The fake Node appends real protocol
 * `usage` observations when the prompt is the
 * `usage:<in>,<out>,<cacheRead>,<cacheWrite>` sentinel (`-` = the harness
 * never reported that channel). Ground truth is the counters in the sentinel
 * itself; the ring percentage and popover numbers are asserted against that
 * arithmetic, never against the page.
 */

test.describe.configure({ mode: "serial" });

const evidence =
  process.env.REMUDA_EVIDENCE === "1"
    ? path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
    : path.join(path.dirname(fileURLToPath(import.meta.url)), "../../test-results/evidence");

async function shot(page: Page, name: string) {
  await mkdir(evidence, { recursive: true });
  await page.screenshot({
    path: path.join(evidence, name),
    animations: "disabled",
  });
}

/** Raise the shared 8-instance cap, same dance as ux-code.hub. */
async function raiseCap(page: Page, to = 24) {
  const hosts = await page.evaluate(async () => {
    const response = await fetch("/v1/hosts", { credentials: "include" });
    return response.json();
  });
  const host = (hosts.items ?? []).find((h: { label?: string }) => h.label === "e2e-fake-node");
  if (!host) return null;
  const hostId = (host.hostId ?? host.id) as string;
  const previous = (host.maxInstances ?? 8) as number;
  if (previous < to) {
    await page.evaluate(
      ({ id, value }) =>
        fetch(`/v1/hosts/${id}`, {
          method: "PATCH",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ maxInstances: value }),
        }),
      { id: hostId, value: to },
    );
  }
  return { hostId, previous };
}

async function createReadySession(page: Page): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-prompt").fill("ux-usage fixture");
  const creating = page.waitForResponse(
    (response) =>
      response.request().method() === "POST" &&
      new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const res = await creating;
  expect(res.ok(), `instance create failed: ${res.status()}`).toBe(true);
  const instanceId = (await res.json()).instance.instanceId as string;
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });

  // The create-time approval keeps the composer disabled until answered.
  await page.evaluate(async (id) => {
    const list = await fetch("/v1/interactions", { credentials: "include" });
    const body = (await list.json()) as {
      items?: {
        id: string;
        instanceId?: string;
        state?: string;
        request?: { inputDigest?: string; options?: { id: string }[] };
      }[];
    };
    for (const item of (body.items ?? []).filter(
      (it) => it.instanceId === id && it.state === "pending",
    )) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" },
        }),
      });
    }
  }, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 30_000 });
  return instanceId;
}

async function sendUsageTurn(page: Page, spec: string, waitIdle = false, mobile = false) {
  // After turn 1 the fake harness reports idle; the composer must have
  // settled its previous `sending` (the store awaits catchup after the
  // journal echo — an Enter pressed meanwhile is a deliberate no-op).
  if (waitIdle) {
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "idle", {
      timeout: 30_000,
    });
  }
  const composer = page.getByTestId("composer-input");
  await expect(composer).toBeEnabled();
  await expect(page.getByTestId("composer-send")).not.toHaveText("发送中");
  await composer.fill(`usage:${spec}`);
  // The mobile composer ignores Enter (on-screen keyboards send their own
  // newline); the send button is the submit action on touch widths.
  if (mobile) {
    await page.getByTestId("composer-send").click();
  } else {
    await composer.press("Enter");
  }
  await expect(page.getByTestId("transcript")).toContainText(`usage recorded: usage:${spec}`, {
    timeout: 30_000,
  });
}

let cap: { hostId: string; previous: number } | null = null;
const created: string[] = [];

test.beforeEach(async ({ page }) => {
  await login(page);
  if (!cap) cap = await raiseCap(page);
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0, created.length)) {
    await page
      .evaluate(
        (value) =>
          fetch(`/v1/instances/${value}?force=1`, {
            method: "DELETE",
            credentials: "include",
          }),
        id,
      )
      .catch(() => {});
  }
});

test.afterAll(async ({ browser }) => {
  if (!cap) return;
  const page = await browser.newPage();
  try {
    await login(page);
    await page.evaluate(
      ({ id, value }) =>
        fetch(`/v1/hosts/${id}`, {
          method: "PATCH",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ maxInstances: value }),
        }),
      { id: cap!.hostId, value: cap!.previous },
    );
  } finally {
    await page.close();
  }
});

test("context chip: ring percentage and popover rollup over three turns", async ({
  page,
}, testInfo) => {
  test.setTimeout(180_000);
  const instanceId = await createReadySession(page);
  created.push(instanceId);

  // Before any usage observation the chip is an empty ring with a dash and
  // cannot open a popover.
  const chip = page.getByTestId("context-chip");
  await expect(chip).toHaveText("—");
  expect(chip).toHaveAttribute("data-has-popover", "0");
  await chip.click();
  expect(await page.getByTestId("context-usage-popover").count()).toBe(0);

  // Turn 1 — the recorded sequence's first row (4794 / 260 / 29496 read).
  // Next-request context = 4794 + 29496 = 34290 → 17% of the 200k window.
  await sendUsageTurn(page, "4794,260,29496,0");
  await expect(chip).toHaveText("17%", { timeout: 15_000 });
  // The additive Hub rollup rides the instance poll (2 s); the 17% label
  // itself is already painted client-side from the usage event, so wait for
  // the poll-hydrated rollup before exercising the popover.
  await expect(chip).toHaveAttribute("data-has-popover", "1", { timeout: 10_000 });
  const ring = chip.locator("span").first();
  await expect(ring).toHaveAttribute("style", /--ctx-pct:\s*17%/);

  await chip.click();
  const popover = page.getByTestId("context-usage-popover");
  await expect(popover).toBeVisible();
  await expect(page.getByTestId("context-usage-headline")).toHaveText(
    "上下文 34.3k/200.0k (17%)",
  );
  await expect(page.getByTestId("context-usage-bar")).toHaveAttribute("data-pct", "17");
  await expect(page.getByTestId("context-usage-cell-入")).toContainText("4.8k");
  await expect(page.getByTestId("context-usage-cell-出")).toContainText("260");
  await expect(page.getByTestId("context-usage-cell-缓存读")).toContainText("29.5k");
  await expect(page.getByTestId("context-usage-cell-缓存写")).toContainText("0");
  await expect(page.getByTestId("context-usage-turns")).toHaveText("1");
  // The turn lands inside the 60 s window; 5 min is the /5 average.
  await expect(popover.getByRole("row").filter({ hasText: "60 秒" })).toContainText("4.8k");
  await expect(popover.getByRole("row").filter({ hasText: "60 秒" })).toContainText("260");
  await expect(popover.getByRole("row").filter({ hasText: "5 分钟" })).toContainText("959");
  await expect(popover.getByRole("row").filter({ hasText: "5 分钟" })).toContainText("52");
  await expect(page.getByTestId("context-usage-close")).toBeVisible();
  if (process.env.REMUDA_EVIDENCE === "1") await shot(page, "context-usage-1-desktop.png");

  // Turn 2 — 1839 / 185 / 33592: last-turn context = 35431 → 18%, totals add.
  await sendUsageTurn(page, "1839,185,33592,0", true);
  await expect(chip).toHaveText("18%", { timeout: 15_000 });
  // Moving the pointer to the composer dismisses the hover-anchored panel;
  // reopen to read the refreshed numbers.
  await chip.click();
  await expect(page.getByTestId("context-usage-headline")).toHaveText(
    "上下文 35.4k/200.0k (18%)",
  );
  await expect(page.getByTestId("context-usage-cell-入")).toContainText("6.6k");
  await expect(page.getByTestId("context-usage-cell-出")).toContainText("445");
  await expect(page.getByTestId("context-usage-cell-缓存读")).toContainText("63.1k");
  await expect(page.getByTestId("context-usage-turns")).toHaveText("2");

  // Turn 3 — Grok-shaped: output only (`-` channels). Session sums keep
  // growing for reported channels; the context figure becomes unknown and
  // the headline names the channel that would supply it.
  await sendUsageTurn(page, "-,420,-,-", true);
  await expect(chip).toHaveText("—", { timeout: 15_000 });
  await chip.click();
  await expect(page.getByTestId("context-usage-headline")).toHaveText(
    "上下文 —/200.0k (—%)",
  );
  const headlineTitle = await page
    .getByTestId("context-usage-headline")
    .getAttribute("title");
  expect(headlineTitle ?? "").toMatch(/input|cacheRead|cacheCreation/);
  await expect(page.getByTestId("context-usage-cell-入")).toContainText("6.6k");
  await expect(page.getByTestId("context-usage-cell-出")).toContainText("865");
  await expect(page.getByTestId("context-usage-cell-缓存读")).toContainText("63.1k");
  await expect(page.getByTestId("context-usage-turns")).toHaveText("3");

  await page.keyboard.press("Escape");
  await expect(popover).toHaveCount(0);
});

test("context chip popover becomes a sheet at 390 px touch width", async ({ browser }) => {
  // The default desktop context already ran the cap raise; a narrow context
  // reuses the enrolled login cookie.
  const context = await browser.newContext({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
  });
  const narrow = await context.newPage();
  try {
    await login(narrow);
    const instanceId = await createReadySession(narrow);
    created.push(instanceId);
    await sendUsageTurn(narrow, "4794,260,29496,0", false, true);
    // D-042 (c-composer): at compact widths the context chip rides inside the
    // composer options sheet, not the collapsed bar. Open the sheet first.
    await expect(narrow.getByTestId("context-chip")).toHaveCount(0);
    await narrow.getByTestId("model-effort-chip").click();
    await expect(narrow.getByTestId("composer-options-sheet")).toBeVisible();
    const chip = narrow.getByTestId("context-chip");
    await expect(chip).toHaveText("17%", { timeout: 15_000 });
    await expect(chip).toHaveAttribute("data-has-popover", "1", { timeout: 10_000 });
    await chip.click();
    const sheet = narrow.getByTestId("context-usage-popover");
    await expect(sheet).toBeVisible();
    expect(sheet).toHaveAttribute("data-mobile", "1");
    // Close affordance exists and dismisses the sheet.
    await narrow.getByTestId("context-usage-close").click();
    await expect(sheet).toHaveCount(0);
  } finally {
    await context.close();
  }
});
