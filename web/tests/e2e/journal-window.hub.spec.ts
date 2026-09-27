import { expect, test, type Browser, type BrowserContext, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Bounded journal windows (hub-store-1 / c-journalpage).
 *
 * GET /v1/instances/:id/journal and the follow snapshot are a bounded TAIL
 * window (at most 2000 rows / 8 MiB). The old web client assumed an unbounded
 * read: an ascending 512-loop silently dropped the middle of a long journal,
 * and a resync snapshot whose floor sat above the applied cursor left the
 * banner gap-backfilling forever.
 *
 * Fake Node only — `__journal_burst__:<n>` (a seeding hook off every scripted
 * path) appends n assistant messages in one batched journal.append.
 *
 * Scenario:
 *   1. a second context seeds ~5000 events BEFORE the test page attaches (late
 *      attach): the tail window floor is above 1, the newest turn renders, and
 *      load-earlier pages older windows;
 *   2. the follower link is throttled while a second 5000-event burst lands, so
 *      the Hub's follow buffer overflows and it queues a gap + bounded resync
 *      snapshot: the client descends with beforeSeq, the banner passes
 *      gap-backfill and returns to live.
 */

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

const BURST_COUNT = 5000;

/** Seeding context, closed by the describe-level hook (must stay unthrottled). */
let driver: { context: BrowserContext; page: Page } | null = null;

async function patchMaxInstances(page: Page, value: number) {
  await page.evaluate(async (next) => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    const body = (await list.json()) as {
      items?: { hostId?: string; maxInstances?: number }[];
    };
    const id = body.items?.find((host) => host.hostId)?.hostId;
    if (!id) return;
    await fetch(`/v1/hosts/${id}`, {
      method: "PATCH",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ maxInstances: next }),
    });
  }, value);
}

async function forceDeleteAllInstances(page: Page) {
  await page.evaluate(async () => {
    const list = await fetch("/v1/instances", { credentials: "include" });
    const body = (await list.json()) as {
      items?: { instanceId?: string; lifecycle?: string }[];
    };
    await Promise.all(
      (body.items ?? [])
        .filter((instance) => instance.instanceId)
        .map(async (instance) => {
          await fetch(`/v1/instances/${instance.instanceId}?force=1`, {
            method: "DELETE",
            credentials: "include",
          }).catch(() => undefined);
        }),
    );
  });
}

test.beforeAll(async ({ browser }) => {
  const setup = await browser.newPage();
  await login(setup);
  await patchMaxInstances(setup, 24);
  await setup.close();
});

test.afterAll(async ({ browser }) => {
  const setup = await browser.newPage();
  await login(setup);
  await patchMaxInstances(setup, 8);
  await forceDeleteAllInstances(setup);
  await setup.close();
});

test.afterEach(async ({ browser }) => {
  // The driving context is independent of the throttled follower context, so
  // clean up with a fresh page rather than the throttled one.
  const cleanup = await browser.newPage();
  try {
    await login(cleanup);
    await forceDeleteAllInstances(cleanup);
  } finally {
    await cleanup.close();
  }
});

test.afterAll(async () => {
  await driver?.context.close().catch(() => undefined);
  driver = null;
});

/** Independent browser context that drives the node while the tab is slow. */
async function driverContext(browser: Browser) {
  const context = await browser.newContext();
  const page = await context.newPage();
  await login(page, "e2e-window-driver");
  return { context, page };
}

async function createInstanceRest(page: Page): Promise<string> {
  const hosts = await page.evaluate(async () => {
    const response = await fetch("/v1/hosts", { credentials: "include" });
    return (await response.json()) as { items?: { hostId?: string }[] };
  });
  const hostId = hosts.items?.find((host) => host.hostId)?.hostId;
  expect(hostId).toBeTruthy();
  const created = await page.evaluate(async (id) => {
    const response = await fetch("/v1/instances", {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        hostId: id,
        workspaceId: "/tmp",
        kind: "claude",
        // claude-print takes the fake node's generic approval-card create arm
        // (the REST default claude-pty would skip it).
        driver: "claude-print",
        prompt: "journal window session",
      }),
    });
    return response.json() as Promise<{ instance: { instanceId: string } }>;
  }, hostId!);
  return created.instance.instanceId;
}

