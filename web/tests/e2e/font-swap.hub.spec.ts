import { expect, test, type Locator, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Late web-font swap (visual-system.md §2).
 *
 * IBM Plex Mono is the only bundled web font and loads with
 * `font-display: swap`: code, paths and IDs first paint in the system
 * monospace, then re-flow once the woff2 arrives. The transcript is
 * virtualised and restores a saved reading position, so a swap that lands
 * after the restore must not move the row the reader was on, and a pinned
 * transcript must stay pinned.
 *
 * Every woff2 response is held for 800ms, so the swap reliably lands after
 * the first rows render. Fake Node only: `__journal_burst__:<n>` appends n
 * assistant rows in one journal append.
 */

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

/** Hold on every woff2 response; FONT_SWAP_DELAY_MS overrides it. */
const FONT_DELAY_MS = Number(process.env.FONT_SWAP_DELAY_MS ?? 800);
/** Sub-pixel rounding plus one line of scroll-anchoring slack. */
const DRIFT_PX = 4;
/** Enough sans rows below the code block to park it near the top and scroll. */
const BURST = 24;

const created: string[] = [];

test.beforeEach(async ({ page }) => {
  await login(page);
});

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

async function delayFonts(page: Page): Promise<void> {
  if (FONT_DELAY_MS <= 0) return;
  await page.route(/\.woff2(?:\?|$)/, async (route) => {
    await new Promise((resolve) => setTimeout(resolve, FONT_DELAY_MS));
    await route.continue();
  });
}

async function command(page: Page, instanceId: string, prompt: string): Promise<void> {
  const ok = await page.evaluate(
    async ({ id, text }) => {
      const response = await fetch(`/v1/instances/${id}/commands`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ operation: "instance.send", payload: { prompt: text } }),
      });
      return response.ok;
    },
    { id: instanceId, text: prompt },
  );
  expect(ok).toBe(true);
}

/**
 * A session whose transcript mixes monospace (the fenced reply, the header's
 * path and IDs) with a run of sans rows below it.
 */
async function seedSession(page: Page): Promise<string> {
  const instanceId = await page.evaluate(async () => {
    const hosts = (await (await fetch("/v1/hosts", { credentials: "include" })).json()) as {
      items?: { hostId?: string; label?: string }[];
    };
    const hostId = hosts.items?.find((host) => host.label === "e2e-fake-node")?.hostId ?? hosts.items?.[0]?.hostId;
    const response = await fetch("/v1/instances", {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        hostId,
        workspaceId: "/tmp",
        kind: "claude",
        driver: "claude-print",
        prompt: "font swap session",
      }),
    });
    return ((await response.json()) as { instance: { instanceId: string } }).instance.instanceId;
  });
  created.push(instanceId);

  // The create approval blocks sends until answered.
  await expect
    .poll(
      () =>
        page.evaluate(async (id) => {
          const body = (await (await fetch("/v1/interactions", { credentials: "include" })).json()) as {
            items?: {
              id: string;
              instanceId?: string;
              state?: string;
              request?: { inputDigest?: string; options?: { id: string }[] };
            }[];
          };
          const pending = (body.items ?? []).find((item) => item.instanceId === id && item.state === "pending");
          const optionId = pending?.request?.options?.[0]?.id;
          if (!pending || !optionId) return false;
          await fetch(`/v1/interactions/${pending.id}/answer`, {
            method: "POST",
            credentials: "include",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({
              answer: { kind: "approval", optionId, inputDigest: pending.request?.inputDigest ?? "" },
            }),
          });
          return true;
        }, instanceId),
      { timeout: 15_000 },
    )
    .toBe(true);

  // The fake node answers "show me code" with a fenced ts block. Every row
  // above the anchor stays short: a tall row the virtualiser has never
  // measured is placed by its row estimate on restore, which would move the
  // anchor with or without a font swap.
  await command(page, instanceId, "font swap probe: show me code");
  // The reply must be journaled before the burst lands below it.
  await expect
    .poll(
      () =>
        page.evaluate(async (id) => {
          const body = (await (await fetch(`/v1/instances/${id}/journal`, { credentials: "include" })).json()) as {
            events?: { event?: { payload?: { text?: unknown } } }[];
          };
          return (body.events ?? []).some(
            (event) => typeof event.event?.payload?.text === "string" && event.event.payload.text.includes("```ts"),
          );
        }, instanceId),
      { timeout: 30_000 },
    )
    .toBe(true);
  await command(page, instanceId, `__journal_burst__:${BURST}`);
  // Burst labels count across the whole fake node, so wait on the journal.
  await expect
    .poll(
      () =>
        page.evaluate(async (id) => {
          const body = (await (await fetch(`/v1/instances/${id}/journal`, { credentials: "include" })).json()) as {
            events?: { event?: { payload?: { text?: unknown } } }[];
          };
          return (body.events ?? []).filter(
            (event) => typeof event.event?.payload?.text === "string" && event.event.payload.text.includes("__journal_burst__"),
          ).length;
        }, instanceId),
      { timeout: 30_000 },
    )
    .toBeGreaterThanOrEqual(BURST);
  return instanceId;
}

