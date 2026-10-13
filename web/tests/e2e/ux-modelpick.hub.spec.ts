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
  // D-042: Escape from the focused composer during a still-running turn opens
  // the in-app 打断 Sheet (there is no native dialog to auto-dismiss). Only
  // send the defensive Escape when no interrupt Sheet is already up (a timed
  // wait misses a late mount under gate load), then clear any Sheet it
  // surfaces before clicking the chip so its scrim cannot intercept.
  if ((await page.getByTestId("composer-interrupt").count()) === 0) {
    await page.keyboard.press("Escape").catch(() => undefined);
  }
  const strayConfirm = page.getByTestId("composer-confirm");
  if (await strayConfirm.count()) {
    await strayConfirm.getByTestId("composer-confirm-cancel").click();
    await expect(strayConfirm).toHaveCount(0);
  }
  await page.getByTestId("model-effort-chip").click();
  const open = page.getByTestId("effort-open-list");
  await open.waitFor({ state: "visible", timeout: 10_000 });
  await open.click();
  await expect(page.getByTestId("effort-list")).toBeVisible();
}

test("the tall catalog panel anchors to its trigger, fits, scrolls and switches", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  const instanceId = await createSession(page, "compact-approval tall-catalog geometry");

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

test("a tall catalog list over the parked approval card caps to the strip, scrolls and never oscillates", async ({
  page,
}) => {
  // c-effortui2 r26 item 1: with a parked approval card leaving a USABLE
  // (>=120px) cleared strip over the composer, the popover opens up capped to
  // that strip and the effort-list body scrolls. The flex-column scroll
  // wrapper used to feed the CLAMPED height back into AnchoredPopover's
  // natural-height measurement: the wrapper's own scrollHeight collapsed to
  // its clamped clientHeight, `need` flipped from content-sized (blocked by
  // the card → strip cap) to clamp-sized ("fits" → plain above-room cap) on
  // every ResizeObserver cycle, resizing the panel and stopping the inner
  // scroll. It must settle once.
  await page.setViewportSize({ width: 1440, height: 1000 });
  // Geometry fixture. The dock parks the create-time approval card a fixed
  // ~106px below the effort chip, which is the ~90px NARROW strip (the
  // bounded-fallback geometry where this measurement bug is invisible). A
  // persistent stylesheet lifts the parked card (AnchoredPopover reads
  // getBoundingClientRect, and a stylesheet transform survives the store's
  // 2s re-render that wipes inline styles) so its measured edge leaves the
  // USABLE >=120px strip the strip-cap/non-overlap rule is specified for.
  const instanceId = await createSession(page, "Model picker tall-catalog geometry");
  await page.reload();
  await page.addStyleTag({
    content: "[data-testid='approval-card']{transform:translateY(-34px)!important}",
  });
  await page.getByTestId("model-effort-chip").waitFor({ timeout: 20_000 });
  // Deliberately do NOT clear the approval: its card stays parked over the
  // composer and shrinks the up-room.
  const approval = page.getByTestId("approval-card");
  await expect(approval).toBeVisible({ timeout: 15_000 });
  // Record the panel box on every animation frame FROM the moment the menu
  // mounts, before openModelList returns: the strip-cap ⇄ plain-cap
  // oscillation (124 ⇄ ~600) happens on the first ResizeObserver cycles and a
  // post-settle sample alone misses it.
  await page.evaluate(() => {
    (window as unknown as { __capHeights?: number[] }).__capHeights = [];
    const seen = new WeakSet<object>();
    const obs = new MutationObserver(() => {
      const el = document.querySelector("[data-testid='effort-menu']") as HTMLElement | null;
      if (el && !seen.has(el)) {
        seen.add(el);
        const tick = () => {
          (window as unknown as { __capHeights: number[] }).__capHeights.push(
            Math.round(el.getBoundingClientRect().height),
          );
          requestAnimationFrame(tick);
        };
        requestAnimationFrame(tick);
      }
    });
    obs.observe(document.body, { childList: true, subtree: true, attributes: true });
    (window as unknown as { __capObserver?: MutationObserver }).__capObserver = obs;
  });
  await openModelList(page);
  await expect(page.getByTestId("model-option-m-79")).toBeVisible({ timeout: 10_000 });
  // Let the resize feedback loop run a while.
  await page.waitForTimeout(1200);
  const menu = page.getByTestId("effort-menu");
  const list = page.getByTestId("effort-list");
  const trigger = page.getByTestId("model-effort-chip");

  // The fixture must actually produce the usable strip this test is about.
  const strip = await page.evaluate(() => {
    const q = (t: string) => document.querySelector(`[data-testid='${t}']`) as HTMLElement;
    const t = q("model-effort-chip").getBoundingClientRect();
    const a = q("approval-card").getBoundingClientRect();
    return t.top - (a.top + a.height) - 8 - 8;
  });
  expect(strip, `fixture must leave the >=120px usable strip (got ${strip})`).toBeGreaterThanOrEqual(120);

  // The panel must NEVER jump off the strip cap to the plain above-room cap.
  // With the measurement feedback bug the box oscillates up to ~600px (the
  // 60vh cap) every frame; the fixed measurement keeps it at the strip.
  const capHeights = await page.evaluate(
    () => (window as unknown as { __capHeights?: number[] }).__capHeights ?? [],
  );
  expect(capHeights.length).toBeGreaterThan(10);
  expect(
    Math.max(...capHeights),
    `panel never exceeds the strip cap (saw ${Math.max(...capHeights)}, strip ${strip})`,
  ).toBeLessThanOrEqual(strip + 4);

  // The inner list owns the scroll (81 catalog rows), regardless of the cap.
  const listScrolls = await list.evaluate((el) => {
    const body = el as HTMLElement;
    return body.scrollHeight > body.clientHeight + 1;
  });
  expect(listScrolls).toBe(true);

  // The popover height stays STABLE after the opening frames too.
  const heights = await menu.evaluate(async (el) => {
    const out: number[] = [];
    for (let i = 0; i < 8; i += 1) {
      out.push(Math.round((el as HTMLElement).getBoundingClientRect().height));
      await new Promise((r) => setTimeout(r, 100));
    }
    return out;
  });
  expect(Math.max(...heights) - Math.min(...heights)).toBeLessThanOrEqual(1);

  // Panel caps to the strip and clears the card (non-overlap, bottom edge on
  // the card side of the trigger gap).
  const panelBox = await menu.boundingBox();
  const triggerBox = await trigger.boundingBox();
  const approvalBox = await approval.boundingBox();
  expect(panelBox && triggerBox && approvalBox).toBeTruthy();
  // The panel sits in the strip ABOVE the card: its top edge starts at/below
  // the card's bottom edge, so they never overlap (no edge ordering on
  // bottoms — the panel extends down toward the trigger past the card end).
  expect(panelBox!.y).toBeGreaterThanOrEqual(approvalBox!.y + approvalBox!.height - 8 - 1);
  expect(Math.abs(panelBox!.height - strip) <= 2, `caps to the strip (${panelBox!.height} vs ${strip})`).toBeTruthy();
});

test("distinct full ids sharing one final segment stay separate and post the full id", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  const instanceId = await createSession(page, "Model picker colliding-catalog");
  await page.reload();
  await page.getByTestId("model-effort-chip").waitFor({ timeout: 20_000 });
  await clearApprovals(page, instanceId);
  await openModelList(page);

  // vendor-a/shared and vendor-b/shared are two real models the driver
  // distinguishes by FULL id: two rows, full-id testids/labels, never one
  // merged "shared" radio.
  const rowA = page.getByTestId("model-option-vendor-a-shared");
  const rowB = page.getByTestId("model-option-vendor-b-shared");
  await expect(rowA).toBeVisible();
  await expect(rowB).toBeVisible();
  expect(await page.getByTestId("model-option-shared").count()).toBe(0);
  await expect(rowA).toContainText("vendor-a/shared");
  await expect(rowB).toContainText("vendor-b/shared");

  // Clicking one posts exactly its full id.
  const request = page.waitForRequest(
    (r) =>
      r.method() === "POST" &&
      r.url().endsWith(`/v1/instances/${instanceId}/commands`) &&
      r.postDataJSON()?.operation === "instance.configure",
  );
  await rowA.click();
  expect((await request).postDataJSON().payload.model).toBe("vendor-a/shared");

  // The verdict settles the listed selection on the SAME full id: A selected,
  // B untouched (a short-label match would have selected both / posted B).
  await openModelList(page);
  await expect(page.getByTestId("effort-slider-panel")).toHaveAttribute("data-model-pending", "0", {
    timeout: 10_000,
  });
  await expect(page.getByTestId("effort-slider-panel")).toHaveAttribute(
    "data-model-current",
    "shared",
    { timeout: 10_000 },
  );
  await expect(page.getByTestId("model-option-vendor-a-shared")).toHaveAttribute(
    "data-selected",
    "1",
  );
  await expect(page.getByTestId("model-option-vendor-b-shared")).toHaveAttribute(
    "data-selected",
    "0",
  );
});

