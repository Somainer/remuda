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
  // A null frame means no owning document (worker/shared): never park.
  const frame = route.request().frame();
  if (!frame || frame !== frame.page().mainFrame()) return false;
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
  let disposed = false;
  const parked = new Set<Route>();
  const handler = async (route: Route) => {
    // Anything outside the gated session document (the /sessions page being
    // left, a subframe, a null frame) flows straight through and never parks.
    if (!belongsToSessionDocument(route)) {
      try {
        await route.continue();
      } catch {
        // cancelled by navigation
      }
      return;
    }
    markArrival?.();
    markArrival = null;
    parked.add(route);
    await released;
    // The goto / dispose that owned this request may have aborted it; that is
    // expected, not a test failure.
    try {
      await route.continue(
        opts.revalidate
          ? { headers: { ...route.request().headers(), "cache-control": "no-cache", pragma: "no-cache" } }
          : undefined,
      );
    } catch {
      // Request was aborted by navigation / disposal.
    } finally {
      parked.delete(route);
    }
  };
  await page.route(pick, handler);
  const release = () => open?.();
  const dispose = async () => {
    if (disposed) return;
    disposed = true;
    release();
    await page.unroute(pick, handler).catch(() => undefined);
    // Free anything still parked after unregister (an abandoned test or
    // navigation); idempotent and never throwing.
    await Promise.all(
      [...parked].map(async (route) => {
        try {
          await route.continue();
        } catch {
          // already disposed
        }
      }),
    );
  };
  return {
    waitArrival: () => arrival,
    release,
    dispose,
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
 * path and IDs) with a run of sans rows below it. `wrapProbe` asks the fake
 * node for a pre-wrap fenced block whose LONG lines cross a wrap boundary
 * between the fallback and final faces (the viewer soft-wrap pref is set).
 */
async function seedSession(
  page: Page,
  longBurst = 0,
  wrapProbe: boolean | "mobile" = false,
): Promise<string> {
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
  const wrapPrompt = wrapProbe === "mobile" ? "font wrap probe mobile: show me code" : wrapProbe ? "font wrap probe: show me code" : "font swap probe: show me code";
  await command(page, instanceId, wrapPrompt);
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
    const top = row.getBoundingClientRect().top - el.getBoundingClientRect().top;
    return top >= 0 && top <= el.clientHeight ? top : null;
  }, n);
}

/**
 * ABSOLUTE document top (scrollTop + scroller-relative top) of the anchor
 * row. Unlike rowOffset this is defined when the row is mounted but below the
 * fold (the wrap-probe block can push the saved anchor there), and it is the
 * coordinate the restore actually preserves: a font swap must not change it.
 * Returns null only when the row is not mounted at all.
 */
async function anchorDocTop(scroller: Locator, n: number): Promise<number | null> {
  return scroller.evaluate((el, label) => {
    const re = new RegExp(`journal_burst_* event ${label}\\b`);
    const row = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']")).find((candidate) =>
      re.test(candidate.textContent ?? ""),
    );
    if (!row) return null;
    return el.scrollTop + row.getBoundingClientRect().top - el.getBoundingClientRect().top;
  }, n);
}

/**
 * The restore has quiesced: the anchor row's scroller-relative offset has
 * stayed put across several ResizeObserver MEASUREMENT cycles (two rAFs can
 * elapse with no size commit at all). Any movement or disappearance of the
 * anchor INVALIDATES the run (the counter resets), and the final value is
 * re-validated one frame after the threshold is reached before resolving —
 * so a resize that lands right at the threshold can't be missed. All timers
 * and the observer are torn down together (resolve OR timeout), no leaks.
 */
