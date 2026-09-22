import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Evidence for ui-upgrade task 2 (c-tokens): the two formerly-ghost status
 * tokens --warn / --info now have real per-theme definitions.
 *
 * Each capture shows the effort chip in a session composer with:
 *  - the mismatch line "请求 max → 实际 xhigh" (var(--warn)) — the fake node
 *    clamps max to xhigh; and
 *  - the "排队中" pending tag (var(--info)) — posted via the __queued__ sentinel.
 *
 * Four groups: 390 / 1440 × night / ledger. A default run skips everything and
 * writes no PNGs; only REMUDA_EVIDENCE=1 refreshes the committed evidence.
 */

const evidence = process.env.REMUDA_EVIDENCE === "1";
const evidenceDir = evidence
  ? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
  : path.resolve("test-results/evidence");

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

async function clearApprovals(page: Page, instanceId: string) {
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
  }, instanceId);
}

/** Drive the request=max / effective=xhigh clamp so the warn line is present. */
async function forceMismatch(page: Page) {
  if ((await page.getByTestId("model-effort-mismatch").count()) === 1) return;
  await page.getByTestId("model-effort-chip").click();
  await page.getByTestId("effort-open-list").click();
  await page.getByTestId("effort-tier-max").click();
  await expect(page.getByTestId("model-effort-mismatch")).toBeVisible({ timeout: 15_000 });
  // At compact width the effort card lives inside the full-screen options
  // sheet: Escape only leaves the tier list, so dismiss via the sheet close
  // button. Desktop closes the anchored popover with Escape.
  const sheetClose = page.getByTestId("composer-options-close");
  if (await sheetClose.isVisible().catch(() => false)) {
    await sheetClose.click();
  } else {
    await page.keyboard.press("Escape");
  }
  await expect(page.getByTestId("effort-menu")).toHaveCount(0);
  await expect(page.getByTestId("composer-options-sheet")).toHaveCount(0);
}

/** Post the sentinel the fake node answers with only an effort-queued lifecycle. */
async function postQueuedPushDown(page: Page, instanceId: string) {
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
}

/**
 * The queued tag settles on the next effort read-back, so re-arm and recapture
 * until the span is still on screen in the frame right after the screenshot.
 */
async function capturePending(
  page: Page,
  instanceId: string,
  file: string,
  clip: { x: number; y: number; width: number; height: number },
) {
  const pending = page.getByTestId("model-effort-pending");
  for (let attempt = 0; attempt < 6; attempt++) {
    await postQueuedPushDown(page, instanceId);
    await expect(pending).toHaveText("排队中", { timeout: 10_000 });
    await page.screenshot({ path: file, animations: "disabled", clip });
    // The shot was taken milliseconds ago; if the tag has already settled we
    // cannot trust that frame — re-arm the lifecycle and try again.
    if (await page.locator('[data-testid="model-effort-pending"]:visible').count()) return;
    await expect(page.getByTestId("model-effort-mismatch")).toBeVisible({ timeout: 10_000 });
  }
  throw new Error("queued pending tag never survived a capture frame");
}

test.describe.configure({ mode: "serial" });

test.beforeEach(async ({ page }) => {
  test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture the committed screenshots");
  await login(page);
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

for (const theme of ["night", "ledger"] as const) {
  for (const width of [390, 1440] as const) {
    test(`effort warn + info chips at ${width}px in ${theme}`, async ({ page }) => {
      await page.setViewportSize({ width, height: width <= 767 ? 844 : 900 });
      await page.emulateMedia({ reducedMotion: "reduce" });
      const instanceId = await createSession(page, `tokens evidence ${theme} ${width}`);
      await clearApprovals(page, instanceId);

      // Theme is applied at boot (task 14): persist, then load the session
      // fresh so the whole capture renders in the requested theme.
      await page.evaluate((choice) => {
        localStorage.setItem("runtime.theme.v1", choice);
      }, theme);
      await page.goto(`/s/${instanceId}`);
      await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
      await expect(page.getByTestId("composer-bar")).toBeVisible();

      await forceMismatch(page);
      await expect(page.getByTestId("model-effort-mismatch")).toBeVisible();
      await expect(page.getByTestId("model-effort-pending")).toHaveCount(0);

      // Pre-compute the clip so nothing async races the shots.
      const bar = page.getByTestId("composer-bar");
      const box = await bar.boundingBox();
      expect(box).not.toBeNull();
      const viewportHeight = width <= 767 ? 844 : 900;
      const clip = {
        x: 0,
        y: Math.max(0, box!.y - 24),
        width,
        height: Math.min(box!.height + 24, viewportHeight - Math.max(0, box!.y - 24)),
      };

      await mkdir(evidenceDir, { recursive: true });

      // Panel 1: the warn mismatch line is a durable state, safe to capture.
      await page.screenshot({
        path: path.join(evidenceDir, `ui-upgrade-2-warn-${theme}-${width}.png`),
        animations: "disabled",
        clip,
      });

      // Panel 2: the info "排队中" tag is transient (it settles on read-back),
      // so capturePending re-arms until a frame is caught with it on screen.
      await capturePending(
        page,
        instanceId,
        path.join(evidenceDir, `ui-upgrade-2-info-${theme}-${width}.png`),
        clip,
      );
    });
  }
}
