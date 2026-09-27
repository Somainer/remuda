import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * The badge must count exactly the rows the inbox shows as 待你处理 —
 * including across a REAL instance death with no page reload.
 *
 * The fake node `ghostbadge-live` sentinel creates a genuinely live hook
 * approval (durable interaction.requested journal + live broker): badge 1 /
 * inbox 1. Then `GHOSTNODE_RESTART` via instance.send drops the socket and
 * reconnects under a new epoch that omits the instance, so the Hub's
 * reconcile_reported_instances settles it exited. c-deadcards: that same
 * settlement invalidates the card in the Hub transaction (generation-ended),
 * for BOTH known- and unknown-deadline rows — the client deadline clock
 * (c-ghostbadge r2/r3) remains only a defence for a deadline crossing while an
 * instance is still live. The mounted PhoneShell badge and inbox tier drop to
 * 0/0 in place on the next 2 s interaction.list poll (no reload, in-shell
 * client navigation only) and stay 0 after a real reload; desktop shows the
 * row in 已离队.
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
    await expect(page.getByTestId("phone-inbox-badge")).toHaveText("1");
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

    // Badge and tier reflect the Hub settlement with NO reload and no
    // navigation: the page has stayed on /m/inbox since the card was pinned at
    // 1, and the next 2 s interaction.list poll (a store emission) drops it —
    // the exact badge element pinned above disappears in place. (The client
    // deadline clock remains a defence for a deadline crossing while an
    // instance is still live.)
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (0)", {
      timeout: 15_000,
    });
    await expect(page.locator(`[data-interaction-id="${interactionId}"]`)).toHaveCount(0);
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);

    // In-shell client navigation keeps the same PhoneShell mounted; the badge
    // stays 0 on /m.
    await page.getByTestId("phone-nav-home").click();
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);

    // A genuine reload afterwards keeps it 0 — the 0 is the durable Hub
    // settlement, not live-frame-only state.
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
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (1)");
    await expect(page.getByTestId("phone-inbox-badge")).toHaveText("1");

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

    // Nothing client-side could retire this card (no deadline exists), yet the
    // badge pinned at 1 on this same mounted page disappears in place when the
    // 2 s poll picks up the Hub-settled row — no reload, no navigation.
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (0)", {
      timeout: 15_000,
    });
    await expect(page.locator(`[data-interaction-id="${interactionId}"]`)).toHaveCount(0);
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);

    // In-shell client navigation keeps the same PhoneShell mounted; 0 on /m.
    await page.getByTestId("phone-nav-home").click();
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);

    // A genuine reload afterwards keeps it 0.
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);
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
