import { expect, test, type Page } from "@playwright/test";
import path from "node:path";
import { mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * c-cardsettle: when an instance ends, its still-pending card is invalidated
 * in the SAME Hub transaction and the inbox/badge drop it within one refresh
 * WITHOUT a reload; a late answer gets the not-pending rejection (404).
 *
 * The fake node must be started with HUB_E2E_CARDSETTLE=1: only then does the
 * `cardsettle-live` create sentinel journal a genuinely live approval with an
 * UNKNOWN deadline (nothing client-side can retire it — the drop must come
 * from the Hub settlement, not the deadline clock). The instance is ended by
 * the existing GHOSTNODE_RESTART instance.send sentinel: the node reconnects
 * under a new epoch omitting the instance, so reconcile_reported_instances
 * settles it exited and invalidates the card.
 *
 * Without the trigger flag this file self-skips.
 */
test.describe.configure({ mode: "serial" });

// The fake node journals the card only when HUB_E2E_CARDSETTLE=1; a default
// hub run reports this file as skipped rather than failing on a missing card.
test.skip(
  process.env.HUB_E2E_CARDSETTLE !== "1",
  "start the fake node with HUB_E2E_CARDSETTLE=1 for the cardsettle harness",
);

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

const created: string[] = [];

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

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
  request?: { inputDigest?: string };
};

async function interactionState(page: Page, interactionId: string): Promise<string> {
  return page.evaluate(async (iid) => {
    const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
    const found = (body.items ?? []).find(
      (item: RawInteraction) => item.interactionId === iid || item.id === iid,
    );
    return found?.state ?? "missing";
  }, interactionId);
}

async function pendingInteractionId(page: Page, instanceId: string): Promise<string> {
  const id = await page.evaluate(async (iid) => {
    const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
    const found = (body.items ?? []).find(
      (item: RawInteraction) => item.instanceId === iid && item.state === "pending",
    );
    return found?.interactionId ?? found?.id ?? null;
  }, instanceId);
  expect(id, "the cardsettle session raises a pending approval").toBeTruthy();
  return id as string;
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

async function endInstance(page: Page, instanceId: string): Promise<void> {
  const result = await page.evaluate(
    async ({ id }) => {
      const res = await fetch(`/v1/instances/${id}/commands`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          operation: "instance.send",
          payload: { prompt: "GHOSTNODE_RESTART" },
        }),
      });
      return { status: res.status, body: await res.text() };
    },
    { id: instanceId },
  );
  expect(result.status, `instance.send GHOSTNODE_RESTART: ${result.body}`).toBe(200);
}

