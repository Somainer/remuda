import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * Hold-⌘/Ctrl session switcher (owner nit 「按住 command 之后才显示，按数字键
 * 可以快捷切换」):
 *
 * - holding the platform-primary modifier reveals "⌘ 1".."⌘ 9" / "Ctrl 1"..
 *   "Ctrl 9" badges at the trailing edge of the first nine rows in the shared
 *   tab ordering; releasing hides them;
 * - pressing a digit while held opens exactly the session whose badge carries
 *   that digit — the badges and the handler resolve one ordered list, so the
 *   assertion compares hrefs rather than row positions (status groups reorder
 *   the paint, never the numbering);
 * - the shortcut works on the session detail route too, but never while focus
 *   is in the composer, and never collides with QuickFind (⌘/Ctrl+K owns the
 *   keystroke once its input is focused);
 * - a permanent, discreet hint names the gesture; a 390 px phone has neither
 *   hint, badges nor handler;
 * - a macOS UA spoof proves the ⌘ glyph and Meta+ aria tokens cross-platform.
 *
 * Synthetic fixture only: nine invented sessions on the in-process fake node
 * in `crates/remuda-hub/examples/hub_e2e.rs`. No real model, no real PTY.
 */

test.describe.configure({ mode: "serial" });

const created: string[] = [];
// Committed evidence is refreshed only on request (REMUDA_EVIDENCE=1); every other run —
// including the merge gate, whose verify-tree step rejects a dirty worktree — writes
// the same screenshot under the gitignored test-results/ instead.
const evidence = process.env.REMUDA_EVIDENCE === "1"
  ? path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
  : path.join(path.dirname(fileURLToPath(import.meta.url)), "../../test-results/evidence");

type RowBadge = { href: string | null; badge: string | null; held: string | null; aria: string | null };

async function patchMaxInstances(page: Page, value: number): Promise<void> {
  await page.evaluate(async (next) => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    const body = (await list.json()) as { items?: { hostId?: string }[] };
    const id = body.items?.[0]?.hostId;
    if (!id) throw new Error("fake node host missing");
    await fetch(`/v1/hosts/${id}`, {
      method: "PATCH",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ maxInstances: next }),
    });
  }, value);
}

/**
 * Delete every Instance the in-process fake hub currently holds.
 *
 * This config's specs share one serial Hub and the fake node ships
 * maxInstances 8; some files (e.g. spaces-hub-live) leave their sessions
 * live on purpose, so by the time this file starts the board is not empty.
 * The numbering test needs EXACTLY its own nine fixtures, so sweep first and
 * wait for a stable empty board.
 */
async function purgeInstances(page: Page): Promise<void> {
  // The settle poll may run longer than the 30 s evaluate default.
  page.setDefaultTimeout(120_000);
  const trace = await page.evaluate(async () => {
    const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
    const log: string[] = [];
    // Force-delete settles each row synchronously; includeHistory=true also
    // sweeps host-lost rows the plain list hides. Require THREE consecutive
    // empty reads a few seconds apart before trusting the board as empty (the
    // 422 PLACEMENT_UNSATISFIABLE / stray-row trap otherwise).
    let emptyStreak = 0;
    for (let attempt = 0; attempt < 90; attempt += 1) {
      const body = (await (await fetch("/v1/instances?includeHistory=true", { credentials: "include" })).json()) as {
        items?: { id?: string; instanceId?: string }[];
      };
      // The Hub wire field is `instanceId`; `id` is kept only defensively.
      const ids = (body.items ?? [])
        .map((item) => item.instanceId ?? item.id)
        .filter((id): id is string => Boolean(id));
      if (!ids.length) {
        emptyStreak += 1;
        if (emptyStreak >= 3) return log;
        await sleep(2_000);
        continue;
      }
      emptyStreak = 0;
      log.push(`attempt ${attempt}: ${ids.length} live`);
      for (const id of ids) {
        await fetch(`/v1/instances/${id}?force=1`, { method: "DELETE", credentials: "include" }).catch(() => {});
      }
      await sleep(1_000);
    }
    throw new Error("fake node instances never settled to a stable empty board after purge");
  });
  // eslint-disable-next-line no-console
  console.log(`ux-keys purge: ${trace.length ? trace.join("; ") : "board already empty"}`);
}

