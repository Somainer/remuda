import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

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

async function createSession(page: Page, prompt: string, kind = "claude"): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await page.getByTestId(`new-session-kind-${kind}`).click();
  // c-effortui r5 item 3: effort switch tests require the Herdr claude-pty
  // carrier — shell-pty cannot type version-gated ultracode words and the
  // switch is intentionally locked for it. The fake node reports both; the
  // New Session default is the native shell-pty, so open advanced and select
  // claude-pty.
  if (kind === "claude") {
    await page.getByTestId("new-session-advanced").click();
    await page.getByTestId("new-session-driver-claude-pty").click();
  }
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

const codexTiers = [
  ["low", "Low", "Fast responses with lighter reasoning"],
  ["medium", "Medium", "Balances speed and reasoning depth for everyday tasks"],
  ["high", "High", "Greater reasoning depth for complex problems"],
  ["xhigh", "Extra high", "Extra high reasoning depth for complex problems"],
  ["max", "Max", "For difficult problems when quality matters more than speed · higher usage"],
  ["ultra", "Ultra", "For demanding work using multiple agents · highest usage"],
] as const;

const evidenceDir = process.env.REMUDA_EVIDENCE === "1"
  ? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
  : path.resolve("test-results/evidence");

for (const width of [390, 1440]) {
  test(`Codex max and ultra round-trip with the six-tier picker at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    const instanceId = await createSession(page, "Codex effort round trip", "codex");
    await clearApprovals(page, instanceId);

    for (const [name, look] of [["max", "top"], ["ultra", "ultracode"]] as const) {
      await page.getByTestId("model-effort-chip").click();
      await page.getByTestId("effort-open-list").click();
      const request = page.waitForRequest((r) =>
        r.method() === "POST" && r.url().endsWith(`/v1/instances/${instanceId}/commands`)
        && r.postDataJSON()?.operation === "instance.configure");
      await page.getByTestId(`effort-tier-${name}`).click();
      const payload = (await request).postDataJSON().payload;
      expect(payload.effort.name).toBe(name);
      expect(payload.effort.ultracode ?? false).toBe(false);
      await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-name", name);
      await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-effort-look", look);
      await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-ultracode", "0");
      await page.keyboard.press("Escape");
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-effective", name);
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-source", "remuda");
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-mismatch", "0");

      // Reload from persisted Hub state: neither requested nor observed name
      // may be downgraded to xhigh or turned into Claude's workflow flag.
      await page.reload();
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-effective", name);
      await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-mismatch", "0");
      await page.getByTestId("model-effort-chip").click();
      await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-name", name);
      await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-effort-look", look);
      await expect(page.getByTestId("loading-snapshot")).toHaveCount(0);
      await mkdir(evidenceDir, { recursive: true });
      await page.screenshot({ path: path.join(evidenceDir, `effort-codex-tiers-1-${name}-${width}.png`), animations: "disabled" });
      await page.keyboard.press("Escape");
    }

    await page.getByTestId("model-effort-chip").click();
    await page.getByTestId("effort-open-list").click();
    const rows = page.getByTestId("effort-list").locator('[data-testid^="effort-tier-"]');
    await expect(rows).toHaveCount(6);
    for (const [i, [name, label, description]] of codexTiers.entries()) {
      await expect(rows.nth(i)).toHaveAttribute("data-testid", `effort-tier-${name}`);
      await expect(rows.nth(i)).toHaveText(`${label}${description}`);
      await expect(rows.nth(i)).toHaveAttribute("title", description);
      await expect(rows.nth(i)).toBeVisible();
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
    await page.screenshot({ path: path.join(evidenceDir, `effort-codex-tiers-1-list-${width}.png`), animations: "disabled" });
  });
}

// D-056 evidence renders: five-stop slider with the orthogonal Ultracode
// switch at phone (390px, options sheet) and desktop (1440px, anchored
// popover). Captured only with REMUDA_EVIDENCE=1; self-skips otherwise.
// (The owner's WebKit-iPhone real-keyboard pass is separate; these are the
// committed chromium renders of both surfaces.)
for (const width of [390, 1440] as const) {
  test.describe(`D-056 switch evidence ${width}px`, { tag: ["@evidence"] }, () => {
    test.skip(process.env.REMUDA_EVIDENCE !== "1", "set REMUDA_EVIDENCE=1 for evidence renders");

    test("five stops + ultracode switch render under the pill", async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      const instanceId = await createSession(page, `effort switch evidence ${width}`);
      await clearApprovals(page, instanceId);
      await page.getByTestId("model-effort-chip").click();
      await expect(page.getByTestId("effort-slider")).toBeVisible();
      await expect(page.getByTestId("effort-slider")).toHaveAttribute(
        "data-tiers",
        "low,medium,high,xhigh,max",
      );
      await expect(page.getByTestId("effort-ultracode-switch")).toBeVisible();
      // The narrow (390px) layout renders the same card at viewport width;
      // the phone options-sheet surface is exercised by the mobile/WebKit
      // project in the non-hub config. Both widths are captured here.
      await mkdir(evidenceDir, { recursive: true });
      // Switch ON to capture the ember state on the switch row.
      await page.getByTestId("effort-ultracode-switch").click();
      await expect(page.getByTestId("effort-ultracode-switch")).toHaveAttribute("aria-checked", "true");
      await page.screenshot({
        path: path.join(evidenceDir, `effort-toggle-1-${width}.png`),
        animations: "disabled",
      });
    });
  });
}

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

/** Open the effort popover (idempotent: a no-op if already open). */
async function openPopover(page: Page) {
  if (await page.getByTestId("effort-slider").isVisible().catch(() => false)) return;
  await page.getByTestId("model-effort-chip").click();
  await expect(page.getByTestId("effort-slider")).toBeVisible();
}

/** Close the effort popover if it is open. */
async function closePopover(page: Page) {
  if (await page.getByTestId("effort-slider").isVisible().catch(() => false)) {
    await page.keyboard.press("Escape");
  }
}

/** Move the slider via keyboard; leaves the popover closed. */
async function moveSlider(page: Page, key: "ArrowRight" | "ArrowLeft" | "Home" | "End") {
  await openPopover(page);
  const slider = page.getByTestId("effort-slider");
  await slider.focus();
  await page.keyboard.press(key);
  await closePopover(page);
}

/** Pick a native tier row in the popover list; leaves the popover closed. */
async function pickTier(page: Page, stop: string) {
  await openPopover(page);
  await page.getByTestId("effort-open-list").click();
  await page.getByTestId(`effort-tier-${stop}`).click();
  await closePopover(page);
}

/** Flip the D-056 ultracode switch to the desired state; leaves popover open
 *  so subsequent assertions can read the slider/switch. */
async function setSwitch(page: Page, on: boolean) {
  await openPopover(page);
  const sw = page.getByTestId("effort-ultracode-switch");
  await expect(sw).toBeEnabled();
  if ((await sw.getAttribute("aria-checked")) !== (on ? "true" : "false")) {
    await sw.click();
  }
}

/** Post a configure with a sentinel effort the UI slider never offers. */
async function postEffortConfigure(
  page: Page,
  instanceId: string,
  effort: Record<string, unknown>,
) {
  await page.evaluate(
    async ({ id, effort }) => {
      const res = await fetch(`/v1/instances/${id}/commands`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ operation: "instance.configure", payload: { effort } }),
      });
      if (!res.ok) throw new Error(`configure ${res.status}`);
    },
    { id: instanceId, effort },
  );
}

test("a level switch reaches the fake node and the read-back drives the chip", async ({ page }) => {
  const instanceId = await createSession(page, "effort round trip");
  await clearApprovals(page, instanceId);

  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("?", { timeout: 10_000 });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-effective", "unknown");

  // high (index 2) → xhigh (index 3); the wire carries the native word plus
  // an explicit boolean, never the legacy alias.
  const xhighRequest = page.waitForRequest(
    (r) =>
      r.method() === "POST"
      && r.url().endsWith(`/v1/instances/${instanceId}/commands`)
      && r.postDataJSON()?.payload?.effort?.name === "xhigh"
      && r.postDataJSON()?.payload?.effort?.ultracode === false,
  );
  await moveSlider(page, "ArrowRight");
  await xhighRequest;
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("xhigh", { timeout: 15_000 });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-source", "remuda");
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-mismatch", "0");
});

test("plain max clamps to xhigh while max+ultracode stays on max (D-056)", async ({ page }) => {
  const instanceId = await createSession(page, "effort clamp vs switch");
  await clearApprovals(page, instanceId);
  // LEVEL to max with the switch OFF: the fake model lacks xhigh, so it
  // clamps to xhigh and the mismatch line renders.
  await moveSlider(page, "End");
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("xhigh", { timeout: 15_000 });
  const mismatch = page.getByTestId("model-effort-mismatch");
  await expect(mismatch).toBeVisible();
  expect(await mismatch.textContent()).toContain("请求 max");
  expect(await mismatch.textContent()).toContain("实际 xhigh");

  // Now turn the SWITCH on at max: D-056 keeps the level at max even when the
  // model would otherwise clamp — the read-back is {max, ultracode:on}.
  const onRequest = page.waitForRequest(
    (r) =>
      r.postDataJSON()?.payload?.effort?.name === "max"
      && r.postDataJSON()?.payload?.effort?.ultracode === true,
  );
  await setSwitch(page, true);
  await onRequest;
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("max", { timeout: 15_000 });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ultracode-effective", "on");
});

test("the orthogonal switch turns ultracode on at the current tier without moving the slider", async ({ page }) => {
  const instanceId = await createSession(page, "effort decoupled switch");
  await clearApprovals(page, instanceId);
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("?");

  // LEVEL to xhigh first (a tier the fake echoes unclamped).
  await pickTier(page, "xhigh");
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("xhigh", { timeout: 15_000 });

  // Then the SWITCH on: {name:"xhigh",ultracode:true}; the slider stays put.
  const onRequest = page.waitForRequest(
    (r) =>
      r.method() === "POST"
      && r.url().endsWith(`/v1/instances/${instanceId}/commands`)
      && r.postDataJSON()?.payload?.effort?.name === "xhigh"
      && r.postDataJSON()?.payload?.effort?.ultracode === true,
  );
  await setSwitch(page, true);
  await onRequest;
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("xhigh", { timeout: 15_000 });
  await expect(page.getByTestId("model-effort-ultracode")).toHaveText(/ultracode/);
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ultracode-effective", "on");
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
  await openPopover(page);
  await expect(page.getByTestId("effort-slider")).toHaveAttribute("data-name", "xhigh");
  await expect(page.getByTestId("effort-ultracode-switch")).toHaveAttribute("aria-checked", "true");
  await page.keyboard.press("Escape");

  // A terminal-side /effort moves the axes locally and posts no configure.
  let configureCalls = 0;
  await page.route("**/v1/instances/*/commands", async (route) => {
    if (route.request().method() === "POST" && route.request().postDataJSON()?.operation === "instance.configure") {
      configureCalls += 1;
    }
    await route.continue();
  });
  await page.getByTestId("composer-input").fill("/effort:low");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("low", { timeout: 15_000 });
  await page.waitForTimeout(800);
  expect(configureCalls).toBe(0);
});

test("three consecutive switches (flag on, level, flag off/on) each settle", async ({ page }) => {
  const instanceId = await createSession(page, "effort multi switch");
  await clearApprovals(page, instanceId);
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("?");

  // 1) Flag ON at the current level (high); the slider does not move.
  let req = page.waitForRequest(
    (r) =>
      r.url().endsWith(`/v1/instances/${instanceId}/commands`)
      && r.postDataJSON()?.payload?.effort?.name === "high"
      && r.postDataJSON()?.payload?.effort?.ultracode === true,
  );
  await setSwitch(page, true);
  await req;
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ultracode-effective", "on", {
    timeout: 15_000,
  });

  // 2) LEVEL to xhigh keeps the flag on (it rides along at any tier).
  req = page.waitForRequest(
    (r) =>
      r.url().endsWith(`/v1/instances/${instanceId}/commands`)
      && r.postDataJSON()?.payload?.effort?.name === "xhigh"
      && r.postDataJSON()?.payload?.effort?.ultracode === true,
  );
  await pickTier(page, "xhigh");
  await req;
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("xhigh", { timeout: 15_000 });
  await expect(page.getByTestId("model-effort-ultracode")).toHaveText(/ultracode/);

  // 3) Flag OFF then ON — each settles from its own read-back.
  await setSwitch(page, false);
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ultracode-effective", "off", {
    timeout: 15_000,
  });
  await setSwitch(page, true);
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-ultracode-effective", "on", {
    timeout: 15_000,
  });
  await expect(page.getByTestId("model-effort-pending")).toHaveCount(0);
});

// The sentinel configure words (__queued__ / __degrade__ / __ultra_refuse…)
// are understood ONLY by the in-process fake Node in
// crates/remuda-hub/examples/hub_e2e.rs. Those tests gate on the harness knob
// at COLLECTION time rather than probing the trigger with a sentinel POST
// (a probe against a real Node types garbage into a live session). The gate
// runs the in-process hub with HUB_E2E_FAKE_NODE=1; a default full-suite run
// skips them, exactly like HUB_E2E_API_ROUTE for the api-route suite.
test.describe("fake-node sentinel configure outcomes", () => {
  test.skip(
    process.env.HUB_E2E_FAKE_NODE !== "1",
    "set HUB_E2E_FAKE_NODE=1 for the in-process hub_e2e fake Node",
  );

test("a model refusal disables only the switch, never ends the session", async ({ page }) => {
  const instanceId = await createSession(page, "effort switch refusals");
  await clearApprovals(page, instanceId);
  await postEffortConfigure(page, instanceId, {
    name: "__ultra_refuse_model__",
    ultracode: true,
    index: 2,
  });
  await openPopover(page);
  const row = page.getByTestId("effort-ultracode");
  await expect(row).toHaveAttribute("data-disabled", "1", { timeout: 10_000 });
  await expect(row).toHaveAttribute("data-reason", "ultracode-unavailable-for-model");
  await expect(page.getByTestId("effort-ultracode-switch")).toBeDisabled();
  await expect(page.getByTestId("effort-ultracode-reason")).toContainText(/ultracode/i);
  // The tier axis stays usable (a refused switch is a configure outcome only).
  await expect(page.getByTestId("effort-slider")).not.toHaveAttribute("aria-disabled", "true");
  await page.keyboard.press("Escape");
});

test("a queued level shows 排队中; a degraded verdict clears the indicator", async ({ page }) => {
  const instanceId = await createSession(page, "effort queue and reject");
  await clearApprovals(page, instanceId);
  await expect(page.getByTestId("model-effort-chip-label")).toHaveText("?");

  await postEffortConfigure(page, instanceId, { name: "__queued__:xhigh", ultracode: false, index: 3 });
  await expect(page.getByTestId("model-effort-pending")).toHaveText("排队中", { timeout: 10_000 });
  await expect(page.getByTestId("model-effort-chip")).toHaveAttribute("data-effort-pending", "queued");

  await postEffortConfigure(page, instanceId, {
    name: "__degrade__:max:dialog-kept",
    ultracode: false,
    index: 4,
  });
  await expect(page.getByTestId("model-effort-pending")).toHaveCount(0, { timeout: 10_000 });
});

});
