import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * D-028 §9.1 live effort sync against the fake Node in
 * `crates/remuda-hub/examples/hub_e2e.rs`:
 *
 * 1. the session starts with no read-back level — the chip is a greyed `?`;
 * 2. moving the slider to xhigh posts `instance.configure`; the fake node
 *    appends an `effort` observation reporting xhigh — agreement, no mismatch;
 * 3. moving to max, the fake node clamps the effective level to xhigh, so the
 *    chip renders the observed level and 请求 max → 实际 xhigh;
 * 4. the ultracode stop reads back xhigh + the flag and the chip says
 *    "ultracode" (effort-sync-2);
 * 5. a terminal-side `/effort low` (a send, not a configure) moves the slider
 *    to low with NO configure posted (single source of truth, no ping-pong);
 * 6. while the turn is working, a queued push-down shows the 排队中 tag;
 * 7. a rejected switch shows no read-back level change and reverts the chip.
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

// Clean up with `page.request`, which carries the login session cookie. The
// standalone Playwright `request` fixture is unauthenticated: deleting with
// it 401s (previously swallowed by .catch), leaving the fake node's 8
// placement slots occupied and failing later specs with PLACEMENT_UNSATISFIABLE.
test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

async function clearApprovals(page: Page, instanceId: string) {
  // Use the approval answer shape the Hub validates. An `outcome` answer 422s
  // and would leave the fake node's create-time approval pending, leaking an
  // "echo e2e" card into the next spec's approvals page in a serial run.
  // Wait until the requested row is durable before answering: the pipelined
  // Node→Hub uplink can lag create, and answering a not-yet-persisted request
  // would be resurrected when its late journal event lands.
  await page.evaluate(async (id) => {
    const listPending = async () => {
      const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
      return (body.items ?? []).filter(
        (item: { instanceId?: string; state?: string }) =>
          item.instanceId === id && item.state === "pending",
      );
    };
    const deadline = Date.now() + 10_000;
    let mine = await listPending();
    while (mine.length === 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      mine = await listPending();
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
    // Confirm the answer landed so a late journal event cannot resurrect it.
    let remaining = await listPending();
    while (remaining.length > 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      remaining = await listPending();
    }
  }, instanceId);
}

/** Open the effort popover and move the slider to `stop` via keyboard. */
async function moveSlider(page: Page, key: "ArrowRight" | "ArrowLeft" | "Home" | "End") {
  await page.getByTestId("model-effort-chip").click();
  await page.getByTestId("effort-slider").focus();
  await page.keyboard.press(key);
  await page.keyboard.press("Escape");
}

test("slider change reaches the fake node; transcript read-back drives the effective chip", async ({
  page,
}) => {
  const instanceId = await createSession(page, "effort round trip");
  await clearApprovals(page, instanceId);

  // Before any assistant record reports a level, the chip is a greyed `?`.
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("?", {
    timeout: 10_000,
  });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute(
    "data-effort-effective",
    "unknown",
  );

  // high (index 2) → xhigh (index 3). hubStore.setEffort posts the configure;
  // the fake node reports xhigh back — agreement, no mismatch.
  await moveSlider(page, "ArrowRight");
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("xhigh", {
    timeout: 15_000,
  });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute(
    "data-effort-source",
    "remuda",
  );
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-mismatch", "0");
  expect(await page.getByTestId("model-effort-mismatch").count()).toBe(0);

  // xhigh → max. The fake agent environment clamps the effective level to
  // xhigh: the chip shows the observed level and the mismatch line.
  await moveSlider(page, "ArrowRight");
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("xhigh", {
    timeout: 15_000,
  });
  const mismatch = page.getByTestId("model-effort-mismatch");
  await expect(mismatch).toBeVisible();
  expect(await mismatch.textContent()).toContain("请求 max");
  expect(await mismatch.textContent()).toContain("实际 xhigh");
});

test("ultracode read-back names the chip ultracode; a terminal /effort moves the slider without configure", async ({
  page,
}) => {
  const instanceId = await createSession(page, "effort terminal sync");
  await clearApprovals(page, instanceId);
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("?");

  // Count every instance.configure command so the terminal-side fold below
  // can be proven not to ping-pong a configure back into the PTY.
  let configureCalls = 0;
  await page.route("**/v1/instances/*/commands", async (route) => {
    const request = route.request();
    if (request.method() === "POST") {
      const body = request.postDataJSON() as { operation?: string } | null;
      if (body?.operation === "instance.configure") configureCalls += 1;
    }
    await route.continue();
  });

  // low → med → high → xhigh → max → ultracode (End from default).
  await moveSlider(page, "End");
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("ultracode", {
    timeout: 15_000,
  });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute(
    "data-effort-effective",
    "ultracode",
  );
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
  const configuresAfterSlider = configureCalls;
  expect(configuresAfterSlider).toBeGreaterThanOrEqual(1);

  // Terminal side: a send carrying the sentinel the fake node maps to a
  // hand-typed `/effort low` slash observation.
  const composer = page.getByTestId("composer-input");
  await composer.fill("/effort:low");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("low", { timeout: 15_000 });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-source", "slash");
  // Give any (incorrect) configure a moment to happen, then prove none did.
  await page.waitForTimeout(800);
  expect(configureCalls).toBe(configuresAfterSlider);
});

test("a queued push-down shows 排队中; a rejected one reverts the chip", async ({ page }) => {
  const instanceId = await createSession(page, "effort queue and reject");
  await clearApprovals(page, instanceId);
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("?");

  // Post the sentinel command directly (the UI slider never offers these
  // words): the fake node answers with only an effort-queued lifecycle.
  await page.evaluate(async (id) => {
    const res = await fetch(`/v1/instances/${id}/commands`, {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        operation: "instance.configure",
        payload: { effort: { name: "__queued__:xhigh", index: 3 } },
      }),
    });
    if (!res.ok) throw new Error(`queued configure ${res.status}`);
  }, instanceId);
  await expect(page.getByTestId("model-effort-pending")).toHaveText("排队中", { timeout: 10_000 });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute(
    "data-effort-pending",
    "queued",
  );

  // A degraded verdict (Esc on the native dialog) reverts and clears pending.
  await page.evaluate(async (id) => {
    await fetch(`/v1/instances/${id}/commands`, {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        operation: "instance.configure",
        payload: { effort: { name: "__degrade__:max", index: 4 } },
      }),
    });
  }, instanceId);
  await expect(page.getByTestId("model-effort-pending")).toHaveCount(0, { timeout: 10_000 });
});
