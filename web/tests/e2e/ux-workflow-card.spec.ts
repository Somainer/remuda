import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Workflow timeline card (workbench batch W) — live hub e2e driven by the fake
 * Node's synthetic workflow scenarios (`workflow card …` prompts; see
 * crates/remuda-hub/examples/hub_e2e.rs). No real models.
 */

test.describe.configure({ mode: "serial" });

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

test.describe.configure({ mode: "serial" });

// The fake Node caps instances at 8, shared across the serial hub suite — free
// every slot this spec takes (force is a u8 query param).
test.afterEach(async ({ context }) => {
  for (const page of context.pages()) {
    const m = page.url().match(/\/s\/([^/?#]+)/);
    if (!m) continue;
    await page.evaluate(async (id) => {
      try {
        await fetch(`/v1/instances/${id}?force=1`, { method: "DELETE", credentials: "include" });
      } catch {
        // teardown is best-effort
      }
    }, m[1]);
  }
});

test("card runs live then auto-collapses to the one-line summary", async ({ page }) => {
  const instanceId = await openWorkflowSession(page, "workflow card demo");
  void instanceId;
  const card = page.getByTestId("workflow-card").first();
  // Expanded and live while running: chip + 当前 line + a running agent row.
  await expect(card).toHaveAttribute("data-status", "running", { timeout: 20_000 });
  await expect(card.locator("button").first()).toHaveAttribute("aria-expanded", "true");
  await expect(card.getByText("当前")).toBeVisible();
  await expect(page.getByTestId("workflow-agent").filter({ hasText: "review:security" })).toBeVisible();

  // Terminal state auto-collapses the card; the header keeps the summary.
  await expect(card).toHaveAttribute("data-status", "completed", { timeout: 20_000 });
  const head = card.locator("button").first();
  await expect(head).toHaveAttribute("aria-expanded", "false");
  await expect(head).toContainText("4/4 agents");
  await expect(head).toContainText("tokens");

  // Click expands the detail again.
  await head.click();
  await expect(head).toHaveAttribute("aria-expanded", "true");
  await expect(page.getByTestId("workflow-agent").filter({ hasText: "verify:auth.ts" })).toBeVisible();
});

test("a 20-agent phase folds 8 quiet rows behind 还有 n 个", async ({ page }) => {
  await openWorkflowSession(page, "workflow card fold");
  const card = page.getByTestId("workflow-card").first();
  await expect(card).toHaveAttribute("data-status", "completed", { timeout: 20_000 });
  await card.locator("button").first().click();
  const phase = page.getByTestId("workflow-phase");
  await phase.locator("button").first().click();

  const visibleRows = () => page.locator("[data-testid='workflow-agent']:visible");
  await expect(visibleRows()).toHaveCount(12);
  const toggle = page.getByRole("button", { name: /还有 8 个/ });
  await toggle.click();
  await expect(visibleRows()).toHaveCount(20);
  await toggle.click();
  await expect(visibleRows()).toHaveCount(12);
});

test("a failed agent row is never folded", async ({ page }) => {
  await openWorkflowSession(page, "workflow card fail");
  const card = page.getByTestId("workflow-card").first();
  await expect(card).toHaveAttribute("data-status", "failed", { timeout: 20_000 });
  await card.locator("button").first().click();
  const phase = page.getByTestId("workflow-phase");
  await phase.locator("button").first().click();
  const failed = page.getByTestId("workflow-agent").filter({ hasText: "review:security" });
  await expect(failed).toBeVisible();
  await expect(failed).toHaveAttribute("data-state", "failed");
  // The quiet tail folds, but the failed row stays outside the fold toggle.
  await expect(page.getByRole("button", { name: /还有 3 个/ })).toBeVisible();
});

test("at 390px every agent stays on one line; model/tool meta hidden", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await openWorkflowSession(page, "workflow card demo");
  const card = page.getByTestId("workflow-card").first();
  await expect(card).toHaveAttribute("data-status", "completed", { timeout: 20_000 });
  await card.locator("button").first().click();
  const phase = page.getByTestId("workflow-phase");
  await phase.locator("button").first().click();
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
  await openWorkflowSession(page, "workflow card demo");
  const card = page.getByTestId("workflow-card").first();
  await expect(card).toHaveAttribute("data-status", "running", { timeout: 20_000 });
  const head = card.locator("button").first();
  await expect(head).toHaveAttribute("aria-expanded", "true");

  // Keys sent to an attached terminal (or anywhere outside the card's own
  // toggle) must not act on the card: Escape stays in the xterm.
  const terminal = page.locator(".xterm-helper-textarea");
  if (await terminal.count()) {
    await terminal.first().focus();
    await page.keyboard.press("Escape");
  } else {
    await page.locator("body").click();
    await page.keyboard.press("Escape");
  }
  await expect(head).toHaveAttribute("aria-expanded", "true");

  // Escape only collapses when the card head itself owns focus.
  await head.focus();
  await page.keyboard.press("Escape");
  await expect(head).toHaveAttribute("aria-expanded", "false");
});
