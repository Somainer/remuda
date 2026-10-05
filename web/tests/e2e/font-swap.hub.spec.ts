import { expect, test, type Locator, type Page, type Route } from "@playwright/test";
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
 * Where the swap lands relative to the restore is CONTROLLED AT THE NETWORK
 * LAYER, never by a timer: one arm holds every woff2 response and releases it
 * only after the restored anchor has stopped moving on fallback metrics (a
 * genuinely late swap); the other holds the woff2, lets rows paint on the
 * fallback face, and releases it the frame the still-running restore first
 * places its anchor (a swap landing mid-restore). The owner requirement
 * holds in both: the saved reading position survives.
 * Fake Node only: `__journal_burst__:<n>` appends n assistant rows in one
 * journal append. The long-journal case writes more than the Hub's tail
 * window first, so the restore runs on a bounded replay.
 */

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

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

/**
 * The request belongs to the `/s/<id>` document this gate is about: same
 * main frame, frame URL already on the session route. Requests of a document
 * the browser is leaving (the `/sessions` hop) are never parked, so a later
 * goto cannot cancel a parked request out from under the gate.
 */
function belongsToSessionDocument(route: Route): boolean {
  const frame = route.request().frame();
  if (frame !== frame.page().mainFrame()) return false;
  return new URL(frame.url()).pathname.startsWith("/s/");
}

/**
 * Intercept every request `pick` matches and hold it at the network layer
 * until `release()` — arrival ORDER is controlled by the test instead of
 * guessed from a delay. Requests arriving after the release continue at once.
 *
 * The gate only ever parks requests of the CURRENT `/s/<id>` document (see
 * belongsToSessionDocument): install it immediately before the session goto,
 * after the `/sessions` hop, or a request parked on the old document gets
 * cancelled by the goto and `continue()` would reject on the happy path.
 * Cancelled requests are tolerated anyway (a navigation abort races release).
 *
 * `waitArrival()` resolves once a request is actually parked in the gate, so
 * an arm can prove its precondition (the font has not loaded / the seed has
 * not returned) rather than assuming it from elapsed time.
 */
async function gateRoute(
  page: Page,
  pick: RegExp | ((url: URL) => boolean),
  opts: { revalidate?: boolean } = {},
): Promise<{ waitArrival: () => Promise<void>; release: () => void; dispose: () => Promise<void> }> {
  let markArrival: (() => void) | null = null;
  const arrival = new Promise<void>((resolve) => {
    markArrival = resolve;
  });
  let open: (() => void) | null = null;
  const released = new Promise<void>((resolve) => {
    open = resolve;
  });
  // A document whose warm disk cache already holds an immutable woff2 would
  // never re-request it: force revalidation (merged onto the request's own
  // headers — continue() replaces, not merges) so a later visit stays gated.
  const handler = async (route: Route) => {
    // Anything outside the gated session document (the /sessions page being
    // left, a subframe) flows straight through and never parks.
    if (!belongsToSessionDocument(route)) return route.continue();
    markArrival?.();
    markArrival = null;
    await released;
    // The goto that owned this request may have navigated away while it was
    // parked; continuing a disposed route rejects — that is expected, not a
    // test failure.
    try {
      await route.continue(
        opts.revalidate
          ? { headers: { ...route.request().headers(), "cache-control": "no-cache", pragma: "no-cache" } }
          : undefined,
      );
    } catch {
      // Request was aborted by navigation.
    }
  };
  await page.route(pick, handler);
  return {
    waitArrival: () => arrival,
    release: () => open?.(),
    dispose: () => page.unroute(pick, handler),
  };
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

/**
 * The restore has quiesced: the anchor row's scroller-relative offset has
 * stayed put across several ResizeObserver MEASUREMENT cycles, not merely two
 * animation frames (two rAFs can elapse with no size commit at all, so the
 * font could be released while the restore is still correcting). The
 * transcript's list element resizes whenever rows re-measure (mount heights,
 * the padTop estimate, the font swap); we observe it and require the offset
 * unchanged across consecutive cycles, with a quiet-period fallback and a
 * bounded wait. This is the synchronisation point for a font released
 * strictly around the restore — an event, not a guessed delay.
 */
async function waitAnchorStable(
  page: Page,
  scroller: Locator,
  anchor: number,
  { timeout = 15_000 }: { timeout?: number } = {},
): Promise<number> {
  const result = await scroller.evaluate(
    (el, { label, deadlineMs }) =>
      new Promise<string>((resolve) => {
        // scroller > .list (spacer + every rendered row): its border box grows
        // with each measured height and each padTop estimate commit.
        const list = el.firstElementChild;
        const offset = (): number | null => {
          const re = new RegExp(`journal_burst_* event ${label}\\b`);
          const row = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']")).find(
            (candidate) => re.test(candidate.textContent ?? ""),
          );
          if (!row) return null;
          return row.getBoundingClientRect().top - el.getBoundingClientRect().top;
        };
        const REQUIRED = 3;
        let last = offset();
        let stable = 0;
        let cycles = 0;
        let settled: string | null = null;
        const sample = (source: "ro" | "quiet") => {
          if (source === "ro") cycles += 1;
          // No settling on a timer before at least one real measurement cycle.
          if (source === "quiet" && cycles === 0) return;
          const next = offset();
          if (next === null) return;
          if (last !== null && Math.abs(next - last) <= 1) stable += 1;
          else stable = 0;
          last = next;
          if (stable >= REQUIRED) settled = `stable:${next}`;
        };
        const ro = new ResizeObserver(() => sample("ro"));
        if (list) ro.observe(list);
        // Also sample on a short timer: once size commits stop, the RO simply
        // goes quiet and that itself has to finish the wait.
        const quiet = window.setInterval(() => sample("quiet"), 100);
        const timer = window.setTimeout(() => {
          window.clearInterval(quiet);
          ro.disconnect();
          resolve(settled ?? (last === null ? "missing" : "moving"));
        }, deadlineMs);
        const check = window.setInterval(() => {
          if (settled === null) return;
          window.clearInterval(check);
          window.clearInterval(quiet);
          window.clearTimeout(timer);
          ro.disconnect();
          resolve(settled);
        }, 30);
      }),
    { label: anchor, deadlineMs: timeout },
  );
  expect(result, "the restored anchor never settled across measurement cycles").toMatch(/^stable:/);
  return Number(result.slice("stable:".length));
}

