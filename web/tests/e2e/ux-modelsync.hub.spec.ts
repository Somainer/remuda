import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

/**
 * D-028 §9.1 live model sync against the fake Node in
 * `crates/remuda-hub/examples/hub_e2e.rs`:
 *
 * 1. the picker shows the GATEWAY-discovered list (the launch snapshot carries
 *    `modelCatalog`), not a static builtin guess — e2e/fast, e2e/plain,
 *    claude-e2e-only all render;
 * 2. clicking a model posts instance.configure {model}; the `/model` verdict
 *    read-back marks it the current model (data-model-current);
 * 3. a typed alias that resolves to a different concrete id renders the
 *    resolved selection and 请求 → 实际 mismatch;
 * 4. a terminal-side `/model <id>` (a send) moves the picker with NO configure
 *    posted (single source of truth, no ping-pong);
 * 5. a `not-found` rejection reverts the selection and toasts;
 * 6. a queued switch shows the 排队中 tag.
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

const evidenceDir = process.env.REMUDA_EVIDENCE === "1"
  ? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
  : path.resolve("test-results/evidence");

async function openModelList(page: Page) {
  // The chip toggles the effort popover; the list view inside is a second
  // click. Re-open from a clean state so a previous list/dialog doesn't wedge
  // the toggle.
  await page.keyboard.press("Escape").catch(() => undefined);
  // D-042: Escape from the focused composer during a still-running turn now
  // opens the in-app 打断 Sheet (no native dialog to auto-dismiss). If that
  // defensive Escape surfaced it, cancel it so its scrim does not block the
  // chip click below.
  const strayConfirm = page.getByTestId("composer-confirm");
  await strayConfirm.waitFor({ state: "attached", timeout: 300 }).catch(() => undefined);
  if (await strayConfirm.count()) {
    await strayConfirm.getByTestId("composer-confirm-cancel").click();
  }
  await page.getByTestId("model-effort-chip").click();
  const open = page.getByTestId("effort-open-list");
  await open.waitFor({ state: "visible", timeout: 10_000 });
  await open.click();
  await expect(page.getByTestId("effort-list")).toBeVisible();
}

async function postConfigure(page: Page, instanceId: string, model: string) {
  await page.evaluate(
    async ({ id, model }) => {
      const res = await fetch(`/v1/instances/${id}/commands`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ operation: "instance.configure", payload: { model } }),
      });
      if (!res.ok) throw new Error(`configure ${res.status}`);
    },
    { id: instanceId, model },
  );
}

test("the picker lists the gateway-discovered models and selection read-backs", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  const instanceId = await createSession(page, "Model picker discovery");

  // The launch snapshot (catalog + current model) is appended during create;
  // reload from persisted Hub state so the journal backfill applies it before
  // the picker opens (mirrors the reconnect path the effort spec exercises).
  await page.reload();
  await page.getByTestId("model-effort-chip").waitFor({ timeout: 20_000 });
  await openModelList(page);
  const panel = page.getByTestId("effort-slider-panel");
  // The launch snapshot's gateway catalog (not builtin opus/sonnet/haiku).
  for (const short of ["fast", "plain", "auto", "claude-e2e-only"]) {
    await expect(page.getByTestId(`model-option-${short}`)).toBeVisible({ timeout: 10_000 });
  }
  // Launch model e2e/auto is the current selection.
  await expect(panel).toHaveAttribute("data-model-current", "auto");
  await expect(page.getByTestId("model-option-auto")).toHaveAttribute("data-selected", "1");
  await mkdir(evidenceDir, { recursive: true });
  await page.screenshot({
    path: path.join(evidenceDir, "model-sync-1-picker-gateway-list.png"),
    animations: "disabled",
  });

  // Pick e2e/fast → instance.configure {model:"e2e/fast"}; the verdict read-back
  // settles the current model on fast.
  const request = page.waitForRequest(
    (r) =>
      r.method() === "POST" &&
      r.url().endsWith(`/v1/instances/${instanceId}/commands`) &&
      r.postDataJSON()?.operation === "instance.configure",
  );
  await page.getByTestId("model-option-fast").click();
  const payload = (await request).postDataJSON().payload;
  expect(payload.model).toBe("e2e/fast");
  // Reopen (the click closed the list) and poll until the verdict read-back
  // settles: pending clears and the current model becomes fast.
  await openModelList(page);
  await expect(panel).toHaveAttribute("data-model-pending", "0", { timeout: 10_000 });
  await expect(panel).toHaveAttribute("data-model-current", "fast", { timeout: 10_000 });
  await expect(page.getByTestId("model-option-fast")).toHaveAttribute("data-selected", "1");
  await expect(page.getByTestId("model-option-plain")).toHaveAttribute("data-selected", "0");
});

test("a typed alias resolving to a different id renders the mismatch", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  const instanceId = await createSession(page, "Model alias resolution");
  // Clicking fast is the optimistic request; rewrite the configure to the
  // resolve sentinel so the fake verdict reports a different concrete id.
  await page.route("**/v1/instances/*/commands", async (route) => {
    const body = route.request().postDataJSON();
    if (body?.operation === "instance.configure" && body.payload?.model) {
      body.payload.model = `__resolve__:${body.payload.model}=e2e/plain`;
      await route.continue({ postData: JSON.stringify(body) });
    } else {
      await route.continue();
    }
  });
  await openModelList(page);
  await page.getByTestId("model-option-fast").click();
  // Wait for the read-back (queued/pending clears), then reopen the list.
  await expect
    .poll(
      async () =>
        page.evaluate((id) =>
          fetch(`/v1/instances/${id}/journal`, { credentials: "include" })
            .then((r) => r.json())
            .then((d) => (d.events ?? []).some((e: unknown) =>
              JSON.stringify(e).includes("e2e/plain"))),
          instanceId),
      { timeout: 10_000 },
    )
    .toBe(true);
  await openModelList(page);
  const panel = page.getByTestId("effort-slider-panel");
  await expect(panel).toHaveAttribute("data-model-current", "plain");
  await expect(panel).toHaveAttribute("data-model-mismatch", "1");
  await expect(page.getByTestId("model-option-mismatch")).toContainText("fast");
  await expect(page.getByTestId("model-option-mismatch")).toContainText("plain");
  await expect(page.getByTestId("model-option-plain")).toHaveAttribute("data-selected", "1");
});

test("a terminal-side /model moves the picker without posting configure", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  const instanceId = await createSession(page, "Terminal model fold");
  await clearApprovals(page, instanceId);

  let configurePosts = 0;
  await page.route("**/v1/instances/*/commands", async (route) => {
    try {
      if (route.request().postDataJSON()?.operation === "instance.configure") configurePosts += 1;
    } catch {
      /* not JSON */
    }
    await route.continue();
  });

  // Send a terminal-side slash command; the fake node echoes a model obs.
  await page.getByTestId("composer-input").fill("/model:e2e/fast");
  await page.getByTestId("composer-input").press("Enter");
  await openModelList(page);
  await expect(page.getByTestId("effort-slider-panel")).toHaveAttribute(
    "data-model-current",
    "fast",
  );
  await expect(page.getByTestId("model-option-fast")).toHaveAttribute("data-selected", "1");
  // The fold never called back into instance.configure (no ping-pong).
  expect(configurePosts).toBe(0);
});