/** Fill the new-session form and start, returning the created instance id. */
async function createSession(page: Page, name: string, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  // Pin the directory explicitly: the fake node now registers several g2
  // workspaces and a fresh context's first <option> is not necessarily e2e.
  await page.getByTestId("new-session-workspace").selectOption("wsp_e2e");

  await page.getByTestId("new-session-advanced").click();
  await page.locator("label").filter({ hasText: "name" }).locator("input").fill(name);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  const creating = page.waitForResponse(
    (response) => response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const response = await creating;
  // beforeAll swept the board; a 422 here means a force-deleted predecessor
  // is still settling its node stop under host contention. Wait it out and
  // retry the form once rather than flake the whole file.
  if (response.status() === 422) {
    await page.waitForTimeout(2_000);
    await page.goto("/sessions/new");
    await hostPicker.selectOption(hostId!);
    await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, { timeout: 20_000 });
    await page.getByTestId("new-session-advanced").click();
    await page.locator("label").filter({ hasText: "name" }).locator("input").fill(name);
    await page.getByTestId("new-session-prompt").fill(prompt);
    const retry = page.waitForResponse(
      (r) => r.request().method() === "POST" && new URL(r.url()).pathname === "/v1/instances",
    );
    await page.getByTestId("new-session-start").click();
    const retried = await retry;
    expect(retried.ok(), `instance create failed: ${retried.status()} ${await retried.text()}`).toBe(true);
    const retriedId = (await retried.json()).instance.instanceId as string;
    created.push(retriedId);
    await expect(page).toHaveURL(new RegExp(`/s/${retriedId}`), { timeout: 20_000 });
    return retriedId;
  }
  expect(response.ok(), `instance create failed: ${response.status()} ${await response.text()}`).toBe(true);
  const instanceId = (await response.json()).instance.instanceId as string;
  created.push(instanceId);
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  return instanceId;
}

async function rowBadges(page: Page): Promise<RowBadge[]> {
  return page.getByTestId("session-row").evaluateAll((rows) =>
    rows.map((row) => {
      const badge = row.querySelector<HTMLElement>("[data-held]");
      return {
        href: row.getAttribute("href"),
        badge: badge?.textContent?.trim() ?? null,
        held: badge?.getAttribute("data-held") ?? null,
        aria: row.getAttribute("aria-keyshortcuts"),
      };
    }),
  );
}

/**
 * Pin every test document to the `wsp_e2e` Space.
 *
 * The fake node registers several g2 workspaces and a new browser context
 * defaults to the alphabetically-first Space (a g2 directory), so without
 * this the list paints a different Space than the one the fixture sessions
 * were created in and `session-row` resolves to zero. The app reads
 * `remuda.spaces.v1` once at boot (same prefs shape SpacesPanel persists), so
 * the pin is installed as an init script and takes effect on every test's
 * first `page.goto`; QuickFind's spec resets the same key.
 */
async function pinE2eSpace(page: Page): Promise<void> {
  const spaceId = await page.evaluate(async () => {
    const hosts = (await (await fetch("/v1/hosts", { credentials: "include" })).json()) as {
      items?: { hostId?: string; label?: string }[];
    };
    const hostId = hosts.items?.find((host) => host.label === "e2e-fake-node")?.hostId;
    if (!hostId) throw new Error("fake node host missing");
    return JSON.stringify([hostId, "wsp_e2e"]);
  });
  await page.context().addInitScript((pinnedSpaceId) => {
    const prefs = {
      version: 1,
      collapsed: false,
      groupCollapsed: {},
      exitedOpen: {},
      names: {},
      order: [],
      selectedSpaceId: pinnedSpaceId,
      selectedTabs: {},
      closedTabs: {},
    };
    localStorage.setItem("remuda.spaces.v1", JSON.stringify(prefs));
  }, spaceId);
}

