import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-ghostbadge: the phone-nav inbox badge must count exactly the rows the
 * inbox shows as 待你处理.
 *
 * The fake node's `ghostbadge-expired` sentinel reproduces the demo ghost:
 * while blocked on a hook approval the agent dies hard — the Node journals
 * `interaction.requested` (the Hub's durable row, known deadline already in
 * the past) but never journals answered/expired/invalidated, and the
 * instance settles exited. The Hub keeps `interactions.state='pending'`
 * forever (crates/remuda-hub/src/store.rs `reconcile_reported_instances`
 * settles instances only), so before the web fix the raw
 * `state === "pending"` badge showed 1 while /m/inbox said 待你处理 (0).
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

/**
 * Wait until the Hub durably shows the ghost for this instance: the
 * interaction is still state='pending' with a KNOWN deadline already in the
 * past, while the owning instance has settled exited. Asserting this first
 * proves the rest of the test is exercising the ghost, not an empty queue.
 */
async function waitForGhostInteraction(page: Page, instanceId: string): Promise<string> {
  const deadline = Date.now() + 20_000;
  for (;;) {
    const body = await page.evaluate(async () => {
      const res = await fetch("/v1/interactions", { credentials: "include" });
      return (await res.json()) as { items?: RawInteraction[] };
    });
    const found = (body.items ?? []).find(
      (item) => item.instanceId === instanceId && item.state === "pending",
    );
    if (found) {
      const deadlineValue = Date.parse(found.deadline?.value ?? "");
      if (
        found.deadline?.state === "known" &&
        Number.isFinite(deadlineValue) &&
        deadlineValue < Date.now()
      ) {
        return found.interactionId ?? found.id!;
      }
    }
    if (Date.now() > deadline) {
      throw new Error(
        `ghost interaction (pending + elapsed known deadline) never appeared for ${instanceId}`,
      );
    }
    await page.waitForTimeout(500);
  }
}

test.describe("390px ghost badge: badge and 待你处理 agree", () => {
  test.use({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
  });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    // Instance removal purges the Hub's interactions rows too (http.rs
    // delete_instance), so the ghost cannot leak into later serial specs.
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test("a pending card whose instance died and deadline elapsed counts in neither badge nor inbox", async ({
    page,
  }) => {
    const instanceId = await createSession(page, "ghostbadge-expired sentinel");
    const interactionId = await waitForGhostInteraction(page, instanceId);

    // The owning instance is exited: the interaction is the ghost the brief
    // describes (durable pending, process gone).
    await expect
      .poll(
        async () => {
          const body = await page.evaluate(async () => {
            const res = await fetch("/v1/instances", { credentials: "include" });
            return (await res.json()) as {
              items?: { instanceId?: string; lifecycle?: string }[];
            };
          });
          return (
            body.items?.find((item) => item.instanceId === instanceId)?.lifecycle ?? null
          );
        },
        { timeout: 20_000 },
      )
      .toBe("exited");

    // Phone home: the badge renders nothing — it must not count the raw
    // state='pending' ghost. Before the fix the badge read "1".
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0, { timeout: 15_000 });
    await expect(page.getByTestId("phone-nav-inbox")).not.toHaveAttribute(
      "aria-label",
      /\(1\)$/,
    );

    // The compact inbox: 待你处理 (0) and no row for the ghost.
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (0)");
    await expect(page.locator(`[data-interaction-id="${interactionId}"]`)).toHaveCount(0);
  });
});
