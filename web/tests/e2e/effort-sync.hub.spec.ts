import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

/**
 * D-028 §9.1 live effort sync against the fake Node in
 * `crates/remuda-hub/examples/hub_e2e.rs`:
 *
 * 1. the session starts with no read-back level — the chip is a greyed `?`;
 * 2. moving the slider to xhigh posts `instance.configure`; the fake node
 *    appends an `effort` observation reporting xhigh — agreement, no mismatch;
 * 3. moving to max, the fake node clamps the effective level to xhigh, so the
 *    chip renders the observed level and 请求 max → 实际 xhigh.
 *
 * No real model is invoked — the fake node writes the transcript events.
 */

test.describe.configure({ mode: "serial" });

const created: string[] = [];

async function createSession(page: Page, prompt: string, kind = "claude"): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await page.getByTestId(`new-session-kind-${kind}`).click();
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

test.beforeEach(async ({ page }) => {
  await login(page);
});

const codexTiers = [
  ["low", "Low", "Fast responses with lighter reasoning"],
  ["medium", "Medium", "Balances speed and reasoning depth for everyday tasks"],
  ["high", "High", "Greater reasoning depth for complex problems"],
  ["xhigh", "Extra high", "Extra high reasoning depth for complex problems"],
  ["max", "Max", "For difficult problems when quality matters more than speed · higher usage"],
  ["ultra", "Ultra", "For demanding work using multiple agents · highest usage"],
] as const;

const evidenceDir = process.env.REMUDA_EVIDENCE === "1"
  ? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
  : path.resolve("test-results/evidence");

for (const width of [390, 1440]) {
  test(`Codex max and ultra round-trip with the six-tier picker at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    const instanceId = await createSession(page, "Codex effort round trip", "codex");
    await clearApprovals(page, instanceId);

    for (const [name, look] of [["max", "top"], ["ultra", "ultracode"]] as const) {
      await page.getByTestId("model-effort-chip").click();
      await page.getByTestId("effort-open-list").click();
      const request = page.waitForRequest((r) =>
        r.method() === "POST" && r.url().endsWith(`/v1/instances/${instanceId}/commands`)
        && r.postDataJSON()?.operation === "instance.configure");
      await page.getByTestId(`effort-tier-${name}`).click();
      const payload = (await request).postDataJSON().payload;
      expect(payload.effort.name).toBe(name);
      expect(payload.effort.ultracode ?? false).toBe(false);
      await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-name", name);
      await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-effort-look", look);
      await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-ultracode", "0");
      await page.keyboard.press("Escape");
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-effective", name);
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-source", "remuda");
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-mismatch", "0");

      // Reload from persisted Hub state: neither requested nor observed name
      // may be downgraded to xhigh or turned into Claude's workflow flag.
      await page.reload();
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-effective", name);
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-mismatch", "0");
      await page.getByTestId("model-effort-chip").click();
      await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-name", name);
      await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-effort-look", look);
      await expect(page.getByTestId("loading-snapshot")).toHaveCount(0);
      await mkdir(evidenceDir, { recursive: true });
      await page.screenshot({ path: path.join(evidenceDir, `effort-codex-tiers-1-${name}-${width}.png`), animations: "disabled" });
      await page.keyboard.press("Escape");
    }

    await page.getByTestId("model-effort-chip").click();
    await page.getByTestId("effort-open-list").click();
    const rows = page.getByTestId("effort-list").locator('[data-testid^="effort-tier-"]');
    await expect(rows).toHaveCount(6);
    for (const [i, [name, label, description]] of codexTiers.entries()) {
      await expect(rows.nth(i)).toHaveAttribute("data-testid", `effort-tier-${name}`);
      await expect(rows.nth(i)).toHaveText(`${label}${description}`);
      await expect(rows.nth(i)).toHaveAttribute("title", description);
      await expect(rows.nth(i)).toBeVisible();
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
    await page.screenshot({ path: path.join(evidenceDir, `effort-codex-tiers-1-list-${width}.png`), animations: "disabled" });
  });
}

// Clean up with `page.request`, which carries the login session cookie. The
// standalone Playwright `request` fixture is unauthenticated: deleting with
// it 401s (previously swallowed by .catch), leaving the fake node's 8
// placement slots occupied and failing later specs with PLACEMENT_UNSATISFIABLE.
test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

async function clearApprovals(page: Page, instanceId: string) {
  // Use the approval answer shape the Hub validates. An `outcome` answer 422s
  // and would leave the fake node's create-time approval pending, leaking an
  // "echo e2e" card into the next spec's approvals page in a serial run.
  // Wait until the requested row is durable before answering: the pipelined
  // Node→Hub uplink can lag create, and answering a not-yet-persisted request
  // would be resurrected when its late journal event lands.
  await page.evaluate(async (id) => {
    const listPending = async () => {
      const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
      return (body.items ?? []).filter(
        (item: { instanceId?: string; state?: string }) =>
          item.instanceId === id && item.state === "pending",
      );
    };
    const deadline = Date.now() + 10_000;
    let mine = await listPending();
    while (mine.length === 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      mine = await listPending();
    }
    for (const item of mine) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.interactionId ?? item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          answer: {
            kind: "approval",
            optionId,
            inputDigest: item.request?.inputDigest ?? "",
          },
        }),
      });
    }
    // Confirm the answer landed so a late journal event cannot resurrect it.
    let remaining = await listPending();
    while (remaining.length > 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      remaining = await listPending();
    }
  }, instanceId);
}

/** Open the effort popover and move the slider to `stop` via keyboard. */
async function moveSlider(page: Page, key: "ArrowRight" | "ArrowLeft" | "Home" | "End") {
  await page.getByTestId("model-effort-chip").click();
  await page.getByTestId("effort-slider").focus();
  await page.keyboard.press(key);
  await page.keyboard.press("Escape");
}

test("slider change reaches the fake node; transcript read-back drives the effective chip", async ({
  page,
}) => {
  const instanceId = await createSession(page, "effort round trip");
  await clearApprovals(page, instanceId);

  // Before any assistant record reports a level, the chip is a greyed `?`.
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("?", {
    timeout: 10_000,
  });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute(
    "data-effort-effective",
    "unknown",
  );

  // high (index 2) → xhigh (index 3). hubStore.setEffort posts the configure;
  // the fake node reports xhigh back — agreement, no mismatch.
  await moveSlider(page, "ArrowRight");
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("xhigh", {
    timeout: 15_000,
  });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute(
    "data-effort-source",
    "remuda",
  );
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-mismatch", "0");
  expect(await page.getByTestId("model-effort-mismatch").count()).toBe(0);

  // xhigh → max. The fake agent environment clamps the effective level to
  // xhigh: the chip shows the observed level and the mismatch line.
  await moveSlider(page, "ArrowRight");
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("xhigh", {
    timeout: 15_000,
  });
  const mismatch = page.getByTestId("model-effort-mismatch");
  await expect(mismatch).toBeVisible();
  expect(await mismatch.textContent()).toContain("请求 max");
  expect(await mismatch.textContent()).toContain("实际 xhigh");
});
