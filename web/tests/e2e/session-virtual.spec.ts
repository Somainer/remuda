import { expect, test } from "@playwright/test";

function row(page: import("@playwright/test").Page, text: string) {
  return page.getByTestId("session-row").filter({ hasText: text }).first();
}

async function openNamedSession(page: import("@playwright/test").Page, title: string) {
  await page.goto("/sessions");
  const listed = page.getByTestId("session-row").filter({ hasText: title }).first();
  if ((await listed.count()) > 0) {
    await listed.click();
    return;
  }
  await page.goto("/s/ins_mock_long");
  await expect(page.getByTestId("session-page")).toBeVisible();
  await row(page, title).click();
}

test.describe("transcript virtualization and session chrome", () => {
  test("2000-event fixture stays windowed and jump-to-latest is cheap", async ({ page }, info) => {
    test.skip(info.project.name === "mobile-webkit", "scroll perf case is desktop");
    await page.goto("/s/ins_mock_long");
    await expect(page.getByTestId("transcript")).toBeVisible();
    const scroller = page.getByTestId("transcript-scroller");
    await expect(scroller).toBeVisible();
    const rows = page.getByTestId("transcript-row");
    await expect.poll(async () => rows.count()).toBeGreaterThan(0);
    expect(await rows.count()).toBeLessThan(80);
    await scroller.evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect(page.getByTestId("jump-latest")).toBeVisible();
    const elapsed = await scroller.evaluate((el) => {
      const t0 = performance.now();
      const max = Math.max(0, el.scrollHeight - el.clientHeight);
      for (let y = 0; y <= max; y += 800) el.scrollTop = y;
      el.scrollTop = max;
      return performance.now() - t0;
    });
    expect(elapsed).toBeLessThan(1500);
    expect(await rows.count()).toBeLessThan(80);
    await scroller.evaluate((el) => {
      el.scrollTop = 0;
    });
    await page.getByTestId("jump-latest").click();
    const atBottom = await scroller.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight < 48);
    expect(atBottom).toBe(true);
  });

  test("gap mock shows 正在补事件 then settles", async ({ page }) => {
    await page.goto("/s/ins_mock_gap");
    await expect(page.getByTestId("journal-banner")).toHaveAttribute("data-state", "gap-backfill", { timeout: 8_000 });
    await expect(page.getByTestId("journal-banner")).toContainText("正在补事件");
    await expect(page.getByTestId("journal-banner")).toHaveCount(0, { timeout: 8_000 });
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-journal", "live");
  });

  test("stale mock stays readonly", async ({ page }) => {
    await page.goto("/s/ins_mock_stale");
    await expect(page.getByTestId("journal-banner")).toBeVisible({ timeout: 8_000 });
    await expect(page.getByTestId("journal-banner")).toHaveAttribute("data-state", "readonly-stale", { timeout: 8_000 });
    await expect(page.getByTestId("session-meta")).toContainText("只读");
  });

  test("Compact/Full persists and collapse-all folds tools", async ({ page }) => {
    await openNamedSession(page, "看 TaskManager spill");
    await expect(page.getByTestId("density-toggle")).toHaveAttribute("data-mode", "compact");
    await page.getByTestId("density-toggle").click();
    await expect(page.getByTestId("density-toggle")).toHaveAttribute("data-mode", "full");
    await page.reload();
    await expect(page.getByTestId("density-toggle")).toHaveAttribute("data-mode", "full");
    await expect(page.getByTestId("tool-card").first()).toHaveAttribute("data-folded", "0");
    await page.getByTestId("collapse-all").click();
    await expect(page.getByTestId("tool-card").first()).toHaveAttribute("data-folded", "1");
  });

  test("Cmd/Ctrl+Enter sends on desktop", async ({ page }, info) => {
    test.skip(info.project.name === "mobile-webkit", "Cmd+Enter is desktop");
    await openNamedSession(page, "空闲会话");
    const box = page.getByTestId("composer-input");
    await box.fill("from shortcut");
    await box.press("ControlOrMeta+Enter");
    await expect(page.getByText("from shortcut").first()).toBeVisible();
  });
});
