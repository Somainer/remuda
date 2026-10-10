import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-ghostbadge round 2 / c-cardsettle round 9: the badge must count exactly
 * the rows the inbox shows as 待你处理 — and a card of an ENDED session never
 * stays pending, even while its own deadline is still open.
 *
 * The fake node `ghostbadge-live` sentinel creates a genuinely live hook
 * approval (durable interaction.requested journal + live broker) carrying a
 * short known deadline: badge 1 / inbox 1. Then `GHOSTNODE_RESTART` via
 * instance.send drops the socket and reconnects under a new epoch that omits
 * the instance, so the Hub's reconcile_reported_instances settles it exited
 * and invalidates the pending card in the SAME transaction
 * (generation-ended), regardless of the still-future known deadline — the
 * Hub has no deadline sweeper, so an exemption would leave the durable row
 * pending and the badge counting it forever. The settlement control frame
 * flips the badge 1 -> 0 on the same mounted PhoneShell (no reload, in-shell
 * client navigation only); the durable state is invalidated; a reload
 * re-confirms 0.
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

test.describe("390px ghost badge across a real node restart", () => {
  /** Settlement frames per follow socket URL, captured from before login. */
  let settlementFramesByUrl: Record<string, string[]>;

  test.use({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
  });

  test.beforeEach(async ({ page }) => {
    settlementFramesByUrl = {};
    page.on("websocket", (ws) => {
      ws.on("framereceived", ({ payload }) => {
        if (typeof payload === "string" && payload.includes('"type":"settlement"')) {
          (settlementFramesByUrl[ws.url()] ??= []).push(payload);
        }
      });
    });
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test("live card is 1/1; node restart settles instance and card together; badge flips to 0/0 in place", async ({
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

    // Stall the periodic interaction poll BEFORE the restart. With no poll,
    // the only thing that can remove the card on this mounted page is the
    // Hub settlement frame's own pin + refresh — so the flip below fails if
    // settlement handling is a no-op (and it cannot be the deadline clock:
    // the durable row becomes invalidated immediately).
    await page.evaluate(() => {
      const debug = (window as unknown as { __remudaHub?: { stopPoll: () => void } }).__remudaHub;
      debug?.stopPoll();
    });

    // --- End the instance for real: a new-epoch node hello that omits it. ---
    await postCommand(page, instanceId, "GHOSTNODE_RESTART");
    await expect
      .poll(() => instanceLifecycle(page, instanceId), { timeout: 20_000 })
      .toBe("exited");

    // The settlement control frame for THIS card is observed on an
    // unfiltered /v1/follow socket.
    await expect
      .poll(
        () =>
          Object.values(settlementFramesByUrl)
            .flat()
            .some((text) => text.includes(`"interactionId":"${interactionId}"`)),
        { timeout: 20_000 },
      )
      .toBe(true);
    const carryingUrls = Object.entries(settlementFramesByUrl)
      .filter(([, frames]) =>
        frames.some((text) => text.includes(`"interactionId":"${interactionId}"`)),
      )
      .map(([url]) => new URL(url).search);
    expect(carryingUrls, "the settlement rides the unfiltered follow bus").toContain("");

    // c-cardsettle r9: the Hub settles the card in the SAME transaction as
    // the exited instance, even while the card's KNOWN deadline is still
    // open — there is no Hub deadline sweeper, so a pending row here would
    // be counted and answerable forever.
    const settled = await page.evaluate(async (iid) => {
      const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
      const found = (body.items ?? []).find(
        (item: RawInteraction) => item.interactionId === iid || item.id === iid,
      );
      return found?.state ?? "missing";
    }, interactionId);
    expect(settled).toBe("invalidated");

    // --- With NO reload and NO periodic poll: the frame (not the deadline
    // clock) flips the inbox tier 1 -> 0 and drops the card on the STILL
    // mounted PhoneShell. Plain eventual assertions, no elapsed-time bound.
    // ---
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (0)");
    await expect(page.locator(`[data-interaction-id="${interactionId}"]`)).toHaveCount(0);

    // The exact badge element pinned above flips in place — no reload, not
    // even a route change (round 3: round 2 reloaded via goto before this
    // assertion, so it never proved the persistent shell updated itself).
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);

    // Navigate WITHIN the mounted shell: the bottom-bar Link is a client-side
    // route, the shell is never remounted, and the badge stays 0 on /m.
    await page.getByTestId("phone-nav-home").click();
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);

    // A genuine reload afterwards keeps it 0 — the 0 is the durable state,
    // not live-frame-only state.
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);
  });
});
