import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * cb-command-queue (protocol.md §8.5) against the in-process fake Node in
 * `crates/remuda-hub/examples/hub_e2e.rs`:
 *
 * 1. a send the Node holds behind a busy turn (the `__held__` sentinel:
 *    accepted, never written) shows in the session's 「排队中」 list; 编辑
 *    rewrites it through the Node's `command.edit` (same commandId), 撤回
 *    withdraws it through `command.withdraw`, and the list empties;
 * 2. an effort change while the worker's turn runs (the `hold-working`
 *    sentinel) is answered `deferred: next-turn` and the chip reads
 *    下一回合生效;
 * 3. a configure based on a stale runtime-config generation is a 409: the
 *    page reloads the current values and says so.
 *
 * The sentinels exist only in the fake Node, so the file self-skips unless
 * the gate runs it with HUB_E2E_FAKE_NODE=1 (as effort-sync does).
 */

test.describe.configure({ mode: "serial" });

const evidenceDir =
  process.env.REMUDA_EVIDENCE === "1"
    ? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
    : null;

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
  // The PTY carrier is the one whose Node holds prompts and defers switches.
  await page.getByTestId("new-session-advanced").click();
  await page.getByTestId("new-session-driver-claude-pty").click();
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  return new URL(page.url()).pathname.split("/").pop()!;
}

async function send(page: Page, text: string) {
  const input = page.getByTestId("composer-input");
  await input.click();
  await input.fill(text);
  await page.keyboard.press("Enter");
}

async function activity(page: Page, instanceId: string): Promise<string> {
  return page.evaluate(async (id) => {
    const body = await (await fetch(`/v1/instances/${id}`, { credentials: "include" })).json();
    return String(body?.activity ?? "");
  }, instanceId);
}

async function pickTier(page: Page, stop: string) {
  if (!(await page.getByTestId("effort-slider").isVisible().catch(() => false))) {
    await page.getByTestId("model-effort-chip").click();
  }
  await page.getByTestId("effort-open-list").click();
  await page.getByTestId(`effort-tier-${stop}`).click();
  if (await page.getByTestId("effort-slider").isVisible().catch(() => false)) {
    await page.keyboard.press("Escape");
  }
}

test.describe("cb-command-queue", () => {
  test.skip(
    process.env.HUB_E2E_FAKE_NODE !== "1",
    "set HUB_E2E_FAKE_NODE=1 for the in-process hub_e2e fake Node",
  );

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test("a Node-held send is listed 排队中, edited and withdrawn", async ({ page }) => {
    const instanceId = await createSession(page, "queued sends");
    await send(page, "__held__ run the gate later");
    const row = page.getByTestId("queued-send");
    await expect(row).toHaveCount(1, { timeout: 20_000 });
    await expect(row).toHaveAttribute("data-held", "node");
    await expect(row).toContainText("__held__ run the gate later");
    await expect(page.getByTestId("queued-sends")).toContainText("排队中 · 1");
    if (evidenceDir) {
      await mkdir(evidenceDir, { recursive: true });
      await page.screenshot({ path: path.join(evidenceDir, "cmdqueue-1-list.png") });
    }

    const commandId = await row.getAttribute("data-command-id");
    await row.getByRole("button", { name: "编辑" }).click();
    const editor = page.getByRole("textbox", { name: "编辑排队中的消息" });
    await editor.fill("__held__ run the gate later, then report");
    const patch = page.waitForResponse(
      (response) =>
        response.request().method() === "PATCH" && response.url().endsWith(`/v1/commands/${commandId}`),
    );
    await page.getByRole("button", { name: "保存" }).click();
    expect((await patch).status()).toBe(200);
    await expect(page.getByTestId("queued-send")).toContainText("then report", { timeout: 10_000 });
    await expect(page.getByTestId("queued-send")).toHaveAttribute("data-command-id", commandId!);

    await page.getByTestId("queued-send").getByRole("button", { name: "撤回" }).click();
    await expect(page.getByTestId("queued-sends")).toHaveCount(0, { timeout: 10_000 });
    const listed = await page.evaluate(async (id) => {
      const body = await (
        await fetch(`/v1/commands?mine=1&state=queued&instanceId=${id}`, { credentials: "include" })
      ).json();
      return (body.commands ?? []).length;
    }, instanceId);
    expect(listed).toBe(0);
  });

  test("an effort change during a turn reads 下一回合生效", async ({ page }) => {
    const instanceId = await createSession(page, "deferred effort");
    await send(page, "keep going hold-working");
    await expect.poll(() => activity(page, instanceId), { timeout: 20_000 }).toBe("working");
    // The fake applies a real effort word at once; the queued sentinel keeps
    // the switch waiting like a PTY driver behind a running turn.
    await page.route(`**/v1/instances/${instanceId}/commands`, async (route) => {
      const body = route.request().postDataJSON();
      if (route.request().method() === "POST" && body?.operation === "instance.configure" && body.payload?.effort) {
        body.payload.effort.name = `__queued__:${body.payload.effort.name}`;
        await route.continue({ postData: JSON.stringify(body) });
        return;
      }
      await route.continue();
    });
    const answer = page.waitForResponse(
      (response) =>
        response.request().method() === "POST" &&
        response.url().endsWith(`/v1/instances/${instanceId}/commands`) &&
        response.request().postDataJSON()?.operation === "instance.configure",
    );
    await pickTier(page, "high");
    const configured = await (await answer).json();
    expect(configured.command.deferred).toEqual({ boundary: "next-turn" });
    const tag = page.getByTestId("model-effort-pending");
    await expect(tag).toHaveText("下一回合生效", { timeout: 10_000 });
    await expect(tag).toHaveAttribute("data-deferred", "next-turn");
    if (evidenceDir) {
      await page.screenshot({ path: path.join(evidenceDir, "cmdqueue-2-deferred.png") });
    }
    const config = await page.evaluate(async (id) => {
      return (await fetch(`/v1/instances/${id}/config`, { credentials: "include" })).json();
    }, instanceId);
    expect(config.pending[0]).toMatchObject({ state: "deferred", boundary: "next-turn" });
  });

  test("a stale generation is refused and the current values reload", async ({ page }) => {
    const instanceId = await createSession(page, "stale generation");
    // Someone else (Main, another device) wrote since this page read.
    await page.route(`**/v1/instances/${instanceId}/commands`, async (route) => {
      const body = route.request().postDataJSON();
      if (route.request().method() === "POST" && body?.operation === "instance.configure") {
        body.payload.expectedGeneration = 999_999;
        await route.continue({ postData: JSON.stringify(body) });
        return;
      }
      await route.continue();
    });
    const answer = page.waitForResponse(
      (response) =>
        response.request().method() === "POST" &&
        response.url().endsWith(`/v1/instances/${instanceId}/commands`) &&
        response.request().postDataJSON()?.operation === "instance.configure",
    );
    await pickTier(page, "low");
    const refused = await answer;
    expect(refused.status()).toBe(409);
    expect((await refused.json()).code).toBe("CONFIG_GENERATION_CONFLICT");
    await expect(page.getByText(/已载入当前值/).first()).toBeVisible({ timeout: 10_000 });
    await expect(page.getByTestId("model-effort-pending")).toHaveCount(0);
  });
});