test("vendor /auto ids are distinct full models: selection, pending and posting all stay exact", async ({ page }) => {
  // r26 r3: a final "/auto" segment is NOT a cross-model alias. Only bare
  // `auto` / `passthrough/auto` collapse; vendor-a/auto, vendor-b/auto and
  // vendor-b/auto_model are concrete models with exact full-id identity.
  await page.setViewportSize({ width: 1440, height: 1000 });
  const tid = (id: string) =>
    `model-option-${id.replace(/[^a-zA-Z0-9]+/g, "-").toLowerCase()}`;
  const postConfigure = async (id: string, model: string) =>
    page.evaluate(
      async ({ id, model }) => {
        const res = await fetch(`/v1/instances/${id}/commands`, {
          method: "POST",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ operation: "instance.configure", payload: { model } }),
        });
        return res.ok;
      },
      { id, model },
    );
  const waitForJournal = async (id: string, needle: string) =>
    expect
      .poll(
        () =>
          page.evaluate(
            ({ id, needle }) =>
              fetch(`/v1/instances/${id}/journal`, { credentials: "include" })
                .then((r) => r.json())
                .then((d) => (d.events ?? []).some((e: unknown) => JSON.stringify(e).includes(needle))),
            { id, needle },
          ),
        { timeout: 10_000 },
      )
      .toBe(true);

  // Case 1+2: both vendor ids listed (same-spelling and /auto vs /auto_model).
  for (const { prompt, other, current } of [
    { prompt: "Model colliding-auto-model identity", other: "vendor-a/auto", current: "vendor-b/auto" },
    { prompt: "Model colliding-auto-model-2 identity", other: "vendor-a/auto", current: "vendor-b/auto_model" },
  ] as const) {
    const instanceId = await createSession(page, prompt);
    await page.reload();
    await page.getByTestId("model-effort-chip").waitFor({ timeout: 20_000 });
    await clearApprovals(page, instanceId);
    // Resolve the running model to the vendor-b id (accepted configure), then
    // queue a separate pending switch to vendor-a.
    expect(await postConfigure(instanceId, current)).toBe(true);
    await waitForJournal(instanceId, `"id":"${current}"`);
    expect(await postConfigure(instanceId, `__queued__:${other}`)).toBe(true);
    await waitForJournal(instanceId, "model-queued");

    await openModelList(page);
    await expect(page.getByTestId(tid(other))).toBeVisible();
    await expect(page.getByTestId(tid(current))).toBeVisible();
    expect(await page.getByTestId("model-option-auto").count()).toBe(0);
    expect(page.getByTestId(tid(current))).toHaveAttribute("data-selected", "1");
    expect(page.getByTestId(tid(other))).toHaveAttribute("data-selected", "0");
    expect(page.getByTestId(tid(other))).toHaveAttribute("data-model-pending", "queued");
    expect(page.getByTestId(tid(current))).toHaveAttribute("data-model-pending", "0");

    // Clicking posts the exact full id.
    const request = page.waitForRequest(
      (r) =>
        r.method() === "POST" &&
        r.url().endsWith(`/v1/instances/${instanceId}/commands`) &&
        r.postDataJSON()?.operation === "instance.configure",
    );
    await page.getByTestId(tid(other)).click();
    expect((await request).postDataJSON().payload.model).toBe(other);
  }

  // Case 3: catalog lists ONLY vendor-a/auto; the vendor-b read-back is an
  // unlisted running model and must be appended as its own selected row.
  const instanceId = await createSession(page, "Model auto-append identity");
  await page.reload();
  await page.getByTestId("model-effort-chip").waitFor({ timeout: 20_000 });
  await clearApprovals(page, instanceId);
  expect(await postConfigure(instanceId, "vendor-b/auto")).toBe(true);
  await waitForJournal(instanceId, '"id":"vendor-b/auto"');
  await openModelList(page);
  await expect(page.getByTestId(tid("vendor-a/auto"))).toBeVisible();
  const appended = page.getByTestId(tid("vendor-b/auto"));
  await expect(appended).toBeVisible();
  expect(appended).toHaveAttribute("data-selected", "1");
  expect(page.getByTestId(tid("vendor-a/auto"))).toHaveAttribute("data-selected", "0");
  expect(await page.getByTestId("model-option-auto").count()).toBe(0);
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});
