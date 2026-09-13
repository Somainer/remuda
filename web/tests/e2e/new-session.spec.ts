import { expect, test } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence");

test.describe("new session sheet", () => {
  test("mobile 30s path: focus prompt, type, start", async ({ page }) => {
    await page.goto("/sessions/new");
    const prompt = page.getByTestId("new-session-prompt");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await expect(prompt).toBeFocused();
    await expect(page.getByTestId("new-session-perm-bypassPermissions")).toBeVisible();
    await expect(page.getByTestId("new-session-delegation-none")).toBeVisible();
    await page.getByTestId("new-session-perm-bypassPermissions").click();
    await expect(page.getByTestId("new-session-yolo-hint")).toBeVisible();
    await prompt.fill("查 bolt TaskManager spill");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByTestId("session-page")).toBeVisible();
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "starting");
    await expect(page.getByTestId("session-page").getByTestId("message")).toContainText("查 bolt TaskManager spill");
  });

  test("kind terminal uses shell-pty and opens the terminal tab", async ({ page }) => {
    await page.goto("/sessions/new");
    await expect(page.getByTestId("new-session-sheet")).toBeVisible();
    await page.getByTestId("new-session-kind-terminal").click();
    await expect(page.getByTestId("new-session-terminal-driver")).toContainText("shell-pty");
    await expect(page.getByTestId("new-session-start")).toBeEnabled();
    await page.getByTestId("new-session-start").click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-driver", "shell-pty");
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
    await expect(page.locator("[data-tty-lab='1']")).toBeVisible();
    await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("view-switch")).toHaveAttribute("data-view", "tty");
    await expect(page.getByTestId("view-switch-tty")).toHaveAttribute("aria-checked", "true");
    await expect(page.getByTestId("view-switch-structured")).toHaveAttribute("aria-checked", "false");
    if (test.info().project.name === "chromium") {
      await page.screenshot({ path: path.join(evidence, "terminal-1-new-session.png"), animations: "disabled" });
    }
  });
});