async function waitAnchorStable(
  page: Page,
  scroller: Locator,
  anchor: number,
  { timeout = 15_000 }: { timeout?: number } = {},
): Promise<number> {
  const result = await scroller.evaluate(
    (el, { label, deadlineMs }) =>
      new Promise<string>((resolvePromise) => {
        const list = el.firstElementChild;
        const offset = (): number | null => {
          const re = new RegExp(`journal_burst_* event ${label}\\b`);
          const row = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']")).find(
            (candidate) => re.test(candidate.textContent ?? ""),
          );
          if (!row) return null;
          const top = row.getBoundingClientRect().top - el.getBoundingClientRect().top;
          // Only an ON-SCREEN row counts: a mounted but below-the-fold row
          // would otherwise report a "stable" offset the reader never sees.
          if (top < 0 || top > el.clientHeight) return null;
          return top;
        };
        const REQUIRED = 3;
        let last: number | null = offset();
        let stable = 0;
        let settledAt: number | null = null;
        let revalidating = false;
        const timers: number[] = [];
        const setTimer = (fn: TimerHandler, ms: number) => {
          const id = window.setTimeout(fn, ms);
          timers.push(id);
          return id;
        };
        let ro: ResizeObserver | null = null;
        const finish = (value: string) => {
          timers.forEach((id) => window.clearTimeout(id));
          ro?.disconnect();
          resolvePromise(value);
        };
        setTimer(() => {
          finish(settledAt !== null ? `stable:${settledAt}` : last === null ? "missing" : "moving");
        }, deadlineMs);
        // Movement/disappearance resets the streak; reaching the threshold
        // arms a ONE-frame revalidation rather than resolving immediately.
        const sample = () => {
          const next = offset();
          if (next === null) {
            stable = 0;
            last = null;
            return;
          }
          if (last !== null && Math.abs(next - last) <= 1) {
            if (revalidating) return;
            stable += 1;
            if (stable >= REQUIRED) {
              revalidating = true;
              setTimer(() => {
                // Re-read after another frame: any movement/removal here
                // invalidates and restarts the streak.
                const check = offset();
                if (check !== null && Math.abs(check - next) <= 1) {
                  settledAt = check;
                  finish(`stable:${check}`);
                } else {
                  revalidating = false;
                  stable = 0;
                }
              }, 32);
            }
          } else {
            stable = 0;
            revalidating = false;
          }
          last = next;
        };
        // Measurement-driven samples (row heights, padTop estimate, font swap
        // all resize the list element).
        ro = new ResizeObserver(() => sample());
        if (list) ro.observe(list);
        // Quiet fallback: once size commits stop the RO goes silent; poll so
        // a settled window with no further resize still completes.
        const tick = () => {
          if (!revalidating) sample();
          timers.push(window.setTimeout(tick, 100));
        };
        timers.push(window.setTimeout(tick, 100));
      }),
    { label: anchor, deadlineMs: timeout },
  );
  expect(result, "the restored anchor never settled across measurement cycles").toMatch(/^stable:/);
  return Number(result.slice("stable:".length));
}

/**
 * Deterministic mid-restore barrier: wait until the saved-position restore is
 * PROVABLY still pending (the scroller's test-only data-restore-active
 * attribute, live on every commit) AND the saved anchor row is already
 * painted on the fallback face. Releasing a held font at this point races the
 * restore's tail; the attribute (not a laid-out row, which the restore may
 * have settled inside a layout effect before paint) is the source of truth.
 */
async function waitRestoreActiveAndPainted(
  page: Page,
  scroller: Locator,
  anchor: number,
  { timeout = 15_000 }: { timeout?: number } = {},
): Promise<number> {
  const handle = await page.waitForFunction(
    ({ label }) => {
      const el = document.querySelector<HTMLElement>("[data-testid='transcript-scroller']");
      if (!el || el.getAttribute("data-restore-active") !== "1") return null;
      const re = new RegExp(`journal_burst_* event ${label}\\b`);
      const row = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']")).find((candidate) =>
        re.test(candidate.textContent ?? ""),
      );
      if (!row) return null;
      const top = row.getBoundingClientRect().top - el.getBoundingClientRect().top;
      return top >= 0 && top <= el.clientHeight ? top : null;
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

/**
 * Wait for the web font to actually be available, with a deadline and a
 * diagnostic that also fails when the document/page was abandoned (closed or
 * navigated off the session route) instead of hanging until Playwright's
 * generic timeout.
 */
async function waitFontLoaded(page: Page, scroller: Locator, when: string, timeout = 10_000): Promise<void> {
  await expect
    .poll(
      async () => {
        if (page.isClosed()) return "page-closed";
        const url = page.mainFrame()?.url() ?? "";
        if (!/\/s\//.test(url)) return `abandoned-document:${url}`;
        return (await monoLoaded(page)) ? "loaded" : "waiting";
      },
      {
        timeout,
        message: `${when}: IBM Plex Mono never became available (deadline ${timeout}ms; page closed or document abandoned?)`,
      },
    )
    .toBe("loaded");
  void scroller;
}

async function afterSwap(page: Page, scroller: Locator): Promise<void> {
  await waitFontLoaded(page, scroller, "afterSwap");
  // Let the virtualiser re-measure and any scroll anchoring settle.
  await page.evaluate(
    () => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve(null)))),
  );
  await page.waitForTimeout(300);
}

