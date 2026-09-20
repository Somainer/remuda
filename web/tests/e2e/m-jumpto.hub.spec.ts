import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * c-mjumpto (mobile-ui plan task 7, ui-spec §4.7 / §1.3, D-049): the grouped
 * Jump To sheet.
 *
 * 390px on /s/:id/tty:
 *  - the nine-key bar's 跳转 key opens QuickFind in GROUPED mode — project
 *    headers carry the name, live git branch (feat/workbench-g2 from the fake
 *    node's changes proxy) and the buildSpaces() blocked count;
 *  - the leaves are still sessions inside the one listbox (no pane layer);
 *  - search scope is unchanged (title / space / host / id): the
 *    transcript-only word "echo" returns nothing;
 *  - the clock/list toggle works and persists per device.
 * 1440px: ⌘K regression only — the flat ranked listbox, no groups and no
 * toggle, with the combobox/listbox/aria-activedescendant contract intact.
 *
 * Synthetic data only: the in-process fake node (hub_e2e.rs). The blocked
 * session is the scripted "mhome-blocked" approval gate, same fixture
 * m-home.hub.spec.ts uses.
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

const created: string[] = [];

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function resolveHost(page: Page): Promise<string> {
  const response = await page.request.get("/v1/hosts");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { items?: { id?: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  expect(host?.id, "fake node host").toBeTruthy();
  return host!.id!;
}

/** API-created session (terminal/shell-pty), like m-keybar's helper. */
async function createInstance(page: Page, workspaceId: string, prompt: string): Promise<string> {
  const hostId = await resolveHost(page);
  const response = await page.request.post("/v1/instances", {
    data: { hostId, workspaceId, kind: "terminal", driver: "shell-pty", prompt },
  });
  expect(response.ok(), `create instance: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId?: string; id?: string } };
  const id = body.instance.instanceId ?? body.instance.id;
  expect(id).toBeTruthy();
  created.push(id!);
  return id!;
}

/** Form-created claude session whose prompt parks it on the blocked gate. */
async function createBlockedSession(page: Page): Promise<string> {
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
  const workspacePicker = page.getByTestId("new-session-workspace");
  await expect(workspacePicker.locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await workspacePicker.selectOption("wsp_e2e");
  await page.getByTestId("new-session-prompt").fill("MJUMP_BLOCKED mhome-blocked approval gate");
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

async function patchCap(page: Page, value: number): Promise<void> {
  const response = await page.request.get("/v1/hosts");
  const body = (await response.json()) as {
    items?: { id?: string; maxInstances?: number; label?: string }[];
  };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  if (host?.id && (host.maxInstances ?? 8) < value) {
    await page.request.patch(`/v1/hosts/${host.id}`, { data: { maxInstances: value } });
  }
}

async function forceDeleteAllInstances(page: Page) {
  await page.evaluate(async () => {
    const body = (await (await fetch("/v1/instances", { credentials: "include" })).json()) as {
      items?: { instanceId?: string; id?: string }[];
    };
    await Promise.all(
      (body.items ?? []).map((i) =>
        fetch(`/v1/instances/${i.instanceId ?? i.id}?force=1`, {
          method: "DELETE",
          credentials: "include",
        }).catch(() => undefined),
      ),
    );
  });
}

async function gotoTty(page: Page, id: string) {
  await page.goto(`/s/${id}/tty`);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
  await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 20_000 });
  await expect(page.locator(".xterm")).toBeVisible();
  await expect(page.getByTestId("phone-keybar")).toBeVisible();
}

function groupNamed(page: Page, name: string) {
  // Exact-text filter: "remuda-e2e" is a prefix of "remuda-e2e-second".
  return page
    .getByTestId("quickfind-group")
    .filter({ has: page.getByTestId("quickfind-group-project").filter({ hasText: new RegExp(`^${name}$`) }) });
}

test.describe("390px grouped Jump To from the key bar", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test.beforeAll(async ({ browser }) => {
    const setup = await browser.newPage();
    await login(setup);
    await patchCap(setup, 24);
    // Start from a clean instance table so the blocked-count header reflects
    // exactly the one blocked session this file creates.
    await forceDeleteAllInstances(setup);
    await setup.close();
  });

  test.afterAll(async ({ browser }) => {
    const cleanup = await browser.newPage();
    await login(cleanup);
    await forceDeleteAllInstances(cleanup).catch(() => undefined);
    await patchCap(cleanup, 8).catch(() => undefined);
    await cleanup.close();
  });

  test("opens grouped with project + branch headers and the buildSpaces blocked count; leaves are sessions", async ({
    page,
  }) => {
    await createBlockedSession(page);
    await createInstance(page, "wsp_e2e_second", "m jumpto second workspace");
    const terminalId = await createInstance(page, "wsp_e2e", "m jumpto tty strip");

    await gotoTty(page, terminalId);
    await page.getByTestId("phone-key-jump").click();

    // Opening mounts the panel through the spaces drawer; grouped headers
    // appear once the branch proxy resolves.
    await expect(page.getByTestId("quickfind-panel")).toBeVisible();
    await expect(page.getByTestId("quickfind-group")).not.toHaveCount(0);
    const first = groupNamed(page, "remuda-e2e");
    await expect(first).toBeVisible();
    await expect(first).toContainText("feat/workbench-g2", { timeout: 15_000 });
    await expect(first).toContainText("1 待处理");
    expect(await first.getAttribute("data-blocked")).toBe("1");

    // The second registered project renders its own group, blocked count 0.
    const second = groupNamed(page, "remuda-e2e-second");
    await expect(second).toBeVisible();
    await expect(second).toContainText("feat/workbench-g2");
    await expect(second).toContainText("0 待处理");

    // No pane hierarchy: every leaf is a session option in the one listbox.
    await expect(page.locator("#quickfind-listbox [role='option']").first()).toBeVisible();
    expect(await page.getByTestId("quickfind-result").count()).toBeGreaterThanOrEqual(3);
    expect(await page.getByRole("listbox").count()).toBe(1);

    await page.emulateMedia({ reducedMotion: "reduce" });
    await shot(page, "mobile-ui-7-jumpto-390.png");
  });

  test("search keeps the title/space/host/id scope; a transcript-only word returns nothing", async ({ page }) => {
    await createBlockedSession(page);
    const terminalId = await createInstance(page, "wsp_e2e", "m jumpto scope strip");
    await gotoTty(page, terminalId);
    await page.getByTestId("phone-key-jump").click();
    await expect(page.getByTestId("quickfind-panel")).toBeVisible();
    await expect(page.getByTestId("quickfind-group")).toBeVisible();

    const input = page.getByTestId("quickfind-input");
    await input.fill("MJUMP_BLOCKED");
    await expect(page.getByTestId("quickfind-result")).toHaveCount(1, { timeout: 10_000 });

    // "echo" exists only in the blocked session's transcript / next-step
    // sentence; rankQuickFind never searches bodies.
    await input.fill("echo");
    await expect(page.getByTestId("quickfind-result")).toHaveCount(0);
    await expect(page.getByTestId("quickfind-empty")).toBeVisible();

    // Project name still matches.
    await input.fill("remuda-e2e");
    await expect(page.getByTestId("quickfind-result")).not.toHaveCount(0);
  });

  test("clock/list ordering switches, sorts projects in list mode and persists per device", async ({ page }) => {
    await createBlockedSession(page);
    await createInstance(page, "wsp_e2e_second", "m jumpto order second");
    const terminalId = await createInstance(page, "wsp_e2e", "m jumpto order strip");
    const storageKey = "remuda.mobile.quickfind.order.v1";

    await gotoTty(page, terminalId);
    await page.getByTestId("phone-key-jump").click();
    await expect(groupNamed(page, "remuda-e2e")).toBeVisible();
    await expect(groupNamed(page, "remuda-e2e-second")).toBeVisible();

    // Default clock ordering; toggle to list.
    await expect(page.getByTestId("quickfind-order-clock")).toHaveAttribute("aria-pressed", "true");
    await page.getByTestId("quickfind-order-list").click();
    await expect(page.getByTestId("quickfind-order-list")).toHaveAttribute("aria-pressed", "true");
    expect(await page.evaluate((key) => localStorage.getItem(key), storageKey)).toBe("list");

    // List = project name order: remuda-e2e before remuda-e2e-second.
    const projectNames = () =>
      page.getByTestId("quickfind-group-project").evaluateAll((nodes) => nodes.map((n) => n.textContent));
    await expect.poll(projectNames).toContain("remuda-e2e");
    expect((await projectNames())!.indexOf("remuda-e2e")).toBeLessThan(
      (await projectNames())!.indexOf("remuda-e2e-second"),
    );

    // The choice survives reload and reopen (device-local localStorage).
    await page.goto(`/s/${terminalId}/tty`);
    await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 20_000 });
    await page.getByTestId("phone-key-jump").click();
    await expect(page.getByTestId("quickfind-panel")).toBeVisible();
    await expect(page.getByTestId("quickfind-order-list")).toHaveAttribute("aria-pressed", "true");
    await page.getByTestId("quickfind-order-clock").click();
    expect(await page.evaluate((key) => localStorage.getItem(key), storageKey)).toBe("clock");
  });

  test("Enter still routes to the selected session from a grouped sheet", async ({ page }) => {
    const blockedId = await createBlockedSession(page);
    const terminalId = await createInstance(page, "wsp_e2e", "m jumpto enter strip");
    await gotoTty(page, terminalId);
    await page.getByTestId("phone-key-jump").click();
    await expect(page.getByTestId("quickfind-result")).not.toHaveCount(0);

    // Filtering lands on the one blocked session; Enter navigates exactly it.
    await page.getByTestId("quickfind-input").fill("MJUMP_BLOCKED");
    await expect(page.getByTestId("quickfind-result")).toHaveCount(1);
    const box = page.getByTestId("quickfind-input");
    expect(await box.getAttribute("aria-activedescendant")).toBe("quickfind-option-0");
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(new RegExp(`/s/${blockedId}`));
    await expect(page.getByTestId("quickfind-panel")).toHaveCount(0);
  });
});

test.describe("1440px desktop Cmd-K regression", () => {
  test("Cmd-K opens the flat ranked listbox without groups or the phone toggle", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await login(page);
    // Two sessions so ArrowDown has somewhere to move (the cursor wraps with one).
    const firstId = await createInstance(page, "wsp_e2e", "m jumpto desktop regression one");
    const secondId = await createInstance(page, "wsp_e2e", "m jumpto desktop regression two");
    try {
      await page.goto("/sessions");
      await expect(page.getByTestId("session-list")).toBeVisible();
      await page.keyboard.press("Control+k");

      const panel = page.getByTestId("quickfind-panel");
      await expect(panel).toBeVisible();
      await expect(panel).toHaveAttribute("data-variant", "popover");
      await expect(page.getByTestId("quickfind-group")).toHaveCount(0);
      await expect(page.getByTestId("quickfind-order-clock")).toHaveCount(0);
      await expect(page.getByTestId("quickfind-order-list")).toHaveCount(0);
      // Wait for both cached instances to arrive before asserting cursor wrap.
      await expect(page.getByTestId("quickfind-result")).toHaveCount(2);

      // The combobox/listbox/aria-activedescendant contract is unchanged.
      const box = page.getByTestId("quickfind-input");
      await expect(box).toBeFocused();
      expect(await box.getAttribute("aria-activedescendant")).toBe("quickfind-option-0");
      await page.keyboard.press("ArrowDown");
      expect(await box.getAttribute("aria-activedescendant")).toBe("quickfind-option-1");
      await expect(page.getByTestId("quickfind-result").nth(1)).toHaveAttribute("data-selected", "true");

      await page.emulateMedia({ reducedMotion: "reduce" });
      await shot(page, "mobile-ui-7-jumpto-1440.png");
    } finally {
      await page.request.delete(`/v1/instances/${firstId}?force=1`).catch(() => undefined);
      await page.request.delete(`/v1/instances/${secondId}?force=1`).catch(() => undefined);
    }
  });
});
