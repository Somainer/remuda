import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Workflow timeline card (workbench batch W) — live hub e2e driven by the fake
 * Node's synthetic workflow scenarios (`workflow card …` prompts; see
 * crates/remuda-hub/examples/hub_e2e.rs). No real models.
 */

// Release every instance this spec creates. The hub suite is serial against one
// fake Node capped at maxInstances 8; leaving a live row spends a slot for
// every later spec (force is a u8 query param, not a boolean).
test.afterEach(async ({ page }) => {
  const m = page.url().match(/\/s\/([^/?#]+)/);
  if (!m) return;
  const res = await page.request.delete(`/v1/instances/${m[1]}?force=1`);
  expect(res.ok() || res.status() === 404).toBeTruthy();
});

async function answerPending(page: Page, instanceId: string) {
  await expect
    .poll(
      async () =>
        page.evaluate(async (id) => {
          const list = await fetch("/v1/interactions", { credentials: "include" });
          const body = (await list.json()) as {
            items?: {
              id: string;
              instanceId?: string;
              state?: string;
              request?: { kind?: string; inputDigest?: string; options?: { id: string }[] };
            }[];
          };
          const mine = (body.items ?? []).filter((item) => item.instanceId === id && item.state === "pending");
          for (const item of mine) {
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
          return mine.length;
        }, instanceId),
      { timeout: 20_000 },
    )
    .toBe(0);
}

async function openWorkflowSession(page: Page, prompt: string): Promise<string> {
  await login(page);
  await page.getByTitle("新建", { exact: true }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await answerPending(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  await page.getByTestId("composer-input").fill(prompt);
  await page.getByTestId("composer-send").click();
  return instanceId;
}

async function ensurePhaseOpen(page: Page, nth = 0) {
  const head = page.locator("[data-testid='workflow-phase'] button").nth(nth);
  if ((await head.getAttribute("aria-expanded")) !== "true") await head.click();
  await expect(head).toHaveAttribute("aria-expanded", "true");
}

test("card runs live then auto-collapses to the one-line summary", async ({ page }) => {
  const instanceId = await openWorkflowSession(page, "workflow card demo running");
  void instanceId;
  const card = page.getByTestId("workflow-card").first();
  // Expanded and live while running: chip + 当前 line + a running agent row.
  await expect(card).toHaveAttribute("data-status", "running", { timeout: 20_000 });
  await expect(card.locator("button").first()).toHaveAttribute("aria-expanded", "true");
  await expect(card.getByText("当前")).toBeVisible();
  await expect(page.getByTestId("workflow-agent").filter({ hasText: "review:security" })).toBeVisible();

  // Drive the terminal half deterministically (same workflow/tool ids).
  await page.getByTestId("composer-input").fill("workflow card demo done");
  await page.getByTestId("composer-send").click();

  // Terminal state auto-collapses the card; the header keeps the summary.
  await expect(card).toHaveAttribute("data-status", "completed", { timeout: 20_000 });
  const head = card.locator("button").first();
  await expect(head).toHaveAttribute("aria-expanded", "false");
  await expect(head).toContainText("4/4 agents");
  await expect(head).toContainText("tokens");

  // Click expands the detail again; completed phases stay collapsed, open Verify.
  await head.click();
  await expect(head).toHaveAttribute("aria-expanded", "true");
  await ensurePhaseOpen(page, 1);
  await expect(page.getByTestId("workflow-agent").filter({ hasText: "verify:auth.ts" })).toBeVisible();
});

test("a 20-agent phase folds 8 quiet rows behind 还有 n 个", async ({ page }) => {
  await openWorkflowSession(page, "workflow card fold");
  const card = page.getByTestId("workflow-card").first();
  await expect(card).toHaveAttribute("data-status", "completed", { timeout: 20_000 });
  await card.locator("button").first().click();
  await ensurePhaseOpen(page, 0);

  const visibleRows = () => page.locator("[data-testid='workflow-agent']:visible");
  await expect(visibleRows()).toHaveCount(12);
  // The toggle changes text (还有 ↔ 收起); locate its row stably.
  const toggle = page.locator("[data-testid='workflow-phase'] ul li button").filter({ hasText: /个/ });
  await expect(toggle).toContainText("还有");
  await toggle.click();
  await expect(visibleRows()).toHaveCount(20);
  await expect(toggle).toContainText("收起");
  await toggle.click();
  await expect(visibleRows()).toHaveCount(12);
});

test("a failed agent row is never folded", async ({ page }) => {
  await openWorkflowSession(page, "workflow card fail");
  const card = page.getByTestId("workflow-card").first();
  await expect(card).toHaveAttribute("data-status", "failed", { timeout: 20_000 });
  await card.locator("button").first().click();
  await ensurePhaseOpen(page, 0);
  const failed = page.getByTestId("workflow-agent").filter({ hasText: "review:security" });
  await expect(failed).toBeVisible();
  await expect(failed).toHaveAttribute("data-state", "failed");
  // The quiet tail folds, but the failed row stays outside the fold toggle.
  await expect(
    page.locator("[data-testid='workflow-phase'] ul li button").filter({ hasText: /还有 3 个/ }),
  ).toBeVisible();
});

test("at 390px every agent stays on one line; model/tool meta hidden", async ({ page }) => {
  // Start at desktop width: on a 390px shell the 新建 link lives behind the
  // mobile nav and is not clickable.
  await page.setViewportSize({ width: 1280, height: 900 });
  await openWorkflowSession(page, "workflow card demo running");
  const card = page.getByTestId("workflow-card").first();
  await expect(card).toHaveAttribute("data-status", "running", { timeout: 20_000 });
  await page.getByTestId("composer-input").fill("workflow card demo done");
  await page.getByTestId("composer-send").click();
  await expect(card).toHaveAttribute("data-status", "completed", { timeout: 20_000 });
  await card.locator("button").first().click();
  await ensurePhaseOpen(page, 0);
  // Now shrink: agent rows must stay one line with model/tool meta hidden.
  await page.setViewportSize({ width: 390, height: 844 });
  const rows = page.getByTestId("workflow-agent");
  await expect(rows.first()).toBeVisible();
  const count = await rows.count();
  for (let i = 0; i < count; i++) {
    const box = await rows.nth(i).boundingBox();
    expect(box, `agent ${i} rendered`).toBeTruthy();
    // One line: the row's height stays within two meta line heights.
    expect(box!.height).toBeLessThanOrEqual(28);
  }
});

test("no detail degrades to a flat row with an explanation note", async ({ page }) => {
  await openWorkflowSession(page, "workflow card legacy");
  const card = page.getByTestId("workflow-card-flat");
  await expect(card).toBeVisible({ timeout: 20_000 });
  await expect(card).toContainText("阶段明细");
  await expect(card.getByTestId("workflow-agent")).toHaveCount(0);
  // No expand affordance.
  await expect(card.locator("button")).toHaveCount(0);
});

test("Escape in the terminal / outside the card never closes the open card", async ({ page }) => {
  await openWorkflowSession(page, "workflow card demo running");
  const card = page.getByTestId("workflow-card").first();
  await expect(card).toHaveAttribute("data-status", "running", { timeout: 20_000 });
  const head = card.locator("button").first();
  await expect(head).toHaveAttribute("aria-expanded", "true");

  // Keys sent to an attached terminal must not act on the card: Escape stays
  // in the xterm. If no terminal is attached (the fake-node session can come
  // up without one), Escape from the composer textarea must also be a no-op
  // for the card — never click the page body, whose centre may be the card.
  const terminal = page.locator(".xterm-helper-textarea");
  if (await terminal.count()) {
    await terminal.first().focus();
    await page.keyboard.press("Escape");
  } else {
    await page.getByTestId("composer-input").focus();
    await page.keyboard.press("Escape");
  }
  await expect(head).toHaveAttribute("aria-expanded", "true");

  // Escape only collapses when the card head itself owns focus.
  await head.focus();
  await page.keyboard.press("Escape");
  await expect(head).toHaveAttribute("aria-expanded", "false");
});
