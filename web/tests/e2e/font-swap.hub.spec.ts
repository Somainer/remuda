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
 * assistant rows in one journal append. The long-journal case writes more
 * than the Hub's tail window first, so the restore runs on a bounded replay.
 */

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

/** Hold on every woff2 response; FONT_SWAP_DELAY_MS overrides it. */
const FONT_DELAY_MS = Number(process.env.FONT_SWAP_DELAY_MS ?? 800);
/** Sub-pixel rounding plus one line of scroll-anchoring slack. */
const DRIFT_PX = 4;
/** Enough sans rows below the code block to park it near the top and scroll. */
const BURST = 24;
/** Longer than the Hub's 2000-row tail window, so the tab replays a bounded tail. */
const LONG_BURST = 2400;

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
async function seedSession(page: Page, longBurst = 0): Promise<string> {
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
  if (longBurst > 0) {
    // A journal longer than the Hub's tail window (2000 rows): the tab only
    // replays the bounded tail, so the rows above the anchor are a window
    // floor, not the start of the session.
    await command(page, instanceId, `__journal_burst__:${longBurst}`);
    await expect
      .poll(async () => Number((await journalTail(page, instanceId)).durableSeq ?? 0), { timeout: 60_000 })
      .toBeGreaterThanOrEqual(longBurst);
  }
  await command(page, instanceId, "font swap probe: show me code");
  // The reply must be journaled before the burst lands below it.
  await expect
    .poll(async () => textsAfterLastCode(await journalTail(page, instanceId)), { timeout: 30_000 })
    .not.toBeNull();
  await command(page, instanceId, `__journal_burst__:${BURST}`);
  // Burst labels count across the whole fake node, so wait on the journal.
  await expect
    .poll(
      async () =>
        (textsAfterLastCode(await journalTail(page, instanceId)) ?? []).filter((text) => text.includes("__journal_burst__"))
          .length,
      { timeout: 30_000 },
    )
    .toBeGreaterThanOrEqual(BURST);
  if (longBurst > 0) {
    const tail = await journalTail(page, instanceId);
    expect(Number(tail.fromSeq), "the journal is longer than one tail window").toBeGreaterThan(1);
    expect(tail.reachedAfterSeq, "the tail window is partial").toBe(false);
  }
  return instanceId;
}

type JournalTail = {
  durableSeq?: string | number;
  fromSeq?: string | number | null;
  reachedAfterSeq?: boolean;
  events?: { event?: { payload?: { text?: unknown } } }[];
};

/** The Hub's bounded tail window of the journal. */
async function journalTail(page: Page, instanceId: string): Promise<JournalTail> {
  return page.evaluate(
    async (id) => (await (await fetch(`/v1/instances/${id}/journal`, { credentials: "include" })).json()) as JournalTail,
    instanceId,
  );
}

/** Texts after the newest fenced-code reply, or null while there is none. */
function textsAfterLastCode(tail: JournalTail): string[] | null {
  const texts = (tail.events ?? []).map((event) =>
    typeof event.event?.payload?.text === "string" ? event.event.payload.text : "",
  );
  const last = texts.findLastIndex((text) => text.includes("```ts"));
  return last < 0 ? null : texts.slice(last + 1);
}

/**
 * The tab is live and shows the newest burst row. A long journal lands in
 * stages (the bounded tail, then the rest), so any burst row is not enough.
 */
