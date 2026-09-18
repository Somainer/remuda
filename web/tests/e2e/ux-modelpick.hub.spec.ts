import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Anchored model picker (c-modelpick) against the fake Node in
 * `crates/remuda-hub/examples/hub_e2e.rs`:
 *
 * 1. Tall catalog (81 ids): the panel anchors to ITS OWN trigger chip (left
 *    edge inside the chip's box), fits wholly inside the viewport, caps its
 *    height and scrolls inside the list body. Clicking a listed model posts
 *    instance.configure, the Hub queues it (200) and the verdict read-back
 *    marks the model current with the "listed" selection path.
 * 2. An id the session's own discovery did NOT list: the verbatim input still
 *    posts configure, the typed `/model <id>` reaches the terminal (native
 *    user node), the verdict settles the picker, and the recorded path reads
 *    as the typed-id fallback.
 *
 * No real model is invoked — the fake node writes the transcript events.
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

test.beforeEach(async ({ page }) => {
  await login(page);
});

// The fake harness raises a pending approval on every create, which parks a
// card over the composer and disables the input; answer it so the picker
// opens in the normal (idle, card-free) geometry state.
async function clearApprovals(page: Page, instanceId: string) {
  await page.evaluate(async (id) => {
    const list = async () => {
      const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
      return (body.items ?? []).filter(
        (item: { instanceId?: string; state?: string }) =>
          item.instanceId === id && item.state === "pending",
      );
    };
    const deadline = Date.now() + 10_000;
    let mine = await list();
    while (mine.length === 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      mine = await list();
    }
    for (const item of mine) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.interactionId ?? item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          answer: {
            kind: "approval",
            optionId,
            inputDigest: item.request?.inputDigest ?? "",
          },
        }),
      });
    }
  }, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 15_000 });
  // The interaction is removed from the pending list on answer; wait for the
  // 2 s poll to reflect that so the parked card has unmounted before any
  // popover geometry is measured.
  await expect(page.getByTestId("approval-card")).toHaveCount(0);
}

async function openModelList(page: Page) {
  await page.keyboard.press("Escape").catch(() => undefined);
  await page.getByTestId("model-effort-chip").click();
  const open = page.getByTestId("effort-open-list");
  await open.waitFor({ state: "visible", timeout: 10_000 });
  await open.click();
  await expect(page.getByTestId("effort-list")).toBeVisible();
}

test("the tall catalog panel anchors to its trigger, fits, scrolls and switches", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  const instanceId = await createSession(page, "Model picker tall-catalog geometry");

  // Apply the persisted launch snapshot before opening the picker.
  await page.reload();
  await page.getByTestId("model-effort-chip").waitFor({ timeout: 20_000 });
  await clearApprovals(page, instanceId);
  await openModelList(page);

  // The launch catalog carries the tall, scoped-cache list.
  await expect(page.getByTestId("model-option-m-79")).toBeVisible({ timeout: 10_000 });
  expect(await page.getByTestId("model-option-m-00").count()).toBe(1);

  const panel = page.getByTestId("effort-menu");
  const chip = page.getByTestId("model-effort-chip");
  const panelBox = await panel.boundingBox();
  const chipBox = await chip.boundingBox();
  expect(panelBox).toBeTruthy();
  expect(chipBox).toBeTruthy();
  const viewport = page.viewportSize()!;

  // The whole panel is inside the viewport — the old bug showed only the
  // bottom strip of a card whose top ran off-screen.
  expect(panelBox!.x).toBeGreaterThanOrEqual(0);
  expect(panelBox!.y).toBeGreaterThanOrEqual(0);
  expect(panelBox!.x + panelBox!.width).toBeLessThanOrEqual(viewport.width + 1);
  expect(panelBox!.y + panelBox!.height).toBeLessThanOrEqual(viewport.height + 1);

  // Anchored to the chip that opened it, not the composer's left edge.
  expect(Math.abs(panelBox!.x - chipBox!.x)).toBeLessThanOrEqual(2);
  expect(panelBox!.x).toBeLessThanOrEqual(chipBox!.x + chipBox!.width + 1);
  // 60vh cap: 81 rows never render 1200+ px tall.
  expect(panelBox!.height).toBeLessThanOrEqual(viewport.height * 0.6 + 2);

  // The list body owns the scroll; both ends are reachable.
  const list = page.getByTestId("effort-list");
  const scrollMetrics = await list.evaluate((el) => {
    const body = el as HTMLElement;
    const scrollable = body.scrollHeight > body.clientHeight + 1;
    body.scrollTop = 0;
    const top = body.scrollTop;
    body.scrollTop = body.scrollHeight;
    const bottom = body.scrollTop;
    return { scrollable, top, bottom, clientHeight: body.clientHeight };
  });
  expect(scrollMetrics.scrollable).toBe(true);
  expect(scrollMetrics.top).toBe(0);
  expect(scrollMetrics.bottom).toBeGreaterThan(0);

  // First tier row is reachable from the scroll top.
  await list.evaluate((el) => ((el as HTMLElement).scrollTop = 0));
  const firstRow = page.getByTestId("effort-tier-low");
  await expect(firstRow).toBeVisible();

  // Pick a catalog id mid-list. The click posts instance.configure; the Hub
  // queues the command (200) and the fake node's verdict read-back settles it.
  const requestPromise = page.waitForRequest(
    (r) =>
      r.method() === "POST" &&
      r.url().endsWith(`/v1/instances/${instanceId}/commands`) &&
      r.postDataJSON()?.operation === "instance.configure",
  );
  const responsePromise = page.waitForResponse(
    (r) => r.url().endsWith(`/v1/instances/${instanceId}/commands`) && r.request().method() === "POST",
  );
  await page.getByTestId("model-option-m-03").click();
  const request = await requestPromise;
  expect(request.postDataJSON().payload.model).toBe("e2e/m-03");
  expect((await responsePromise).ok()).toBe(true);

  await openModelList(page);
  const sliderPanel = page.getByTestId("effort-slider-panel");
  await expect(sliderPanel).toHaveAttribute("data-model-pending", "0", { timeout: 10_000 });
  await expect(sliderPanel).toHaveAttribute("data-model-current", "m-03", { timeout: 10_000 });
  // The switch came from the session's own listed catalog.
  await expect(sliderPanel).toHaveAttribute("data-model-path", "listed");
  const pickedRow = page.getByTestId("model-option-m-03");
  await expect(pickedRow).toHaveAttribute("data-selected", "1");
  await expect(pickedRow).toHaveAttribute("data-model-path", "listed");
  await expect(page.getByTestId("model-option-path")).toContainText("列表内");
});

