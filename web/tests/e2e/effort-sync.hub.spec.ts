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
 *    chip renders the observed level and 请求 max → 实际 xhigh.
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

test.afterAll(async ({ request }) => {
  for (const id of created.splice(0)) {
    await request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

async function clearApprovals(page: Page, instanceId: string) {
  await page.evaluate(async (id) => {
    const list = await (await fetch("/v1/interactions", { credentials: "include" })).json();
    for (const item of list.items ?? []) {
      if (item.instanceId !== id || item.state !== "pending") continue;
      await fetch(`/v1/interactions/${item.interactionId}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ answer: { kind: "outcome", outcome: "allow" } }),
      });
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
