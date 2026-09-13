import { expect, test, type Page } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

const dir = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence");
const access = process.env.VITE_ACCESS_CODE ?? "";

async function login(page: Page) {
  await page.goto("/login");
  await expect(page.getByTestId("login-page")).toBeVisible();
  await page.getByTestId("login-tab-bootstrap").click();
  await page.getByTestId("login-device-name").fill("terminal-e2e");
  await page.getByTestId("login-bootstrap-token").fill(access);
  await page.getByTestId("login-submit").click();
  await expect(page).toHaveURL(/\/sessions/, { timeout: 20_000 });
}

async function startTerminal(page: Page): Promise<string> {
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  await expect(page.getByTestId("new-session-host")).toBeVisible({ timeout: 20_000 });
  await page.getByTestId("new-session-kind-terminal").click();
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", { timeout: 30_000 });
  return new URL(page.url()).pathname.split("/")[2];
}

/**
 * Focus the terminal for typing. xterm parks its 3x10px helper textarea at the
 * cursor, which Playwright often judges "outside of the viewport", so drive
 * focus directly instead of clicking it. A readOnly textarea still swallows the
 * keystrokes, so this keeps the stdin regression visible.
 */
async function focusTerminal(page: Page) {
  await page.locator(".xterm-helper-textarea").first().evaluate((el) => (el as HTMLTextAreaElement).focus());
  await expect
    .poll(() => page.evaluate(() => document.activeElement?.className ?? ""))
    .toContain("xterm-helper-textarea");
}

/** Scrollback slider offset; null when the terminal has no scrollback yet. */
async function sliderTop(page: Page): Promise<number | null> {
  return page.evaluate(() => {
    const slider = document.querySelector<HTMLElement>(".xterm-scrollable-element > .scrollbar.vertical > .slider");
    if (!slider) return null;
    const top = /top: ([\d.]+)px/.exec(slider.getAttribute("style") ?? "");
    return top ? Number(top[1]) : null;
  });
}

/** Fill the scrollback so a wheel gesture has somewhere to go. */
async function fillScrollback(page: Page) {
  await page.keyboard.type("seq 1 400");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("tty-ansi-preview")).toContainText("400", { timeout: 15_000 });
  await page.waitForTimeout(500);
}

async function enableTracking(page: Page) {
  await page.keyboard.type("printf '\\033[?1000h\\033[?1006h'; cat -v");
  await page.keyboard.press("Enter");
  await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-mouse", /x10|vt200|drag|any/, {
    timeout: 15_000,
  });
}

