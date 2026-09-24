import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-ghostbadge round 2: the badge must count exactly the rows the inbox shows
 * as 待你处理 — including across a REAL instance death and a deadline crossing
 * with no page reload.
 *
 * The fake node `ghostbadge-live` sentinel creates a genuinely live hook
 * approval (durable interaction.requested journal + live broker) carrying a
 * short known deadline: badge 1 / inbox 1. Then `GHOSTNODE_RESTART` via
 * instance.send drops the socket and reconnects under a new epoch that omits
 * the instance, so the Hub's reconcile_reported_instances settles it exited
 * and the new process serves no interaction.list for it. The durable row is
 * still state='pending' until the deadline crosses; the shared deadline
 * clock then flips the projection to expired in place — badge 0 / 待你处理
 * (0), observed on the already-mounted /m and /m/inbox pages (no reload).
 */
test.describe.configure({ mode: "serial" });

const created: string[] = [];

async function createSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await page.getByTestId("new-session-kind-claude").click();
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

type RawInteraction = {
  interactionId?: string;
  id?: string;
  instanceId?: string;
  state?: string;
  deadline?: { state?: string; value?: string };
};

async function pendingInteractionId(page: Page, instanceId: string): Promise<string> {
  const id = await page.evaluate(async (iid) => {
    const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
    const found = (body.items ?? []).find(
      (item: RawInteraction) => item.instanceId === iid && item.state === "pending",
    );
    return found?.interactionId ?? found?.id ?? null;
  }, instanceId);
  expect(id, "the ghost session raises a pending approval").toBeTruthy();
  return id as string;
}

async function postCommand(page: Page, id: string, prompt: string): Promise<void> {
  const result = await page.evaluate(
    async ({ id, prompt }) => {
      const res = await fetch(`/v1/instances/${id}/commands`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ operation: "instance.send", payload: { prompt } }),
      });
      return { status: res.status, body: await res.text() };
    },
    { id, prompt },
  );
  expect(result.status, `instance.send: ${result.body}`).toBe(200);
}

async function instanceLifecycle(page: Page, instanceId: string): Promise<string | null> {
  return page.evaluate(async (iid) => {
    const res = await fetch("/v1/instances", { credentials: "include" });
    const body = (await res.json()) as {
      items?: { instanceId?: string; lifecycle?: string }[];
    };
    return body.items?.find((item) => item.instanceId === iid)?.lifecycle ?? null;
  }, instanceId);
}

async function interactionState(page: Page, interactionId: string): Promise<string> {
  return page.evaluate(async (iid) => {
    const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
    const found = (body.items ?? []).find(
      (item: RawInteraction) => item.interactionId === iid || item.id === iid,
    );
    return found?.state ?? "missing";
  }, interactionId);
}

test.describe("390px ghost badge across a real node restart and deadline", () => {
  test.use({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
  });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test("live card is 1/1; node restart Hub-invalidates it (generation-ended) and badge/tier drop to 0/0", async ({
    page,
  }) => {
    const instanceId = await createSession(page, "ghostbadge-live sentinel");
    const interactionId = await pendingInteractionId(page, instanceId);

    // --- While the agent is blocked, badge and inbox both count the card. ---
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveText("1", { timeout: 15_000 });

    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (1)");
    await expect(page.locator(`[data-interaction-id="${interactionId}"]`)).toBeVisible();

    // --- End the instance for real: a new-epoch node hello that omits it.
    // c-deadcards: the Hub now invalidates the card in the SAME transaction
    // that settles the instance exited — no waiting on its deadline. ---
    await postCommand(page, instanceId, "GHOSTNODE_RESTART");
    await expect
      .poll(() => instanceLifecycle(page, instanceId), { timeout: 20_000 })
      .toBe("exited");
    await expect
      .poll(() => interactionState(page, interactionId), { timeout: 20_000 })
      .toBe("invalidated");

    // Badge and tier reflect the Hub settlement; on the mounted inbox the
    // next poll drops it (the client deadline clock remains a defence for a
    // deadline crossing while an instance is still live).
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (0)", {
      timeout: 15_000,
    });
    await expect(page.locator(`[data-interaction-id="${interactionId}"]`)).toHaveCount(0);

    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);
  });

  // c-deadcards: with an UNKNOWN deadline nothing client-side can retire the
  // card; the Hub itself must invalidate it when the instance ends. A real
  // new-epoch restart settles the instance and the card leaves 待你处理 /
  // badge immediately (no deadline crossing involved).
  test("unknown-deadline card leaves 待你处理 (badge 0) when the node restarts — Hub-settled, no deadline", async ({
    page,
  }) => {
    const instanceId = await createSession(page, "ghostbadge-live-nodeadline sentinel");
    const interactionId = await pendingInteractionId(page, instanceId);

    // Live 1/1 before the end (unknown deadline — the clock can never flip it).
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveText("1", { timeout: 15_000 });
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (1)");

    // End the instance for real via a new-epoch node hello omitting it.
    await postCommand(page, instanceId, "GHOSTNODE_RESTART");
    await expect
      .poll(() => instanceLifecycle(page, instanceId), { timeout: 20_000 })
      .toBe("exited");

    // The Hub invalidated the unknown-deadline card itself: durable state is
    // invalidated (generation-ended), not pending.
    await expect
      .poll(() => interactionState(page, interactionId), { timeout: 20_000 })
      .toBe("invalidated");

    // Badge 0 and the card is gone from 待你处理, observed on the mounted
    // pages (the 2 s poll picks the Hub-settled row; no deadline crossing).
    await page.goto("/m");
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0, { timeout: 15_000 });
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (0)");
    await expect(page.locator(`[data-interaction-id="${interactionId}"]`)).toHaveCount(0);
  });
});

// Desktop: the Hub-settled unknown-deadline card leaves the actionable queue
// AND appears in the 已离队 (departed) presentation on /approvals.
test.describe("1440px desktop departed presentation", () => {
  test.use({ viewport: { width: 1440, height: 900 }, isMobile: false });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test("unknown-deadline card moves to 已离队 after the node restart settles its instance", async ({
    page,
  }) => {
    const instanceId = await createSession(page, "ghostbadge-live-nodeadline sentinel");
    const interactionId = await pendingInteractionId(page, instanceId);

    // On desktop it is an actionable queue row first (not in 已离队).
    await page.goto("/approvals");
    await expect(page.getByTestId("approvals-page")).toBeVisible();
    await expect(
      page.locator(`[data-interaction-id="${interactionId}"]`).first(),
    ).toBeVisible();
    await expect(
      page.locator(`[data-testid="inbox-departed"] [data-interaction-id="${interactionId}"]`),
    ).toHaveCount(0);

    // End the instance for real.
    await postCommand(page, instanceId, "GHOSTNODE_RESTART");
    await expect
      .poll(() => instanceLifecycle(page, instanceId), { timeout: 20_000 })
      .toBe("exited");

    // The card leaves the queue and lands in 已离队 (invalidated/generation-
    // ended), surviving a reload.
    await expect(
      page.locator(`[data-testid="inbox-departed"] [data-interaction-id="${interactionId}"]`),
    ).toBeVisible({ timeout: 15_000 });
    await page.reload();
    await expect(
      page.locator(`[data-testid="inbox-departed"] [data-interaction-id="${interactionId}"]`),
    ).toBeVisible({ timeout: 15_000 });
  });
});