test("an unlisted typed id still configures, reaches the terminal and settles as the typed fallback", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  const instanceId = await createSession(page, "Model picker typed fallback");
  await page.reload();
  await page.getByTestId("model-effort-chip").waitFor({ timeout: 20_000 });
  await clearApprovals(page, instanceId);
  await openModelList(page);

  // The id is not in the launch catalog (auto/fast/plain/claude-e2e-only).
  expect(await page.getByTestId("model-option-typed-77").count()).toBe(0);

  const requestPromise = page.waitForRequest(
    (r) =>
      r.method() === "POST" &&
      r.url().endsWith(`/v1/instances/${instanceId}/commands`) &&
      r.postDataJSON()?.operation === "instance.configure",
  );
  await page.getByTestId("effort-model-type").fill("e2e/typed-77");
  await page.getByTestId("effort-model-type").press("Enter");
  const request = await requestPromise;
  expect(request.postDataJSON().payload.model).toBe("e2e/typed-77");

  // The typed `/model <id>` bytes reached the terminal: the fake node echoes
  // a native-typed user node, and the verdict attributes the switch "typed".
  await expect
    .poll(
      async () =>
        page.evaluate((id) =>
          fetch(`/v1/instances/${id}/journal`, { credentials: "include" })
            .then((r) => r.json())
            .then((d) => (d.events ?? []).some((e: unknown) =>
              JSON.stringify(e).includes("/model e2e/typed-77"))),
          instanceId),
      { timeout: 10_000 },
    )
    .toBe(true);
  await expect
    .poll(
      async () =>
        page.evaluate((id) =>
          fetch(`/v1/instances/${id}/journal`, { credentials: "include" })
            .then((r) => r.json())
            .then((d) => (d.events ?? []).some((e: unknown) =>
              JSON.stringify(e).includes('"selectionPath":"typed"'))),
          instanceId),
      { timeout: 10_000 },
    )
    .toBe(true);

  await openModelList(page);
  const sliderPanel = page.getByTestId("effort-slider-panel");
  await expect(sliderPanel).toHaveAttribute("data-model-pending", "0", { timeout: 10_000 });
  await expect(sliderPanel).toHaveAttribute("data-model-current", "typed-77", { timeout: 10_000 });
  await expect(sliderPanel).toHaveAttribute("data-model-path", "typed");
  const row = page.getByTestId("model-option-typed-77");
  await expect(row).toHaveAttribute("data-selected", "1");
  await expect(row).toHaveAttribute("data-model-path", "typed");
  await expect(page.getByTestId("model-option-path")).toContainText("直输 id");
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});
