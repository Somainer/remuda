import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * Header progress bar follows OSC 9;4 (native-config, 2026-09-16).
 *
 * The overlay pins `terminalProgressBarEnabled=true`; the Node's emulator
 * parses ConEmu `OSC 9;4` sequences the harness emits and the `tty.mode`
 * notice (the same channel alt-screen rides) updates the web header: a thin
 * indeterminate bar while the turn runs, a filled percent, an error tint, and
 * hidden on state 0.
 *
 * Fake Node only — the in-process harness in
 * crates/remuda-hub/examples/hub_e2e.rs. Typing TTYPROG_* + Enter emits the raw
 * OSC sequence plus the parsed `progress` notice, exactly like the Node's
 * local emulator pump.
 */
const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

async function patchMaxInstances(page: Page, value: number): Promise<number | undefined> {
  return page.evaluate(async (next) => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    const body = (await list.json()) as {
      items?: { hostId?: string; maxInstances?: number }[];
    };
    const id = body.items?.find((host) => host.hostId)?.hostId;
    if (!id) return undefined;
    const previous = body.items?.find((host) => host.hostId === id)?.maxInstances;
    await fetch(`/v1/hosts/${id}`, {
      method: "PATCH",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ maxInstances: next }),
    });
    return previous;
  }, value);
}

async function forceDeleteAllInstances(page: Page) {
  // The list key is `instanceId` (not `id`); a wrong key deletes
  // `/v1/instances/undefined` and leaves a live/requested row holding a
  // placement slot, which makes a later serial spec hit the fake node's
  // maxInstances cap. Wait for each delete to settle so no slot leaks.
  await page.evaluate(async () => {
    const list = await fetch("/v1/instances", { credentials: "include" });
    const body = (await list.json()) as {
      items?: { instanceId?: string; lifecycle?: string }[];
    };
    await Promise.all(
      (body.items ?? [])
        .filter((instance) => instance.instanceId)
        .filter(
          (instance) =>
            instance.lifecycle !== "exited" &&
            instance.lifecycle !== "failed" &&
            instance.lifecycle !== "closed",
        )
        .map(async (instance) => {
          const response = await fetch(
            `/v1/instances/${instance.instanceId}?force=1`,
            { method: "DELETE", credentials: "include" },
          );
          if (!response.ok) throw new Error(`force delete failed: ${response.status()}`);
        }),
    );
  });
}

test.beforeAll(async ({ browser }) => {
  const setup = await browser.newPage();
  await login(setup);
  await patchMaxInstances(setup, 24);
  await setup.close();
});

test.afterAll(async ({ browser }) => {
  const setup = await browser.newPage();
  await login(setup);
  await patchMaxInstances(setup, 8);
  await forceDeleteAllInstances(setup).catch(() => undefined);
  await setup.close();
});

test.afterEach(async ({ page }) => {
  await forceDeleteAllInstances(page).catch(() => undefined);
});

async function startTerminalSession(page: Page): Promise<void> {
  await forceDeleteAllInstances(page);
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20_000,
  });
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await page.getByTestId("new-session-kind-terminal").click();
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/")[2]!;
  await page.goto(`/s/${instanceId}/tty`);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
  await expect(page.locator("[data-tty-lab='1']")).toBeVisible();
}

async function focusTerminal(page: Page) {
  await page
    .locator(".xterm-helper-textarea")
    .first()
    .evaluate((el) => (el as HTMLTextAreaElement).focus());
}

async function runSentinel(page: Page, sentinel: string) {
  await focusTerminal(page);
  await page.keyboard.type(sentinel);
  await page.keyboard.press("Enter");
}

test("OSC 9;4 drives the header bar: indeterminate → percent → error → hidden", async ({
  page,
}) => {
  await login(page);
  await startTerminalSession(page);

  const lab = page.locator("[data-tty-lab='1']");
  const bar = page.getByTestId("tty-progress-bar");

  // Attach happens before any OSC sequence: no bar rendered, dataset hidden.
  await expect(lab).toHaveAttribute("data-tty-progress", "hidden");
  await expect(bar).toHaveCount(0);

  // State 3: indeterminate busy bar.
  await runSentinel(page, "TTYPROG_INDET");
  await expect(lab).toHaveAttribute("data-tty-progress", "indeterminate", { timeout: 15_000 });
  await expect(bar).toHaveAttribute("data-progress-state", "indeterminate");
  await expect(bar).toHaveAttribute("role", "progressbar");
  await shot(page, "native-progress-1-indeterminate.png");

  // State 1;50: determinate fill with an aria percent.
  await runSentinel(page, "TTYPROG_PERCENT");
  await expect(bar).toHaveAttribute("data-progress-state", "percent");
  await expect(bar).toHaveAttribute("aria-valuenow", "50");
  await shot(page, "native-progress-2-percent.png");

  // State 2: error tint.
  await runSentinel(page, "TTYPROG_ERROR");
  await expect(bar).toHaveAttribute("data-progress-state", "error");
  await shot(page, "native-progress-3-error.png");

  // State 0: done hides the bar.
  await runSentinel(page, "TTYPROG_DONE");
  await expect(lab).toHaveAttribute("data-tty-progress", "hidden");
  await expect(bar).toHaveCount(0);
});
