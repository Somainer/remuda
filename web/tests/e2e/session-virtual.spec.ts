import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";
import {
  BATCH_E_DENIED_CALL,
  BATCH_E_FAILED_CALL,
  BATCH_E_MARKER_FAR,
  BATCH_E_MARKER_MID,
  BATCH_E_TITLE,
} from "../../src/fixtures/session/batchE";

/**
 * Workbench batch E — §5 P1-2 acceptance:
 *
 * - in-transcript search hits *loaded* nodes outside the virtual window and
 *   scrolls to them (2,000-event synthetic fixture, mock mode);
 * - reading position and follow state survive leaving/re-entering a session;
 * - tool failures stay visible inline, never hidden behind a fold;
 * - streaming transcript text never reaches the live region (bounded change
 *   count, hub fake-node mode);
 * - searching never writes to the journal or drives the native session.
 *
 * The 2,000-event geometry tests run against the Vite mock (the Hub fake node
 * appends events one at a time and cannot synthesize that journal); the
 * streaming/no-write/restore invariants run against the Hub + fake node. The
 * hub Playwright project tags itself with metadata.appMode; the mock project
 * leaves metadata unset.
 */

const HUB_BATCH_E = "/s/ins_mock_batch_e";
const hubMode = (info: { project: { metadata: Record<string, unknown> } }) => info.project.metadata.appMode === "hub";

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
  test.beforeEach(({ }, info) => {
    test.skip(hubMode(info), "mock-fixture cases run under the Vite mock config");
  });

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

test.describe("batch E: in-transcript search on the 2000-event fixture", () => {
  test.beforeEach(async ({ page }, info) => {
    test.skip(hubMode(info), "2000-event synthetic journal is a Vite-mock fixture");
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
    // no request at all, and the fake-node Hub test below proves the same
    // invariant against a real server.
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
    await page.goto("/sessions");
    await expect(page.getByTestId("session-row").filter({ hasText: BATCH_E_TITLE })).toBeVisible();
  });
});

// --- Hub fake-node subset ---------------------------------------------------

