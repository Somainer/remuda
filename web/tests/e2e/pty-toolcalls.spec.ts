import { expect, test } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Opt-in capture for docs/design/evidence/pty-toolcalls-1.md.
 *
 * Needs a live `remuda dev` with a finished claude-pty turn; point
 * PTY_TOOLCALLS_INSTANCE at it. Skipped by default so a normal run neither
 * needs a Hub nor rewrites the tracked screenshot.
 */
const instanceId = process.env.PTY_TOOLCALLS_INSTANCE;

test.skip(!instanceId, "set PTY_TOOLCALLS_INSTANCE to capture pty tool-call evidence");

test("claude-pty structured view shows hydrated tool cards", async ({ page }) => {
  await login(page, "pty-toolcalls");
  await page.goto(`/s/${instanceId}`);
  // A claude-pty session opens on the terminal; the hydrated conversation is
  // the point of this capture. A settled turn arrives compacted.
  await page.getByRole("radio", { name: "结构" }).click();
  const compact = page.getByTestId("compact-fold");
  await expect(compact.first()).toBeVisible({ timeout: 30_000 });
  await compact.first().click();
  await expect(page.getByTestId("tool-card").first()).toBeVisible({ timeout: 30_000 });
  await expect(page.getByTestId("tool-card")).toHaveCount(2);
  await page.setViewportSize({ width: 1440, height: 1200 });
  await page.screenshot({
    path: process.env.PTY_TOOLCALLS_SHOT ?? "/tmp/pty-toolcalls-1-after.png",
    fullPage: true,
  });
});