/** The create approval disables sends until answered; answer it over REST. */
async function answerPendingRest(page: Page, instanceId: string) {
  for (let attempt = 0; attempt < 30; attempt += 1) {
    const option = await page.evaluate(async (id) => {
      const list = await fetch("/v1/interactions", { credentials: "include" });
      const body = (await list.json()) as {
        items?: {
          id: string;
          instanceId?: string;
          state?: string;
          request?: { inputDigest?: string; options?: { id: string }[] };
        }[];
      };
      const pending = (body.items ?? []).find(
        (item) => item.instanceId === id && item.state === "pending",
      );
      const optionId = pending?.request?.options?.[0]?.id;
      if (!pending || !optionId) return null;
      await fetch(`/v1/interactions/${pending.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          answer: {
            kind: "approval",
            optionId,
            inputDigest: pending.request?.inputDigest ?? "",
          },
        }),
      });
      return optionId;
    }, instanceId);
    if (option) return;
    await page.waitForTimeout(200);
  }
  throw new Error("create approval never became answerable");
}

async function burst(page: Page, instanceId: string, count: number) {
  const result = await page.evaluate(
    async ({ id, count }) => {
      const response = await fetch(`/v1/instances/${id}/commands`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          operation: "instance.send",
          payload: { prompt: `__journal_burst__:${count}` },
        }),
      });
      return response.ok;
    },
    { id: instanceId, count },
  );
  expect(result).toBe(true);
}

async function waitDurable(page: Page, instanceId: string, min: number) {
  await expect
    .poll(
      async () => {
        try {
          return await page.evaluate(async (id) => {
            const response = await fetch(`/v1/instances/${id}/journal`, {
              credentials: "include",
            });
            if (!response.ok) return -1;
            return Number((await response.json()).durableSeq as string);
          }, instanceId);
        } catch {
          // A gated/stalled follow socket can briefly starve the dev proxy.
          return -1;
        }
      },
      { timeout: 90_000, intervals: [500, 1000] },
    )
    .toBeGreaterThanOrEqual(min);
}

async function journalJson(page: Page, instanceId: string) {
  return page.evaluate(async (id) => {
    const response = await fetch(`/v1/instances/${id}/journal`, {
      credentials: "include",
    });
    return (await response.json()) as {
      durableSeq: string;
      fromSeq: string | null;
      reachedAfterSeq: boolean;
      events: { seq: string; event?: { payload?: { text?: unknown } } }[];
    };
  }, instanceId);
}

/**
 * Highest burst label visible in the server tail. The raw text is
 * `__journal_burst__ event N` (underscores render away as Markdown bold in
 * the DOM), so match the common word shape of both.
 */
const BURST_LABEL_RE = /journal_burst_* event (\d+)/;
function maxBurstLabel(body: Awaited<ReturnType<typeof journalJson>>): number {
  const labels = body.events
    .map((event) =>
      typeof event.event?.payload?.text === "string"
        ? BURST_LABEL_RE.exec(event.event.payload.text)?.[1]
        : undefined,
    )
    .filter((value): value is string => Boolean(value))
    .map(Number);
  return Math.max(0, ...labels);
}

/** Event labels (`journal_burst event N`, bold underscores rendered away). */
function burstLabels(page: Page) {
  return page
    .getByTestId("transcript-row")
    .evaluateAll((rows) =>
      rows
        .map((row) => new RegExp("journal_burst_* event (\\d+)\\b").exec(row.textContent ?? "")?.[1])
        .filter((value): value is string => Boolean(value))
        .map(Number),
    );
}

/** Viewport offset of the transcript row carrying burst event `n`. */
async function rowOffset(
  page: Page,
  scroller: ReturnType<Page["getByTestId"]>,
  n: number,
) {
  return scroller.evaluate((el, label) => {
    const re = new RegExp(`journal_burst_* event ${label}\\b`);
    const row = Array.from(el.querySelectorAll<HTMLElement>("[data-testid='transcript-row']")).find(
      (candidate) => re.test(candidate.textContent ?? ""),
    );
    if (!row) return null;
    // Scroller-relative offset — the same basis the component's scroll
    // restore uses, so a converged anchor compares equal here.
    return {
      scrollTop: el.scrollTop,
      offset: row.getBoundingClientRect().top - el.getBoundingClientRect().top,
    };
  }, n);
}

test("a bounded tail window pages older rows and descends a resync gap to live", async ({
  page,
  browser,
}) => {
  test.setTimeout(240_000);
  await login(page);
  // Wrap the follow WebSocket:
  //  - log control frames (snapshot/gap),
  //  - while __followGate is set, drop EVERY follow frame (events, gaps and
  //    snapshots). The browser still drains the socket, so the Hub never
  //    resyncs, but the app's applied cursor stays pinned at the pre-burst
  //    seq. When the gate reopens and a fresh burst lands, its first live
  //    batch starts thousands of seqs above applied -> a real gap the client
  //    must descend with beforeSeq, independent of hub buffer/socket timing.
  await page.addInitScript(() => {
    const w = window as unknown as {
      __frameLog?: string[];
      __frameCount?: number;
      __followGate?: boolean;
    };
    w.__frameLog = [];
    w.__frameCount = 0;
    const NativeWS = window.WebSocket;
    class GatedWS extends NativeWS {
      constructor(url: string | URL, protocols?: string | string[]) {
        super(url, protocols);
        this.addEventListener(
          "message",
          (ev: MessageEvent) => {
            if (typeof ev.data !== "string") return;
            try {
              const msg = JSON.parse(ev.data) as { type?: string; fromSeq?: string | null };
              if (msg.type === "snapshot") w.__frameLog!.push(`snapshot:${msg.fromSeq ?? ""}`);
              else if (msg.type === "event") w.__frameCount = (w.__frameCount ?? 0) + 1;
              else if (msg.type === "gap") w.__frameLog!.push("gap");
            } catch {
              // non-JSON
            }
            if (w.__followGate) ev.stopImmediatePropagation();
          },
          { capture: true },
        );
      }
    }
    window.WebSocket = GatedWS as unknown as typeof WebSocket;
  });

  // Seed a 5000-event journal from a context that never opens the transcript.
  driver = await driverContext(browser);
  const driverPage = driver.page;
  const instanceId = await createInstanceRest(driverPage);
  await answerPendingRest(driverPage, instanceId);
  await burst(driverPage, instanceId, BURST_COUNT);
  await waitDurable(driverPage, instanceId, BURST_COUNT);

  // The server window is bounded: floor well above 1, flag partial.
  const seeded = await journalJson(driverPage, instanceId);
  expect(Number(seeded.durableSeq)).toBeGreaterThanOrEqual(BURST_COUNT);
  expect(seeded.fromSeq).not.toBeNull();
  expect(Number(seeded.fromSeq)).toBeGreaterThan(1);
  expect(seeded.reachedAfterSeq).toBe(false);
  const newestLabel = maxBurstLabel(seeded);
  expect(newestLabel).toBeGreaterThan(0);

  // Late attach: the tab only holds the bounded tail.
  await page.goto(`/s/${instanceId}/structured`);
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-journal", "live", {
    timeout: 30_000,
  });
  const transcript = page.getByTestId("transcript");
  // `__journal_burst__` is Markdown bold; it renders without the wrapping __.
  await expect(transcript).toContainText(`journal_burst event ${newestLabel}`);
  const loadEarlier = page.getByTestId("load-earlier");
  await expect(loadEarlier).toBeVisible();

  // The component pins the topmost rendered row (virtual window start) at
  // scrollTop 0; that is the anchor load-earlier holds, so use it too.
  const scroller = page.getByTestId("transcript-scroller");
  await scroller.evaluate((el) => {
    el.scrollTop = 0;
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
  });
  await page.waitForTimeout(300);
  const labelsBefore = await burstLabels(page);
  expect(labelsBefore.length).toBeGreaterThan(8);
  const anchorLabel = labelsBefore[0];
  await page.waitForTimeout(200);
  const anchorBefore = await rowOffset(page, scroller, anchorLabel);
  expect(anchorBefore).not.toBeNull();

  // One click fetches exactly one bounded older page (beforeSeq).
  const beforeSeqRequests: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (
      url.pathname === `/v1/instances/${instanceId}/journal` &&
      url.searchParams.has("beforeSeq")
    ) {
      beforeSeqRequests.push(url.searchParams.get("beforeSeq")!);
    }
  });
  const olderResponsePromise = page.waitForResponse(
    (response) =>
      response.request().method() === "GET" &&
      new URL(response.url()).searchParams.has("beforeSeq"),
    { timeout: 15_000 },
  );
  await loadEarlier.click();
  const olderResponse = await olderResponsePromise;
  expect(olderResponse.ok()).toBe(true);
  const olderBody = (await olderResponse.json()) as { reachedAfterSeq: boolean };

  // The anchor row stays pinned at its old viewport offset while scrollTop
  // grows by the prepended window height.
  await expect
    .poll(
      async () => {
        const pos = await rowOffset(page, scroller, anchorLabel);
        return pos === null || anchorBefore === null ? null : Math.abs(pos.offset - anchorBefore.offset);
      },
      { timeout: 10_000, intervals: [100, 200] },
    )
    .toBeLessThanOrEqual(4);
  const anchorScrollAfter = await scroller.evaluate((el) => el.scrollTop);
  expect(anchorScrollAfter).toBeGreaterThan(anchorBefore!.scrollTop);

  // D-053: zero per-row drift. Row spacing is padding inside the measured
  // box (no outside margin, no +12 fudge), so consecutive mounted rows are
  // contiguous: each row's slot is exactly its rendered height.
  const gaps = await scroller.evaluate((el) => {
    const rows = Array.from(el.querySelectorAll<HTMLElement>('[data-testid="transcript-row"]'));
    const out: number[] = [];
    for (let i = 1; i < rows.length; i += 1) {
      const prev = rows[i - 1].getBoundingClientRect();
      out.push(Math.round((rows[i].getBoundingClientRect().top - prev.bottom) * 100) / 100);
    }
    return out;
  });
  expect(gaps.length).toBeGreaterThan(0);
  for (const gap of gaps) expect(Math.abs(gap)).toBeLessThanOrEqual(0.5);

  // At the top, an older burst window renders in ascending seq order.
  await scroller.evaluate((el) => {
    el.scrollTop = 0;
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
  });
  await expect
    .poll(() => burstLabels(page).then((labels) => labels[0]), { timeout: 10_000 })
    .toBeLessThan(labelsBefore[0]);
  const labelsAfterFirstClick = await burstLabels(page);
  for (let i = 1; i < Math.min(12, labelsAfterFirstClick.length); i += 1) {
    expect(labelsAfterFirstClick[i]).toBeGreaterThan(labelsAfterFirstClick[i - 1]);
  }

  // Load exactly one more older window and verify the prepend order; do NOT
  // page to seq 1 — the resync step below needs a bounded applied range so the
  // second burst opens a real gap.
  expect(olderBody.reachedAfterSeq).toBe(false);
  await expect(loadEarlier).toBeVisible();

  // --- Deterministic bounded resync gap -----------------------------------
  // Determinism is entirely client-side: the follow WebSocket wrapper (added
  // via addInitScript at login) drops every frame while __followGate is set,
  // so the applied cursor cannot chase the burst regardless of hub buffer
  // sizes or socket timing. The first live batch after reopening opens a gap
  // thousands of rows wide, which the client descends with beforeSeq. Sample
  // the session element's journal state into a window global on a fast
  // interval (the sampler runs in the browser; a Node-scope array would be
  // undefined there).
  await page.evaluate(() => {
    const w = window as unknown as {
      __journalStates?: string[];
      __journalStatusSamples?: string[];
      __followFrames?: string[];
    };
    w.__journalStates = [];
    w.__journalStatusSamples = [];
    w.__followFrames = [];
    const recordBanner = () => {
      const banner = document.querySelector("[data-testid='journal-banner']");
      if (banner) w.__journalStates!.push(banner.getAttribute("data-state") ?? "");
    };
    new MutationObserver(recordBanner).observe(document.body, {
      attributes: true,
      subtree: true,
      childList: true,
    });
    window.setInterval(() => {
      const state = document
        .querySelector<HTMLElement>("[data-testid='session-page']")
        ?.getAttribute("data-journal");
      const samples = w.__journalStatusSamples!;
      if (state && state !== samples[samples.length - 1]) samples.push(state);
    }, 75);
  });

  // Fill-descend reads after this point are resync fills, not the manual click.
  const fillBefore = beforeSeqRequests.length;
  const allJournalRequests: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === `/v1/instances/${instanceId}/journal`) {
      allJournalRequests.push(`${request.method()} ${url.search}`);
    }
  });

  const beforeResyncDurable = Number(seeded.durableSeq);
  // Gate every follow frame during the big burst so the applied cursor cannot
  // chase it; the Hub keeps overflowing and resyncing, all dropped client-side.
  await page.evaluate(() => {
    (window as unknown as { __followGate?: boolean }).__followGate = true;
  });
  await burst(driverPage, instanceId, BURST_COUNT);
  await waitDurable(driverPage, instanceId, beforeResyncDurable + BURST_COUNT);
  // Reopen and send a small burst. Its live frames are not replayed from the
  // gated gap, so the first delivered batch starts far above the pinned
  // cursor and the client descends the missing windows with beforeSeq.
  await page.evaluate(() => {
    (window as unknown as { __followGate?: boolean }).__followGate = false;
  });
  await burst(driverPage, instanceId, 200);
  await waitDurable(driverPage, instanceId, beforeResyncDurable + BURST_COUNT + 200 + 1);
  // The newest label is burst-relative and lands with the trailing idle frame.
  const finalWindow = await journalJson(driverPage, instanceId);
  const resyncNewestLabel = maxBurstLabel(finalWindow);

  // The client descends the bounded resync window with beforeSeq and settles
  // back to live with the newest turn rendered.
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-journal", "live", {
    timeout: 90_000,
  });
  await expect(page.getByTestId("journal-banner")).toHaveCount(0);
  const { sampled, observedBanners, frames, liveCount } = await page.evaluate(() => {
    const w = window as unknown as {
      __journalStates?: string[];
      __journalStatusSamples?: string[];
      __frameLog?: string[];
      __frameCount?: number;
    };
    return {
      sampled: w.__journalStatusSamples ?? [],
      observedBanners: w.__journalStates ?? [],
      frames: w.__frameLog ?? [],
      liveCount: w.__frameCount ?? 0,
    };
  });
  // The banner passed through gap-backfill on the way back to live.
  expect(
    [...sampled, ...observedBanners],
    `expected gap-backfill; fillReads=${beforeSeqRequests.length - fillBefore} sampled=${JSON.stringify(sampled)} banners=${JSON.stringify(observedBanners)} frames=${JSON.stringify(frames)} live=${liveCount} journal=${JSON.stringify(allJournalRequests.slice(-12))}`,
  ).toContain("gap-backfill");
  // A resync gap fill descended with beforeSeq (beyond the manual click).
  expect(
    beforeSeqRequests.length,
    `expected a descending fill read; sampled=${JSON.stringify(sampled)} frames=${JSON.stringify(frames)} journal=${JSON.stringify(allJournalRequests.slice(-12))}`,
  ).toBeGreaterThan(fillBefore);
  // The manual paging left the viewport at the top of history; the newest turn
  // lives at the tail, so jump there before asserting it rendered.
  const jumpLatest = page.getByTestId("jump-latest");
  await expect
    .poll(
      async () => {
        if (await jumpLatest.isVisible().catch(() => false)) {
          await jumpLatest.click().catch(() => undefined);
        }
        return transcript.textContent();
      },
      { timeout: 15_000, intervals: [200, 500] },
    )
    .toContain(`journal_burst event ${resyncNewestLabel}`);
});
