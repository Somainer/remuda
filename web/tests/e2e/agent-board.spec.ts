import { expect, test } from "@playwright/test";

function row(page: import("@playwright/test").Page, text: string) {
  return page.getByTestId("board-card").filter({ hasText: text }).first();
}

test.describe("agent board", () => {
  test("shows kind badges, worktree, status triple, snippet, and DONE", async ({ page }) => {
    await page.goto("/sessions");
    const grok = row(page, "grok-canary");
    await expect(grok).toBeVisible();
    await expect(grok).toHaveAttribute("data-kind", "grok");
    await expect(grok.getByTestId("board-done")).toContainText("DONE");
    await expect(grok.getByTestId("board-snippet")).toContainText("DONE ");
    await expect(grok).toContainText("wt/x-acpwire/canary");
    await expect(grok).toContainText("ready");
    await expect(grok).toContainText("connected");
    await expect(row(page, "codex-worker")).toHaveAttribute("data-kind", "codex");
    await expect(row(page, "agy-board")).toHaveAttribute("data-kind", "agy");
    await expect(row(page, "claude-pty")).toHaveAttribute("data-kind", "claude");
  });

  test("quick send, keys, and stop", async ({ page }) => {
    await page.goto("/sessions");
    const codex = row(page, "codex-worker");
    await codex.getByTestId("board-prompt").fill("PAUSE");
    await codex.getByTestId("board-send").click();
    await expect(codex.getByTestId("board-snippet")).toContainText("PAUSE");
    await codex.getByTestId("board-key-enter").click();
    await expect(codex.getByTestId("board-snippet")).toContainText("^ENTER");
    await codex.getByTestId("board-key-esc").click();
    await expect(codex.getByTestId("board-snippet")).toContainText("^ESC");
    await codex.getByTestId("board-stop").click();
    await expect(codex).toHaveAttribute("data-status", "exited");
  });

  test("fleet toolbar broadcasts to selected instances", async ({ page }) => {
    await page.goto("/sessions");
    await row(page, "codex-worker").getByTestId("board-select").check();
    await row(page, "agy-board").getByTestId("board-select").check();
    await expect(page.getByTestId("board-fleet")).toContainText("2 已选");
    await page.getByTestId("board-broadcast").fill("hello fleet");
    await page.getByTestId("board-broadcast-send").click();
    await expect(row(page, "codex-worker").getByTestId("board-snippet")).toContainText("hello fleet");
    await expect(row(page, "agy-board").getByTestId("board-snippet")).toContainText("hello fleet");
  });
});
