import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * P1-1 acceptance (`docs/design/workbench-ux-exploration.md` §5):
 *
 * - ⌘/Ctrl+K opens QuickFind, arrow keys move the active row and Enter opens
 *   exactly the selected instance (React Router navigation, no reload);
 * - Escape closes it, returns focus to the trigger, and changes neither the
 *   active Space nor any URL filter;
 * - next to an attached terminal the finder never steals keystrokes: ⌘K does
 *   not open over xterm and the ESC byte reaches the fake-harness process;
 * - the sheet is usable at 390 px.
 *
 * Everything runs against the fake node in `crates/remuda-hub/examples/
 * hub_e2e.rs`, whose built-in TTY double echoes QUICKFIND_ESC_RECEIVED when an
 * ESC byte arrives. No real model, no real PTY.
 */

test.describe.configure({ mode: "serial" });

const created: string[] = [];

async function patchMaxInstances(page: Page, value: number): Promise<number | undefined> {
  return page.evaluate(async (next) => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    const body = (await list.json()) as { items?: { hostId?: string; maxInstances?: number }[] };
    const id = body.items?.[0]?.hostId;
    if (!id) return undefined;
    const previous = body.items?.[0]?.maxInstances;
    await fetch(`/v1/hosts/${id}`, {
      method: "PATCH",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ maxInstances: next }),
    });
    return previous;
  }, value);
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

  await page.getByTestId("new-session-advanced").click();
  const nameField = page.locator("label").filter({ hasText: "name" }).locator("input");
  await nameField.fill(name);

  await page.getByTestId("new-session-prompt").fill(prompt);
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  const creating = page.waitForResponse(
    (response) => response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const response = await creating;
  expect(response.ok(), `instance create failed: ${response.status()} ${await response.text()}`).toBe(true);
  const instanceId = (await response.json()).instance.instanceId as string;
  created.push(instanceId);
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  return instanceId;
}

async function createTerminal(page: Page): Promise<string> {
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-host")).toBeVisible({ timeout: 20_000 });
  await page.getByTestId("new-session-kind-terminal").click();
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  const creating = page.waitForResponse(
    (response) => response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const response = await creating;
  expect(response.ok()).toBe(true);
  const instanceId = (await response.json()).instance.instanceId as string;
  created.push(instanceId);
  await expect(page).toHaveURL(new RegExp(`/s/${instanceId}`), { timeout: 20_000 });
  // The fake node reports a shell-pty without a structured signal tier, so
  // the session opens on the conversation tab by default; this test exercises
  // the raw terminal projection explicitly.
  await page.goto(`/s/${instanceId}/tty`);
  return instanceId;
}

test.beforeAll(async ({ browser }) => {
  // The fake node advertises maxInstances 8 shared by every hub spec; the
  // finder suite needs a few slots (two named sessions + a terminal).
  const page = await browser.newPage();
  await login(page);
  await patchMaxInstances(page, 16);
  await page.close();
});

test.afterAll(async ({ browser }) => {
  const page = await browser.newPage();
  await login(page);
  await patchMaxInstances(page, 8);
  await page.close();
});

test.afterEach(async ({ page }) => {
  // force=1 is a u8; the Hub 400s on force=true. Sessions created here must
  // release their placement slots for the later hub specs.
  for (const id of created.splice(0)) {
    await page.evaluate(async (instanceId) => {
      await fetch(`/v1/instances/${instanceId}?force=1`, { method: "DELETE", credentials: "include" }).catch(() => {});
    }, id);
  }
});

async function focusTerminal(page: Page) {
  await page.locator(".xterm-helper-textarea").first().evaluate((el) => (el as HTMLTextAreaElement).focus());
  await expect.poll(() => page.evaluate(() => document.activeElement?.className ?? "")).toContain("xterm-helper-textarea");
}

test("shortcut opens QuickFind and arrows + Enter land on the selected instance", async ({ page }) => {
  await login(page);
  // Distinct names are the finder's titles; both share the "QF" prefix so one
  // query matches both and arrow keys have somewhere to move.
  const older = await createSession(page, "QF Zebra Target", "quickfind zebra prompt");
  const newer = await createSession(page, "QF Alpha Source", "quickfind alpha prompt");

  await page.goto("/sessions");
  await expect(page.getByTestId("session-list")).toBeVisible();

  // The entry point advertises the shortcut.
  await expect(page.getByTestId("quickfind-trigger")).toHaveAttribute("title", /⌘\/Ctrl\+K/);

  // Shortcut opens with focus in the field.
  await page.keyboard.press("Control+k");
  const panel = page.getByTestId("quickfind-panel");
  await expect(panel).toBeVisible();
  await expect(page.getByTestId("quickfind-input")).toBeFocused();

  // Both cached sessions are offered, newest first (recency tie-break).
  await page.getByTestId("quickfind-input").fill("QF");
  const rows = page.getByTestId("quickfind-result");
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(0)).toHaveAttribute("data-instance-id", newer);
  expect(await page.getByTestId("quickfind-input").getAttribute("aria-activedescendant")).toBe("quickfind-option-0");

  // ArrowDown moves the selection; Enter opens that exact instance.
  await page.keyboard.press("ArrowDown");
  expect(await page.getByTestId("quickfind-input").getAttribute("aria-activedescendant")).toBe("quickfind-option-1");
  await expect(rows.nth(1)).toHaveAttribute("data-selected", "true");
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(new RegExp(`/s/${older}`));
  await expect(panel).toHaveCount(0);
});