async function loaded(page: Page, instanceId?: string): Promise<void> {
  await expect(page.getByTestId("transcript")).toContainText(/journal_burst_* event \d+/, { timeout: 30_000 });
  if (!instanceId) return;
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-journal", "live", { timeout: 30_000 });
  const newest = Math.max(
    ...(textsAfterLastCode(await journalTail(page, instanceId)) ?? []).map((text) =>
      Number(/__journal_burst__ event (\d+)/.exec(text)?.[1] ?? 0),
    ),
  );
  await expect(page.getByTestId("transcript")).toContainText(`journal_burst event ${newest}`, { timeout: 30_000 });
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

/**
 * Save a reading position just below the code block, leave, then restore it
 * twice: once with the font cached (the no-swap control) and once with the
 * woff2 held so the swap lands after the restore.
 */
async function savedPositionSurvivesSwap(page: Page, longBurst: number): Promise<void> {
  await page.setViewportSize({ width: 1440, height: 900 });
  const instanceId = await seedSession(page, longBurst);

  await page.goto(`/s/${instanceId}`);
  const scroller = page.getByTestId("transcript-scroller");
  await loaded(page, instanceId);
  await afterSwap(page);

  // Reach the code block (virtualised out while pinned to the bottom). A
  // short journal walks up as a reader would. In a long window a pixel walk
  // jumps over unmeasured rows (a Transcript virtualiser issue outside this
  // spec) and can miss the block, so there the transcript's own search jumps
  // to it by row index.
  const code = scroller.locator("pre").first();
  if (longBurst > 0) {
    await page.getByTestId("transcript-search-open").click();
    const search = page.getByTestId("transcript-search-input");
    await search.fill("here is the function");
    await search.press("Enter");
    await expect(page.getByTestId("transcript-search-count")).toHaveText(/^1\//);
    await page.getByTestId("transcript-search-close").click();
    await expect(code).toBeAttached({ timeout: 10_000 });
  } else {
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
  }

  // Park the code block's tail near the top of the viewport and anchor on
  // the first burst row below it, then leave.
  const anchor = await scroller.evaluate((el) => {
    // The newest reply's fenced block (the last one rendered).
    const pre = Array.from(el.querySelectorAll("pre")).at(-1);
    if (!pre) throw new Error("code block not rendered");
    el.scrollTop += pre.getBoundingClientRect().bottom - el.getBoundingClientRect().top - 200;
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
    const below = pre.getBoundingClientRect().bottom - 1;
    const labels = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']"))
      .filter((row) => row.getBoundingClientRect().top >= below)
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
  await expect.poll(() => rowOffset(scroller, anchor), { timeout: 30_000 }).not.toBeNull();
  await afterSwap(page);
  const control = (await rowOffset(scroller, anchor))!;
  const viewport = await scroller.evaluate((el) => el.clientHeight);

  // Fresh document with the woff2 held (routing also bypasses the HTTP
  // cache): rows restore on the fallback monospace, the swap lands after.
  await delayFonts(page);
  await page.goto("/sessions");
  await expect(page.getByTestId("session-list")).toBeVisible();
  await page.goto(`/s/${instanceId}`);
  await expect.poll(() => rowOffset(scroller, anchor), { timeout: 30_000 }).not.toBeNull();
  const swappedBeforeRestore = await monoLoaded(page);
  const beforeSwap = (await rowOffset(scroller, anchor))!;
  await afterSwap(page);
  const settled = (await rowOffset(scroller, anchor))!;

  // `saved` vs `control` is the restore's own precision with no font change
  // at all (Transcript places never-measured rows by its row estimate), a
  // baseline gap reported separately. What this spec owns is that the swap
  // adds nothing on top: the swapped restore is no further from the saved
  // position than the no-swap restore, and nothing moves when the font lands.
  const measured = `longBurst=${longBurst} saved=${saved} control=${control} beforeSwap=${beforeSwap} settled=${settled} swappedBeforeRestore=${swappedBeforeRestore}`;
  test.info().annotations.push({ type: "font-swap", description: measured });
  console.log(`FONTSWAP ${measured}`);
  if (FONT_DELAY_MS > 0) {
    expect(swappedBeforeRestore, `the delayed font arrived before the restore (${measured})`).toBe(false);
  }
  // The restore brings the saved row back on screen.
  expect(control, `the saved row restores inside the viewport (${measured})`).toBeGreaterThanOrEqual(0);
  expect(control, `the saved row restores inside the viewport (${measured})`).toBeLessThan(viewport);
  expect(
    Math.abs(settled - saved!),
    `a late swap moved the restore further from the saved position than the no-swap control (${measured})`,
  ).toBeLessThanOrEqual(Math.abs(control - saved!) + DRIFT_PX);
  expect(Math.abs(settled - beforeSwap), `row drifted when the web font swapped in (${measured})`).toBeLessThanOrEqual(
    DRIFT_PX,
  );
  await assertRowsStacked(scroller);
}

test("a saved reading position survives a late monospace swap", async ({ page }) => {
  test.setTimeout(120_000);
  await savedPositionSurvivesSwap(page, 0);
});

test("a saved position in a bounded long journal survives a late monospace swap", async ({ page }) => {
  test.setTimeout(240_000);
  await savedPositionSurvivesSwap(page, LONG_BURST);
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
