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

  test("Hub settlement drops the inbox card and badge in place, late answer is 404", async ({
    page,
  }) => {
    const instanceId = await createSession(page, "cardsettle-live sentinel");
    const interactionId = await pendingInteractionId(page, instanceId);

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

    // r3 item 4: stop the periodic interaction poll BEFORE ending the
    // instance. With no poll, the only thing that can remove the card is the
    // settlement frame's own refresh — so the assertions below fail if
    // settlement handling is a no-op.
    await page.evaluate(() => {
      const debug = (window as unknown as { __remudaHub?: { stopPoll: () => void } }).__remudaHub;
      debug?.stopPoll();
    });

    // End the instance for real: new-epoch node hello omits it.
    await endInstance(page, instanceId);

    // The settlement control frame for THIS card is observed on SOME socket…
    await expect
      .poll(
        () =>
          Object.values(settlementFramesByUrl)
            .flat()
            .some((text) => text.includes(`"interactionId":"${interactionId}"`)),
        { timeout: 20_000 },
      )
      .toBe(true);

    // The frame is observed on an unfiltered /v1/follow socket (the store's
    // own global settlement socket is one of these; the per-instance session
    // socket cannot carry it here).
    const carryingUrls = Object.entries(settlementFramesByUrl)
      .filter(([, frames]) =>
        frames.some((text) => text.includes(`"interactionId":"${interactionId}"`)),
      )
      .map(([url]) => new URL(url).search);
    expect(
      carryingUrls,
      "the settlement must arrive on the unfiltered follow bus",
    ).toContain("");

    // The store's OWN global settlement callback fired and its immediate pin
    // already projects the card invalidated — with polling stopped this is the
    // only thing that can remove the card, so the assertion fails if
    // settlement handling is a no-op.
    const storeState = () =>
      page.evaluate((iid) => {
        const debug = (window as unknown as {
          __remudaHub?: {
            settlementCount: () => number;
            interactionState: (id: string) => string | undefined;
          };
        }).__remudaHub;
        return {
          count: debug?.settlementCount() ?? 0,
          state: debug?.interactionState(iid) ?? "missing",
        };
      }, interactionId);
    await expect
      .poll(async () => (await storeState()).count, { timeout: 10_000 })
      .toBeGreaterThan(0);
    await expect
      .poll(async () => (await storeState()).state, { timeout: 5_000 })
      .toBe("invalidated");

    // …and the card leaves the queue and the badge agrees with NO reload and
    // NO periodic poll — the frame-driven refresh did it. Plain eventual
    // assertions, no elapsed-time bound.
    await expect(page.getByTestId("m-inbox-tier-pending")).toHaveText("待你处理 (0)");
    await expect(page.locator(`[data-interaction-id="${interactionId}"]`)).toHaveCount(0);
    await expect(page.getByTestId("phone-inbox-badge")).toHaveCount(0);
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