/** The newest burst row is on screen once the pinned transcript has loaded. */
async function loaded(page: Page): Promise<void> {
  await expect(page.getByTestId("transcript")).toContainText(/journal_burst_* event \d+/, { timeout: 30_000 });
}

/** Scroller-relative top of the row carrying burst event `n`. */
async function rowOffset(scroller: Locator, n: number): Promise<number | null> {
  return scroller.evaluate((el, label) => {
    const re = new RegExp(`journal_burst_* event ${label}\\b`);
    const row = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']")).find((candidate) =>
      re.test(candidate.textContent ?? ""),
    );
    if (!row) return null;
    return row.getBoundingClientRect().top - el.getBoundingClientRect().top;
  }, n);
}

/** Rendered rows never overlap once the new metrics are measured. */
async function assertRowsStacked(scroller: Locator): Promise<void> {
  const overlaps = await scroller.evaluate((el) => {
    const rows = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']"))
      .map((row) => row.getBoundingClientRect())
      .filter((rect) => rect.height > 0)
      .sort((a, b) => a.top - b.top);
    const bad: string[] = [];
    for (let i = 1; i < rows.length; i += 1) {
      if (rows[i]!.top < rows[i - 1]!.bottom - 1) bad.push(`${rows[i - 1]!.bottom}>${rows[i]!.top}`);
    }
    return bad;
  });
  expect(overlaps, "transcript rows overlap after the font swap").toEqual([]);
}

async function monoLoaded(page: Page): Promise<boolean> {
  return page.evaluate(() => document.fonts.check('400 13px "IBM Plex Mono"'));
}

async function afterSwap(page: Page): Promise<void> {
  await expect.poll(() => monoLoaded(page), { timeout: 10_000 }).toBe(true);
  // Let the virtualiser re-measure and any scroll anchoring settle.
  await page.evaluate(
    () => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve(null)))),
  );
  await page.waitForTimeout(300);
}