test("a not-found rejection reverts the selection and toasts", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  const instanceId = await createSession(page, "Model not found revert");
  await clearApprovals(page, instanceId);
  await postConfigure(page, instanceId, "__notfound__:e2e/ghost");
  // A rejection toast is shown.
  await expect(page.getByText(/模型切换被拒绝/)).toBeVisible();
  await openModelList(page);
  // Selection reverted to the launch model auto, not the refused ghost.
  await expect(page.getByTestId("effort-slider-panel")).toHaveAttribute("data-model-current", "auto");
  await expect(page.getByTestId("model-option-ghost")).toHaveCount(0);
});

test("a queued model switch shows the 排队中 tag", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  const instanceId = await createSession(page, "Model queued tag");
  await clearApprovals(page, instanceId);
  await postConfigure(page, instanceId, "__queued__:e2e/fast");
  await openModelList(page);
  await expect(page.getByTestId("effort-slider-panel")).toHaveAttribute(
    "data-model-pending",
    "queued",
  );
  await expect(page.getByTestId("model-option-fast")).toHaveAttribute(
    "data-model-pending",
    "queued",
  );
  await expect(page.getByTestId("model-option-pending")).toContainText("排队中");
});

async function clearApprovals(page: Page, instanceId: string) {
  // The create-time approval keeps the composer disabled until answered.
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
          answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" },
        }),
      });
    }
  }, instanceId);
  // Wait for the composer to become editable.
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 15_000 });
}

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});
