import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

const here = path.dirname(fileURLToPath(import.meta.url));

/**
 * c-wfdrill: subagent grouping + drill-in, live hub e2e driven by the fake
 * Node's `workflow card drill` scenario (crates/remuda-hub/examples/hub_e2e.rs).
 *
 * The scenario streams a two-phase workflow whose running member's own Bash
 * hook observations carry the member's native agent id: the structured
 * transcript must fold them UNDER the member row (never flatten them into the
 * main transcript), and the member opens a bounded on-demand drill-in view of
 * its sidechain transcript at /s/:instanceId/agents/:agentId.
 */

const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/ux-wfdrill");

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

const SEC_AGENT = "aae139d44933cefe2";
const QUEUED_AGENT = "a600b756a51671bdd";

test.afterEach(async ({ page }) => {
  const m = page.url().match(/\/s\/([^/?#]+)/);
  if (!m) return;
  const res = await page.request.delete(`/v1/instances/${m[1]}?force=1`);
  expect(res.ok() || res.status() === 404).toBeTruthy();
});

async function raiseCap(page: Page, to = 24) {
  const hosts = await page.evaluate(async () => {
    const r = await fetch("/v1/hosts", { credentials: "include" });
    return r.json() as Promise<{ items?: { hostId?: string; id?: string; label?: string; maxInstances?: number }[] }>;
  });
  const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
  if (!host || (host.maxInstances ?? 8) >= to) return;
  await page.evaluate(
    async ({ id, value }) => {
      await fetch(`/v1/hosts/${id}`, {
        method: "PATCH",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ maxInstances: value }),
      });
    },
    { id: (host.hostId ?? host.id) as string, value: to },
  );
}

test.beforeEach(async ({ page }) => {
  await login(page);
  await raiseCap(page);
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
              request?: { inputDigest?: string; options?: { id: string }[] };
            }[];
          };
          const mine = (body.items ?? []).filter((i) => i.instanceId === id && i.state === "pending");
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

async function openDrillSession(page: Page): Promise<string> {
  await page.getByTitle("新建", { exact: true }).click();
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await page.getByTestId("new-session-prompt").fill("workflow card drill");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await answerPending(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  // The scripted scenario is bound to instance.send (the composer resend),
  // same as the other workflow-card specs.
  await page.getByTestId("composer-input").fill("workflow card drill");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("workflow-card").first()).toHaveAttribute("data-status", "running", {
    timeout: 20_000,
  });
  return instanceId;
}

async function openReviewPhase(page: Page) {
  const head = page.locator("[data-testid='workflow-phase'] button").first();
  if ((await head.getAttribute("aria-expanded")) !== "true") await head.click();
  await expect(head).toHaveAttribute("aria-expanded", "true");
}

test("subagent tool calls fold under the workflow member, not the main transcript", async ({ page }) => {
  await openDrillSession(page);
  await openReviewPhase(page);

  // The member's own Bash hook observation is NOT a top-level transcript row.
  await expect(page.locator("[data-anchor='toolu_sec_bash1']")).toHaveCount(0);
  // The main agent's tool call stays top-level.
  await expect(page.locator("[data-anchor='toolu_main_note']")).toBeVisible();

  // The member row carries a foldable live summary of the subagent's calls.
  const memberRow = page.getByTestId("workflow-agent").filter({ hasText: "review:security" });
  await expect(memberRow.locator("button").filter({ hasText: /1 tool calls/ })).toBeVisible();
  await memberRow.locator("button").filter({ hasText: /1 tool calls/ }).click();
  // The folded Bash row renders inside the member row with its command.
  await expect(memberRow).toContainText("reviewing auth path");
});

test("a member drills into its sidechain transcript and returns to the session", async ({ page }) => {
  const instanceId = await openDrillSession(page);
  await openReviewPhase(page);

  // Every member is openable via the agents/:id route — no fake child instance.
  const memberRow = page.getByTestId("workflow-agent").filter({ hasText: "review:security" });
  await memberRow.getByTestId("workflow-agent-open-btn").click();
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}/agents/${SEC_AGENT}$`));

  const view = page.getByTestId("subagent-view");
  await expect(view).toBeVisible();
  // Prompt (first user record), model/tokens header, the subagent's own tool
  // rows through the same structured pipeline, and its final text.
  await expect(page.getByTestId("subagent-header")).toContainText("claude-opus-5");
  await expect(view).toContainText("Review the auth module for token handling bugs");
  await expect(view).toContainText("Grep");
  await expect(view).toContainText("auth review done: token refresh race found in sessions.rs");
  await shot(page, "workflow-drill-in-view-1440-night.png");

  // Back returns to the parent session; its reading position is untouched.
  await page.getByTestId("subagent-back").click();
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}$`));
  await expect(page.getByTestId("workflow-card").first()).toBeVisible();
});

test("a queued member without a transcript shows 启动中 in the drill view", async ({ page }) => {
  const instanceId = await openDrillSession(page);
  // Navigate directly: the queued member is known but its jsonl has not landed.
  await page.goto(`/s/${instanceId}/agents/${QUEUED_AGENT}`);
  await expect(page.getByTestId("subagent-starting")).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId("subagent-scroller")).toHaveCount(0);
});

test("evidence: drill grouping at 1440 and 390", async ({ page }) => {
  test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture committed screenshots");
  await page.setViewportSize({ width: 1440, height: 900 });
  await openDrillSession(page);
  const card = page.getByTestId("workflow-card").first();
  await openReviewPhase(page);
  await card.locator("button").filter({ hasText: /1 tool calls/ }).first().click();
  await shot(page, "workflow-drill-grouping-1440-night.png");

  await page.setViewportSize({ width: 390, height: 844 });
  await shot(page, "workflow-drill-grouping-390-night.png");

  // Drill-in viewport captures.
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.getByTestId("workflow-agent-open-btn").first().click();
  await expect(page.getByTestId("subagent-view")).toBeVisible();
  await shot(page, "workflow-drill-in-view-1440-night.png");
  await page.setViewportSize({ width: 390, height: 844 });
  await shot(page, "workflow-drill-in-view-390-night.png");
});
