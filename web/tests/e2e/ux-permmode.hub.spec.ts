import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const evidenceDir = process.env.REMUDA_EVIDENCE === "1"
  ? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
  : path.resolve("test-results/evidence");

async function clearApprovals(page: Page, instanceId: string) {
  // Mirror the effort-sync helper: resolve any pending approval before typing.
  await page
    .evaluate(async (id) => {
      const listPending = async () => {
        const body = await (
          await fetch("/v1/interactions", { credentials: "include" })
        ).json();
        return (body.items ?? []).filter(
          (item: { instanceId?: string; state?: string }) =>
            item.instanceId === id && item.state === "pending",
        );
      };
      let mine = await listPending();
      const deadline = Date.now() + 10_000;
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
      while (mine.length > 0 && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 100));
        mine = await listPending();
      }
      return mine;
    }, instanceId);
}

/**
 * Runtime permission-mode switching against the fake Node in
 * `crates/remuda-hub/examples/hub_e2e.rs`.
 *
 * 1. the composer menu lists Claude's six real modes (dontAsk is launch-only
 *    and therefore greyed);
 * 2. picking a mode posts `instance.configure` with `permissionMode`; the fake
 *    node appends a `permission` observation, and the chip settles;
 * 3. a mode changed in the terminal itself (sent as `__perm__:<mode>`) folds
 *    into the chip locally with NO configure posted back (no ping-pong);
 * 4. a queued then degraded sentinel drives the pending tag then reverts.
 *
 * No real model is invoked.
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

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

test("the composer permission menu lists the six real Claude modes with dontAsk launch-only", async ({
  page,
}) => {
  await createSession(page, "perm menu vocabulary");
  await page.getByTestId("permission-chip").click();
  const ids = [
    "manual",
    "acceptEdits",
    "plan",
    "auto",
    "bypassPermissions",
    "dontAsk",
  ];
  for (const id of ids) {
    await expect(page.getByTestId(`permission-option-${id}`)).toBeVisible();
  }
  // dontAsk is the one launch-only row in a normal session.
  await expect(page.getByTestId("permission-option-dontAsk")).toBeDisabled();
  await expect(page.getByTestId("permission-option-dontAsk")).toHaveAttribute(
    "data-launch-only",
    "1",
  );
  await mkdir(evidenceDir, { recursive: true });
  await page.screenshot({
    path: path.join(evidenceDir, "permission-modes-1-menu-1440.png"),
    animations: "disabled",
  });
});

test("picking a live mode posts a configure and settles the chip from the read-back", async ({
  page,
}) => {
  const instanceId = await createSession(page, "perm switch to auto");

  // Initial mode before any read-back.
  await expect(page.getByTestId("permission-chip")).toHaveAttribute("data-permission", "manual");

  await page.getByTestId("permission-chip").click();
  const configureRequest = page.waitForRequest(
    (r) =>
      r.method() === "POST" &&
      r.url().endsWith(`/v1/instances/${instanceId}/commands`) &&
      r.postDataJSON()?.operation === "instance.configure",
  );
  await page.getByTestId("permission-option-auto").click();
  const request = await configureRequest;
  expect(request.postDataJSON()?.payload.permissionMode).toBe("auto");

  // The fake node appends a `permission` observation (source remuda); the
  // chip settles on the read-back word and clears the pending tag.
  await expect(page.getByTestId("permission-chip")).toHaveAttribute("data-permission", "auto");
  await expect(page.getByTestId("permission-chip")).not.toHaveAttribute("data-pending");
  await mkdir(evidenceDir, { recursive: true });
  await page.screenshot({
    path: path.join(evidenceDir, "permission-modes-1-chip-auto-1440.png"),
    animations: "disabled",
  });
});

test("a terminal-side mode change folds into the chip without a configure", async ({
  page,
}) => {
  const instanceId = await createSession(page, "terminal perm fold");
  await clearApprovals(page, instanceId);
  let configures = 0;
  page.on("request", (request) => {
    if (
      request.method() === "POST" &&
      request.url().endsWith(`/v1/instances/${instanceId}/commands`) &&
      request.postDataJSON()?.operation === "instance.configure"
    ) {
      configures += 1;
    }
  });

  // A bare shift+tab in the terminal is observed, not pushed back.
  await page.getByTestId("composer-input").fill("__perm__:plan");
  await page.keyboard.press("Enter");

  await expect(page.getByTestId("permission-chip")).toHaveAttribute("data-permission", "plan");
  // Give any (incorrect) configure a moment to happen, then prove none did.
  await page.waitForTimeout(500);
  expect(configures).toBe(0);
});

test("a queued then rejected switch shows the pending tag then reverts", async ({ page }) => {
  const instanceId = await createSession(page, "perm queued degrade");

  // The fake node answers a queued sentinel with only permission-queued.
  await page.evaluate(
    ([id]) =>
      fetch(`/v1/instances/${id}/commands`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          operation: "instance.configure",
          payload: { permissionMode: "__queued__:plan" },
        }),
      }),
    [instanceId],
  );
  await expect(page.getByTestId("permission-chip")).toHaveAttribute("data-pending", "queued");

  // A degraded verdict clears the pending state and surfaces a toast.
  await page.evaluate(
    ([id]) =>
      fetch(`/v1/instances/${id}/commands`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          operation: "instance.configure",
          payload: { permissionMode: "__degrade__:plan" },
        }),
      }),
    [instanceId],
  );
  await expect(page.getByTestId("permission-chip")).not.toHaveAttribute("data-pending");
});
