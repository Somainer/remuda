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
    await page.locator(".xterm textarea, .xterm-helper-textarea").first().click({ force: true });
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