test.describe("live remote terminal", () => {
  test.describe.configure({ mode: "serial" });
  test.skip(!access, "VITE_ACCESS_CODE required against remuda dev");

  test("New Session kind terminal opens the terminal tab", async ({ page }) => {
    await login(page);
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await expect(page.getByTestId("new-session-host")).toBeVisible({ timeout: 20_000 });
    await page.getByTestId("new-session-kind-terminal").click();
    await expect(page.getByTestId("new-session-terminal-driver")).toContainText("shell-pty");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
    const session = page.getByTestId("session-page");
    await expect(session).toBeVisible();
    await expect(session).toHaveAttribute("data-view", "tty");
    const lab = page.locator("[data-tty-lab='1']");
    await expect(lab).toBeVisible();
    await expect(page.getByTestId("tty-mode-pill")).toBeVisible();
    await expect(page.getByTestId("tty-keybar")).toBeVisible();
    await expect(page.getByRole("link", { name: "结构" })).toBeVisible();
    await expect(lab).toHaveAttribute("data-tty-status", /live|connecting|reconnecting/, { timeout: 10_000 });
    await page.waitForTimeout(1500);
    // A4: applyFit measures `.host` (which carries the padding), so the outer
    // `.viewport` never overflows and the bottom row is not clipped.
    const overflow = await page.evaluate(() => {
      const viewport = document.querySelector<HTMLElement>("[data-tty-lab='1'] [role='region']");
      return viewport ? viewport.scrollHeight - viewport.clientHeight : null;
    });
    expect(overflow).toBe(0);
    await page.screenshot({ path: path.join(dir, "terminal-1-shell.png"), animations: "disabled" });
  });

  test("type echo and mouse click round-trip on a shell PTY", async ({ page }) => {
    await login(page);
    await page.goto("/sessions/new");
    await page.getByTestId("new-session-kind-terminal").click();
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
    const lab = page.locator("[data-tty-lab='1']");
    await expect(lab).toBeVisible();
    await expect(lab).toHaveAttribute("data-tty-status", "live", { timeout: 30_000 });
    await focusTerminal(page);
    await page.keyboard.type("echo TERMUI_ECHO");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("tty-ansi-preview")).toContainText("TERMUI_ECHO", { timeout: 15_000 });
    await page.screenshot({ path: path.join(dir, "terminal-1-echo.png"), animations: "disabled" });

    await page.keyboard.type("printf '\\033[?1000h\\033[?1006h'; cat");
    await page.keyboard.press("Enter");
    await page.waitForTimeout(600);
    const host = page.locator("[data-tty-lab='1'] .xterm");
    const box = await host.boundingBox();
    expect(box).toBeTruthy();
    await page.mouse.click(box!.x + Math.min(80, box!.width * 0.35), box!.y + Math.min(48, box!.height * 0.35));
    await expect(page.getByTestId("tty-raw-tail")).toContainText("[<", { timeout: 10_000 });
    await page.screenshot({ path: path.join(dir, "terminal-1-mouse.png"), animations: "disabled" });
    await page.keyboard.press("Control+c");
  });

  test("stdin, mouse and scroll survive a sidebar session switch (A→B→A)", async ({ page }) => {
    await login(page);

    const a = await startTerminal(page);
    const b = await startTerminal(page);
    expect(a).not.toEqual(b);

    const lab = page.locator("[data-tty-lab='1']");
    const switchTo = async (id: string) => {
      await page.locator(`[data-testid=session-row][href^="/s/${id}"]`).first().click();
      await expect(page).toHaveURL(new RegExp(`/s/${id}`), { timeout: 20_000 });
      await expect(lab).toHaveAttribute("data-tty-status", "live", { timeout: 30_000 });
      await page.waitForTimeout(800);
    };

    // B is current; go back to A through the sidebar without unmounting TerminalView.
    await switchTo(a);
    await switchTo(b);
    await switchTo(a);

    // The rebuilt Terminal must still take stdin: xterm gates keys AND mouse
    // AND wheel on disableStdin, so a stale `true` kills all three at once.
    await expect(lab).toHaveAttribute("data-tty-io", "raw");
    expect(await page.locator(".xterm-helper-textarea").first().evaluate((el) => (el as HTMLTextAreaElement).readOnly)).toBe(false);

    await focusTerminal(page);
    await page.keyboard.type("echo TERMUI_AFTER_SWITCH");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("tty-ansi-preview")).toContainText("TERMUI_AFTER_SWITCH", { timeout: 15_000 });

    // Mouse tracking app: wheel must produce an SGR report, not silence.
    await page.keyboard.type("printf '\\033[?1000h\\033[?1006h'; cat -v");
    await page.keyboard.press("Enter");
    await expect(lab).toHaveAttribute("data-tty-mouse", /x10|vt200|drag|any/, { timeout: 15_000 });
    const box = await page.locator("[data-tty-lab='1'] .xterm").boundingBox();
    expect(box).toBeTruthy();
    await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
    await page.mouse.wheel(0, -300);
    await expect(page.getByTestId("tty-raw-tail")).toContainText(/\[<6[45];/, { timeout: 10_000 });
    await page.mouse.click(box!.x + Math.min(80, box!.width * 0.35), box!.y + Math.min(48, box!.height * 0.35));
    await expect(page.getByTestId("tty-raw-tail")).toContainText("[<0;", { timeout: 10_000 });
    await page.screenshot({ path: path.join(dir, "terminal-2-after-switch.png"), animations: "disabled" });
    await page.keyboard.press("Control+c");
  });

  test("wheel scrolls locally without tracking and reports with it (A2)", async ({ page }) => {
    await login(page);
    await startTerminal(page);
    const lab = page.locator("[data-tty-lab='1']");
    await focusTerminal(page);
    await fillScrollback(page);

    const box = await page.locator("[data-tty-lab='1'] .xterm").boundingBox();
    expect(box).toBeTruthy();
    const centre = async () => page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);

    // (a) No tracking: the wheel scrolls the scrollback and sends nothing.
    await expect(lab).toHaveAttribute("data-tty-mouse", "none");
    const restingTail = (await page.getByTestId("tty-raw-tail").textContent()) ?? "";
    const before = await sliderTop(page);
    expect(before).not.toBeNull();
    await centre();
    await page.mouse.wheel(0, -400);
    await page.waitForTimeout(400);
    expect(await sliderTop(page)).toBeLessThan(before!);
    expect((await page.getByTestId("tty-raw-tail").textContent()) ?? "").toBe(restingTail);
    await page.keyboard.press("End");

    // (b) Tracking on: the same gesture reports SGR wheel buttons 64/65.
    await enableTracking(page);
    await centre();
    await page.mouse.wheel(0, -300);
    await expect(page.getByTestId("tty-raw-tail")).toContainText(/\[<6[45];/, { timeout: 10_000 });
    await expect(page.getByTestId("tty-mouse-reports")).toBeVisible();
    await page.screenshot({ path: path.join(dir, "terminal-2-tracking-on.png"), animations: "disabled" });

    // (c) 鼠标上报 off: the report is swallowed and the wheel scrolls locally again.
    await page.getByTestId("tty-mouse-reports").click();
    await expect(lab).toHaveAttribute("data-tty-mouse-reports", "0");
    const trackedTail = (await page.getByTestId("tty-raw-tail").textContent()) ?? "";
    const trackedSlider = await sliderTop(page);
    await centre();
    await page.mouse.wheel(0, -400);
    await page.waitForTimeout(400);
    expect((await page.getByTestId("tty-raw-tail").textContent()) ?? "").toBe(trackedTail);
    if (trackedSlider != null) expect(await sliderTop(page)).toBeLessThanOrEqual(trackedSlider);

    // (d) 重置终端模式 clears the emulator (and tells the remote to stop), so
    // tracking is gone and the toggle retires.
    await page.keyboard.press("Control+c");
    await page.waitForTimeout(400);
    await page.getByTestId("tty-mouse-reset").click();
    await expect(lab).toHaveAttribute("data-tty-mouse", "none", { timeout: 10_000 });
    await expect(page.getByTestId("tty-mouse-reports")).toHaveCount(0);
    await page.screenshot({ path: path.join(dir, "terminal-2-tracking-reset.png"), animations: "disabled" });

    // The wheel scrolls locally again now that nothing is tracking.
    const resetSlider = await sliderTop(page);
    if (resetSlider != null) {
      await centre();
      await page.mouse.wheel(0, -400);
      await page.waitForTimeout(400);
      expect(await sliderTop(page)).toBeLessThanOrEqual(resetSlider);
    }
  });

  test("narrow viewport keeps the mouse alive in keys mode (A3)", async ({ page }) => {
    await login(page);
    await startTerminal(page);
    const lab = page.locator("[data-tty-lab='1']");
    await focusTerminal(page);
    await enableTracking(page);

    // A narrow desktop window flips the workbench to its compact layout. It
    // must not take the pointer with it: only a coarse pointer loses direct
    // keyboard input, and even then mouse reports have to keep flowing.
    await page.setViewportSize({ width: 700, height: 900 });
    await page.waitForTimeout(600);
    await expect(lab).toHaveAttribute("data-tty-status", "live");

    const box = await page.locator("[data-tty-lab='1'] .xterm").boundingBox();
    expect(box).toBeTruthy();
    await page.mouse.click(box!.x + Math.min(70, box!.width * 0.3), box!.y + Math.min(44, box!.height * 0.3));
    await expect(page.getByTestId("tty-raw-tail")).toContainText("[<0;", { timeout: 10_000 });
    await page.screenshot({ path: path.join(dir, "terminal-2-narrow-mouse.png"), animations: "disabled" });
    await page.keyboard.press("Control+c");
    await page.setViewportSize({ width: 1280, height: 720 });
  });

  test("grok pty session defaults to the terminal tab", async ({ page }) => {
    await login(page);
    await page.goto("/sessions/new");
    const grok = page.getByTestId("new-session-kind-grok");
    if (await grok.isDisabled()) test.skip(true, "grok CLI not installed on this host");
    await grok.click();
    await page.getByTestId("new-session-prompt").fill("Reply with the single word PONG and wait.");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty", { timeout: 20_000 });
    const lab = page.locator("[data-tty-lab='1']");
    await expect(lab).toBeVisible();
    await page.waitForTimeout(1500);
    await page.screenshot({ path: path.join(dir, "terminal-1-grok.png"), animations: "disabled" });
  });
});
