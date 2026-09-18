import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-wfcard: a running workflow must stay a visible live card — never swept
 * into the compact fold — with per-agent duration/idle/queue/token clocks;
 * dismissal (persisted, per workflow id) is the only thing that folds it, and
 * it survives a reload. Driven by the fake Node's `workflow card live`
 * scenario (crates/remuda-hub/examples/hub_e2e.rs). No real models.
 */

// Release every instance this spec creates. The hub suite is serial against
// one fake Node capped at maxInstances 8; leaving a live row spends a slot
// for every later spec (force is a u8 query param, not a boolean).
test.afterEach(async ({ page }) => {
  const m = page.url().match(/\/s\/([^/?#]+)/);
  if (!m) return;
  const res = await page.request.delete(`/v1/instances/${m[1]}?force=1`);
  expect(res.ok() || res.status() === 404).toBeTruthy();
});

/** maxInstances 8 is shared across the whole serial hub suite; raise it for
 * this file and restore the default afterwards (same pattern as
 * ux-workflow-card.hub.spec.ts). */
async function raiseCap(page: Page, to: number): Promise<void> {
  const hosts = await page.evaluate(async () => {
    const response = await fetch("/v1/hosts", { credentials: "include" });
    return response.json() as Promise<{
      items?: { hostId?: string; id?: string; label?: string; maxInstances?: number }[];
    }>;
  });
  const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
  if (!host) return;
  const hostId = (host.hostId ?? host.id) as string;
  if ((host.maxInstances ?? 8) >= to) return;
  await page.evaluate(
    async ({ id, value }) => {
      await fetch(`/v1/hosts/${id}`, {
        method: "PATCH",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ maxInstances: value }),
      });
    },
    { id: hostId, value: to },
  );
}

let hubRan = false;

test.beforeEach(async ({ page }) => {
  await login(page);
  await raiseCap(page, 24);
  hubRan = true;
});

test.afterAll(async ({ browser }) => {
  if (!hubRan) return;
  const page = await browser.newPage();
  try {
    await login(page);
    const hosts = await page.evaluate(async () => {
      const response = await fetch("/v1/hosts", { credentials: "include" });
      return response.json() as Promise<{
        items?: { hostId?: string; id?: string; label?: string }[];
      }>;
    });
    const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
    if (host) {
      await page.evaluate(
        async (id) => {
          await fetch(`/v1/hosts/${id}`, {
            method: "PATCH",
            credentials: "include",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ maxInstances: 8 }),
          }).catch(() => {});
        },
        (host.hostId ?? host.id) as string,
      );
    }
  } finally {
    await page.close();
  }
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

async function openLiveSession(page: Page): Promise<string> {
  await page.getByTitle("新建", { exact: true }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await page.getByTestId("new-session-prompt").fill("workflow card live");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await answerPending(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  await page.getByTestId("composer-input").fill("workflow card live");
  await page.getByTestId("composer-send").click();
  return instanceId;
}

const CLOCK = /\d+m \d{2}s/;

test("a live workflow stays a visible card with per-agent clocks; dismiss folds and survives reload", async ({
  page,
}) => {
  await openLiveSession(page);
  const card = page.getByTestId("workflow-card").first();

  // Visible while the run is alive — WITHOUT opening any fold: the Review
  // phase is expanded by default while running.
  await expect(card).toHaveAttribute("data-status", "running", { timeout: 20_000 });
  const head = page.getByTestId("workflow-card-head");
  await expect(head).toHaveAttribute("aria-expanded", "true");

  const runningAgent = page.getByTestId("workflow-agent").filter({ hasText: "review:security" });
  await expect(runningAgent).toBeVisible();
  // Per-agent duration and tokens render real values, never the em dash.
  await expect(runningAgent.getByTestId("workflow-agent-duration")).not.toHaveText("—");
  await expect(runningAgent.getByTestId("workflow-agent-duration")).toHaveText(CLOCK);
  await expect(runningAgent.getByTestId("workflow-agent-tokens")).toHaveText("21k");
  // Idle ticks off lastProgressAt (2 s stale); queue wait is startedAt minus
  // the run launch (160 s) and is fixed rather than ticking.
  await expect(runningAgent.getByTestId("workflow-agent-idle")).toHaveText(CLOCK);
  await expect(runningAgent.getByTestId("workflow-agent-queue")).toHaveText(/2m 4[01]s/);

  // The completed member in the same phase carries its own clocks/tokens.
  const doneAgent = page.getByTestId("workflow-agent").filter({ hasText: "review:perf" });
  await expect(doneAgent).toBeVisible();
  await expect(doneAgent.getByTestId("workflow-agent-duration")).toHaveText("1m 48s");
  await expect(doneAgent.getByTestId("workflow-agent-tokens")).toHaveText("39k");

  // The turn's ordinary Bash tool + thought are folded; the workflow is not.
  const fold = page.getByTestId("compact-fold");
  await expect(fold).toContainText("1 次工具 · 1 段思考");

  // Dismiss the card: it leaves the top level and joins the compact fold.
  await head.click();
  await expect(fold).toContainText("2 次工具 · 1 段思考");
  await expect(page.getByTestId("workflow-card")).toHaveCount(0);

  // Reload: dismissal is persisted (runtime localStorage per workflow id).
  await page.reload();
  await expect(page.getByTestId("session-page")).toBeVisible({ timeout: 20_000 });
  await expect(page.getByTestId("workflow-card")).toHaveCount(0);
  await expect(page.getByTestId("compact-fold")).toContainText("2 次工具 · 1 段思考");

  // Re-opening the fold still reaches the (collapsed) card, and clicking it
  // lifts the dismissal: the card returns to the top level, expanded.
  await page.getByTestId("compact-fold").click();
  const foldedHead = page.getByTestId("workflow-card-head");
  await expect(foldedHead).toBeVisible();
  await expect(foldedHead).toHaveAttribute("aria-expanded", "false");
  await foldedHead.click();
  await expect(page.getByTestId("workflow-card-head")).toHaveAttribute("aria-expanded", "true");
  await expect(page.getByTestId("compact-fold")).toContainText("1 次工具 · 1 段思考");
});