test.describe("batch E: Hub fake-node invariants", () => {
  /** Every mutating native-driving request; searching must add none. */
  const NATIVE_ACTION = /\/v1\/instances\/[^/]+\/(?:commands|input|interrupt)$|\/v1\/interactions\/[^/]+\/answer$/;
  const created: string[] = [];

  test.beforeEach(async ({ page }, info) => {
    test.skip(!hubMode(info), "fake-node cases run under test:e2e:hub");
    await login(page);
    await raiseCap(page, 24);
  });

  test.afterEach(async ({ page }, info) => {
    if (!hubMode(info)) return;
    for (const id of created.splice(0)) {
      await page.evaluate(async (instanceId) => {
        await fetch(`/v1/instances/${instanceId}?force=1`, { method: "DELETE", credentials: "include" }).catch(() => {});
      }, id);
    }
  });

  async function createSession(page: Page, prompt: string): Promise<string> {
    await page.goto("/sessions/new");
    const hostPicker = page.getByTestId("new-session-host");
    await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
    const hostId = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
    expect(hostId).toBeTruthy();
    await hostPicker.selectOption(hostId!);
    await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, { timeout: 20_000 });
    await page.getByTestId("new-session-prompt").fill(prompt);
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
    return instanceId;
  }

  async function answerPending(page: Page, instanceId: string): Promise<void> {
    await page.evaluate(async (id) => {
      const list = await fetch("/v1/interactions", { credentials: "include" });
      const body = (await list.json()) as {
        items?: {
          id: string;
          instanceId?: string;
          state?: string;
          request?: { inputDigest?: string; options?: { id: string }[] };
        }[];
      };
      for (const item of body.items ?? []) {
        if (item.instanceId !== id || item.state !== "pending") continue;
        const optionId = item.request?.options?.[0]?.id;
        if (!optionId) continue;
        await fetch(`/v1/interactions/${item.id}/answer`, {
          method: "POST",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" } }),
        });
      }
    }, instanceId);
  }

  async function readySession(page: Page, prompt: string): Promise<string> {
    const instanceId = await createSession(page, prompt);
    await answerPending(page, instanceId);
    await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 30_000 });
    return instanceId;
  }

  async function sendTurn(page: Page, prompt: string): Promise<void> {
    const composer = page.getByTestId("composer-input");
    await composer.fill(prompt);
    await composer.press("Enter");
    await expect(page.getByTestId("transcript")).toContainText(`echo: ${prompt}`, { timeout: 30_000 });
  }

  test("search is read-only: it never writes the journal or drives the native session", async ({ page }) => {
    const marker = "zulu-4173";
    await readySession(page, `seed ${marker}`);
    await sendTurn(page, `followup ${marker}`);

    const nativeWrites: string[] = [];
    page.on("request", (request) => {
      if (NATIVE_ACTION.test(new URL(request.url()).pathname) && request.method() !== "GET") {
        nativeWrites.push(request.method());
      }
    });

    await page.getByTestId("transcript-search-open").click();
    const input = page.getByTestId("transcript-search-input");
    await input.fill(marker);
    await expect(page.getByTestId("transcript-search-count")).not.toHaveText("0/0");
    await input.press("Enter");
    await expect(page.locator('[data-search-current="1"]').first()).toBeVisible();
    await page.getByTestId("transcript-search-prev").click();
    await page.getByTestId("transcript-search-next").click();
    await page.getByTestId("transcript-search-close").click();
    await page.waitForTimeout(500);
    expect(nativeWrites, `search issued native actions: ${nativeWrites.join(", ")}`).toEqual([]);
  });

  test("reading position and follow state survive leaving and re-entering", async ({ page }) => {
    const instanceId = await readySession(page, "batch-e restore seed");
    // Enough turns that the transcript actually scrolls in the viewport. The
    // fake node reports `working` between turns, so later sends may queue;
    // every send still appends and echoes regardless.
    for (let i = 0; i < 10; i += 1) await sendTurn(page, `restore filler turn ${i} xyz`);

    const scroller = page.getByTestId("transcript-scroller");
    await scroller.evaluate((el) => {
      el.scrollTop = 0;
      el.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    await expect(page.getByTestId("jump-latest")).toBeVisible();
    await page.waitForTimeout(300);

    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    await page.goto(`/s/${instanceId}`);
    await expect(page.getByTestId("transcript-row")).not.toHaveCount(0, { timeout: 15_000 });
    expect(await scroller.evaluate((el) => el.scrollTop)).toBeLessThan(160);

    // Re-pin and confirm follow itself is what restores.
    await scroller.evaluate((el) => {
      el.scrollTop = el.scrollHeight;
      el.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    await page.waitForTimeout(300);
    await page.goto("/sessions");
    await page.goto(`/s/${instanceId}`);
    await expect(page.getByTestId("transcript-row")).not.toHaveCount(0, { timeout: 15_000 });
    const atBottom = await scroller.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight < 64);
    expect(atBottom).toBe(true);
  });

  test("streaming transcript text produces a bounded number of live-region changes", async ({ page }) => {
    await readySession(page, "batch-e stream seed");
    const region = page.getByTestId("live-region");
    await expect(region).toHaveAttribute("aria-live", "polite");
    await expect(page.getByTestId("transcript")).toHaveAttribute("aria-live", "off");

    // Count polite-region DOM mutations across the streamed turn. The counter
    // lives on window because an evaluate() return value cannot keep a
    // MutationObserver alive.
    await page.evaluate(() => {
      const el = document.querySelector('[data-testid="live-region"]');
      const w = window as unknown as { __batchELiveChanges?: number };
      w.__batchELiveChanges = 0;
      if (el) {
        new MutationObserver(() => {
          w.__batchELiveChanges = (w.__batchELiveChanges ?? 0) + 1;
        }).observe(el, { childList: true, characterData: true, subtree: true });
      }
    });

    await sendTurn(page, "stream bounded-region-delta-9931");
    // Let any debounced announcement settle.
    await page.waitForTimeout(800);
    const count = await page.evaluate(() => (window as unknown as { __batchELiveChanges?: number }).__batchELiveChanges ?? 0);
    // The whole open/append/close chain streamed body text: the polite region
    // may carry unrelated app noise (announce + clear), but must not
    // machine-gun once per chunk.
    expect(count).toBeLessThanOrEqual(4);
    const announced = (await region.textContent()) ?? "";
    expect(announced).not.toContain("bounded-region-delta");
  });
});

/** maxInstances:8 is shared across every hub spec; bump it for this run. */
async function raiseCap(page: Page, to: number): Promise<void> {
  const hosts = await page.evaluate(async () => {
    const response = await fetch("/v1/hosts", { credentials: "include" });
    return response.json() as Promise<{
      items?: { hostId?: string; id?: string; label?: string; maxInstances?: number }[];
    }>;
  });
  const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
  if (!host) return;
  const hostId = (host.hostId ?? host.id) as string;
  if ((host.maxInstances ?? 8) >= to) return;
  await page.evaluate(
    async ({ id, value }) => {
      await fetch(`/v1/hosts/${id}`, {
        method: "PATCH",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ maxInstances: value }),
      });
    },
    { id: hostId, value: to },
  );
}