/** Late answer to the settled card: the existing not-pending rejection (404). */
async function answerStatus(
  page: Page,
  interactionId: string,
): Promise<{ status: number; body: string }> {
  return page.evaluate(
    async (iid) => {
      // Use the card's OWN inputDigest so the answer body deserialises; the
      // rejection we assert (404, not-pending) happens strictly after body
      // validation, on the durable row's state.
      const list = await (await fetch("/v1/interactions", { credentials: "include" })).json();
      const item = (list.items ?? []).find(
        (row: RawInteraction) => row.interactionId === iid || row.id === iid,
      );
      const digest = item?.request?.inputDigest ?? "";
      const res = await fetch(`/v1/interactions/${iid}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          answer: { kind: "approval", optionId: "allow-once", inputDigest: digest },
        }),
      });
      return { status: res.status, body: await res.text() };
    },
    interactionId,
  );
}

test.describe("390px cardsettle: pending card drops when its session ends", () => {
  /** Inbound settlement frames captured from BEFORE login, when the store's
   * global follow socket is opened at bootstrap. */
  let settlementFrames: string[];

  test.use({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
  });

  test.beforeEach(async ({ page }) => {
    settlementFrames = [];
    // Register before login: the settlement follow socket is created at
    // bootstrap, so a listener added afterwards would miss it.
    page.on("websocket", (ws) => {
      ws.on("framereceived", ({ payload }) => {
        if (typeof payload === "string" && payload.includes('"type":"settlement"')) {
          settlementFrames.push(payload);
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

  test("Hub settlement drops the inbox card and badge in place, late answer is 404", async ({
    page,
  }) => {
    const instanceId = await createSession(page, "cardsettle-live sentinel");
    const interactionId = await pendingInteractionId(page, instanceId);
    // r2 item 8: prove the drop is driven by the settlement FRAME, not the 2 s
    // poll. Once THIS card's settlement frame arrives, it must be gone within
    // LESS than one poll interval (2000 ms): the trailing coalesce (300 ms)
    // plus one interaction fetch is well under that; a poll-driven drop could
    // not be.
    const waitForSettlementFrame = async () => {
      await expect
        .poll(
          () =>
            settlementFrames.some((text) =>
              text.includes(`"interactionId":"${interactionId}"`),
            ),
          { timeout: 20_000 },
        )
        .toBe(true);
    };

    // Live 1/1 before the end (unknown deadline — only the Hub can retire it).
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveText("1", { timeout: 15_000 });
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (1)");
    await expect(page.getByTestId("phone-inbox-badge")).toHaveText("1");
    await expect(page.locator(`[data-interaction-id="${interactionId}"]`)).toBeVisible();
    await shot(page, "cardsettle-mobile-pending.png");

    // End the instance for real: new-epoch node hello omits it.
    await endInstance(page, instanceId);
    await expect
      .poll(() => instanceLifecycle(page, instanceId), { timeout: 20_000 })
      .toBe("exited");

    // The settlement control frame must arrive…
    await waitForSettlementFrame();
    // …and the card/badge drop within less than one 2000 ms poll interval —
    // this cannot be the periodic poll, it is the frame-driven refresh.
    const frameAt = Date.now();
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (0)", {
      timeout: 1_500,
    });
    await expect(page.locator(`[data-interaction-id="${interactionId}"]`)).toHaveCount(0);
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);
    expect(Date.now() - frameAt, "drop within one poll interval, from the frame").toBeLessThan(
      2_000,
    );
    await shot(page, "cardsettle-mobile-settled.png");

    // The durable row really is invalidated (checked after the UI assertion).
    await expect
      .poll(() => interactionState(page, interactionId), { timeout: 20_000 })
      .toBe("invalidated");

    // In-shell client navigation keeps the same shell mounted; badge stays 0.
    await page.getByTestId("phone-nav-home").click();
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);

    // A genuine reload keeps it 0 — the 0 is the durable Hub settlement.
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);

    // Answering the settled card gets the existing not-pending error: 404
    // (invalidated/unknown), never a 500 or a silent success.
    const late = await answerStatus(page, interactionId);
    expect(late.status, `late answer: ${late.body}`).toBe(404);
  });
});

// Desktop: the settled card lands in 已离队 with the generation-ended wording.
test.describe("1440px cardsettle departed presentation", () => {
  test.use({ viewport: { width: 1440, height: 900 }, isMobile: false });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test("the settled card moves to 已离队 and a late answer is 404", async ({ page }) => {
    const instanceId = await createSession(page, "cardsettle-live sentinel");
    const interactionId = await pendingInteractionId(page, instanceId);

    await page.goto("/approvals");
    await expect(page.getByTestId("approvals-page")).toBeVisible();
    await expect(
      page.locator(`[data-interaction-id="${interactionId}"]`).first(),
    ).toBeVisible();
    await expect(
      page.locator(`[data-testid="inbox-departed"] [data-interaction-id="${interactionId}"]`),
    ).toHaveCount(0);

    await endInstance(page, instanceId);
    await expect
      .poll(() => instanceLifecycle(page, instanceId), { timeout: 20_000 })
      .toBe("exited");

    // The card leaves the actionable queue and lands in 已离队.
    const departedRow = page.locator(
      `[data-testid="inbox-departed"] [data-interaction-id="${interactionId}"]`,
    );
    await expect(departedRow).toBeVisible({ timeout: 15_000 });
    await expect(departedRow).toContainText("进程已结束");
    await shot(page, "cardsettle-desktop-departed.png");
    // It survives a reload (durable invalidated row, recently departed).
    await page.reload();
    await expect(
      page.locator(`[data-testid="inbox-departed"] [data-interaction-id="${interactionId}"]`),
    ).toBeVisible({ timeout: 15_000 });

    const late = await answerStatus(page, interactionId);
    expect(late.status, `late answer: ${late.body}`).toBe(404);
  });
});