/**
 * Height of the mounted transcript ROW containing the wrap-probe fenced
 * block. A genuine font swap changes glyph advances; with pre-wrap long lines
 * that cross a wrap boundary this changes the block's (and therefore the
 * row's) height — the metric that actually exercises re-anchoring, unlike a
 * non-wrapping <pre> whose height is identical across faces. Returns null if
 * the block is not mounted in the current virtual window.
 */
async function wrapBlockRowHeight(scroller: Locator): Promise<number | null> {
  return scroller.evaluate(() => {
    const block = Array.from(document.querySelectorAll<HTMLElement>("[data-testid='code-block']")).find((b) =>
      (b.querySelector("[data-testid='code-code']")?.textContent?.trim().startsWith("mmmmmmmmmmmmmmmmmmmm") ?? false),
    );
    if (!block) return null;
    const row = block.closest<HTMLElement>("[data-testid='transcript-row']");
    return (row ?? block).getBoundingClientRect().height;
  });
}

/**
 * Park either the saved anchor (requireAnchor, default) or the wrap block
 * itself at a fixed viewport offset, wait for it (and, when anchoring, the
 * block) to mount, and read the block's transcript-row HEIGHT. The scroll is
 * a measurement-only disturbance.
 */
function isWrapBlock(b: HTMLElement): boolean {
  return b.querySelector("[data-testid='code-code']")?.textContent?.trim().startsWith("mmmmmmmmmmmmmmmmmmmm") ?? false;
}
async function parkBlockHeight(
  scroller: Locator,
  anchor: number,
  viewportTop: number,
  opts: { requireAnchor?: boolean } = {},
): Promise<number> {
  const needAnchor = opts.requireAnchor !== false;
  // Remember the natural scroll position; restore it after measuring so the
  // block-height probe never contaminates the anchor-position assertion.
  const savedScrollTop = await scroller.evaluate((el) => el.scrollTop);
  await scroller.evaluate(
    ({ target, label, needAnchor: need }) => {
      const el = document.querySelector<HTMLElement>("[data-testid='transcript-scroller']")!;
      const wrapBlock = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='code-block']")).find((b) =>
        b.querySelector("[data-testid='code-code']")?.textContent?.trim().startsWith("mmmmmmmmmmmmmmmmmmmm"),
      );
      const re = new RegExp(`journal_burst_* event ${label}\\b`);
      const anchorRow = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']")).find((c) =>
        re.test(c.textContent ?? ""),
      );
      const targetEl = need ? anchorRow : wrapBlock;
      if (targetEl) {
        el.scrollTop += targetEl.getBoundingClientRect().top - el.getBoundingClientRect().top - target;
        el.dispatchEvent(new Event("scroll", { bubbles: true }));
      }
    },
    { target: viewportTop, label: anchor, needAnchor },
  );
  await expect
    .poll(
      () =>
        scroller.evaluate((el, label) => {
          const block = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='code-block']")).find((b) =>
            b.querySelector("[data-testid='code-code']")?.textContent?.trim().startsWith("mmmmmmmmmmmmmmmmmmmm"),
          );
          if (!block) return null;
          if (label < 0) return "ready";
          const re = new RegExp(`journal_burst_* event ${label}\\b`);
          const anchorRow = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']")).find((c) =>
            re.test(c.textContent ?? ""),
          );
          return anchorRow ? "ready" : null;
        }, needAnchor ? anchor : -1),
      {
        timeout: 10_000,
        message: needAnchor
          ? "wrap block and anchor not mounted together at the parked position"
          : "wrap block not mounted at the parked position",
      },
    )
    .toBe("ready");
  const height = await scroller.evaluate(() => {
    const block = Array.from(document.querySelectorAll<HTMLElement>("[data-testid='code-block']")).find((b) =>
      b.querySelector("[data-testid='code-code']")?.textContent?.trim().startsWith("mmmmmmmmmmmmmmmmmmmm"),
    )!;
    const blockRow = block.closest<HTMLElement>("[data-testid='transcript-row']");
    return (blockRow ?? block).getBoundingClientRect().height;
  });
  // Restore the natural scroll position and let the virtualiser settle.
  await scroller.evaluate((top) => {
    const el = document.querySelector<HTMLElement>("[data-testid='transcript-scroller']")!;
    el.scrollTop = top;
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
  }, savedScrollTop);
  await new Promise((r) => setTimeout(r, 200));
  return height;
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
  // The height-changing wrap fixture is used for the short journals (its row
  // is parkable next to the anchor). The 2400-row bounded journal keeps the
  // compact original fixture: there the block can be virtualized away from
  // the restored anchor, and this test's job is bounded-tail restore
  // robustness; the real height-delta re-anchor proof is the two short arms.
  const wrapProbe = longBurst === 0;
  const instanceId = await seedSession(page, longBurst, wrapProbe);
  // Soft-wrap ON so the font swap changes the wrap-probe block's HEIGHT.
  await page.evaluate(() => localStorage.setItem("runtime.code-wrap", "1"));

  await page.goto(`/s/${instanceId}`);
  const scroller = page.getByTestId("transcript-scroller");
  await loaded(page, instanceId);
  await afterSwap(page, scroller);


  // Reach the code block (virtualised out while pinned to the bottom). A
  // short journal walks up as a reader would. In a long window a pixel walk
  // jumps over unmeasured rows (a Transcript virtualiser issue outside this
  // spec) and can miss the block, so there the transcript's own search jumps
  // to it by row index.
  const code = scroller.locator("pre").first();
  if (longBurst > 0) {
    // The code block sits mid-journal in the bounded replay; search jumps to it.
    await page.getByTestId("transcript-search-open").click();
    const search = page.getByTestId("transcript-search-input");
    await search.fill(wrapProbe ? "font_swap_probe" : "here is the function");
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

  // Park so the wrap-probe block's bottom sits near the top of the viewport
  const PARK_BOTTOM = 140;
  const anchor = await scroller.evaluate((el, parkBottom) => {
    const pre = Array.from(el.querySelectorAll("pre")).at(-1);
    if (!pre) throw new Error("code block not rendered");
    el.scrollTop += pre.getBoundingClientRect().bottom - el.getBoundingClientRect().top - parkBottom;
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
    const below = pre.getBoundingClientRect().bottom - 1;
    const labels = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']"))
      .filter((row) => row.getBoundingClientRect().top >= below)
      .map((row) => /journal_burst_* event (\d+)\b/.exec(row.textContent ?? "")?.[1])
      .filter((value): value is string => Boolean(value))
      .map(Number);
    return Math.min(...labels);
  }, PARK_BOTTOM);
  expect(Number.isFinite(anchor), "a burst row renders below the code block").toBe(true);
  await page.waitForTimeout(400);
  // Track the anchor by ABSOLUTE document top: it survives the restore even
  // when the tall wrap-probe block pushes the anchor below the fold.
  const saved = await anchorDocTop(scroller, anchor);
  expect(saved, "saved anchor mounted").not.toBeNull();

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
  await expect.poll(() => anchorDocTop(scroller, anchor), { timeout: 30_000 }).not.toBeNull();
  await afterSwap(page, scroller);
  const control = (await anchorDocTop(scroller, anchor))!;
  const controlInput = await consumed();
  const blockControl = wrapProbe ? await wrapBlockRowHeight(scroller) : null;

  // The gated arm: a fresh document (routing also bypasses the HTTP cache)
  // where the swap is forced to land at one exact point relative to the
  // restore. Both arms restore from the byte-identical saved record.
  let leftByControl: string | null = null;
  let beforeSwap: number | null = null;
  let settled: number;
  // Height of the transcript row carrying the wrap-probe block, on the
  // fallback face and after the swap. A meaningful delta PROVES the swap moved
  // real layout above the anchor (the thing re-anchoring has to correct).
  let blockFallback: number | null = null;
  let blockSwapped: number | null = null;
  // Minimum real height change: more than one 13px/1.6 line of slack.
  const BLOCK_HEIGHT_DELTA_PX = 20;
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
    try {
      await page.goto(`/s/${instanceId}`);
      await expect.poll(() => anchorDocTop(scroller, anchor), { timeout: 30_000 }).not.toBeNull();
      await fontGate.waitArrival();
      expect(await monoLoaded(page), "the held woff2 must not have swapped in yet").toBe(false);
      // Anchor ABSOLUTE document top at the restored fallback position,
      // recorded BEFORE any measurement scroll disturbs it.
      beforeSwap = (await anchorDocTop(scroller, anchor))!;
      // The 300-m block is mounted right above the on-screen anchor; read its
      // height with no measurement scroll.
      blockFallback = wrapProbe ? await wrapBlockRowHeight(scroller) : null;
      fontGate.release();
      await afterSwap(page, scroller);
      // The anchor's post-swap absolute document top; read before the swapped
      // block-height measurement.
      settled = (await anchorDocTop(scroller, anchor))!;
      blockSwapped = wrapProbe ? await wrapBlockRowHeight(scroller) : null;
    } finally {
      await fontGate.dispose();
    }
  } else {
    // The swap lands WHILE the restore is provably active. The woff2 is held
    // at the network layer, so rows paint on the FALLBACK face; the
    // ?restoreProbe hook keeps the restore PENDING while that font is
    // unavailable (instead of finalizing on the fallback settle). We release
    // the font only while data-restore-active="1" AND the anchor is already
    // painted, then the restore corrects against the final metrics.
    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    await reinstate();
    const fontGate = await gateRoute(page, /\.woff2(?:\?|$)/, { revalidate: true });
    try {
      await page.goto(`/s/${instanceId}?restoreProbe=1`);
      await fontGate.waitArrival();
      // The anchor mounts on the fallback face while the restore is still
      // armed (the ?restoreProbe hook keeps it pending until the font loads).
      await expect.poll(() => anchorDocTop(scroller, anchor), { timeout: 30_000 }).not.toBeNull();
      await expect
        .poll(() => scroller.getAttribute("data-restore-active"), { timeout: 5_000 })
        .toBe("1");
      expect(await monoLoaded(page), "rows must paint on the fallback face before the swap").toBe(false);
      const advanceFallback = await monoAdvance(page);
      // Anchor docTop before the font is released (restore still armed); the
      // 300-m block is compact enough to be mounted alongside the anchor, so
      // read its height with NO measurement scroll.
      beforeSwap = (await anchorDocTop(scroller, anchor))!;
      blockFallback = wrapProbe ? await wrapBlockRowHeight(scroller) : null;
      // Release the gate while the restore attribute is still live ("1",
      // asserted above): the held woff2 can now only begin applying at a turn
      // the restore is active — deterministic, no racy post-hoc recorder.
      fontGate.release();
      await waitFontLoaded(page, scroller, "mid arm release");
      await page.evaluate(
        () => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve(null)))),
      );
      settled = (await anchorDocTop(scroller, anchor))!;
      blockSwapped = wrapProbe ? await wrapBlockRowHeight(scroller) : null;
      const advanceSwapped = await monoAdvance(page);
      expect(advanceSwapped, "the held font never swapped to the final face").not.toBe(advanceFallback);
    } finally {
      await fontGate.dispose();
    }
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
  const measured = `order=${order} longBurst=${longBurst} saved=${saved} control=${control} beforeSwap=${beforeSwap} settled=${settled} blockControl=${blockControl} blockFallback=${blockFallback} blockSwapped=${blockSwapped} input=${original} leftByControl=${leftByControl}`;
  test.info().annotations.push({ type: "font-swap", description: measured });
  console.log(`FONTSWAP ${measured}`);
  // The restore recovers the saved ABSOLUTE document position (it may be
  // below the fold because the tall wrap-probe block precedes the anchor).
  expect(control, `the saved row did not restore (${measured})`).toBeGreaterThanOrEqual(0);
  // The swap must have moved REAL layout (wrap-probe fixture only — the
  // short journals): the block's transcript row changed height by at least a
  // full line between fallback and final faces. Without this, drift 0 would
  // prove nothing (a non-wrapping <pre> has identical height across faces).
  // The bounded long journal uses the compact fixture and asserts drift only;
  // the height-reanchor proof is the two short arms.
  if (wrapProbe) {
    expect(blockFallback, `no fallback block height sampled (${measured})`).not.toBeNull();
    expect(blockSwapped, `no swapped block height sampled (${measured})`).not.toBeNull();
    expect(
      Math.abs(blockSwapped! - blockFallback!),
      `the swap did not change the wrap-probe row height — drift would be unprovable (${measured})`,
    ).toBeGreaterThanOrEqual(BLOCK_HEIGHT_DELTA_PX);
  }
  // RE-ANCHOR / RESTORE INVARIANT. The saved reading position is a *view*
  // offset within the anchor row, not an absolute document coordinate, so the
  // restored absolute doc-top legitimately differs between faces while the
  // taller fallback block is present (beforeSwap). Once the final face is
  // applied, the swap must not degrade the restore: settled must converge to
  // exactly where a no-swap Plex visit places the row (control), within the
  // sub-pixel rounding slack — and it must be no worse than the no-swap
  // baseline relative to the saved record. Combined with the real
  // block-height delta above, this proves the layout actually moved and the
  // reader still lands on the correct position, rather than drift 0 hiding
  // behind a height-identical non-wrapping <pre>.
  expect(
    Math.abs(settled - control),
    `after the swap the restore did not converge to the no-swap Plex control position (${measured})`,
  ).toBeLessThanOrEqual(DRIFT_PX);
  expect(
    Math.abs(settled - saved!),
    `the swap moved the restore further from the saved position than the no-swap control (${measured})`,
  ).toBeLessThanOrEqual(Math.abs(control - saved!) + DRIFT_PX);
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
  // Wrap probe + soft wrap: the swap changes real row heights (measured on
  // the final revisit) at the 390px pane, so "stays pinned" is asserted
  // against an actual layout change, not an identical-height block.
  const instanceId = await seedSession(page, 0, "mobile");
  await page.evaluate(() => localStorage.setItem("runtime.code-wrap", "1"));
  const scroller = page.getByTestId("transcript-scroller");
  const bottomGap = () => scroller.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
  const assertPinned = (when: string) =>
    expect.poll(bottomGap, { timeout: 10_000, message: `${when}: pinned without any manual scroll` }).toBeLessThan(64);

  // First visit, fresh document: a CLOSED woff2 gate installed before the
  // goto. Rows render on the fallback face; the transcript must be pinned on
  // its own (pin re-pins on every size commit) — no manual scroll anywhere.
  const firstGate = await gateRoute(page, /\.woff2(?:\?|$)/);
  try {
    await page.goto(`/s/${instanceId}`);
    await loaded(page);
    await firstGate.waitArrival();
    expect(await monoLoaded(page), "the held woff2 must not have swapped in yet").toBe(false);
    await assertPinned("before the first swap");
    await expect(page.getByTestId("jump-latest"), "pinned transcript shows no jump-latest chip").not.toBeVisible();
    firstGate.release();
    await afterSwap(page, scroller);
    await assertPinned("after the first swap");
    await expect(page.getByTestId("jump-latest")).not.toBeVisible();
  } finally {
    await firstGate.dispose();
  }

  // Leave and come back. On this 390px viewport the router redirects
  // /sessions to the mobile home /m (ViewportGate), so leave there. The
  // final visit gets its OWN fresh closed gate (the first document cached the
  // woff2, so force revalidation): verify the font is genuinely unloaded AND
  // the transcript is already pinned on fallback before release, that the
  // swap changes real block height at 390px, and that it stays pinned — with
  // no manual scroll to hide a failure.
  await page.goto("/m");
  await expect(page.getByTestId("home-list")).toBeVisible();
  const finalGate = await gateRoute(page, /\.woff2(?:\?|$)/, { revalidate: true });
  try {
    await page.goto(`/s/${instanceId}`);
    await expect(page.getByTestId("transcript-row")).not.toHaveCount(0, { timeout: 15_000 });
    await finalGate.waitArrival();
    expect(await monoLoaded(page), "the final visit must start with the font unloaded").toBe(false);
    await assertPinned("the final visit before the swap");
    await expect(page.getByTestId("jump-latest")).not.toBeVisible();
  // In a PINNED transcript the pin effect re-scrolls to the bottom after
  // commits, so each attempt scrolls the block up and reads its height in one
  // poll tick (after the virtualiser has mounted from the previous tick).
  const scrollToBlockOnce = (): Promise<number | null> =>
    scroller.evaluate(() => {
      const el = document.querySelector<HTMLElement>("[data-testid='transcript-scroller']")!;
      const block = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='code-block']")).find((b) =>
        b.querySelector("[data-testid='code-code']")?.textContent?.trim().startsWith("mmmmmmmmmmmmmmmmmmmm"),
      );
      if (!block) {
        // Walk upward from the pinned bottom to mount the block.
        el.scrollTop = Math.max(0, el.scrollTop - el.clientHeight * 0.6);
        el.dispatchEvent(new Event("scroll", { bubbles: true }));
        return null;
      }
      el.scrollTop = block.offsetTop;
      el.dispatchEvent(new Event("scroll", { bubbles: true }));
      void el.offsetHeight;
      const mounted = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='code-block']")).find((b) =>
        b.querySelector("[data-testid='code-code']")?.textContent?.trim().startsWith("mmmmmmmmmmmmmmmmmmmm"),
      );
      if (!mounted) return null;
      const row = mounted.closest<HTMLElement>("[data-testid='transcript-row']");
      const h = (row ?? mounted).getBoundingClientRect().height;
      return h > 0 ? h : null;
    });
  const pinnedBlockHeight = async (): Promise<number> => {
    const deadline = Date.now() + 15_000;
    let last: number | null = null;
    while (Date.now() < deadline) {
      last = await scrollToBlockOnce();
      if (last !== null) return last;
      await new Promise((r) => setTimeout(r, 150));
    }
    throw new Error("wrap block not measurable at the parked position");
  };
  const blockFallback = await pinnedBlockHeight();
  expect(blockFallback, "wrap block measurable on the fallback face at 390px").not.toBeNull();
  // Re-pin at the bottom.
  await page.getByTestId("jump-latest").click().catch(() => undefined);
  await assertPinned("back at the bottom before release");

    finalGate.release();
    await afterSwap(page, scroller);
    await assertPinned("the final visit after the swap");
    await expect(page.getByTestId("jump-latest")).not.toBeVisible();

    // Measure the swapped block (390px) synchronously and require a delta.
    const blockSwapped = await pinnedBlockHeight();
    expect(blockSwapped, "wrap block measurable on the swapped face at 390px").not.toBeNull();
    expect(
      Math.abs(blockSwapped - blockFallback),
      `390px: the swap did not change the wrap-probe row height (fallback=${blockFallback} swapped=${blockSwapped})`,
    ).toBeGreaterThanOrEqual(20);

    await page.getByTestId("jump-latest").click().catch(() => undefined);
    await assertRowsStacked(scroller);
  } finally {
    await finalGate.dispose();
  }
});
