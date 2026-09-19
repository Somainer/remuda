import { expect, test, type Page } from "@playwright/test";
import {
  BATCH_E_DENIED_CALL,
  BATCH_E_FAILED_CALL,
  BATCH_E_MARKER_FAR,
  BATCH_E_MARKER_MID,
  BATCH_E_TITLE,
} from "../../src/fixtures/session/batchE";

/**
 * Mock/Vite-backed batch of session-virtual.
 *
 * The Hub + fake-node invariants (search writes nothing, reading position
 * across real navigation, live-region silence during a streamed turn) live in
 * session-virtual.hub.spec.ts, picked up only by playwright.hub.config.ts.
 *
 * Workbench batch E — §5 P1-2 acceptance covered here:
 * - 2,000-event virtualization stays cheap;
 * - in-transcript search hits *loaded* nodes outside the virtual window;
 * - reading position and follow survive leaving/re-entering;
 * - failed/denied tools stay visible inline, never folded away;
 * - long TaskTrack prompts truncate with an expand affordance.
 */

const HUB_BATCH_E = "/s/ins_mock_batch_e";

function row(page: Page, text: string) {
  return page.getByTestId("session-row").filter({ hasText: text }).first();
}

async function openNamedSession(page: Page, title: string) {
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
    // The diagnostic meta row now lives inside the 运行详情 disclosure
    // (D-040); the 只读 suffix moves with it.
    await page.getByTestId("run-details-summary").click();
    await expect(page.getByTestId("session-meta")).toContainText("只读");
  });

  test("Compact/Full persists and collapse-all folds tools", async ({ page }, info) => {
    // Density lives inline on desktop; below the 640px compact fold it moves
    // into the header ⋯ sheet (D-040), so open the sheet before every lookup
    // — no viewport pin, so both chromium and mobile-webkit exercise it.
    const compact = info.project.name === "mobile-webkit";
    const densityToggle = () =>
      compact
        ? page.getByTestId("session-more-sheet").getByTestId("density-toggle")
        : page.getByTestId("density-toggle");
    const openDensityMenu = async () => {
      if (compact) {
        await page.getByTestId("session-more-open").click();
        await expect(page.getByTestId("session-more-sheet")).toBeVisible();
      }
    };
    const closeDensityMenu = async () => {
      if (compact) await page.keyboard.press("Escape");
    };

    await openNamedSession(page, "看 TaskManager spill");
    // D-041: a settled ordinary card is already folded by default on the
    // mobile-webkit (390px) project; the desktop project starts unfolded. The
    // point of this case is the explicit collapse-all that follows.
    await openDensityMenu();
    await expect(densityToggle()).toHaveAttribute("data-mode", "compact");
    await densityToggle().click();
    await closeDensityMenu();
    await page.reload();
    await openDensityMenu();
    await expect(densityToggle()).toHaveAttribute("data-mode", "full");
    await closeDensityMenu();
    await expect(page.getByTestId("tool-card").first()).toHaveAttribute(
      "data-folded",
      compact ? "1" : "0",
    );
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

test.describe("batch E: in-transcript search on the 2000-event fixture", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto(HUB_BATCH_E);
    await expect(page.getByTestId("transcript")).toBeVisible();
    await expect(page.getByTestId("transcript-row")).not.toHaveCount(0);
  });

  test("hits loaded nodes outside the virtual window and scrolls to them", async ({ page }) => {
    const scroller = page.getByTestId("transcript-scroller");
    const rows = page.getByTestId("transcript-row");
    expect(await rows.count()).toBeLessThan(80);

    await page.getByTestId("transcript-search-open").click();
    const input = page.getByTestId("transcript-search-input");
    await input.fill(BATCH_E_MARKER_FAR);
    await expect(page.getByTestId("transcript-search-count")).toHaveText("1/1");
    await input.press("Enter");

    // Event 7 sits ~1,992 rows above the initial (bottom-pinned) window;
    // geometry-only scrolling (no DOM query) lands at its top.
    expect(await scroller.evaluate((el) => el.scrollTop)).toBeLessThan(1_000);
    await expect(page.locator('[data-search-current="1"]').first()).toHaveAttribute(
      "data-anchor",
      "obj_batch_e_n_7",
    );

    // A second unique marker ~mid-list scrolls deep into the journal.
    await input.fill(BATCH_E_MARKER_MID);
    await expect(page.getByTestId("transcript-search-count")).toHaveText("1/1");
    await input.press("Enter");
    expect(await scroller.evaluate((el) => el.scrollTop)).toBeGreaterThan(30_000);
    await expect(page.locator('[data-search-current="1"]').first()).toHaveAttribute(
      "data-anchor",
      "obj_batch_e_n_1001",
    );
    expect(await rows.count()).toBeLessThan(80);
  });

  test("counts matches and moves prev/next within loaded scope", async ({ page }) => {
    await page.getByTestId("transcript-search-open").click();
    const input = page.getByTestId("transcript-search-input");
    await input.fill("reply");
    const count = page.getByTestId("transcript-search-count");
    // Typing auto-selects the first hit while streaming keeps identity stable.
    await expect.poll(async () => (await count.textContent())?.startsWith("1/")).toBeTruthy();
    const total = Number((await count.textContent())?.split("/")[1] ?? "0");
    expect(total).toBeGreaterThan(900);

    const scroller = page.getByTestId("transcript-scroller");
    const topA = await scroller.evaluate((el) => el.scrollTop);
    await page.getByTestId("transcript-search-next").click();
    await expect.poll(async () => (await count.textContent())?.startsWith("2/")).toBeTruthy();
    const topB = await scroller.evaluate((el) => el.scrollTop);
    expect(topB).not.toBe(topA);
    await page.getByTestId("transcript-search-prev").click();
    await expect.poll(async () => (await count.textContent())?.startsWith("1/")).toBeTruthy();

    // Escape closes the bar and returns the trigger to its collapsed state.
    await input.press("Escape");
    await expect(page.getByTestId("transcript-search-input")).toHaveCount(0);
    await expect(page.getByTestId("transcript-search-open")).toHaveAttribute("aria-expanded", "false");
  });

  test("searching performs no network writes", async ({ page }) => {
    const writes: string[] = [];
    page.on("request", (request) => {
      const method = request.method();
      if (method !== "GET" && method !== "OPTIONS") writes.push(`${method} ${new URL(request.url()).pathname}`);
    });
    await page.getByTestId("transcript-search-open").click();
    await page.getByTestId("transcript-search-input").fill("reply");
    await page.getByTestId("transcript-search-next").click();
    await page.getByTestId("transcript-search-prev").click();
    await page.getByTestId("transcript-search-close").click();
    // Search is a pure projection of assembled loaded nodes; the mock needs
    // no request at all, and the fake-node Hub spec proves the same invariant
    // against a real server.
    expect(writes).toEqual([]);
  });

  test("reading position and follow state restore after navigating away and back", async ({ page }) => {
    const scroller = page.getByTestId("transcript-scroller");
    await scroller.evaluate((el) => {
      el.scrollTop = 12_000;
      el.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    // Give the debounced save a moment in addition to unmount-time flush.
    await page.waitForTimeout(400);

    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    await page.goto(HUB_BATCH_E);
    await expect(page.getByTestId("transcript-row")).not.toHaveCount(0);
    const restored = await scroller.evaluate((el) => el.scrollTop);
    expect(Math.abs(restored - 12_000)).toBeLessThan(200);
    await expect(page.getByTestId("jump-latest")).toBeVisible();

    // Pin to the latest, leave, come back: follow is what restores.
    await scroller.evaluate((el) => {
      el.scrollTop = el.scrollHeight;
      el.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    await page.waitForTimeout(400);
    await page.goto("/sessions");
    await page.goto(HUB_BATCH_E);
    await expect(page.getByTestId("transcript-row")).not.toHaveCount(0);
    const atBottom = await scroller.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight < 48);
    expect(atBottom).toBe(true);
  });

  test("failed and denied tools stay visible inline, immune to collapse-all", async ({ page }) => {
    // The tail carries one failed and one denied tool; a following transcript
    // opens at the bottom where they live.
    const failures = page.getByTestId("tool-failure-tag");
    await expect(failures).toHaveCount(2);
    const outcomes = await page.getByTestId("tool-failure").evaluateAll((els) =>
      els.map((el) => el.getAttribute("data-tool-outcome")),
    );
    expect(outcomes.sort()).toEqual(["denied", "failed"]);

    await page.getByTestId("collapse-all").click();
    await expect(failures).toHaveCount(2);
    for (const card of await page.locator('[data-testid="tool-failure"] [data-testid="tool-card"]').all()) {
      await expect(card).toHaveAttribute("data-folded", "0");
    }
    // No fold swallowed either call.
    await expect(page.locator(`[data-anchor="${BATCH_E_FAILED_CALL}"]`)).toBeVisible();
    await expect(page.locator(`[data-anchor="${BATCH_E_DENIED_CALL}"]`)).toBeVisible();
  });

  test("truncates the long Task prompt with an expand affordance", async ({ page }) => {
    const item = page.getByTestId("task-track-item").first();
    await expect(item).toBeVisible();
    const text = item.getByTestId("task-prompt-text");
    const collapsed = (await text.textContent()) ?? "";
    expect(collapsed.endsWith("…")).toBe(true);
    const toggle = item.getByTestId("task-prompt-toggle");
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    await toggle.click();
    await expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(((await item.getByTestId("task-prompt-text").textContent()) ?? "").length).toBeGreaterThan(collapsed.length);
    await toggle.click();
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
  });

  test("the batch-e session is discoverable by its synthetic title", async ({ page }) => {
    // Sanity: the fixture is registered and listed, not just routable.
    await page.goto("/sessions");
    await expect(page.getByTestId("session-row").filter({ hasText: BATCH_E_TITLE })).toBeVisible();
  });
});
