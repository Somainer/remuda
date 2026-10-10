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

type FontGate = Awaited<ReturnType<typeof gateRoute>>;

/**
 * Wait for a gated woff2 request to actually park, bounded: fail fast with a
 * diagnostic instead of hanging to the test timeout if the revisit served the
 * font from cache (so no request reached the gate) or the page closed.
 */
async function waitFontArrival(page: Page, gate: FontGate, label: string, timeoutMs = 10_000): Promise<void> {
  let arrived = false;
  gate.waitArrival().then(() => {
    arrived = true;
  });
  const start = Date.now();
  while (!arrived && Date.now() - start < timeoutMs) {
    if (page.isClosed()) throw new Error(`${label}: page closed before a woff2 request reached the gate`);
    // A navigation AWAY from the session document (or a worker/shared frame)
    // means the parked request can never arrive either: fail fast instead of
    // hanging to the test timeout.
    const frames = page.frames();
    const onSession = frames.some((f) => {
      try {
        return f === page.mainFrame() && /\/s\//.test(f.url());
      } catch {
        return false;
      }
    });
    if (!onSession) throw new Error(`${label}: document left the /s/ route before a woff2 request parked (${page.mainFrame()?.url()})`);
    await page.waitForTimeout(100);
  }
  if (!arrived) throw new Error(`${label}: no woff2 request parked within ${timeoutMs}ms (served from cache?)`);
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
  const wrapPrompt =
    wrapProbe === "mobile"
      ? "font wrap probe mobile tail: show me code"
      : wrapProbe
        ? "font wrap probe: show me code"
        : "font swap probe: show me code";
  // The PINNED mobile test needs the height-changing block in the MOUNTED
  // TAIL: send the wrap message AFTER the burst rows there so it is the last
  // code block and stays mounted at the pinned bottom without scrolling.
  if (wrapProbe === "mobile") {
    await command(page, instanceId, `__journal_burst__:${BURST}`);
    await command(page, instanceId, wrapPrompt);
    await expect
      .poll(async () => textsAfterLastCode(await journalTail(page, instanceId)), { timeout: 30_000 })
      .not.toBeNull();
  } else {
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
  }
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

/** Stable node id of a transcript row (`id:<data-anchor>`). */
type AnchorRef = `id:${string}`;

/** Scroller-relative top of the anchor row. */
async function rowOffset(scroller: Locator, anchor: AnchorRef): Promise<number | null> {
  return scroller.evaluate(
    (el, ref) => {
      const row = el.querySelector<HTMLElement>(`[data-anchor="${CSS.escape(ref.slice(3))}"]`);
      if (!row) return null;
      const rect = row.getBoundingClientRect();
      const top = rect.top - el.getBoundingClientRect().top;
      // "On screen" = the row INTERSECTS the viewport, not merely has its top
      // edge inside it: a tall saved row (the wrap block) can straddle the
      // viewport top (top slightly negative) while its body fills the top of
      // the screen. Its offset may be a small negative number; drift compares
      // across faces, so that is fine.
      return top < el.clientHeight && top + rect.height > 0 ? top : null;
    },
    anchor,
  );
}

/**
 * ABSOLUTE document top (scrollTop + scroller-relative top) of the anchor
 * row — used only to wait for the row to MOUNT.
 */
async function anchorDocTop(scroller: Locator, anchor: AnchorRef): Promise<number | null> {
  return scroller.evaluate(
    (el, ref) => {
      const row = el.querySelector<HTMLElement>(`[data-anchor="${CSS.escape(ref.slice(3))}"]`);
      if (!row) return null;
      return el.scrollTop + row.getBoundingClientRect().top - el.getBoundingClientRect().top;
    },
    anchor,
  );
}

/**
 * The anchor's SCROLLER-RELATIVE VIEWPORT offset, or null when it is mounted
 * but off-screen. This is the re-anchor oracle: holdReadingAnchor changes
 * scrollTop and the viewport top by equal/opposite amounts, so an ABSOLUTE
 * document top is invariant whether or not the anchor was held; only the
 * viewport offset reveals whether the reader's focal row stayed put.
 */
async function anchorViewport(scroller: Locator, anchor: AnchorRef): Promise<number | null> {
  return rowOffset(scroller, anchor);
}

/**
 * Wait until the restored anchor is mounted AND on-screen, returning its
 * viewport offset. `message` identifies the waiter. Bounded by `timeoutMs`.
 */
async function waitAnchorViewport(
  scroller: Locator,
  n: number,
  timeoutMs: number,
  message: string,
): Promise<number> {
  const deadline = Date.now() + timeoutMs;
  // eslint-disable-next-line no-constant-condition
  while (true) {
    const off = await anchorViewport(scroller, n);
    if (off !== null) return off;
    if (Date.now() > deadline) throw new Error(message);
    await scroller.page().waitForTimeout(100);
  }
}


/**
 * After a font release, poll the wrap block's row HEIGHT until it differs from
 * `fallbackHeight` by at least 20px (the real metrics change landed), then
 * return it. The anchor offset stability is verified separately. Bounded.
 */
async function waitBlockHeightChange(
  page: Page,
  scroller: Locator,
  fallbackHeight: number,
  timeoutMs = 15_000,
): Promise<number> {
  const deadline = Date.now() + timeoutMs;
  let lastSeen: number | null = null;
  while (Date.now() < deadline) {
    await waitFontLoaded(page, scroller, "block height change");
    await page.waitForTimeout(120);
    const h = await wrapBlockRowHeight(scroller);
    if (h !== null) lastSeen = h;
    if (h !== null && Math.abs(h - fallbackHeight) >= 20) return h;
  }
  throw new Error(`wrap block height did not change >=20px within ${timeoutMs}ms (fallback=${fallbackHeight}, last=${lastSeen})`);
}

/**
 * The restore has quiesced AND stopped being active: the anchor row's
 * scroller-relative offset has stayed put across several ResizeObserver
 * MEASUREMENT cycles (two rAFs can elapse with no size commit at all), and the
 * scroller no longer carries data-restore-active="1". Any movement or
 * disappearance of the anchor — including one that arrives during the 32 ms
 * revalidation window — CANCELS that timer and invalidates the run (the
 * counter resets), so a font swap whose last size commit lands right at the
 * threshold cannot release early. The final value is then re-read after the
 * window before resolving. All timers and the observer are torn down together
 * (resolve OR timeout), no leaks.
 */
async function waitAnchorStable(
  scroller: Locator,
  anchor: AnchorRef,
  {
    timeout = 15_000,
    // "settled": the restore has finalized (data-restore-active !== "1") and the
    //   offset holds — used AFTER releasing the font.
    // "armed": the restore is still actively correcting (attribute "1") and the
    //   offset has already converged and holds across measurement cycles — the
    //   deterministic release barrier for BOTH gated arms (items 3/5).
    phase = "settled",
  }: { timeout?: number; phase?: "armed" | "settled" } = {},
): Promise<number> {
  const result = await scroller.evaluate(
    (el, { ref, deadlineMs, phase }) =>
      new Promise<string>((resolvePromise) => {
        const list = el.firstElementChild;
        const findRow = (): HTMLElement | null =>
          el.querySelector(`[data-anchor="${CSS.escape(ref.slice(3))}"]`);
        const offset = (): number | null => {
          const active = el.getAttribute("data-restore-active") === "1";
          // The release barrier requires a STILL-ARMED restore (a held-font
          // restore converges to its saved offset but does not finalize); the
          // post-release barrier requires the restore to be DONE.
          if (phase === "armed" ? !active : active) return null;
          const row = findRow();
          if (!row) return null;
          const rect = row.getBoundingClientRect();
          const top = rect.top - el.getBoundingClientRect().top;
          // An INTERSECTING row counts (a tall saved row may straddle the
          // viewport top); a fully below/above-the-fold row does not.
          if (top >= el.clientHeight || top + rect.height <= 0) return null;
          return top;
        };
        const REQUIRED = 3;
        let last: number | null = offset();
        let stable = 0;
        let settledAt: number | null = null;
        let armedAt: number | null = null;
        let revalTimer: number | null = null;
        const timers: number[] = [];
        const setTimer = (fn: TimerHandler, ms: number) => {
          const id = window.setTimeout(fn, ms);
          timers.push(id);
          return id;
        };
        let ro: ResizeObserver | null = null;
        const finish = (value: string) => {
          timers.forEach((id) => window.clearTimeout(id));
          if (revalTimer !== null) window.clearTimeout(revalTimer);
          ro?.disconnect();
          resolvePromise(value);
        };
        setTimer(() => {
          finish(settledAt !== null ? `stable:${settledAt}` : last === null ? "missing" : "moving");
        }, deadlineMs);
        const cancelRevalidation = () => {
          if (revalTimer !== null) {
            window.clearTimeout(revalTimer);
            revalTimer = null;
          }
          armedAt = null;
          stable = 0;
        };
        // Movement/disappearance resets the streak AND cancels a pending
        // revalidation; reaching the threshold arms a ONE-frame revalidation.
        const sample = () => {
          const next = offset();
          if (next === null) {
            cancelRevalidation();
            last = null;
            return;
          }
          if (armedAt !== null) {
            // A measurement arrived inside the revalidation window: any
            // movement/removal cancels it immediately instead of waiting.
            if (Math.abs(next - armedAt) > 1) {
              cancelRevalidation();
            } else {
              return;
            }
          }
          if (last !== null && Math.abs(next - last) <= 1) {
            stable += 1;
            if (stable >= REQUIRED) {
              armedAt = next;
              revalTimer = setTimer(() => {
                // Re-read after another frame: any movement/removal here
                // invalidates and restarts the streak.
                const check = offset();
                revalTimer = null;
                if (check !== null && armedAt !== null && Math.abs(check - armedAt) <= 1) {
                  settledAt = check;
                  finish(`stable:${check}`);
                } else {
                  cancelRevalidation();
                }
              }, 32);
            }
          } else {
            stable = 0;
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
          if (armedAt === null) sample();
          timers.push(window.setTimeout(tick, 100));
        };
        timers.push(window.setTimeout(tick, 100));
      }),
    { ref: anchor, deadlineMs: timeout, phase },
  );
  expect(
    result,
    phase === "armed"
      ? "the held restore did not hold a stable offset while data-restore-active='1'"
      : "the restored anchor never settled off-restore across measurement cycles",
  ).toMatch(/^stable:/);
  return Number(result.slice("stable:".length));
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
      (b.querySelector("[data-testid='code-code']")?.textContent?.trim().startsWith("⁄".repeat(20)) ?? false),
    );
    if (!block) return null;
    const row = block.closest<HTMLElement>("[data-testid='transcript-row']");
    return (row ?? block).getBoundingClientRect().height;
  });
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
  // Every arm carries the height-changing U+2044 wrap fixture. In the bounded
  // 2400-row journal it is appended just above the final tail rows and the
  // restore/search parks there, so it stays in the mounted window and the long
  // arm also sees a real >=20px reflow (item 4).
  const wrapProbe = true;
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
    // The wrap block sits just above the tail in the bounded replay; search
    // jumps to it so it is mounted.
    await page.getByTestId("transcript-search-open").click();
    const search = page.getByTestId("transcript-search-input");
    await search.fill("wrapping line");
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

  // Park the wrap-block's bottom a little below the viewport top. The scroll
  // persistence flush (250 ms debounce) then saves the row whose region holds
  // the viewport top — for the short journals that is the tall wrap-block row
  // itself; for the bounded tail it is the first burst row just below it. The
  // oracle points at the ACTUAL saved anchorId (below), never a guessed row.
  // Park, choosing WHICH row the reading record anchors on:
  //  - SHORT journals (late/mid arms): park the block bottom a little BELOW the
  //    viewport top so the tall wrap-block row spans the top and is itself the
  //    saved anchor. Its own height changes on the swap and its own top must be
  //    held with no counter-scroll (item 1/3).
  //  - BOUNDED long journal (item 4): park the block bottom just ABOVE the
  //    viewport top (still inside the virtualiser's overscan so it is mounted)
  //    so the viewport top — and the saved anchor — is the first BURST row
  //    strictly BELOW the growing block. That block is a mounted row above the
  //    anchor, so disabling re-anchoring makes the anchor drift by the growth.
  const PARK = longBurst > 0 ? { mode: "above" as const, gap: 16 } : { mode: "above" as const, gap: 4 };
  await scroller.evaluate((el, park) => {
    const block = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='code-block']")).find((b) =>
      b.querySelector("[data-testid='code-code']")?.textContent?.trim().startsWith("⁄".repeat(20)),
    );
    const pre = block?.querySelector("pre") ?? block;
    if (!pre) throw new Error("code block not rendered");
    const preBottom = pre.getBoundingClientRect().bottom - el.getBoundingClientRect().top;
    if (park.mode === "bottom") {
      el.scrollTop += preBottom - park.bottom;
    } else {
      // Put pre.bottom at -gap (just above the viewport top).
      el.scrollTop += preBottom + park.gap;
    }
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
  }, PARK);
  // Let the 250 ms persistence flush write the record, then read the exact
  // anchor the restore will target.
  await page.waitForTimeout(450);
  const readingKey0 = `runtime.reading.v1.${instanceId}`;
  const parkedRecord = await page.evaluate((key) => localStorage.getItem(key), readingKey0);
  expect(parkedRecord, "the park wrote a reading position").not.toBeNull();
  const savedAnchorId = JSON.parse(parkedRecord!).anchorId as string;
  const anchor: AnchorRef = `id:${savedAnchorId}`;
  // Whether the saved anchor IS the growing wrap-block row (short journals):
  // its own top must match across control / fallback / settled.
  const savedAnchorIsWrapBlock = await scroller.evaluate((_el, id: string) => {
    const row = document.querySelector<HTMLElement>(`[data-anchor="${CSS.escape(id)}"]`);
    return !!row?.querySelector("[data-testid='code-code']")?.textContent?.includes("⁄".repeat(20));
  }, savedAnchorId);
  // The parking geometry decides WHICH case we prove: the short journals must
  // save the wrap block itself (self-growth); the bounded tail must save a
  // burst row strictly below it (an above-row growth), which is the only
  // configuration that drifts with re-anchoring disabled.
  expect(savedAnchorIsWrapBlock, `unexpected saved anchor for longBurst=${longBurst}`).toBe(longBurst === 0);
  // The anchor's VIEWPORT offset at the saved reading position.
  const saved = await waitAnchorViewport(scroller, anchor, 10_000, "saved anchor off-screen");
  expect(Number.isFinite(saved), `the saved anchor is not on screen (${saved})`).toBe(true);

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
  const control = await waitAnchorViewport(scroller, anchor, 15_000, "control anchor off-screen");
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
  // Leave to the session list, then return on a fresh document with the byte-
  // identical saved record and a closed font gate (installed only now, after
  // the hop, so the hop cannot cancel a parked request).
  await page.goto("/sessions");
  await expect(page.getByTestId("session-list")).toBeVisible();
  leftByControl = order === "late" ? await page.evaluate((key) => localStorage.getItem(key), readingKey) : null;
  await reinstate();
  const fontGate = await gateRoute(page, /\.woff2(?:\?|$)/, { revalidate: true });
  // The MID arm ("first") holds the woff2 with an ARMED restore (?restoreProbe
  // plus a window flag — a bare query is inert, item 7): the restore converges
  // on the fallback face but stays data-restore-active="1" until release.
  // The LATE arms let the restore FINALIZE on the fallback face before the swap
  // (no probe); the swap then lands strictly after the restore, and only the
  // post-measurement re-anchoring (setRowSize height compensation +
  // holdReadingAnchor) holds the anchor. That distinction is what lets the
  // ?noReanchor seam prove a late-arm drift (the restore effect is already
  // done, so disabling it changes nothing).
  const mid = order === "first";
  const failQuery = process.env.FONTSWAP_NO_REANCHOR === "1" ? (mid ? "&noReanchor=1" : "?noReanchor=1") : "";
  if (mid) {
    await page.addInitScript(() => {
      (window as unknown as { __fontSwapRestoreProbeArmed?: boolean }).__fontSwapRestoreProbeArmed = true;
    });
  }
  try {
    await page.goto(`/s/${instanceId}${mid ? "?restoreProbe=1" : ""}${failQuery}`);
    await waitFontArrival(page, fontGate, `${order} arm`);
    expect(await monoLoaded(page), "the held woff2 must not have swapped in yet").toBe(false);
    // Release barrier:
    //  - mid: the restore is still ARMED but the anchor's viewport offset has
    //    held across measurement cycles;
    //  - late: the restore has FINALIZED and the offset has held.
    await expect
      .poll(() => scroller.getAttribute("data-restore-active"), { timeout: 5_000 })
      .toBe(mid ? "1" : "0");
    beforeSwap = await waitAnchorStable(scroller, anchor, { phase: mid ? "armed" : "settled" });
    blockFallback = wrapProbe ? await wrapBlockRowHeight(scroller) : null;
    expect(await monoLoaded(page), "rows must still be on the fallback face at release").toBe(false);
    const advanceFallback = await monoAdvance(page);
    if (process.env.FONTSWAP_NO_REANCHOR === "1") {
      await page.evaluate(() => {
        (window as unknown as { __fontSwapNoReanchorArmed?: boolean }).__fontSwapNoReanchorArmed = true;
      });
    }
    fontGate.release();
    // The block height moves with the final face; then the anchor must stay on
    // screen at the same offset (mid: the restore finalizes; late: only the
    // growth re-anchor runs).
    blockSwapped = wrapProbe ? await waitBlockHeightChange(page, scroller, blockFallback!) : null;
    settled = await waitAnchorStable(scroller, anchor, { phase: "settled" });
    const advanceSwapped = await monoAdvance(page);
    expect(advanceSwapped, "the held font never swapped to the final face").not.toBe(advanceFallback);
  } finally {
    await fontGate.dispose();
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
  const measured = `order=${order} longBurst=${longBurst} savedViewport=${saved} controlViewport=${control} beforeSwapViewport=${beforeSwap} settledViewport=${settled} blockControl=${blockControl} blockFallback=${blockFallback} blockSwapped=${blockSwapped} input=${original} leftByControl=${leftByControl}`;
  test.info().annotations.push({ type: "font-swap", description: measured });
  console.log(`FONTSWAP ${measured}`);
  // The anchor stayed ON SCREEN (intersecting the viewport) and restored to a
  // valid viewport offset in every arm.
  expect(Number.isFinite(control), `the saved row did not restore on screen (${measured})`).toBe(true);
  expect(Number.isFinite(settled), `the anchor was not on screen after the swap (${measured})`).toBe(true);
  // The swap must have moved REAL layout: the block's transcript row changed
  // HEIGHT by at least 20px between fallback and final faces. Without this, a
  // viewport-offset match would prove nothing (a non-wrapping <pre> has
  // identical height across faces and nothing needed re-anchoring).
  if (wrapProbe) {
    expect(blockFallback, `no fallback block height sampled (${measured})`).not.toBeNull();
    expect(blockSwapped, `no swapped block height sampled (${measured})`).not.toBeNull();
    // Real GROWTH in both directions (U+2044): not an absolute change, which a
    // shrink would satisfy without leaving anything to re-anchor.
    expect(
      blockSwapped! - blockFallback!,
      `the swap did not GROW the wrap-probe row — drift would be unprovable (${measured})`,
    ).toBeGreaterThanOrEqual(BLOCK_HEIGHT_DELTA_PX);
  }
  // RE-ANCHOR INVARIANT (viewport offset). Absolute document tops are blind to
  // scroll re-anchoring (a correction moves scrollTop and the viewport top by
  // equal/opposite amounts), so compare the anchor's SCROLLER-RELATIVE offset.
  // (a) The reader restored at a fallback-face position (beforeSwap); after the
  //     swap the anchor must stay at that SAME viewport spot (settled ≈
  //     beforeSwap) in EVERY arm.
  if (beforeSwap !== null) {
    expect(
      Math.abs(settled - beforeSwap),
      `post-swap anchor drifted from its pre-swap viewport offset — re-anchor failed (${measured})`,
    ).toBeLessThanOrEqual(DRIFT_PX);
  }
  // (b) Round-2 saved-position bound (item 9): the gated restore must be no
  // further from the first-visit SAVED offset than the font-already-loaded
  // control restore is. Never weakened.
  expect(
    Math.abs(settled - saved),
    `swap restore landed farther from the saved offset than the control restore (${measured})`,
  ).toBeLessThanOrEqual(Math.abs(control - saved) + DRIFT_PX);
  // (c) When the saved anchor IS the growing wrap-block row, its own height
  // change must keep its own top: the natural Plex restore position (control),
  // the held fallback position and the post-swap position all coincide. Item 1
  // — fails if the anchor row's own growth scrolls it off (the r4 bug).
  if (savedAnchorIsWrapBlock) {
    expect(
      Math.abs(settled - control),
      `the growing saved row's own top moved vs the font-already-loaded control (${measured})`,
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

test("a saved position in a bounded long journal survives a monospace swap landing mid-restore", async ({ page }) => {
  // The gap-16 park (longBurst>0) saves a burst row strictly BELOW the growing
  // wrap block, so unlike the short mid arm (whose saved anchor IS the wrap
  // row) a broken mid-restore re-anchor makes THIS anchor drift by the block's
  // growth. This is the arm that proves the mid-restore correction, not merely
  // the anchor row's own top.
  test.setTimeout(240_000);
  await savedPositionSurvivesSwap(page, LONG_BURST, "first");
});


test("a pinned transcript stays pinned through a late monospace swap", async ({ page }) => {
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 390, height: 844 });
  // Wrap probe + soft wrap: the swap changes real row heights (measured on
  // the final revisit) at the 390px pane, so "stays pinned" is asserted
  // against an actual layout change, not an identical-height block.
  const instanceId = await seedSession(page, 0, "mobile");
  await page.evaluate(() => localStorage.setItem("runtime.code-wrap", "1"));
  // Disable the HTTP cache for this page (item 6): the final revisit must
  // re-REQUEST the woff2 over the network so the closed gate deterministically
  // sees it. Without this a warm disk cache can satisfy the request before
  // page.route runs and finalGate.waitArrival() would hang to the timeout.
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Network.enable");
  await cdp.send("Network.setCacheDisabled", { cacheDisabled: true });
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
    await waitFontArrival(page, firstGate, "pinned first visit");
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
  // FAIL-PROOF ONLY: FONTSWAP_NO_SIZE_PIN=1 opens this revisit on ?noSizePin=1
  // (a test-only Transcript seam) and arms the disable exactly at release, so
  // the fallback pin below runs with the real product; only post-swap
  // size-driven re-pinning is off, after which the growing block MUST leave a
  // gap. No effect when the env var is absent.
  const noSizePinQuery = process.env.FONTSWAP_NO_SIZE_PIN === "1" ? "?noSizePin=1" : "";
  try {
    await page.goto(`/s/${instanceId}${noSizePinQuery}`);
    await expect(page.getByTestId("transcript-row")).not.toHaveCount(0, { timeout: 15_000 });
    // Bounded, page-close/abandon aware (item 6): fail fast rather than hang
    // if the revisit did not re-request the woff2.
    await waitFontArrival(page, finalGate, "pinned final revisit");
    expect(await monoLoaded(page), "the final visit must start with the font unloaded").toBe(false);
    await assertPinned("the final visit before the swap");
    await expect(page.getByTestId("jump-latest")).not.toBeVisible();
    // The tail wrap block is MOUNTED while pinned (no measurement scroll):
    // read its transcript-row HEIGHT directly, waiting for it to appear.
    const readTailBlockHeight = () =>
      scroller.evaluate(() => {
        const el = document.querySelector<HTMLElement>("[data-testid='transcript-scroller']")!;
        const block = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='code-block']")).find((b) =>
          b.querySelector("[data-testid='code-code']")?.textContent?.includes("⁄".repeat(20)),
        );
        if (!block) return null;
        const row = block.closest<HTMLElement>("[data-testid='transcript-row']");
        const height = (row ?? block).getBoundingClientRect().height;
        return height > 0 ? height : null;
      });
    // Diagnostic snapshot of the mounted window when the tail block never
    // appears: row count, code blocks present (with snippet lengths), scroll
    // geometry, and whether the font has loaded — enough to distinguish
    // "unmounted by the virtual window" from "reply absent from the journal".
    const dumpTailWindow = async (): Promise<string> =>
      scroller.evaluate((el) => {
        const rows = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']"));
        const blocks = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='code-block']")).map((b) => {
          const code = b.querySelector("[data-testid='code-code']")?.textContent?.trim() ?? "";
          return `code(${code.length}:${code.slice(0, 12)})`;
        });
        const anchors = rows.map((r) => r.dataset.anchor?.slice(0, 18) ?? "?");
        const lastText = rows[rows.length - 1]?.textContent?.slice(0, 60).replace(/\s+/g, " ") ?? "";
        return JSON.stringify({
          rows: rows.length,
          first: anchors[0],
          last: anchors[anchors.length - 1],
          lastText,
          blocks,
          scrollTop: el.scrollTop,
          scrollHeight: el.scrollHeight,
          clientHeight: el.clientHeight,
          gap: el.scrollHeight - el.scrollTop - el.clientHeight,
          mono: document.fonts.check('400 13px "IBM Plex Mono"'),
        });
      });
    const tailBlockHeight = async (): Promise<number> => {
      const deadline = Date.now() + 15_000;
      while (Date.now() < deadline) {
        const h = await readTailBlockHeight();
        if (h !== null) return h;
        await new Promise((r) => setTimeout(r, 120));
      }
      throw new Error(`tail wrap block not mounted while pinned (${await dumpTailWindow()})`);
    };
    // Capture the tight baseline gap on the fallback face.
    const fallbackGap = await bottomGap();
    const blockFallback = await tailBlockHeight();
    // FAIL-PROOF ONLY: arm the re-pin disable exactly at release, so the
    // fallback pin above was achieved with the real product.
    if (process.env.FONTSWAP_NO_SIZE_PIN === "1") {
      await page.evaluate(() => {
        (window as unknown as { __fontSwapNoSizePinArmed?: boolean }).__fontSwapNoSizePinArmed = true;
      });
    }

    finalGate.release();
    // Wait for the block HEIGHT to actually move (the real metrics change),
    // THEN for the size-commit re-pin to catch up: the font swap reflows the
    // DOM a frame before the ResizeObserver reports the new size and the pin
    // effect runs, so an early sample shows the full growth as a bottom gap.
    // Require the tight gap AND a stable swapped height across two consecutive
    // samples. Bounded — no fixed delays.
    const tightGap = Math.max(8, fallbackGap + 4);
    const blockSwapped = await new Promise<number>((resolve, reject) => {
      const deadline = Date.now() + 15_000;
      let grew = false;
      let gapStreak = 0;
      let heightStreak = 0;
      let lastH: number | null = null;
      const tick = async () => {
        const gap = await bottomGap();
        const h = await readTailBlockHeight();
        // Require real GROWTH (the U+2044 fallback->Plex swap grows the block),
        // not an absolute change: a shrinkage leaves no gap and would make the
        // pin assertion vacuous (item 9).
        if (h !== null && h - blockFallback >= 20) grew = true;
        if (grew && h !== null) {
          heightStreak = lastH !== null && Math.abs(h - lastH) <= 0.5 ? heightStreak + 1 : 0;
          gapStreak = gap <= tightGap ? gapStreak + 1 : 0;
          if (heightStreak >= 2 && gapStreak >= 2) {
            resolve(h);
            return;
          }
          lastH = h;
        }
        if (Date.now() > deadline) {
          reject(
            new Error(
              `pin: block did not GROW >=20px and re-pin tightly while pinned (fallback=${blockFallback} last=${h} gap=${gap} grew=${grew})`,
            ),
          );
          return;
        }
        setTimeout(tick, 100);
      };
      void tick();
    });

    // Tight baseline: the post-swap gap must match the pre-swap fallback gap,
    // not merely "anywhere inside 64px".
    expect(
      await bottomGap(),
      `pinned gap moved with the height change (fallbackGap=${fallbackGap})`,
    ).toBeLessThanOrEqual(tightGap);
    expect(
      blockSwapped - blockFallback,
      `390px: the tail block did not GROW on the swap (fallback=${blockFallback} swapped=${blockSwapped})`,
    ).toBeGreaterThanOrEqual(20);
    await expect(page.getByTestId("jump-latest")).not.toBeVisible();
    await assertRowsStacked(scroller);
  } finally {
    await finalGate.dispose();
  }
});