test.beforeAll(async ({ browser }) => {
  // Plenty of room: earlier files' force-deletes can lag under host
  // contention, and the fake node ships maxInstances 8. Original restored in
  // afterAll (ux-status.spec uses the same isolation technique).
  const page = await browser.newPage();
  await login(page);
  await purgeInstances(page);
  await patchMaxInstances(page, 32);
  await page.close();
});

test.afterAll(async ({ browser }) => {
  const page = await browser.newPage();
  await login(page);
  for (const id of created.splice(0)) {
    await page.evaluate(async (instanceId) => {
      await fetch(`/v1/instances/${instanceId}?force=1`, { method: "DELETE", credentials: "include" }).catch(() => {});
    }, id);
  }
  await patchMaxInstances(page, 8);
  await page.close();
});

test.beforeEach(async ({ page }) => {
  await login(page);
  await pinE2eSpace(page);
});

test("hold reveals nine badges in tab order; digit opens the matching session; release hides them", async ({ page }, testInfo) => {
  testInfo.setTimeout(240_000);
  const names = ["KEY Alpha", "KEY Bravo", "KEY Charlie", "KEY Delta", "KEY Echo", "KEY Foxtrot", "KEY Golf", "KEY Hotel", "KEY India"];
  for (const name of names) {
    await createSession(page, name, `switcher fixture ${name}`);
  }

  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/sessions");
  await expect(page.getByTestId("session-list")).toBeVisible();
  // Exactly the nine fixtures: beforeAll swept earlier specs' orphans.
  await expect(page.getByTestId("session-row")).toHaveCount(9, { timeout: 30_000 });

  // The permanent hint names the gesture even before the modifier is touched.
  await expect(page.getByTestId("session-switch-hint")).toHaveText("按住 Ctrl 快捷切换");

  // Before the hold: nine badges exist (laid out, never reflowing) but are
  // hidden, and the rows carry their shortcuts permanently.
  let rows = await rowBadges(page);
  const numbered = rows.filter((row) => row.badge);
  expect(numbered).toHaveLength(9);
  // Paint order follows status groups, not tab order; compare the badge set
  // and resolve slots by digit text below.
  expect(numbered.map((row) => row.badge).sort((a, b) => a!.localeCompare(b!))).toEqual(names.map((_, i) => `Ctrl ${i + 1}`));
  for (const row of numbered) {
    expect(row.held).toBe("0");
    expect(row.aria).toBe(`Control+${row.badge!.split(" ")[1]}`);
    await expect(page.locator(`[data-testid="session-row"][href="${row.href}"] [data-held]`)).toBeHidden();
  }

  // Hold: all nine fade in (80 ms transition; allow settle).
  await page.keyboard.down("Control");
  for (const row of numbered) {
    await expect(page.locator(`[data-testid="session-row"][href="${row.href}"] [data-held]`)).toBeVisible();
  }
  rows = await rowBadges(page);
  expect(rows.filter((row) => row.held === "1")).toHaveLength(9);
  await page.waitForTimeout(150);

  // Evidence: held state at 1440 px, synthetic fake-node fixture.
  await mkdir(evidence, { recursive: true });
  await page.screenshot({ path: path.join(evidence, "workbench-keys-1-held-1440.png") });

  // Press 3: the handler must open the session whose badge says "Ctrl 3".
  const slot3 = numbered.find((row) => row.badge === "Ctrl 3")!;
  await page.keyboard.press("3");
  await page.keyboard.up("Control");
  await expect(page).toHaveURL(new RegExp(`/s/${slot3.href!.split("/")[2]}`));

  // Back on the list, releasing hid the badges again.
  await page.goto("/sessions");
  rows = await rowBadges(page);
  expect(rows.every((row) => row.held === "0")).toBe(true);
});

