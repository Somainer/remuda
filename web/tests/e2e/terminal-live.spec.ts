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
    // The sidebar is the Spaces panel; its session rows are plain links.
    // Clicking one is a client-side navigation, so TerminalView stays mounted
    // and only the `[instance.id]` effect re-runs — the bug's exact trigger.
    const switchTo = async (id: string) => {
      const row = page
        .locator(`[data-testid=spaces-panel] a[href="/s/${id}"], [data-testid=session-row][href^="/s/${id}"]`)
        .first();
      await expect(row).toBeVisible({ timeout: 20_000 });
      await row.click();
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

  test("replayed history does not answer terminal queries at the prompt", async ({ page }) => {
    // Count what the page actually writes to the PTY. Screen text alone cannot
    // tell "the query bytes are in the scrollback, as history" from "the
    // emulator just answered them again" — only an outbound channel-3 frame
    // proves the leak, and on the pre-fix tree this records the CPR reply
    // `ESC[2;1R` being typed into the shell.
    await page.addInitScript(() => {
      const w = window as unknown as { __ttySent?: string[] };
      w.__ttySent = [];
      const send = WebSocket.prototype.send;
      WebSocket.prototype.send = function (data: unknown) {
        if (data instanceof ArrayBuffer) {
          const view = new Uint8Array(data);
          // 32-byte binary header; channel 3 is TTY input.
          if (view[1] === 3) {
            w.__ttySent!.push(String.fromCharCode(...view.slice(32)));
          }
        }
        // eslint-disable-next-line prefer-rest-params
        return send.apply(this, arguments as never);
      };
    });
    await login(page);
    const id = await startTerminal(page);
    const lab = page.locator("[data-tty-lab='1']");
    await focusTerminal(page);

    // Put a query-heavy history into the PTY ring buffer, the way a TUI does on
    // startup: CPR, DA1, DA2, OSC 10/11 colours, DECRQM. Then leave the app, so
    // nothing is left to consume the answers.
    await page.keyboard.type(
      "printf '\\033[6n\\033[c\\033[>c\\033]10;?\\007\\033]11;?\\007\\033[?2026$p'",
    );
    await page.keyboard.press("Enter");
    await page.waitForTimeout(1200);
    await page.keyboard.press("Control+c");
    await page.waitForTimeout(300);

    // Clear the screen so anything appearing afterwards came from the replay.
    await page.keyboard.type("clear");
    await page.keyboard.press("Enter");
    await page.waitForTimeout(600);

    // Anything sent from here on is the emulator re-answering history.
    await page.evaluate(() => {
      (window as unknown as { __ttySent: string[] }).__ttySent = [];
    });

    // Force the hub to replay the ring buffer: reconnect, then a full re-attach
    // through a client-side navigation away and back.
    await page.evaluate(() => window.__ttyLab?.disconnect());
    await expect(lab).toHaveAttribute("data-tty-status", "live", { timeout: 30_000 });
    await page.waitForTimeout(1000);
    await page.goto(`/s/${id}/structured`);
    await page.waitForTimeout(600);
    await page.goto(`/s/${id}/tty`);
    await expect(lab).toHaveAttribute("data-tty-status", "live", { timeout: 30_000 });
    await page.waitForTimeout(1500);

    // Nothing at all should have gone to the PTY during the replay.
    const sent = await page.evaluate(() => (window as unknown as { __ttySent: string[] }).__ttySent);
    expect(sent.join("")).toBe("");

    // And the shell must not have been fed junk it could not run.
    const screen = await page.evaluate(() => {
      const term = document.querySelector("[data-tty-lab='1'] .xterm-screen");
      return term?.textContent ?? "";
    });
    expect(screen).not.toMatch(/command not found/);
    await page.screenshot({ path: path.join(dir, "terminal-3-replay-clean.png"), animations: "disabled" });
  });

  test("fullscreen fits the container without a refresh", async ({ page }) => {
    await login(page);
    await startTerminal(page);
    const lab = page.locator("[data-tty-lab='1']");
    await focusTerminal(page);

    // Start a full-screen TUI first, so the fullscreen transition happens with
    // an alt-screen app painting — the case that rendered wrong until reload.
    await page.keyboard.type("printf '\\033[?1049h'; top -l 0 2>/dev/null || vi");
    await page.keyboard.press("Enter");
    await page.waitForTimeout(1200);

    await page.getByTestId("tty-fullscreen").click();
    await expect(lab).toHaveAttribute("data-tty-fullscreen", "1");
    await page.waitForTimeout(1200);

    // The reported grid must match the painted one and fill the container.
    const geo = await page.evaluate(() => {
      const el = document.querySelector("[data-tty-lab='1']")!;
      const host = el.querySelector("[role='region']")!.firstElementChild as HTMLElement;
      const screen = host.querySelector<HTMLElement>(".xterm-screen")!;
      const style = window.getComputedStyle(host);
      const padY = (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0);
      const padX = (parseFloat(style.paddingLeft) || 0) + (parseFloat(style.paddingRight) || 0);
      return {
        rows: Number(el.getAttribute("data-tty-rows")),
        cols: Number(el.getAttribute("data-tty-cols")),
        renderedRows: host.querySelectorAll(".xterm-rows > div").length,
        screenH: screen.getBoundingClientRect().height,
        screenW: screen.getBoundingClientRect().width,
        boxH: host.clientHeight - padY,
        boxW: host.clientWidth - padX,
        viewportOverflow: (() => {
          const vp = el.querySelector<HTMLElement>("[role='region']")!;
          return vp.scrollHeight - vp.clientHeight;
        })(),
      };
    });

    // The grid must fill the fullscreen box, not sit in a short band inside it.
    expect(geo.screenH).toBeLessThanOrEqual(geo.boxH + 1);
    expect(geo.screenH).toBeGreaterThan(geo.boxH * 0.9);
    expect(geo.screenW).toBeLessThanOrEqual(geo.boxW + 1);
    expect(geo.viewportOverflow).toBe(0);
    // The reported geometry must be the painted geometry (the DOM renderer
    // reports rows; webgl paints to a canvas and reports none).
    if (geo.renderedRows > 0) expect(geo.renderedRows).toBe(geo.rows);
    // Sanity: a 1440x900 fullscreen window is far taller than the 27 rows the
    // bug produced.
    expect(geo.rows).toBeGreaterThan(30);
    await page.screenshot({ path: path.join(dir, "terminal-3-fullscreen.png"), animations: "disabled" });

    await page.getByTestId("tty-fullscreen").click();
    await expect(lab).toHaveAttribute("data-tty-fullscreen", "0");
    await page.keyboard.press("q");
    await page.keyboard.press("Control+c");
  });

  test("直连 hides the local input dock entirely", async ({ page }) => {
    await login(page);
    await startTerminal(page);
    const lab = page.locator("[data-tty-lab='1']");

    // 直连: no dock at all, and no reserved strip under the terminal.
    await expect(lab).toHaveAttribute("data-tty-io", "raw");
    await expect(page.getByTestId("tty-dock")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "发送" })).toHaveCount(0);
    await expect(page.getByLabel("本地输入")).toHaveCount(0);
    await page.screenshot({ path: path.join(dir, "terminal-3-direct-no-dock.png"), animations: "disabled" });

    // 本地输入: the real input comes back, enabled.
    await page.getByRole("button", { name: "本地输入" }).click();
    await expect(lab).toHaveAttribute("data-tty-io", "keys");
    const dock = page.getByTestId("tty-dock");
    await expect(dock).toBeVisible();
    const field = page.getByLabel("本地输入");
    await expect(field).toBeVisible();
    await expect(field).toBeEnabled();
    await page.screenshot({ path: path.join(dir, "terminal-3-keys-dock.png"), animations: "disabled" });

    await page.getByRole("button", { name: "直连" }).click();
    await expect(page.getByTestId("tty-dock")).toHaveCount(0);
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