/**
 * The saved anchor has painted on the FALLBACK face for the first time. At
 * this frame the restore's initial estimate jump is on screen but its
 * measurement correction tail has not run: the font released right after this
 * genuinely races the restore, instead of waiting until it has settled.
 */
async function waitAnchorRendered(page: Page, anchor: number, { timeout = 15_000 }: {} = {}): Promise<number> {
  const handle = await page.waitForFunction(
    ({ label }) => {
      const el = document.querySelector<HTMLElement>("[data-testid='transcript-scroller']");
      if (!el) return null;
      const re = new RegExp(`journal_burst_* event ${label}\\b`);
      const row = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']")).find((candidate) =>
        re.test(candidate.textContent ?? ""),
      );
      if (!row) return null;
      return row.getBoundingClientRect().top - el.getBoundingClientRect().top;
    },
    { label: anchor },
    { polling: "raf", timeout },
  );
  return handle.jsonValue() as Promise<number>;
}

/**
 * Advance width of a long mixed monospace string at the transcript's 13px.
 * The web font and the system fallback differ in glyph advances even when a
 * particular short fenced block happens to occupy the same number of lines /
 * pixels of height, so this (not a <pre> height) proves the swap actually
 * changed the metrics the virtualised rows are measured from.
 */
async function monoAdvance(page: Page): Promise<number> {
  return page.evaluate(() => {
    const canvas = document.createElement("canvas");
    const ctx = canvas.getContext("2d");
    if (!ctx) return -1;
    ctx.font = '400 13px "IBM Plex Mono", monospace';
    return ctx.measureText(
      "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ!@#$%^&*()_+-=[]{}|;:',.<>/?~",
    ).width;
  });
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
 * twice: once with the font cached (the no-swap control) and once on a fresh
 * document where the swap's landing point is network-gated — strictly after
 * the restored anchor settles ("late"), or while that restore is still
 * running ("first" — rows have already painted on the fallback face).
 */
async function savedPositionSurvivesSwap(
  page: Page,
  longBurst: number,
  order: "late" | "first",
): Promise<void> {
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

  // The saved reading position is the restore's whole input. A visit
  // re-saves it (scroll persistence, unmount), so snapshot the record the
  // first visit left behind and reinstate it before each arm; an init script
  // records what the session document found at start, before the app read it.
  const readingKey = `runtime.reading.v1.${instanceId}`;
  const original = await page.evaluate((key) => localStorage.getItem(key), readingKey);
  expect(original, "the first visit saved a reading position").not.toBeNull();
  expect(JSON.parse(original!).follow, "the saved position is not pinned").toBe(false);
  await page.addInitScript((key) => {
    (window as unknown as { __readingInput?: string | null }).__readingInput = localStorage.getItem(key);
  }, readingKey);
  const reinstate = async () => {
    await page.evaluate(([key, value]) => localStorage.setItem(key, value), [readingKey, original!] as const);
  };
  const consumed = () =>
    page.evaluate(() => (window as unknown as { __readingInput?: string | null }).__readingInput ?? null);

  // Control: restore with the font already cached, so no swap happens.
  await reinstate();
  await page.goto(`/s/${instanceId}`);
  await expect.poll(() => rowOffset(scroller, anchor), { timeout: 30_000 }).not.toBeNull();
  await afterSwap(page);
  const control = (await rowOffset(scroller, anchor))!;
  const controlInput = await consumed();
  const viewport = await scroller.evaluate((el) => el.clientHeight);

  // The gated arm: a fresh document (routing also bypasses the HTTP cache)
  // where the swap is forced to land at one exact point relative to the
  // restore. Both arms restore from the byte-identical saved record.
  let leftByControl: string | null = null;
  let beforeSwap: number | null = null;
  let settled: number;
  if (order === "late") {
    // Hold every woff2 at the network layer; rows restore on the fallback
    // monospace. Release only once the restored anchor has stopped moving,
    // so the swap lands strictly after the restore by construction. The
    // gate is installed only now — AFTER the /sessions hop — and scoped to
    // this document, so the hop cannot cancel a parked request.
    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    leftByControl = await page.evaluate((key) => localStorage.getItem(key), readingKey);
    await reinstate();
    const fontGate = await gateRoute(page, /\.woff2(?:\?|$)/);
    await page.goto(`/s/${instanceId}`);
    await expect.poll(() => rowOffset(scroller, anchor), { timeout: 30_000 }).not.toBeNull();
    await fontGate.waitArrival();
    expect(await monoLoaded(page), "the held woff2 must not have swapped in yet").toBe(false);
    beforeSwap = await waitAnchorStable(page, scroller, anchor);
    fontGate.release();
    await afterSwap(page);
    settled = await waitAnchorStable(page, scroller, anchor);
  } else {
    // The swap lands WHILE the restore is still running (a genuinely early
    // swap, but after fallback paint — not before any row exists). Hold the
    // woff2 only: the journal loads normally, rows paint on the FALLBACK face,
    // and the saved-position restore runs on fallback metrics. Release on the
    // first frame the saved anchor is painted, before its ResizeObserver
    // measurement tail settles, so the swap races the restore rather than the
    // first paint. The gate is installed after the /sessions hop and scoped to
    // this document; the no-cache headers make it a real fetch even though the
    // control arm cached the woff2.
    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    await reinstate();
    const fontGate = await gateRoute(page, /\.woff2(?:\?|$)/, { revalidate: true });
    await page.goto(`/s/${instanceId}`);
    await fontGate.waitArrival();
    // The saved anchor paints on the fallback face; the font is still held, so
    // this is strictly before the swap (and before the restore's measurement
    // tail settles).
    beforeSwap = await waitAnchorRendered(page, anchor);
    expect(await monoLoaded(page), "rows must paint on the fallback face before the swap").toBe(false);
    const advanceFallback = await monoAdvance(page);
    fontGate.release();
    await afterSwap(page);
    settled = await waitAnchorStable(page, scroller, anchor);
    // The swap genuinely changed the face's metrics: glyph advances differ
    // while the anchor row stayed put.
    const advanceSwapped = await monoAdvance(page);
    expect
      .soft(
        advanceSwapped !== advanceFallback,
        `the font swap did not change monospace advances (fallback=${advanceFallback} swapped=${advanceSwapped})`,
      )
      .toBe(true);
  }
  const armInput = await consumed();

  // Both arms restore from the byte-identical record the first visit saved.
  expect(controlInput, "the control arm restored from the original saved record").toBe(original);
  expect(armInput, "the gated arm restored from the original saved record").toBe(original);

  // `saved` vs `control` is the restore's own precision with no font change
  // at all (Transcript places never-measured rows by its row estimate), a
  // baseline gap reported separately. What this spec owns is that the swap
  // adds nothing on top: the gated restore is no further from the saved
  // position than the no-swap restore, and the anchor does not move at the
  // moment the font lands (in BOTH arms — swap racing the restore, and swap
  // after the fallback restore settled).
  const measured = `order=${order} longBurst=${longBurst} saved=${saved} control=${control} beforeSwap=${beforeSwap} settled=${settled} input=${original} leftByControl=${leftByControl}`;
  test.info().annotations.push({ type: "font-swap", description: measured });
  console.log(`FONTSWAP ${measured}`);
  // The restore brings the saved row back on screen.
  expect(control, `the saved row restores inside the viewport (${measured})`).toBeGreaterThanOrEqual(0);
  expect(control, `the saved row restores inside the viewport (${measured})`).toBeLessThan(viewport);
  expect(
    Math.abs(settled - saved!),
    `the swap moved the restore further from the saved position than the no-swap control (${measured})`,
  ).toBeLessThanOrEqual(Math.abs(control - saved!) + DRIFT_PX);
  if (beforeSwap !== null) {
    expect(
      Math.abs(settled - beforeSwap),
      `row drifted when the web font swapped in (${measured})`,
    ).toBeLessThanOrEqual(DRIFT_PX);
  }
  await assertRowsStacked(scroller);
}

test("a saved reading position survives a late monospace swap", async ({ page }) => {
  test.setTimeout(120_000);
  await savedPositionSurvivesSwap(page, 0, "late");
});

test("a saved reading position survives a monospace swap landing mid-restore", async ({ page }) => {
  test.setTimeout(120_000);
  await savedPositionSurvivesSwap(page, 0, "first");
});

test("a saved position in a bounded long journal survives a late monospace swap", async ({ page }) => {
  test.setTimeout(240_000);
  await savedPositionSurvivesSwap(page, LONG_BURST, "late");
});

test("a pinned transcript stays pinned through a late monospace swap", async ({ page }) => {
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 390, height: 844 });
  const instanceId = await seedSession(page);
  const scroller = page.getByTestId("transcript-scroller");
  const bottomGap = () => scroller.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
  const assertPinned = (when: string) =>
    expect.poll(bottomGap, { timeout: 10_000, message: `${when}: pinned without any manual scroll` }).toBeLessThan(64);

  // First visit, fresh document: a CLOSED woff2 gate installed before the
  // goto. Rows render on the fallback face; the transcript must be pinned on
  // its own (pin re-pins on every size commit) — no manual scroll anywhere.
  const firstGate = await gateRoute(page, /\.woff2(?:\?|$)/);
  await page.goto(`/s/${instanceId}`);
  await loaded(page);
  await firstGate.waitArrival();
  expect(await monoLoaded(page), "the held woff2 must not have swapped in yet").toBe(false);
  await assertPinned("before the first swap");
  await expect(page.getByTestId("jump-latest"), "pinned transcript shows no jump-latest chip").not.toBeVisible();
  firstGate.release();
  await afterSwap(page);
  await assertPinned("after the first swap");
  await expect(page.getByTestId("jump-latest")).not.toBeVisible();
  await firstGate.dispose();

  // Leave and come back. The final visit gets its OWN fresh closed gate (the
  // first document cached the woff2, so force revalidation): verify the font
  // is genuinely unloaded AND the transcript is already pinned on fallback
  // before release, then verify it stays pinned after the swap — again with
  // no manual scroll to hide a failure.
  await page.goto("/sessions");
  await expect(page.getByTestId("session-list")).toBeVisible();
  const finalGate = await gateRoute(page, /\.woff2(?:\?|$)/, { revalidate: true });
  await page.goto(`/s/${instanceId}`);
  await expect(page.getByTestId("transcript-row")).not.toHaveCount(0, { timeout: 15_000 });
  await finalGate.waitArrival();
  expect(await monoLoaded(page), "the final visit must start with the font unloaded").toBe(false);
  await assertPinned("the final visit before the swap");
  await expect(page.getByTestId("jump-latest")).not.toBeVisible();
  finalGate.release();
  await afterSwap(page);
  await assertPinned("the final visit after the swap");
  await expect(page.getByTestId("jump-latest")).not.toBeVisible();
  await assertRowsStacked(scroller);
});
