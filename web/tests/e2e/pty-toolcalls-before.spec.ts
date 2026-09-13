import { expect, test } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Before-state companion capture for docs/design/evidence/pty-toolcalls-1.md:
 * the 结构 view of a claude-pty turn whose transcript was never tailed.
 */
const instanceId = process.env.PTY_TOOLCALLS_INSTANCE;

test.skip(!instanceId, "set PTY_TOOLCALLS_INSTANCE to capture the pty before-state");

test("claude-pty structured view without transcript hydration", async ({ page }) => {
  await login(page, "pty-toolcalls-before");
  await page.goto(`/s/${instanceId}`);
  await page.getByRole("radio", { name: "结构" }).click();
  await expect(page.getByTestId("tool-card")).toHaveCount(0);
  await page.setViewportSize({ width: 1440, height: 1200 });
  await page.screenshot({
    path: process.env.PTY_TOOLCALLS_SHOT ?? "/tmp/pty-toolcalls-1-before.png",
    fullPage: true,
  });
});