test("a saved reading position survives a late monospace swap", async ({ page }) => {
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 1440, height: 900 });
  const instanceId = await seedSession(page);

  await page.goto(`/s/${instanceId}`);
  const scroller = page.getByTestId("transcript-scroller");
  await loaded(page);
  await afterSwap(page);

  // Walk up to the code block (virtualised out while pinned to the bottom).
  const code = page.getByTestId("transcript").locator("pre").first();
  await expect
    .poll(
      async () => {
        if ((await code.count()) > 0) return true;
        await scroller.evaluate((el) => {
          el.scrollTop = Math.max(0, el.scrollTop - el.clientHeight * 0.8);
          el.dispatchEvent(new Event("scroll", { bubbles: true }));
        });
        return false;
      },
      { timeout: 30_000, intervals: [200] },
    )
    .toBe(true);

  // Park the code block's tail near the top of the viewport and anchor on
  // the first burst row rendered below it, then leave.
  const anchor = await scroller.evaluate((el) => {
    // The reply's fenced block (the last one rendered).
    const pre = Array.from(el.querySelectorAll("pre")).at(-1);
    if (!pre) throw new Error("code block not rendered");
    el.scrollTop += pre.getBoundingClientRect().bottom - el.getBoundingClientRect().top - 200;
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
    const labels = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']"))
      .map((row) => /journal_burst_* event (\d+)\b/.exec(row.textContent ?? "")?.[1])
      .filter((value): value is string => Boolean(value))
      .map(Number);
    return Math.min(...labels);
  });
  expect(Number.isFinite(anchor), "a burst row renders below the code block").toBe(true);
  await page.waitForTimeout(400);
  const saved = await rowOffset(scroller, anchor);
  expect(saved).not.toBeNull();
  // The anchor is on screen, so the restore is observable.
  expect(saved!).toBeGreaterThanOrEqual(0);
  expect(saved!).toBeLessThan(900);

  await page.goto("/sessions");
  await expect(page.getByTestId("session-list")).toBeVisible();

  // Control: restore with the font already cached, so no swap happens.
  await page.goto(`/s/${instanceId}`);
  await expect.poll(() => rowOffset(scroller, anchor), { timeout: 15_000 }).not.toBeNull();
  await afterSwap(page);
  const control = (await rowOffset(scroller, anchor))!;

  // Fresh document with the woff2 held (routing also bypasses the HTTP
  // cache): rows restore on the fallback monospace, the swap lands after.
  await delayFonts(page);
  await page.goto("/sessions");
  await expect(page.getByTestId("session-list")).toBeVisible();
  await page.goto(`/s/${instanceId}`);
  await expect.poll(() => rowOffset(scroller, anchor), { timeout: 15_000 }).not.toBeNull();
  const swappedBeforeRestore = await monoLoaded(page);
  const beforeSwap = (await rowOffset(scroller, anchor))!;
  await afterSwap(page);
  const settled = (await rowOffset(scroller, anchor))!;

  // `saved` vs `control` is the restore's own precision, independent of
  // fonts (Transcript sizes never-measured rows above the anchor by its row
  // estimate); it is recorded, not asserted here.
  const measured = `saved=${saved} control=${control} beforeSwap=${beforeSwap} settled=${settled} swappedBeforeRestore=${swappedBeforeRestore}`;
  test.info().annotations.push({ type: "font-swap", description: measured });
  if (FONT_DELAY_MS > 0) {
    expect(swappedBeforeRestore, `the delayed font arrived before the restore (${measured})`).toBe(false);
  }
  expect(Math.abs(settled - control), `a late swap changed where the row restores (${measured})`).toBeLessThanOrEqual(
    DRIFT_PX,
  );
  expect(Math.abs(settled - beforeSwap), `row drifted when the web font swapped in (${measured})`).toBeLessThanOrEqual(
    DRIFT_PX,
  );
  await assertRowsStacked(scroller);
});

test("a pinned transcript stays pinned through a late monospace swap", async ({ page }) => {
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 390, height: 844 });
  const instanceId = await seedSession(page);
  await delayFonts(page);

  await page.goto(`/s/${instanceId}`);
  const scroller = page.getByTestId("transcript-scroller");
  await loaded(page);
  await afterSwap(page);
  await scroller.evaluate((el) => {
    el.scrollTop = el.scrollHeight;
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
  });
  await page.waitForTimeout(300);

  await page.goto("/sessions");
  await page.goto(`/s/${instanceId}`);
  await expect(page.getByTestId("transcript-row")).not.toHaveCount(0, { timeout: 15_000 });
  await afterSwap(page);
  const gap = await scroller.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
  expect(gap, "pinned transcript lost the bottom after the swap").toBeLessThan(64);
  await expect(page.getByTestId("jump-latest")).not.toBeVisible();
  await assertRowsStacked(scroller);
});