test("digit switching works on a session detail route, but never while the composer has focus", async ({ page }) => {
  await page.goto("/sessions");
  await expect(page.getByTestId("session-row")).toHaveCount(9, { timeout: 30_000 });
  const numbered = (await rowBadges(page)).filter((row) => row.badge);
  expect(numbered.length).toBeGreaterThanOrEqual(5);
  const slot2 = numbered.find((row) => row.badge === "Ctrl 2")!;
  const slot5 = numbered.find((row) => row.badge === "Ctrl 5")!;

  await page.goto(`/s/${slot5.href!.split("/")[2]}`);
  await expect(page.getByTestId("composer")).toBeVisible();

  // The fake node launches every session blocked on a synthetic Bash
  // approval, which disables the composer textarea. Answer it first (the
  // approval card lives above the composer on the detail page).
  await expect(page.getByTestId("approval-card")).toBeVisible();
  await page.getByRole("button", { name: "允许一次" }).click();
  await expect(page.getByTestId("approval-card")).toHaveCount(0);
  const composerInput = page.getByTestId("composer-input");
  await expect(composerInput).toBeEnabled();

  // Focus inside the composer: the chord belongs to the text field.
  await composerInput.focus();
  await expect(composerInput).toBeFocused();
  const urlBefore = page.url();
  await page.keyboard.down("Control");
  await page.keyboard.press("3");
  await page.keyboard.up("Control");
  expect(page.url()).toBe(urlBefore);

  // Once focus leaves typing surfaces, the same gesture works from the
  // detail route (list hidden, handler still live).
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await page.keyboard.down("Control");
  await page.keyboard.press("2");
  await page.keyboard.up("Control");
  await expect(page).toHaveURL(new RegExp(`/s/${slot2.href!.split("/")[2]}`));
});

test("does not collide with QuickFind: a digit pressed over the open finder stays in the finder", async ({ page }) => {
  await page.goto("/sessions");
  await expect(page.getByTestId("session-row")).toHaveCount(9, { timeout: 30_000 });
  await page.keyboard.press("Control+k");
  const panel = page.getByTestId("quickfind-panel");
  await expect(panel).toBeVisible();
  await expect(page.getByTestId("quickfind-input")).toBeFocused();

  const urlBefore = page.url();
  await page.keyboard.down("Control");
  await page.keyboard.press("3");
  await page.keyboard.up("Control");
  // No session switch; the finder still owns the keystroke and the panel.
  expect(page.url()).toBe(urlBefore);
  await expect(panel).toBeVisible();
  await expect(page.getByTestId("quickfind-input")).toBeFocused();
});

test("at 390 px there is no hint, no badge and no handler", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/sessions");
  await expect(page.getByTestId("session-list")).toBeVisible();

  await expect(page.getByTestId("session-switch-hint")).toHaveCount(0);
  await expect(page.locator("[data-held]")).toHaveCount(0);

  const urlBefore = page.url();
  await page.keyboard.down("Control");
  await page.keyboard.press("3");
  await page.keyboard.up("Control");
  expect(page.url()).toBe(urlBefore);
});

test("macOS UA renders the ⌘ glyph and Meta+ shortcuts", async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgentData", { configurable: true, value: { platform: "macOS" } });
    Object.defineProperty(navigator, "platform", { configurable: true, value: "MacIntel" });
  });
  await login(page);
  await page.goto("/sessions");
  await expect(page.getByTestId("session-row")).toHaveCount(9, { timeout: 30_000 });
  await expect(page.getByTestId("session-switch-hint")).toHaveText("按住 ⌘ 快捷切换");

  const numbered = (await rowBadges(page)).filter((row) => row.badge);
  const slot1 = numbered.find((row) => row.badge === "⌘ 1");
  expect(slot1).toBeTruthy();
  expect(slot1!.aria).toBe("Meta+1");

  await page.keyboard.down("Meta");
  await expect(page.locator(`[data-testid="session-row"][href="${slot1!.href}"] [data-held]`)).toBeVisible();
  await page.keyboard.up("Meta");
});