test("Escape restores focus and keeps the active Space and URL unchanged", async ({ page }) => {
  await login(page);
  await createSession(page, "QF Escape Keep", "quickfind escape prompt");
  await page.goto("/sessions");
  await expect(page.getByTestId("session-list")).toBeVisible();

  const activeSpace = page.getByTestId("space-select").and(page.locator('[aria-pressed="true"]')).first();
  const spaceIdBefore = await activeSpace.getAttribute("data-space-id");

  const trigger = page.getByTestId("quickfind-trigger");
  await trigger.click();
  await expect(page.getByTestId("quickfind-panel")).toBeVisible();
  await page.getByTestId("quickfind-input").fill("escape");
  await expect(page.getByTestId("quickfind-result")).not.toHaveCount(0);

  await page.keyboard.press("Escape");
  await expect(page.getByTestId("quickfind-panel")).toHaveCount(0);
  await expect(trigger).toBeFocused();
  expect(new URL(page.url()).pathname + new URL(page.url()).search).toBe("/sessions");

  const activeAfter = page.getByTestId("space-select").and(page.locator('[aria-pressed="true"]')).first();
  await expect(activeAfter).toHaveAttribute("data-space-id", spaceIdBefore ?? "");
});

test("does not intercept keystrokes inside an attached terminal", async ({ page }) => {
  await login(page);
  await createTerminal(page);
  const lab = page.locator("[data-tty-lab='1']");
  await expect(lab).toHaveAttribute("data-tty-status", "live", { timeout: 30_000 });
  await expect(page.getByTestId("tty-ansi-preview")).toContainText("fake-harness terminal");
  await focusTerminal(page);

  // ⌘K over xterm belongs to the native process; the finder must not open.
  await page.keyboard.press("Control+k");
  await expect(page.getByTestId("quickfind-panel")).toHaveCount(0);

  // And a plain Escape reaches the PTY. The fake-harness acknowledges the
  // byte with an on-screen marker; no panel may appear.
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("quickfind-panel")).toHaveCount(0);
  await expect(page.getByTestId("tty-ansi-preview")).toContainText("QUICKFIND_ESC_RECEIVED", { timeout: 15_000 });
  // Focus never left the terminal.
  await expect.poll(() => page.evaluate(() => document.activeElement?.className ?? "")).toContain("xterm-helper-textarea");
});

test("is usable at a 390 px viewport", async ({ page }) => {
  await login(page);
  const target = await createSession(page, "QF Phone Sheet", "quickfind phone prompt");
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/sessions");

  // On a phone the panel lives behind the Spaces drawer.
  await page.getByTestId("spaces-drawer-open").click();
  await expect(page.getByTestId("spaces-drawer")).toBeVisible();
  await page.getByTestId("quickfind-trigger").click();

  const panel = page.getByTestId("quickfind-panel");
  await expect(panel).toBeVisible();
  await expect(panel).toHaveAttribute("data-variant", "sheet");
  // Wait for the 140 ms rise animation to settle — getBoundingClientRect
  // includes the entry transform, so measuring mid-animation reads as
  // overflow. Half a pixel of tolerance covers subpixel rounding.
  await expect
    .poll(async () => {
      const box = await panel.evaluate((el) => {
        const rect = el.getBoundingClientRect();
        return { width: rect.width, bottom: rect.bottom };
      });
      return box.width <= 390.5 && box.bottom <= 844.5;
    })
    .toBe(true);

  await page.getByTestId("quickfind-input").fill("phone");
  const result = page.getByTestId("quickfind-result");
  await expect(result).toHaveCount(1);
  // 44 px touch target: the row is at least that tall at this width.
  expect((await result.boundingBox())!.height).toBeGreaterThanOrEqual(43);
  await result.click();
  await expect(page).toHaveURL(new RegExp(`/s/${target}`));
});
