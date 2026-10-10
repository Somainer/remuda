// a6299755cfd9ec16b582dcc9e3dc8df7406fbeb1
import { expect, test } from "@playwright/test";
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

test.describe.configure({
  mode: "serial"
});
test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");
const BURST_COUNT = 5000;

/** Seeding context, closed by the describe-level hook (must stay unthrottled). */
let driver = null;
async function patchMaxInstances(page, value) {
  await page.evaluate(async next => {
    var _body$items;
    const list = await fetch("/v1/hosts", {
      credentials: "include"
    });
    const body = await list.json();
    const id = (_body$items = body.items) === null || _body$items === void 0 || (_body$items = _body$items.find(host => host.hostId)) === null || _body$items === void 0 ? void 0 : _body$items.hostId;
    if (!id) return;
    await fetch(`/v1/hosts/${id}`, {
      method: "PATCH",
      credentials: "include",
      headers: {
        "content-type": "application/json"
      },
      body: JSON.stringify({
        maxInstances: next
      })
    });
  }, value);
}
async function forceDeleteAllInstances(page) {
  await page.evaluate(async () => {
    var _body$items2;
    const list = await fetch("/v1/instances", {
      credentials: "include"
    });
    const body = await list.json();
    await Promise.all(((_body$items2 = body.items) !== null && _body$items2 !== void 0 ? _body$items2 : []).filter(instance => instance.instanceId).map(async instance => {
      await fetch(`/v1/instances/${instance.instanceId}?force=1`, {
        method: "DELETE",
        credentials: "include"
      }).catch(() => undefined);
    }));
  });
}
test.beforeAll(async ({
  browser
}) => {
  const setup = await browser.newPage();
  await login(setup);
  await patchMaxInstances(setup, 24);
  await setup.close();
});
test.afterAll(async ({
  browser
}) => {
  const setup = await browser.newPage();
  await login(setup);
  await patchMaxInstances(setup, 8);
  await forceDeleteAllInstances(setup);
  await setup.close();
});
test.afterEach(async ({
  browser
}) => {
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
  var _driver;
  await ((_driver = driver) === null || _driver === void 0 ? void 0 : _driver.context.close().catch(() => undefined));
  driver = null;
});

/** Independent browser context that drives the node while the tab is slow. */
async function driverContext(browser) {
  const context = await browser.newContext();
  const page = await context.newPage();
  await login(page, "e2e-window-driver");
  return {
    context,
    page
  };
}
async function createInstanceRest(page) {
  var _hosts$items;
  const hosts = await page.evaluate(async () => {
    const response = await fetch("/v1/hosts", {
      credentials: "include"
    });
    return await response.json();
  });
  const hostId = (_hosts$items = hosts.items) === null || _hosts$items === void 0 || (_hosts$items = _hosts$items.find(host => host.hostId)) === null || _hosts$items === void 0 ? void 0 : _hosts$items.hostId;
  expect(hostId).toBeTruthy();
  const created = await page.evaluate(async id => {
    const response = await fetch("/v1/instances", {
      method: "POST",
      credentials: "include",
      headers: {
        "content-type": "application/json"
      },
      body: JSON.stringify({
        hostId: id,
        workspaceId: "/tmp",
        kind: "claude",
        // claude-print takes the fake node's generic approval-card create arm
        // (the REST default claude-pty would skip it).
        driver: "claude-print",
        prompt: "journal window session"
      })
    });
    return response.json();
  }, hostId);
  return created.instance.instanceId;
}

/** The create approval disables sends until answered; answer it over REST. */
async function answerPendingRest(page, instanceId) {
  for (let attempt = 0; attempt < 30; attempt += 1) {
    const option = await page.evaluate(async id => {
      var _body$items3, _pending$request, _pending$request$inpu, _pending$request2;
      const list = await fetch("/v1/interactions", {
        credentials: "include"
      });
      const body = await list.json();
      const pending = ((_body$items3 = body.items) !== null && _body$items3 !== void 0 ? _body$items3 : []).find(item => item.instanceId === id && item.state === "pending");
      const optionId = pending === null || pending === void 0 || (_pending$request = pending.request) === null || _pending$request === void 0 || (_pending$request = _pending$request.options) === null || _pending$request === void 0 || (_pending$request = _pending$request[0]) === null || _pending$request === void 0 ? void 0 : _pending$request.id;
      if (!pending || !optionId) return null;
      await fetch(`/v1/interactions/${pending.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: {
          "content-type": "application/json"
        },
        body: JSON.stringify({
          answer: {
            kind: "approval",
            optionId,
            inputDigest: (_pending$request$inpu = (_pending$request2 = pending.request) === null || _pending$request2 === void 0 ? void 0 : _pending$request2.inputDigest) !== null && _pending$request$inpu !== void 0 ? _pending$request$inpu : ""
          }
        })
      });
      return optionId;
    }, instanceId);
    if (option) return;
    await page.waitForTimeout(200);
  }
  throw new Error("create approval never became answerable");
}
async function burst(page, instanceId, count) {
  const result = await page.evaluate(async ({
    id,
    count
  }) => {
    const response = await fetch(`/v1/instances/${id}/commands`, {
      method: "POST",
      credentials: "include",
      headers: {
        "content-type": "application/json"
      },
      body: JSON.stringify({
        operation: "instance.send",
        payload: {
          prompt: `__journal_burst__:${count}`
        }
      })
    });
    return response.ok;
  }, {
    id: instanceId,
    count
  });
  expect(result).toBe(true);
}
async function waitDurable(page, instanceId, min) {
  await expect.poll(async () => {
    try {
      return await page.evaluate(async id => {
        const response = await fetch(`/v1/instances/${id}/journal`, {
          credentials: "include"
        });
        if (!response.ok) return -1;
        return Number((await response.json()).durableSeq);
      }, instanceId);
    } catch {
      // A gated/stalled follow socket can briefly starve the dev proxy.
      return -1;
    }
  }, {
    timeout: 90000,
    intervals: [500, 1000]
  }).toBeGreaterThanOrEqual(min);
}
async function journalJson(page, instanceId) {
  return page.evaluate(async id => {
    const response = await fetch(`/v1/instances/${id}/journal`, {
      credentials: "include"
    });
    return await response.json();
  }, instanceId);
}

/**
 * Highest burst label visible in the server tail. The raw text is
 * `__journal_burst__ event N` (underscores render away as Markdown bold in
 * the DOM), so match the common word shape of both.
 */
const BURST_LABEL_RE = /journal_burst_* event (\d+)/;
function maxBurstLabel(body) {
  const labels = body.events.map(event => {
    var _event$event, _BURST_LABEL_RE$exec;
    return typeof ((_event$event = event.event) === null || _event$event === void 0 || (_event$event = _event$event.payload) === null || _event$event === void 0 ? void 0 : _event$event.text) === "string" ? (_BURST_LABEL_RE$exec = BURST_LABEL_RE.exec(event.event.payload.text)) === null || _BURST_LABEL_RE$exec === void 0 ? void 0 : _BURST_LABEL_RE$exec[1] : undefined;
  }).filter(value => Boolean(value)).map(Number);
  return Math.max(0, ...labels);
}

/** Event labels (`journal_burst event N`, bold underscores rendered away). */
function burstLabels(page) {
  return page.getByTestId("transcript-row").evaluateAll(rows => rows.map(row => {
    var _RegExp$exec, _row$textContent;
    return (_RegExp$exec = new RegExp("journal_burst_* event (\\d+)\\b").exec((_row$textContent = row.textContent) !== null && _row$textContent !== void 0 ? _row$textContent : "")) === null || _RegExp$exec === void 0 ? void 0 : _RegExp$exec[1];
  }).filter(value => Boolean(value)).map(Number));
}

/** Viewport offset of the transcript row carrying burst event `n`. */
async function rowOffset(page, scroller, n) {
  return scroller.evaluate((el, label) => {
    const re = new RegExp(`journal_burst_* event ${label}\\b`);
    const row = Array.from(el.querySelectorAll("[data-testid='transcript-row']")).find(candidate => {
      var _candidate$textConten;
      return re.test((_candidate$textConten = candidate.textContent) !== null && _candidate$textConten !== void 0 ? _candidate$textConten : "");
    });
    if (!row) return null;
    // Scroller-relative offset — the same basis the component's scroll
    // restore uses, so a converged anchor compares equal here.
    return {
      scrollTop: el.scrollTop,
      offset: row.getBoundingClientRect().top - el.getBoundingClientRect().top
    };
  }, n);
}
test("a bounded tail window pages older rows and descends a resync gap to live", async ({
  page,
  browser
}) => {
  test.setTimeout(240000);
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
    const w = window;
    w.__frameLog = [];
    w.__frameCount = 0;
    const NativeWS = window.WebSocket;
    class GatedWS extends NativeWS {
      constructor(url, protocols) {
        super(url, protocols);
        this.addEventListener("message", ev => {
          if (typeof ev.data !== "string") return;
          try {
            var _msg$fromSeq, _w$__frameCount;
            const msg = JSON.parse(ev.data);
            if (msg.type === "snapshot") w.__frameLog.push(`snapshot:${(_msg$fromSeq = msg.fromSeq) !== null && _msg$fromSeq !== void 0 ? _msg$fromSeq : ""}`);else if (msg.type === "event") w.__frameCount = ((_w$__frameCount = w.__frameCount) !== null && _w$__frameCount !== void 0 ? _w$__frameCount : 0) + 1;else if (msg.type === "gap") w.__frameLog.push("gap");
          } catch {
            // non-JSON
          }
          if (w.__followGate) ev.stopImmediatePropagation();
        }, {
          capture: true
        });
      }
    }
    window.WebSocket = GatedWS;
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
    timeout: 30000
  });
  const transcript = page.getByTestId("transcript");
  // `__journal_burst__` is Markdown bold; it renders without the wrapping __.
  await expect(transcript).toContainText(`journal_burst event ${newestLabel}`);
  const loadEarlier = page.getByTestId("load-earlier");
  await expect(loadEarlier).toBeVisible();

  // The component pins the topmost rendered row (virtual window start) at
  // scrollTop 0; that is the anchor load-earlier holds, so use it too.
  const scroller = page.getByTestId("transcript-scroller");
  await scroller.evaluate(el => {
    el.scrollTop = 0;
    el.dispatchEvent(new Event("scroll", {
      bubbles: true
    }));
  });
  await page.waitForTimeout(300);
  const labelsBefore = await burstLabels(page);
  expect(labelsBefore.length).toBeGreaterThan(8);
  const anchorLabel = labelsBefore[0];
  await page.waitForTimeout(200);
  const anchorBefore = await rowOffset(page, scroller, anchorLabel);
  expect(anchorBefore).not.toBeNull();

  // One click fetches exactly one bounded older page (beforeSeq).
  const beforeSeqRequests = [];
  page.on("request", request => {
    const url = new URL(request.url());
    if (url.pathname === `/v1/instances/${instanceId}/journal` && url.searchParams.has("beforeSeq")) {
      beforeSeqRequests.push(url.searchParams.get("beforeSeq"));
    }
  });
  const olderResponsePromise = page.waitForResponse(response => response.request().method() === "GET" && new URL(response.url()).searchParams.has("beforeSeq"), {
    timeout: 15000
  });
  await loadEarlier.click();
  const olderResponse = await olderResponsePromise;
  expect(olderResponse.ok()).toBe(true);
  const olderBody = await olderResponse.json();

  // The anchor row stays pinned at its old viewport offset while scrollTop
  // grows by the prepended window height.
  await expect.poll(async () => {
    const pos = await rowOffset(page, scroller, anchorLabel);
    return pos === null || anchorBefore === null ? null : Math.abs(pos.offset - anchorBefore.offset);
  }, {
    timeout: 10000,
    intervals: [100, 200]
  }).toBeLessThanOrEqual(4);
  const anchorScrollAfter = await scroller.evaluate(el => el.scrollTop);
  expect(anchorScrollAfter).toBeGreaterThan(anchorBefore.scrollTop);

  // D-053: zero per-row drift. Row spacing is padding inside the measured
  // box (no outside margin, no +12 fudge), so consecutive mounted rows are
  // contiguous: each row's slot is exactly its rendered height.
  const gaps = await scroller.evaluate(el => {
    const rows = Array.from(el.querySelectorAll('[data-testid="transcript-row"]'));
    const out = [];
    for (let i = 1; i < rows.length; i += 1) {
      const prev = rows[i - 1].getBoundingClientRect();
      out.push(Math.round((rows[i].getBoundingClientRect().top - prev.bottom) * 100) / 100);
    }
    return out;
  });
  expect(gaps.length).toBeGreaterThan(0);
  for (const gap of gaps) expect(Math.abs(gap)).toBeLessThanOrEqual(0.5);

  // At the top, an older burst window renders in ascending seq order.
  await scroller.evaluate(el => {
    el.scrollTop = 0;
    el.dispatchEvent(new Event("scroll", {
      bubbles: true
    }));
  });
  await expect.poll(() => burstLabels(page).then(labels => labels[0]), {
    timeout: 10000
  }).toBeLessThan(labelsBefore[0]);
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
    const w = window;
    w.__journalStates = [];
    w.__journalStatusSamples = [];
    w.__followFrames = [];
    const recordBanner = () => {
      var _banner$getAttribute;
      const banner = document.querySelector("[data-testid='journal-banner']");
      if (banner) w.__journalStates.push((_banner$getAttribute = banner.getAttribute("data-state")) !== null && _banner$getAttribute !== void 0 ? _banner$getAttribute : "");
    };
    new MutationObserver(recordBanner).observe(document.body, {
      attributes: true,
      subtree: true,
      childList: true
    });
    window.setInterval(() => {
      var _document$querySelect;
      const state = (_document$querySelect = document.querySelector("[data-testid='session-page']")) === null || _document$querySelect === void 0 ? void 0 : _document$querySelect.getAttribute("data-journal");
      const samples = w.__journalStatusSamples;
      if (state && state !== samples[samples.length - 1]) samples.push(state);
    }, 75);
  });

  // Fill-descend reads after this point are resync fills, not the manual click.
  const fillBefore = beforeSeqRequests.length;
  const allJournalRequests = [];
  page.on("request", request => {
    const url = new URL(request.url());
    if (url.pathname === `/v1/instances/${instanceId}/journal`) {
      allJournalRequests.push(`${request.method()} ${url.search}`);
    }
  });
  const beforeResyncDurable = Number(seeded.durableSeq);
  // Gate every follow frame during the big burst so the applied cursor cannot
  // chase it; the Hub keeps overflowing and resyncing, all dropped client-side.
  await page.evaluate(() => {
    window.__followGate = true;
  });
  await burst(driverPage, instanceId, BURST_COUNT);
  await waitDurable(driverPage, instanceId, beforeResyncDurable + BURST_COUNT);
  // Reopen and send a small burst. Its live frames are not replayed from the
  // gated gap, so the first delivered batch starts far above the pinned
  // cursor and the client descends the missing windows with beforeSeq.
  await page.evaluate(() => {
    window.__followGate = false;
  });
  await burst(driverPage, instanceId, 200);
  await waitDurable(driverPage, instanceId, beforeResyncDurable + BURST_COUNT + 200 + 1);
  // The newest label is burst-relative and lands with the trailing idle frame.
  const finalWindow = await journalJson(driverPage, instanceId);
  const resyncNewestLabel = maxBurstLabel(finalWindow);

  // The client descends the bounded resync window with beforeSeq and settles
  // back to live with the newest turn rendered.
  await expect(page.getByTestId("session-page")).toHaveAttribute("data-journal", "live", {
    timeout: 90000
  });
  await expect(page.getByTestId("journal-banner")).toHaveCount(0);
  const {
    sampled,
    observedBanners,
    frames,
    liveCount
  } = await page.evaluate(() => {
    var _w$__journalStatusSam, _w$__journalStates, _w$__frameLog, _w$__frameCount2;
    const w = window;
    return {
      sampled: (_w$__journalStatusSam = w.__journalStatusSamples) !== null && _w$__journalStatusSam !== void 0 ? _w$__journalStatusSam : [],
      observedBanners: (_w$__journalStates = w.__journalStates) !== null && _w$__journalStates !== void 0 ? _w$__journalStates : [],
      frames: (_w$__frameLog = w.__frameLog) !== null && _w$__frameLog !== void 0 ? _w$__frameLog : [],
      liveCount: (_w$__frameCount2 = w.__frameCount) !== null && _w$__frameCount2 !== void 0 ? _w$__frameCount2 : 0
    };
  });
  // The banner passed through gap-backfill on the way back to live.
  expect([...sampled, ...observedBanners], `expected gap-backfill; fillReads=${beforeSeqRequests.length - fillBefore} sampled=${JSON.stringify(sampled)} banners=${JSON.stringify(observedBanners)} frames=${JSON.stringify(frames)} live=${liveCount} journal=${JSON.stringify(allJournalRequests.slice(-12))}`).toContain("gap-backfill");
  // A resync gap fill descended with beforeSeq (beyond the manual click).
  expect(beforeSeqRequests.length, `expected a descending fill read; sampled=${JSON.stringify(sampled)} frames=${JSON.stringify(frames)} journal=${JSON.stringify(allJournalRequests.slice(-12))}`).toBeGreaterThan(fillBefore);
  // The manual paging left the viewport at the top of history; the newest turn
  // lives at the tail, so jump there before asserting it rendered.
  const jumpLatest = page.getByTestId("jump-latest");
  await expect.poll(async () => {
    if (await jumpLatest.isVisible().catch(() => false)) {
      await jumpLatest.click().catch(() => undefined);
    }
    return transcript.textContent();
  }, {
    timeout: 15000,
    intervals: [200, 500]
  }).toContain(`journal_burst event ${resyncNewestLabel}`);
});
//# sourceMappingURL=data:application/json;charset=utf-8;base64,eyJ2ZXJzaW9uIjozLCJuYW1lcyI6WyJleHBlY3QiLCJ0ZXN0IiwibG9naW4iLCJkZXNjcmliZSIsImNvbmZpZ3VyZSIsIm1vZGUiLCJza2lwIiwicHJvY2VzcyIsImVudiIsIkhVQl9FMkVfRVhURVJOQUwiLCJCVVJTVF9DT1VOVCIsImRyaXZlciIsInBhdGNoTWF4SW5zdGFuY2VzIiwicGFnZSIsInZhbHVlIiwiZXZhbHVhdGUiLCJuZXh0IiwiX2JvZHkkaXRlbXMiLCJsaXN0IiwiZmV0Y2giLCJjcmVkZW50aWFscyIsImJvZHkiLCJqc29uIiwiaWQiLCJpdGVtcyIsImZpbmQiLCJob3N0IiwiaG9zdElkIiwibWV0aG9kIiwiaGVhZGVycyIsIkpTT04iLCJzdHJpbmdpZnkiLCJtYXhJbnN0YW5jZXMiLCJmb3JjZURlbGV0ZUFsbEluc3RhbmNlcyIsIl9ib2R5JGl0ZW1zMiIsIlByb21pc2UiLCJhbGwiLCJmaWx0ZXIiLCJpbnN0YW5jZSIsImluc3RhbmNlSWQiLCJtYXAiLCJjYXRjaCIsInVuZGVmaW5lZCIsImJlZm9yZUFsbCIsImJyb3dzZXIiLCJzZXR1cCIsIm5ld1BhZ2UiLCJjbG9zZSIsImFmdGVyQWxsIiwiYWZ0ZXJFYWNoIiwiY2xlYW51cCIsIl9kcml2ZXIiLCJjb250ZXh0IiwiZHJpdmVyQ29udGV4dCIsIm5ld0NvbnRleHQiLCJjcmVhdGVJbnN0YW5jZVJlc3QiLCJfaG9zdHMkaXRlbXMiLCJob3N0cyIsInJlc3BvbnNlIiwidG9CZVRydXRoeSIsImNyZWF0ZWQiLCJ3b3Jrc3BhY2VJZCIsImtpbmQiLCJwcm9tcHQiLCJhbnN3ZXJQZW5kaW5nUmVzdCIsImF0dGVtcHQiLCJvcHRpb24iLCJfYm9keSRpdGVtczMiLCJfcGVuZGluZyRyZXF1ZXN0IiwiX3BlbmRpbmckcmVxdWVzdCRpbnB1IiwiX3BlbmRpbmckcmVxdWVzdDIiLCJwZW5kaW5nIiwiaXRlbSIsInN0YXRlIiwib3B0aW9uSWQiLCJyZXF1ZXN0Iiwib3B0aW9ucyIsImFuc3dlciIsImlucHV0RGlnZXN0Iiwid2FpdEZvclRpbWVvdXQiLCJFcnJvciIsImJ1cnN0IiwiY291bnQiLCJyZXN1bHQiLCJvcGVyYXRpb24iLCJwYXlsb2FkIiwib2siLCJ0b0JlIiwid2FpdER1cmFibGUiLCJtaW4iLCJwb2xsIiwiTnVtYmVyIiwiZHVyYWJsZVNlcSIsInRpbWVvdXQiLCJpbnRlcnZhbHMiLCJ0b0JlR3JlYXRlclRoYW5PckVxdWFsIiwiam91cm5hbEpzb24iLCJCVVJTVF9MQUJFTF9SRSIsIm1heEJ1cnN0TGFiZWwiLCJsYWJlbHMiLCJldmVudHMiLCJldmVudCIsIl9ldmVudCRldmVudCIsIl9CVVJTVF9MQUJFTF9SRSRleGVjIiwidGV4dCIsImV4ZWMiLCJCb29sZWFuIiwiTWF0aCIsIm1heCIsImJ1cnN0TGFiZWxzIiwiZ2V0QnlUZXN0SWQiLCJldmFsdWF0ZUFsbCIsInJvd3MiLCJyb3ciLCJfUmVnRXhwJGV4ZWMiLCJfcm93JHRleHRDb250ZW50IiwiUmVnRXhwIiwidGV4dENvbnRlbnQiLCJyb3dPZmZzZXQiLCJzY3JvbGxlciIsIm4iLCJlbCIsImxhYmVsIiwicmUiLCJBcnJheSIsImZyb20iLCJxdWVyeVNlbGVjdG9yQWxsIiwiY2FuZGlkYXRlIiwiX2NhbmRpZGF0ZSR0ZXh0Q29udGVuIiwic2Nyb2xsVG9wIiwib2Zmc2V0IiwiZ2V0Qm91bmRpbmdDbGllbnRSZWN0IiwidG9wIiwic2V0VGltZW91dCIsImFkZEluaXRTY3JpcHQiLCJ3Iiwid2luZG93IiwiX19mcmFtZUxvZyIsIl9fZnJhbWVDb3VudCIsIk5hdGl2ZVdTIiwiV2ViU29ja2V0IiwiR2F0ZWRXUyIsImNvbnN0cnVjdG9yIiwidXJsIiwicHJvdG9jb2xzIiwiYWRkRXZlbnRMaXN0ZW5lciIsImV2IiwiZGF0YSIsIl9tc2ckZnJvbVNlcSIsIl93JF9fZnJhbWVDb3VudCIsIm1zZyIsInBhcnNlIiwidHlwZSIsInB1c2giLCJmcm9tU2VxIiwiX19mb2xsb3dHYXRlIiwic3RvcEltbWVkaWF0ZVByb3BhZ2F0aW9uIiwiY2FwdHVyZSIsImRyaXZlclBhZ2UiLCJzZWVkZWQiLCJub3QiLCJ0b0JlTnVsbCIsInRvQmVHcmVhdGVyVGhhbiIsInJlYWNoZWRBZnRlclNlcSIsIm5ld2VzdExhYmVsIiwiZ290byIsInRvSGF2ZUF0dHJpYnV0ZSIsInRyYW5zY3JpcHQiLCJ0b0NvbnRhaW5UZXh0IiwibG9hZEVhcmxpZXIiLCJ0b0JlVmlzaWJsZSIsImRpc3BhdGNoRXZlbnQiLCJFdmVudCIsImJ1YmJsZXMiLCJsYWJlbHNCZWZvcmUiLCJsZW5ndGgiLCJhbmNob3JMYWJlbCIsImFuY2hvckJlZm9yZSIsImJlZm9yZVNlcVJlcXVlc3RzIiwib24iLCJVUkwiLCJwYXRobmFtZSIsInNlYXJjaFBhcmFtcyIsImhhcyIsImdldCIsIm9sZGVyUmVzcG9uc2VQcm9taXNlIiwid2FpdEZvclJlc3BvbnNlIiwiY2xpY2siLCJvbGRlclJlc3BvbnNlIiwib2xkZXJCb2R5IiwicG9zIiwiYWJzIiwidG9CZUxlc3NUaGFuT3JFcXVhbCIsImFuY2hvclNjcm9sbEFmdGVyIiwiZ2FwcyIsIm91dCIsImkiLCJwcmV2Iiwicm91bmQiLCJib3R0b20iLCJnYXAiLCJ0aGVuIiwidG9CZUxlc3NUaGFuIiwibGFiZWxzQWZ0ZXJGaXJzdENsaWNrIiwiX19qb3VybmFsU3RhdGVzIiwiX19qb3VybmFsU3RhdHVzU2FtcGxlcyIsIl9fZm9sbG93RnJhbWVzIiwicmVjb3JkQmFubmVyIiwiX2Jhbm5lciRnZXRBdHRyaWJ1dGUiLCJiYW5uZXIiLCJkb2N1bWVudCIsInF1ZXJ5U2VsZWN0b3IiLCJnZXRBdHRyaWJ1dGUiLCJNdXRhdGlvbk9ic2VydmVyIiwib2JzZXJ2ZSIsImF0dHJpYnV0ZXMiLCJzdWJ0cmVlIiwiY2hpbGRMaXN0Iiwic2V0SW50ZXJ2YWwiLCJfZG9jdW1lbnQkcXVlcnlTZWxlY3QiLCJzYW1wbGVzIiwiZmlsbEJlZm9yZSIsImFsbEpvdXJuYWxSZXF1ZXN0cyIsInNlYXJjaCIsImJlZm9yZVJlc3luY0R1cmFibGUiLCJmaW5hbFdpbmRvdyIsInJlc3luY05ld2VzdExhYmVsIiwidG9IYXZlQ291bnQiLCJzYW1wbGVkIiwib2JzZXJ2ZWRCYW5uZXJzIiwiZnJhbWVzIiwibGl2ZUNvdW50IiwiX3ckX19qb3VybmFsU3RhdHVzU2FtIiwiX3ckX19qb3VybmFsU3RhdGVzIiwiX3ckX19mcmFtZUxvZyIsIl93JF9fZnJhbWVDb3VudDIiLCJzbGljZSIsInRvQ29udGFpbiIsImp1bXBMYXRlc3QiLCJpc1Zpc2libGUiXSwic291cmNlcyI6WyJqb3VybmFsLXdpbmRvdy5odWIuc3BlYy50cyJdLCJzb3VyY2VzQ29udGVudCI6WyJpbXBvcnQgeyBleHBlY3QsIHRlc3QsIHR5cGUgQnJvd3NlciwgdHlwZSBCcm93c2VyQ29udGV4dCwgdHlwZSBQYWdlIH0gZnJvbSBcIkBwbGF5d3JpZ2h0L3Rlc3RcIjtcbmltcG9ydCB7IGxvZ2luIH0gZnJvbSBcIi4vaHViLWF1dGhcIjtcblxuLyoqXG4gKiBCb3VuZGVkIGpvdXJuYWwgd2luZG93cyAoaHViLXN0b3JlLTEgLyBjLWpvdXJuYWxwYWdlKS5cbiAqXG4gKiBHRVQgL3YxL2luc3RhbmNlcy86aWQvam91cm5hbCBhbmQgdGhlIGZvbGxvdyBzbmFwc2hvdCBhcmUgYSBib3VuZGVkIFRBSUxcbiAqIHdpbmRvdyAoYXQgbW9zdCAyMDAwIHJvd3MgLyA4IE1pQikuIFRoZSBvbGQgd2ViIGNsaWVudCBhc3N1bWVkIGFuIHVuYm91bmRlZFxuICogcmVhZDogYW4gYXNjZW5kaW5nIDUxMi1sb29wIHNpbGVudGx5IGRyb3BwZWQgdGhlIG1pZGRsZSBvZiBhIGxvbmcgam91cm5hbCxcbiAqIGFuZCBhIHJlc3luYyBzbmFwc2hvdCB3aG9zZSBmbG9vciBzYXQgYWJvdmUgdGhlIGFwcGxpZWQgY3Vyc29yIGxlZnQgdGhlXG4gKiBiYW5uZXIgZ2FwLWJhY2tmaWxsaW5nIGZvcmV2ZXIuXG4gKlxuICogRmFrZSBOb2RlIG9ubHkg4oCUIGBfX2pvdXJuYWxfYnVyc3RfXzo8bj5gIChhIHNlZWRpbmcgaG9vayBvZmYgZXZlcnkgc2NyaXB0ZWRcbiAqIHBhdGgpIGFwcGVuZHMgbiBhc3Npc3RhbnQgbWVzc2FnZXMgaW4gb25lIGJhdGNoZWQgam91cm5hbC5hcHBlbmQuXG4gKlxuICogU2NlbmFyaW86XG4gKiAgIDEuIGEgc2Vjb25kIGNvbnRleHQgc2VlZHMgfjUwMDAgZXZlbnRzIEJFRk9SRSB0aGUgdGVzdCBwYWdlIGF0dGFjaGVzIChsYXRlXG4gKiAgICAgIGF0dGFjaCk6IHRoZSB0YWlsIHdpbmRvdyBmbG9vciBpcyBhYm92ZSAxLCB0aGUgbmV3ZXN0IHR1cm4gcmVuZGVycywgYW5kXG4gKiAgICAgIGxvYWQtZWFybGllciBwYWdlcyBvbGRlciB3aW5kb3dzO1xuICogICAyLiB0aGUgZm9sbG93ZXIgbGluayBpcyB0aHJvdHRsZWQgd2hpbGUgYSBzZWNvbmQgNTAwMC1ldmVudCBidXJzdCBsYW5kcywgc29cbiAqICAgICAgdGhlIEh1YidzIGZvbGxvdyBidWZmZXIgb3ZlcmZsb3dzIGFuZCBpdCBxdWV1ZXMgYSBnYXAgKyBib3VuZGVkIHJlc3luY1xuICogICAgICBzbmFwc2hvdDogdGhlIGNsaWVudCBkZXNjZW5kcyB3aXRoIGJlZm9yZVNlcSwgdGhlIGJhbm5lciBwYXNzZXNcbiAqICAgICAgZ2FwLWJhY2tmaWxsIGFuZCByZXR1cm5zIHRvIGxpdmUuXG4gKi9cblxudGVzdC5kZXNjcmliZS5jb25maWd1cmUoeyBtb2RlOiBcInNlcmlhbFwiIH0pO1xuXG50ZXN0LnNraXAocHJvY2Vzcy5lbnYuSFVCX0UyRV9FWFRFUk5BTCA9PT0gXCIxXCIsIFwiTmVlZHMgdGhlIGluLXByb2Nlc3MgZmFrZSBOb2RlXCIpO1xuXG5jb25zdCBCVVJTVF9DT1VOVCA9IDUwMDA7XG5cbi8qKiBTZWVkaW5nIGNvbnRleHQsIGNsb3NlZCBieSB0aGUgZGVzY3JpYmUtbGV2ZWwgaG9vayAobXVzdCBzdGF5IHVudGhyb3R0bGVkKS4gKi9cbmxldCBkcml2ZXI6IHsgY29udGV4dDogQnJvd3NlckNvbnRleHQ7IHBhZ2U6IFBhZ2UgfSB8IG51bGwgPSBudWxsO1xuXG5hc3luYyBmdW5jdGlvbiBwYXRjaE1heEluc3RhbmNlcyhwYWdlOiBQYWdlLCB2YWx1ZTogbnVtYmVyKSB7XG4gIGF3YWl0IHBhZ2UuZXZhbHVhdGUoYXN5bmMgKG5leHQpID0+IHtcbiAgICBjb25zdCBsaXN0ID0gYXdhaXQgZmV0Y2goXCIvdjEvaG9zdHNcIiwgeyBjcmVkZW50aWFsczogXCJpbmNsdWRlXCIgfSk7XG4gICAgY29uc3QgYm9keSA9IChhd2FpdCBsaXN0Lmpzb24oKSkgYXMge1xuICAgICAgaXRlbXM/OiB7IGhvc3RJZD86IHN0cmluZzsgbWF4SW5zdGFuY2VzPzogbnVtYmVyIH1bXTtcbiAgICB9O1xuICAgIGNvbnN0IGlkID0gYm9keS5pdGVtcz8uZmluZCgoaG9zdCkgPT4gaG9zdC5ob3N0SWQpPy5ob3N0SWQ7XG4gICAgaWYgKCFpZCkgcmV0dXJuO1xuICAgIGF3YWl0IGZldGNoKGAvdjEvaG9zdHMvJHtpZH1gLCB7XG4gICAgICBtZXRob2Q6IFwiUEFUQ0hcIixcbiAgICAgIGNyZWRlbnRpYWxzOiBcImluY2x1ZGVcIixcbiAgICAgIGhlYWRlcnM6IHsgXCJjb250ZW50LXR5cGVcIjogXCJhcHBsaWNhdGlvbi9qc29uXCIgfSxcbiAgICAgIGJvZHk6IEpTT04uc3RyaW5naWZ5KHsgbWF4SW5zdGFuY2VzOiBuZXh0IH0pLFxuICAgIH0pO1xuICB9LCB2YWx1ZSk7XG59XG5cbmFzeW5jIGZ1bmN0aW9uIGZvcmNlRGVsZXRlQWxsSW5zdGFuY2VzKHBhZ2U6IFBhZ2UpIHtcbiAgYXdhaXQgcGFnZS5ldmFsdWF0ZShhc3luYyAoKSA9PiB7XG4gICAgY29uc3QgbGlzdCA9IGF3YWl0IGZldGNoKFwiL3YxL2luc3RhbmNlc1wiLCB7IGNyZWRlbnRpYWxzOiBcImluY2x1ZGVcIiB9KTtcbiAgICBjb25zdCBib2R5ID0gKGF3YWl0IGxpc3QuanNvbigpKSBhcyB7XG4gICAgICBpdGVtcz86IHsgaW5zdGFuY2VJZD86IHN0cmluZzsgbGlmZWN5Y2xlPzogc3RyaW5nIH1bXTtcbiAgICB9O1xuICAgIGF3YWl0IFByb21pc2UuYWxsKFxuICAgICAgKGJvZHkuaXRlbXMgPz8gW10pXG4gICAgICAgIC5maWx0ZXIoKGluc3RhbmNlKSA9PiBpbnN0YW5jZS5pbnN0YW5jZUlkKVxuICAgICAgICAubWFwKGFzeW5jIChpbnN0YW5jZSkgPT4ge1xuICAgICAgICAgIGF3YWl0IGZldGNoKGAvdjEvaW5zdGFuY2VzLyR7aW5zdGFuY2UuaW5zdGFuY2VJZH0/Zm9yY2U9MWAsIHtcbiAgICAgICAgICAgIG1ldGhvZDogXCJERUxFVEVcIixcbiAgICAgICAgICAgIGNyZWRlbnRpYWxzOiBcImluY2x1ZGVcIixcbiAgICAgICAgICB9KS5jYXRjaCgoKSA9PiB1bmRlZmluZWQpO1xuICAgICAgICB9KSxcbiAgICApO1xuICB9KTtcbn1cblxudGVzdC5iZWZvcmVBbGwoYXN5bmMgKHsgYnJvd3NlciB9KSA9PiB7XG4gIGNvbnN0IHNldHVwID0gYXdhaXQgYnJvd3Nlci5uZXdQYWdlKCk7XG4gIGF3YWl0IGxvZ2luKHNldHVwKTtcbiAgYXdhaXQgcGF0Y2hNYXhJbnN0YW5jZXMoc2V0dXAsIDI0KTtcbiAgYXdhaXQgc2V0dXAuY2xvc2UoKTtcbn0pO1xuXG50ZXN0LmFmdGVyQWxsKGFzeW5jICh7IGJyb3dzZXIgfSkgPT4ge1xuICBjb25zdCBzZXR1cCA9IGF3YWl0IGJyb3dzZXIubmV3UGFnZSgpO1xuICBhd2FpdCBsb2dpbihzZXR1cCk7XG4gIGF3YWl0IHBhdGNoTWF4SW5zdGFuY2VzKHNldHVwLCA4KTtcbiAgYXdhaXQgZm9yY2VEZWxldGVBbGxJbnN0YW5jZXMoc2V0dXApO1xuICBhd2FpdCBzZXR1cC5jbG9zZSgpO1xufSk7XG5cbnRlc3QuYWZ0ZXJFYWNoKGFzeW5jICh7IGJyb3dzZXIgfSkgPT4ge1xuICAvLyBUaGUgZHJpdmluZyBjb250ZXh0IGlzIGluZGVwZW5kZW50IG9mIHRoZSB0aHJvdHRsZWQgZm9sbG93ZXIgY29udGV4dCwgc29cbiAgLy8gY2xlYW4gdXAgd2l0aCBhIGZyZXNoIHBhZ2UgcmF0aGVyIHRoYW4gdGhlIHRocm90dGxlZCBvbmUuXG4gIGNvbnN0IGNsZWFudXAgPSBhd2FpdCBicm93c2VyLm5ld1BhZ2UoKTtcbiAgdHJ5IHtcbiAgICBhd2FpdCBsb2dpbihjbGVhbnVwKTtcbiAgICBhd2FpdCBmb3JjZURlbGV0ZUFsbEluc3RhbmNlcyhjbGVhbnVwKTtcbiAgfSBmaW5hbGx5IHtcbiAgICBhd2FpdCBjbGVhbnVwLmNsb3NlKCk7XG4gIH1cbn0pO1xuXG50ZXN0LmFmdGVyQWxsKGFzeW5jICgpID0+IHtcbiAgYXdhaXQgZHJpdmVyPy5jb250ZXh0LmNsb3NlKCkuY2F0Y2goKCkgPT4gdW5kZWZpbmVkKTtcbiAgZHJpdmVyID0gbnVsbDtcbn0pO1xuXG4vKiogSW5kZXBlbmRlbnQgYnJvd3NlciBjb250ZXh0IHRoYXQgZHJpdmVzIHRoZSBub2RlIHdoaWxlIHRoZSB0YWIgaXMgc2xvdy4gKi9cbmFzeW5jIGZ1bmN0aW9uIGRyaXZlckNvbnRleHQoYnJvd3NlcjogQnJvd3Nlcikge1xuICBjb25zdCBjb250ZXh0ID0gYXdhaXQgYnJvd3Nlci5uZXdDb250ZXh0KCk7XG4gIGNvbnN0IHBhZ2UgPSBhd2FpdCBjb250ZXh0Lm5ld1BhZ2UoKTtcbiAgYXdhaXQgbG9naW4ocGFnZSwgXCJlMmUtd2luZG93LWRyaXZlclwiKTtcbiAgcmV0dXJuIHsgY29udGV4dCwgcGFnZSB9O1xufVxuXG5hc3luYyBmdW5jdGlvbiBjcmVhdGVJbnN0YW5jZVJlc3QocGFnZTogUGFnZSk6IFByb21pc2U8c3RyaW5nPiB7XG4gIGNvbnN0IGhvc3RzID0gYXdhaXQgcGFnZS5ldmFsdWF0ZShhc3luYyAoKSA9PiB7XG4gICAgY29uc3QgcmVzcG9uc2UgPSBhd2FpdCBmZXRjaChcIi92MS9ob3N0c1wiLCB7IGNyZWRlbnRpYWxzOiBcImluY2x1ZGVcIiB9KTtcbiAgICByZXR1cm4gKGF3YWl0IHJlc3BvbnNlLmpzb24oKSkgYXMgeyBpdGVtcz86IHsgaG9zdElkPzogc3RyaW5nIH1bXSB9O1xuICB9KTtcbiAgY29uc3QgaG9zdElkID0gaG9zdHMuaXRlbXM/LmZpbmQoKGhvc3QpID0+IGhvc3QuaG9zdElkKT8uaG9zdElkO1xuICBleHBlY3QoaG9zdElkKS50b0JlVHJ1dGh5KCk7XG4gIGNvbnN0IGNyZWF0ZWQgPSBhd2FpdCBwYWdlLmV2YWx1YXRlKGFzeW5jIChpZCkgPT4ge1xuICAgIGNvbnN0IHJlc3BvbnNlID0gYXdhaXQgZmV0Y2goXCIvdjEvaW5zdGFuY2VzXCIsIHtcbiAgICAgIG1ldGhvZDogXCJQT1NUXCIsXG4gICAgICBjcmVkZW50aWFsczogXCJpbmNsdWRlXCIsXG4gICAgICBoZWFkZXJzOiB7IFwiY29udGVudC10eXBlXCI6IFwiYXBwbGljYXRpb24vanNvblwiIH0sXG4gICAgICBib2R5OiBKU09OLnN0cmluZ2lmeSh7XG4gICAgICAgIGhvc3RJZDogaWQsXG4gICAgICAgIHdvcmtzcGFjZUlkOiBcIi90bXBcIixcbiAgICAgICAga2luZDogXCJjbGF1ZGVcIixcbiAgICAgICAgLy8gY2xhdWRlLXByaW50IHRha2VzIHRoZSBmYWtlIG5vZGUncyBnZW5lcmljIGFwcHJvdmFsLWNhcmQgY3JlYXRlIGFybVxuICAgICAgICAvLyAodGhlIFJFU1QgZGVmYXVsdCBjbGF1ZGUtcHR5IHdvdWxkIHNraXAgaXQpLlxuICAgICAgICBkcml2ZXI6IFwiY2xhdWRlLXByaW50XCIsXG4gICAgICAgIHByb21wdDogXCJqb3VybmFsIHdpbmRvdyBzZXNzaW9uXCIsXG4gICAgICB9KSxcbiAgICB9KTtcbiAgICByZXR1cm4gcmVzcG9uc2UuanNvbigpIGFzIFByb21pc2U8eyBpbnN0YW5jZTogeyBpbnN0YW5jZUlkOiBzdHJpbmcgfSB9PjtcbiAgfSwgaG9zdElkISk7XG4gIHJldHVybiBjcmVhdGVkLmluc3RhbmNlLmluc3RhbmNlSWQ7XG59XG5cbi8qKiBUaGUgY3JlYXRlIGFwcHJvdmFsIGRpc2FibGVzIHNlbmRzIHVudGlsIGFuc3dlcmVkOyBhbnN3ZXIgaXQgb3ZlciBSRVNULiAqL1xuYXN5bmMgZnVuY3Rpb24gYW5zd2VyUGVuZGluZ1Jlc3QocGFnZTogUGFnZSwgaW5zdGFuY2VJZDogc3RyaW5nKSB7XG4gIGZvciAobGV0IGF0dGVtcHQgPSAwOyBhdHRlbXB0IDwgMzA7IGF0dGVtcHQgKz0gMSkge1xuICAgIGNvbnN0IG9wdGlvbiA9IGF3YWl0IHBhZ2UuZXZhbHVhdGUoYXN5bmMgKGlkKSA9PiB7XG4gICAgICBjb25zdCBsaXN0ID0gYXdhaXQgZmV0Y2goXCIvdjEvaW50ZXJhY3Rpb25zXCIsIHsgY3JlZGVudGlhbHM6IFwiaW5jbHVkZVwiIH0pO1xuICAgICAgY29uc3QgYm9keSA9IChhd2FpdCBsaXN0Lmpzb24oKSkgYXMge1xuICAgICAgICBpdGVtcz86IHtcbiAgICAgICAgICBpZDogc3RyaW5nO1xuICAgICAgICAgIGluc3RhbmNlSWQ/OiBzdHJpbmc7XG4gICAgICAgICAgc3RhdGU/OiBzdHJpbmc7XG4gICAgICAgICAgcmVxdWVzdD86IHsgaW5wdXREaWdlc3Q/OiBzdHJpbmc7IG9wdGlvbnM/OiB7IGlkOiBzdHJpbmcgfVtdIH07XG4gICAgICAgIH1bXTtcbiAgICAgIH07XG4gICAgICBjb25zdCBwZW5kaW5nID0gKGJvZHkuaXRlbXMgPz8gW10pLmZpbmQoXG4gICAgICAgIChpdGVtKSA9PiBpdGVtLmluc3RhbmNlSWQgPT09IGlkICYmIGl0ZW0uc3RhdGUgPT09IFwicGVuZGluZ1wiLFxuICAgICAgKTtcbiAgICAgIGNvbnN0IG9wdGlvbklkID0gcGVuZGluZz8ucmVxdWVzdD8ub3B0aW9ucz8uWzBdPy5pZDtcbiAgICAgIGlmICghcGVuZGluZyB8fCAhb3B0aW9uSWQpIHJldHVybiBudWxsO1xuICAgICAgYXdhaXQgZmV0Y2goYC92MS9pbnRlcmFjdGlvbnMvJHtwZW5kaW5nLmlkfS9hbnN3ZXJgLCB7XG4gICAgICAgIG1ldGhvZDogXCJQT1NUXCIsXG4gICAgICAgIGNyZWRlbnRpYWxzOiBcImluY2x1ZGVcIixcbiAgICAgICAgaGVhZGVyczogeyBcImNvbnRlbnQtdHlwZVwiOiBcImFwcGxpY2F0aW9uL2pzb25cIiB9LFxuICAgICAgICBib2R5OiBKU09OLnN0cmluZ2lmeSh7XG4gICAgICAgICAgYW5zd2VyOiB7XG4gICAgICAgICAgICBraW5kOiBcImFwcHJvdmFsXCIsXG4gICAgICAgICAgICBvcHRpb25JZCxcbiAgICAgICAgICAgIGlucHV0RGlnZXN0OiBwZW5kaW5nLnJlcXVlc3Q/LmlucHV0RGlnZXN0ID8/IFwiXCIsXG4gICAgICAgICAgfSxcbiAgICAgICAgfSksXG4gICAgICB9KTtcbiAgICAgIHJldHVybiBvcHRpb25JZDtcbiAgICB9LCBpbnN0YW5jZUlkKTtcbiAgICBpZiAob3B0aW9uKSByZXR1cm47XG4gICAgYXdhaXQgcGFnZS53YWl0Rm9yVGltZW91dCgyMDApO1xuICB9XG4gIHRocm93IG5ldyBFcnJvcihcImNyZWF0ZSBhcHByb3ZhbCBuZXZlciBiZWNhbWUgYW5zd2VyYWJsZVwiKTtcbn1cblxuYXN5bmMgZnVuY3Rpb24gYnVyc3QocGFnZTogUGFnZSwgaW5zdGFuY2VJZDogc3RyaW5nLCBjb3VudDogbnVtYmVyKSB7XG4gIGNvbnN0IHJlc3VsdCA9IGF3YWl0IHBhZ2UuZXZhbHVhdGUoXG4gICAgYXN5bmMgKHsgaWQsIGNvdW50IH0pID0+IHtcbiAgICAgIGNvbnN0IHJlc3BvbnNlID0gYXdhaXQgZmV0Y2goYC92MS9pbnN0YW5jZXMvJHtpZH0vY29tbWFuZHNgLCB7XG4gICAgICAgIG1ldGhvZDogXCJQT1NUXCIsXG4gICAgICAgIGNyZWRlbnRpYWxzOiBcImluY2x1ZGVcIixcbiAgICAgICAgaGVhZGVyczogeyBcImNvbnRlbnQtdHlwZVwiOiBcImFwcGxpY2F0aW9uL2pzb25cIiB9LFxuICAgICAgICBib2R5OiBKU09OLnN0cmluZ2lmeSh7XG4gICAgICAgICAgb3BlcmF0aW9uOiBcImluc3RhbmNlLnNlbmRcIixcbiAgICAgICAgICBwYXlsb2FkOiB7IHByb21wdDogYF9fam91cm5hbF9idXJzdF9fOiR7Y291bnR9YCB9LFxuICAgICAgICB9KSxcbiAgICAgIH0pO1xuICAgICAgcmV0dXJuIHJlc3BvbnNlLm9rO1xuICAgIH0sXG4gICAgeyBpZDogaW5zdGFuY2VJZCwgY291bnQgfSxcbiAgKTtcbiAgZXhwZWN0KHJlc3VsdCkudG9CZSh0cnVlKTtcbn1cblxuYXN5bmMgZnVuY3Rpb24gd2FpdER1cmFibGUocGFnZTogUGFnZSwgaW5zdGFuY2VJZDogc3RyaW5nLCBtaW46IG51bWJlcikge1xuICBhd2FpdCBleHBlY3RcbiAgICAucG9sbChcbiAgICAgIGFzeW5jICgpID0+IHtcbiAgICAgICAgdHJ5IHtcbiAgICAgICAgICByZXR1cm4gYXdhaXQgcGFnZS5ldmFsdWF0ZShhc3luYyAoaWQpID0+IHtcbiAgICAgICAgICAgIGNvbnN0IHJlc3BvbnNlID0gYXdhaXQgZmV0Y2goYC92MS9pbnN0YW5jZXMvJHtpZH0vam91cm5hbGAsIHtcbiAgICAgICAgICAgICAgY3JlZGVudGlhbHM6IFwiaW5jbHVkZVwiLFxuICAgICAgICAgICAgfSk7XG4gICAgICAgICAgICBpZiAoIXJlc3BvbnNlLm9rKSByZXR1cm4gLTE7XG4gICAgICAgICAgICByZXR1cm4gTnVtYmVyKChhd2FpdCByZXNwb25zZS5qc29uKCkpLmR1cmFibGVTZXEgYXMgc3RyaW5nKTtcbiAgICAgICAgICB9LCBpbnN0YW5jZUlkKTtcbiAgICAgICAgfSBjYXRjaCB7XG4gICAgICAgICAgLy8gQSBnYXRlZC9zdGFsbGVkIGZvbGxvdyBzb2NrZXQgY2FuIGJyaWVmbHkgc3RhcnZlIHRoZSBkZXYgcHJveHkuXG4gICAgICAgICAgcmV0dXJuIC0xO1xuICAgICAgICB9XG4gICAgICB9LFxuICAgICAgeyB0aW1lb3V0OiA5MF8wMDAsIGludGVydmFsczogWzUwMCwgMTAwMF0gfSxcbiAgICApXG4gICAgLnRvQmVHcmVhdGVyVGhhbk9yRXF1YWwobWluKTtcbn1cblxuYXN5bmMgZnVuY3Rpb24gam91cm5hbEpzb24ocGFnZTogUGFnZSwgaW5zdGFuY2VJZDogc3RyaW5nKSB7XG4gIHJldHVybiBwYWdlLmV2YWx1YXRlKGFzeW5jIChpZCkgPT4ge1xuICAgIGNvbnN0IHJlc3BvbnNlID0gYXdhaXQgZmV0Y2goYC92MS9pbnN0YW5jZXMvJHtpZH0vam91cm5hbGAsIHtcbiAgICAgIGNyZWRlbnRpYWxzOiBcImluY2x1ZGVcIixcbiAgICB9KTtcbiAgICByZXR1cm4gKGF3YWl0IHJlc3BvbnNlLmpzb24oKSkgYXMge1xuICAgICAgZHVyYWJsZVNlcTogc3RyaW5nO1xuICAgICAgZnJvbVNlcTogc3RyaW5nIHwgbnVsbDtcbiAgICAgIHJlYWNoZWRBZnRlclNlcTogYm9vbGVhbjtcbiAgICAgIGV2ZW50czogeyBzZXE6IHN0cmluZzsgZXZlbnQ/OiB7IHBheWxvYWQ/OiB7IHRleHQ/OiB1bmtub3duIH0gfSB9W107XG4gICAgfTtcbiAgfSwgaW5zdGFuY2VJZCk7XG59XG5cbi8qKlxuICogSGlnaGVzdCBidXJzdCBsYWJlbCB2aXNpYmxlIGluIHRoZSBzZXJ2ZXIgdGFpbC4gVGhlIHJhdyB0ZXh0IGlzXG4gKiBgX19qb3VybmFsX2J1cnN0X18gZXZlbnQgTmAgKHVuZGVyc2NvcmVzIHJlbmRlciBhd2F5IGFzIE1hcmtkb3duIGJvbGQgaW5cbiAqIHRoZSBET00pLCBzbyBtYXRjaCB0aGUgY29tbW9uIHdvcmQgc2hhcGUgb2YgYm90aC5cbiAqL1xuY29uc3QgQlVSU1RfTEFCRUxfUkUgPSAvam91cm5hbF9idXJzdF8qIGV2ZW50IChcXGQrKS87XG5mdW5jdGlvbiBtYXhCdXJzdExhYmVsKGJvZHk6IEF3YWl0ZWQ8UmV0dXJuVHlwZTx0eXBlb2Ygam91cm5hbEpzb24+Pik6IG51bWJlciB7XG4gIGNvbnN0IGxhYmVscyA9IGJvZHkuZXZlbnRzXG4gICAgLm1hcCgoZXZlbnQpID0+XG4gICAgICB0eXBlb2YgZXZlbnQuZXZlbnQ/LnBheWxvYWQ/LnRleHQgPT09IFwic3RyaW5nXCJcbiAgICAgICAgPyBCVVJTVF9MQUJFTF9SRS5leGVjKGV2ZW50LmV2ZW50LnBheWxvYWQudGV4dCk/LlsxXVxuICAgICAgICA6IHVuZGVmaW5lZCxcbiAgICApXG4gICAgLmZpbHRlcigodmFsdWUpOiB2YWx1ZSBpcyBzdHJpbmcgPT4gQm9vbGVhbih2YWx1ZSkpXG4gICAgLm1hcChOdW1iZXIpO1xuICByZXR1cm4gTWF0aC5tYXgoMCwgLi4ubGFiZWxzKTtcbn1cblxuLyoqIEV2ZW50IGxhYmVscyAoYGpvdXJuYWxfYnVyc3QgZXZlbnQgTmAsIGJvbGQgdW5kZXJzY29yZXMgcmVuZGVyZWQgYXdheSkuICovXG5mdW5jdGlvbiBidXJzdExhYmVscyhwYWdlOiBQYWdlKSB7XG4gIHJldHVybiBwYWdlXG4gICAgLmdldEJ5VGVzdElkKFwidHJhbnNjcmlwdC1yb3dcIilcbiAgICAuZXZhbHVhdGVBbGwoKHJvd3MpID0+XG4gICAgICByb3dzXG4gICAgICAgIC5tYXAoKHJvdykgPT4gbmV3IFJlZ0V4cChcImpvdXJuYWxfYnVyc3RfKiBldmVudCAoXFxcXGQrKVxcXFxiXCIpLmV4ZWMocm93LnRleHRDb250ZW50ID8/IFwiXCIpPy5bMV0pXG4gICAgICAgIC5maWx0ZXIoKHZhbHVlKTogdmFsdWUgaXMgc3RyaW5nID0+IEJvb2xlYW4odmFsdWUpKVxuICAgICAgICAubWFwKE51bWJlciksXG4gICAgKTtcbn1cblxuLyoqIFZpZXdwb3J0IG9mZnNldCBvZiB0aGUgdHJhbnNjcmlwdCByb3cgY2FycnlpbmcgYnVyc3QgZXZlbnQgYG5gLiAqL1xuYXN5bmMgZnVuY3Rpb24gcm93T2Zmc2V0KFxuICBwYWdlOiBQYWdlLFxuICBzY3JvbGxlcjogUmV0dXJuVHlwZTxQYWdlW1wiZ2V0QnlUZXN0SWRcIl0+LFxuICBuOiBudW1iZXIsXG4pIHtcbiAgcmV0dXJuIHNjcm9sbGVyLmV2YWx1YXRlKChlbCwgbGFiZWwpID0+IHtcbiAgICBjb25zdCByZSA9IG5ldyBSZWdFeHAoYGpvdXJuYWxfYnVyc3RfKiBldmVudCAke2xhYmVsfVxcXFxiYCk7XG4gICAgY29uc3Qgcm93ID0gQXJyYXkuZnJvbShlbC5xdWVyeVNlbGVjdG9yQWxsPEhUTUxFbGVtZW50PihcIltkYXRhLXRlc3RpZD0ndHJhbnNjcmlwdC1yb3cnXVwiKSkuZmluZChcbiAgICAgIChjYW5kaWRhdGUpID0+IHJlLnRlc3QoY2FuZGlkYXRlLnRleHRDb250ZW50ID8/IFwiXCIpLFxuICAgICk7XG4gICAgaWYgKCFyb3cpIHJldHVybiBudWxsO1xuICAgIC8vIFNjcm9sbGVyLXJlbGF0aXZlIG9mZnNldCDigJQgdGhlIHNhbWUgYmFzaXMgdGhlIGNvbXBvbmVudCdzIHNjcm9sbFxuICAgIC8vIHJlc3RvcmUgdXNlcywgc28gYSBjb252ZXJnZWQgYW5jaG9yIGNvbXBhcmVzIGVxdWFsIGhlcmUuXG4gICAgcmV0dXJuIHtcbiAgICAgIHNjcm9sbFRvcDogZWwuc2Nyb2xsVG9wLFxuICAgICAgb2Zmc2V0OiByb3cuZ2V0Qm91bmRpbmdDbGllbnRSZWN0KCkudG9wIC0gZWwuZ2V0Qm91bmRpbmdDbGllbnRSZWN0KCkudG9wLFxuICAgIH07XG4gIH0sIG4pO1xufVxuXG50ZXN0KFwiYSBib3VuZGVkIHRhaWwgd2luZG93IHBhZ2VzIG9sZGVyIHJvd3MgYW5kIGRlc2NlbmRzIGEgcmVzeW5jIGdhcCB0byBsaXZlXCIsIGFzeW5jICh7XG4gIHBhZ2UsXG4gIGJyb3dzZXIsXG59KSA9PiB7XG4gIHRlc3Quc2V0VGltZW91dCgyNDBfMDAwKTtcbiAgYXdhaXQgbG9naW4ocGFnZSk7XG4gIC8vIFdyYXAgdGhlIGZvbGxvdyBXZWJTb2NrZXQ6XG4gIC8vICAtIGxvZyBjb250cm9sIGZyYW1lcyAoc25hcHNob3QvZ2FwKSxcbiAgLy8gIC0gd2hpbGUgX19mb2xsb3dHYXRlIGlzIHNldCwgZHJvcCBFVkVSWSBmb2xsb3cgZnJhbWUgKGV2ZW50cywgZ2FwcyBhbmRcbiAgLy8gICAgc25hcHNob3RzKS4gVGhlIGJyb3dzZXIgc3RpbGwgZHJhaW5zIHRoZSBzb2NrZXQsIHNvIHRoZSBIdWIgbmV2ZXJcbiAgLy8gICAgcmVzeW5jcywgYnV0IHRoZSBhcHAncyBhcHBsaWVkIGN1cnNvciBzdGF5cyBwaW5uZWQgYXQgdGhlIHByZS1idXJzdFxuICAvLyAgICBzZXEuIFdoZW4gdGhlIGdhdGUgcmVvcGVucyBhbmQgYSBmcmVzaCBidXJzdCBsYW5kcywgaXRzIGZpcnN0IGxpdmVcbiAgLy8gICAgYmF0Y2ggc3RhcnRzIHRob3VzYW5kcyBvZiBzZXFzIGFib3ZlIGFwcGxpZWQgLT4gYSByZWFsIGdhcCB0aGUgY2xpZW50XG4gIC8vICAgIG11c3QgZGVzY2VuZCB3aXRoIGJlZm9yZVNlcSwgaW5kZXBlbmRlbnQgb2YgaHViIGJ1ZmZlci9zb2NrZXQgdGltaW5nLlxuICBhd2FpdCBwYWdlLmFkZEluaXRTY3JpcHQoKCkgPT4ge1xuICAgIGNvbnN0IHcgPSB3aW5kb3cgYXMgdW5rbm93biBhcyB7XG4gICAgICBfX2ZyYW1lTG9nPzogc3RyaW5nW107XG4gICAgICBfX2ZyYW1lQ291bnQ/OiBudW1iZXI7XG4gICAgICBfX2ZvbGxvd0dhdGU/OiBib29sZWFuO1xuICAgIH07XG4gICAgdy5fX2ZyYW1lTG9nID0gW107XG4gICAgdy5fX2ZyYW1lQ291bnQgPSAwO1xuICAgIGNvbnN0IE5hdGl2ZVdTID0gd2luZG93LldlYlNvY2tldDtcbiAgICBjbGFzcyBHYXRlZFdTIGV4dGVuZHMgTmF0aXZlV1Mge1xuICAgICAgY29uc3RydWN0b3IodXJsOiBzdHJpbmcgfCBVUkwsIHByb3RvY29scz86IHN0cmluZyB8IHN0cmluZ1tdKSB7XG4gICAgICAgIHN1cGVyKHVybCwgcHJvdG9jb2xzKTtcbiAgICAgICAgdGhpcy5hZGRFdmVudExpc3RlbmVyKFxuICAgICAgICAgIFwibWVzc2FnZVwiLFxuICAgICAgICAgIChldjogTWVzc2FnZUV2ZW50KSA9PiB7XG4gICAgICAgICAgICBpZiAodHlwZW9mIGV2LmRhdGEgIT09IFwic3RyaW5nXCIpIHJldHVybjtcbiAgICAgICAgICAgIHRyeSB7XG4gICAgICAgICAgICAgIGNvbnN0IG1zZyA9IEpTT04ucGFyc2UoZXYuZGF0YSkgYXMgeyB0eXBlPzogc3RyaW5nOyBmcm9tU2VxPzogc3RyaW5nIHwgbnVsbCB9O1xuICAgICAgICAgICAgICBpZiAobXNnLnR5cGUgPT09IFwic25hcHNob3RcIikgdy5fX2ZyYW1lTG9nIS5wdXNoKGBzbmFwc2hvdDoke21zZy5mcm9tU2VxID8/IFwiXCJ9YCk7XG4gICAgICAgICAgICAgIGVsc2UgaWYgKG1zZy50eXBlID09PSBcImV2ZW50XCIpIHcuX19mcmFtZUNvdW50ID0gKHcuX19mcmFtZUNvdW50ID8/IDApICsgMTtcbiAgICAgICAgICAgICAgZWxzZSBpZiAobXNnLnR5cGUgPT09IFwiZ2FwXCIpIHcuX19mcmFtZUxvZyEucHVzaChcImdhcFwiKTtcbiAgICAgICAgICAgIH0gY2F0Y2gge1xuICAgICAgICAgICAgICAvLyBub24tSlNPTlxuICAgICAgICAgICAgfVxuICAgICAgICAgICAgaWYgKHcuX19mb2xsb3dHYXRlKSBldi5zdG9wSW1tZWRpYXRlUHJvcGFnYXRpb24oKTtcbiAgICAgICAgICB9LFxuICAgICAgICAgIHsgY2FwdHVyZTogdHJ1ZSB9LFxuICAgICAgICApO1xuICAgICAgfVxuICAgIH1cbiAgICB3aW5kb3cuV2ViU29ja2V0ID0gR2F0ZWRXUyBhcyB1bmtub3duIGFzIHR5cGVvZiBXZWJTb2NrZXQ7XG4gIH0pO1xuXG4gIC8vIFNlZWQgYSA1MDAwLWV2ZW50IGpvdXJuYWwgZnJvbSBhIGNvbnRleHQgdGhhdCBuZXZlciBvcGVucyB0aGUgdHJhbnNjcmlwdC5cbiAgZHJpdmVyID0gYXdhaXQgZHJpdmVyQ29udGV4dChicm93c2VyKTtcbiAgY29uc3QgZHJpdmVyUGFnZSA9IGRyaXZlci5wYWdlO1xuICBjb25zdCBpbnN0YW5jZUlkID0gYXdhaXQgY3JlYXRlSW5zdGFuY2VSZXN0KGRyaXZlclBhZ2UpO1xuICBhd2FpdCBhbnN3ZXJQZW5kaW5nUmVzdChkcml2ZXJQYWdlLCBpbnN0YW5jZUlkKTtcbiAgYXdhaXQgYnVyc3QoZHJpdmVyUGFnZSwgaW5zdGFuY2VJZCwgQlVSU1RfQ09VTlQpO1xuICBhd2FpdCB3YWl0RHVyYWJsZShkcml2ZXJQYWdlLCBpbnN0YW5jZUlkLCBCVVJTVF9DT1VOVCk7XG5cbiAgLy8gVGhlIHNlcnZlciB3aW5kb3cgaXMgYm91bmRlZDogZmxvb3Igd2VsbCBhYm92ZSAxLCBmbGFnIHBhcnRpYWwuXG4gIGNvbnN0IHNlZWRlZCA9IGF3YWl0IGpvdXJuYWxKc29uKGRyaXZlclBhZ2UsIGluc3RhbmNlSWQpO1xuICBleHBlY3QoTnVtYmVyKHNlZWRlZC5kdXJhYmxlU2VxKSkudG9CZUdyZWF0ZXJUaGFuT3JFcXVhbChCVVJTVF9DT1VOVCk7XG4gIGV4cGVjdChzZWVkZWQuZnJvbVNlcSkubm90LnRvQmVOdWxsKCk7XG4gIGV4cGVjdChOdW1iZXIoc2VlZGVkLmZyb21TZXEpKS50b0JlR3JlYXRlclRoYW4oMSk7XG4gIGV4cGVjdChzZWVkZWQucmVhY2hlZEFmdGVyU2VxKS50b0JlKGZhbHNlKTtcbiAgY29uc3QgbmV3ZXN0TGFiZWwgPSBtYXhCdXJzdExhYmVsKHNlZWRlZCk7XG4gIGV4cGVjdChuZXdlc3RMYWJlbCkudG9CZUdyZWF0ZXJUaGFuKDApO1xuXG4gIC8vIExhdGUgYXR0YWNoOiB0aGUgdGFiIG9ubHkgaG9sZHMgdGhlIGJvdW5kZWQgdGFpbC5cbiAgYXdhaXQgcGFnZS5nb3RvKGAvcy8ke2luc3RhbmNlSWR9L3N0cnVjdHVyZWRgKTtcbiAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJzZXNzaW9uLXBhZ2VcIikpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtam91cm5hbFwiLCBcImxpdmVcIiwge1xuICAgIHRpbWVvdXQ6IDMwXzAwMCxcbiAgfSk7XG4gIGNvbnN0IHRyYW5zY3JpcHQgPSBwYWdlLmdldEJ5VGVzdElkKFwidHJhbnNjcmlwdFwiKTtcbiAgLy8gYF9fam91cm5hbF9idXJzdF9fYCBpcyBNYXJrZG93biBib2xkOyBpdCByZW5kZXJzIHdpdGhvdXQgdGhlIHdyYXBwaW5nIF9fLlxuICBhd2FpdCBleHBlY3QodHJhbnNjcmlwdCkudG9Db250YWluVGV4dChgam91cm5hbF9idXJzdCBldmVudCAke25ld2VzdExhYmVsfWApO1xuICBjb25zdCBsb2FkRWFybGllciA9IHBhZ2UuZ2V0QnlUZXN0SWQoXCJsb2FkLWVhcmxpZXJcIik7XG4gIGF3YWl0IGV4cGVjdChsb2FkRWFybGllcikudG9CZVZpc2libGUoKTtcblxuICAvLyBUaGUgY29tcG9uZW50IHBpbnMgdGhlIHRvcG1vc3QgcmVuZGVyZWQgcm93ICh2aXJ0dWFsIHdpbmRvdyBzdGFydCkgYXRcbiAgLy8gc2Nyb2xsVG9wIDA7IHRoYXQgaXMgdGhlIGFuY2hvciBsb2FkLWVhcmxpZXIgaG9sZHMsIHNvIHVzZSBpdCB0b28uXG4gIGNvbnN0IHNjcm9sbGVyID0gcGFnZS5nZXRCeVRlc3RJZChcInRyYW5zY3JpcHQtc2Nyb2xsZXJcIik7XG4gIGF3YWl0IHNjcm9sbGVyLmV2YWx1YXRlKChlbCkgPT4ge1xuICAgIGVsLnNjcm9sbFRvcCA9IDA7XG4gICAgZWwuZGlzcGF0Y2hFdmVudChuZXcgRXZlbnQoXCJzY3JvbGxcIiwgeyBidWJibGVzOiB0cnVlIH0pKTtcbiAgfSk7XG4gIGF3YWl0IHBhZ2Uud2FpdEZvclRpbWVvdXQoMzAwKTtcbiAgY29uc3QgbGFiZWxzQmVmb3JlID0gYXdhaXQgYnVyc3RMYWJlbHMocGFnZSk7XG4gIGV4cGVjdChsYWJlbHNCZWZvcmUubGVuZ3RoKS50b0JlR3JlYXRlclRoYW4oOCk7XG4gIGNvbnN0IGFuY2hvckxhYmVsID0gbGFiZWxzQmVmb3JlWzBdO1xuICBhd2FpdCBwYWdlLndhaXRGb3JUaW1lb3V0KDIwMCk7XG4gIGNvbnN0IGFuY2hvckJlZm9yZSA9IGF3YWl0IHJvd09mZnNldChwYWdlLCBzY3JvbGxlciwgYW5jaG9yTGFiZWwpO1xuICBleHBlY3QoYW5jaG9yQmVmb3JlKS5ub3QudG9CZU51bGwoKTtcblxuICAvLyBPbmUgY2xpY2sgZmV0Y2hlcyBleGFjdGx5IG9uZSBib3VuZGVkIG9sZGVyIHBhZ2UgKGJlZm9yZVNlcSkuXG4gIGNvbnN0IGJlZm9yZVNlcVJlcXVlc3RzOiBzdHJpbmdbXSA9IFtdO1xuICBwYWdlLm9uKFwicmVxdWVzdFwiLCAocmVxdWVzdCkgPT4ge1xuICAgIGNvbnN0IHVybCA9IG5ldyBVUkwocmVxdWVzdC51cmwoKSk7XG4gICAgaWYgKFxuICAgICAgdXJsLnBhdGhuYW1lID09PSBgL3YxL2luc3RhbmNlcy8ke2luc3RhbmNlSWR9L2pvdXJuYWxgICYmXG4gICAgICB1cmwuc2VhcmNoUGFyYW1zLmhhcyhcImJlZm9yZVNlcVwiKVxuICAgICkge1xuICAgICAgYmVmb3JlU2VxUmVxdWVzdHMucHVzaCh1cmwuc2VhcmNoUGFyYW1zLmdldChcImJlZm9yZVNlcVwiKSEpO1xuICAgIH1cbiAgfSk7XG4gIGNvbnN0IG9sZGVyUmVzcG9uc2VQcm9taXNlID0gcGFnZS53YWl0Rm9yUmVzcG9uc2UoXG4gICAgKHJlc3BvbnNlKSA9PlxuICAgICAgcmVzcG9uc2UucmVxdWVzdCgpLm1ldGhvZCgpID09PSBcIkdFVFwiICYmXG4gICAgICBuZXcgVVJMKHJlc3BvbnNlLnVybCgpKS5zZWFyY2hQYXJhbXMuaGFzKFwiYmVmb3JlU2VxXCIpLFxuICAgIHsgdGltZW91dDogMTVfMDAwIH0sXG4gICk7XG4gIGF3YWl0IGxvYWRFYXJsaWVyLmNsaWNrKCk7XG4gIGNvbnN0IG9sZGVyUmVzcG9uc2UgPSBhd2FpdCBvbGRlclJlc3BvbnNlUHJvbWlzZTtcbiAgZXhwZWN0KG9sZGVyUmVzcG9uc2Uub2soKSkudG9CZSh0cnVlKTtcbiAgY29uc3Qgb2xkZXJCb2R5ID0gKGF3YWl0IG9sZGVyUmVzcG9uc2UuanNvbigpKSBhcyB7IHJlYWNoZWRBZnRlclNlcTogYm9vbGVhbiB9O1xuXG4gIC8vIFRoZSBhbmNob3Igcm93IHN0YXlzIHBpbm5lZCBhdCBpdHMgb2xkIHZpZXdwb3J0IG9mZnNldCB3aGlsZSBzY3JvbGxUb3BcbiAgLy8gZ3Jvd3MgYnkgdGhlIHByZXBlbmRlZCB3aW5kb3cgaGVpZ2h0LlxuICBhd2FpdCBleHBlY3RcbiAgICAucG9sbChcbiAgICAgIGFzeW5jICgpID0+IHtcbiAgICAgICAgY29uc3QgcG9zID0gYXdhaXQgcm93T2Zmc2V0KHBhZ2UsIHNjcm9sbGVyLCBhbmNob3JMYWJlbCk7XG4gICAgICAgIHJldHVybiBwb3MgPT09IG51bGwgfHwgYW5jaG9yQmVmb3JlID09PSBudWxsID8gbnVsbCA6IE1hdGguYWJzKHBvcy5vZmZzZXQgLSBhbmNob3JCZWZvcmUub2Zmc2V0KTtcbiAgICAgIH0sXG4gICAgICB7IHRpbWVvdXQ6IDEwXzAwMCwgaW50ZXJ2YWxzOiBbMTAwLCAyMDBdIH0sXG4gICAgKVxuICAgIC50b0JlTGVzc1RoYW5PckVxdWFsKDQpO1xuICBjb25zdCBhbmNob3JTY3JvbGxBZnRlciA9IGF3YWl0IHNjcm9sbGVyLmV2YWx1YXRlKChlbCkgPT4gZWwuc2Nyb2xsVG9wKTtcbiAgZXhwZWN0KGFuY2hvclNjcm9sbEFmdGVyKS50b0JlR3JlYXRlclRoYW4oYW5jaG9yQmVmb3JlIS5zY3JvbGxUb3ApO1xuXG4gIC8vIEQtMDUzOiB6ZXJvIHBlci1yb3cgZHJpZnQuIFJvdyBzcGFjaW5nIGlzIHBhZGRpbmcgaW5zaWRlIHRoZSBtZWFzdXJlZFxuICAvLyBib3ggKG5vIG91dHNpZGUgbWFyZ2luLCBubyArMTIgZnVkZ2UpLCBzbyBjb25zZWN1dGl2ZSBtb3VudGVkIHJvd3MgYXJlXG4gIC8vIGNvbnRpZ3VvdXM6IGVhY2ggcm93J3Mgc2xvdCBpcyBleGFjdGx5IGl0cyByZW5kZXJlZCBoZWlnaHQuXG4gIGNvbnN0IGdhcHMgPSBhd2FpdCBzY3JvbGxlci5ldmFsdWF0ZSgoZWwpID0+IHtcbiAgICBjb25zdCByb3dzID0gQXJyYXkuZnJvbShlbC5xdWVyeVNlbGVjdG9yQWxsPEhUTUxFbGVtZW50PignW2RhdGEtdGVzdGlkPVwidHJhbnNjcmlwdC1yb3dcIl0nKSk7XG4gICAgY29uc3Qgb3V0OiBudW1iZXJbXSA9IFtdO1xuICAgIGZvciAobGV0IGkgPSAxOyBpIDwgcm93cy5sZW5ndGg7IGkgKz0gMSkge1xuICAgICAgY29uc3QgcHJldiA9IHJvd3NbaSAtIDFdLmdldEJvdW5kaW5nQ2xpZW50UmVjdCgpO1xuICAgICAgb3V0LnB1c2goTWF0aC5yb3VuZCgocm93c1tpXS5nZXRCb3VuZGluZ0NsaWVudFJlY3QoKS50b3AgLSBwcmV2LmJvdHRvbSkgKiAxMDApIC8gMTAwKTtcbiAgICB9XG4gICAgcmV0dXJuIG91dDtcbiAgfSk7XG4gIGV4cGVjdChnYXBzLmxlbmd0aCkudG9CZUdyZWF0ZXJUaGFuKDApO1xuICBmb3IgKGNvbnN0IGdhcCBvZiBnYXBzKSBleHBlY3QoTWF0aC5hYnMoZ2FwKSkudG9CZUxlc3NUaGFuT3JFcXVhbCgwLjUpO1xuXG4gIC8vIEF0IHRoZSB0b3AsIGFuIG9sZGVyIGJ1cnN0IHdpbmRvdyByZW5kZXJzIGluIGFzY2VuZGluZyBzZXEgb3JkZXIuXG4gIGF3YWl0IHNjcm9sbGVyLmV2YWx1YXRlKChlbCkgPT4ge1xuICAgIGVsLnNjcm9sbFRvcCA9IDA7XG4gICAgZWwuZGlzcGF0Y2hFdmVudChuZXcgRXZlbnQoXCJzY3JvbGxcIiwgeyBidWJibGVzOiB0cnVlIH0pKTtcbiAgfSk7XG4gIGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKCgpID0+IGJ1cnN0TGFiZWxzKHBhZ2UpLnRoZW4oKGxhYmVscykgPT4gbGFiZWxzWzBdKSwgeyB0aW1lb3V0OiAxMF8wMDAgfSlcbiAgICAudG9CZUxlc3NUaGFuKGxhYmVsc0JlZm9yZVswXSk7XG4gIGNvbnN0IGxhYmVsc0FmdGVyRmlyc3RDbGljayA9IGF3YWl0IGJ1cnN0TGFiZWxzKHBhZ2UpO1xuICBmb3IgKGxldCBpID0gMTsgaSA8IE1hdGgubWluKDEyLCBsYWJlbHNBZnRlckZpcnN0Q2xpY2subGVuZ3RoKTsgaSArPSAxKSB7XG4gICAgZXhwZWN0KGxhYmVsc0FmdGVyRmlyc3RDbGlja1tpXSkudG9CZUdyZWF0ZXJUaGFuKGxhYmVsc0FmdGVyRmlyc3RDbGlja1tpIC0gMV0pO1xuICB9XG5cbiAgLy8gTG9hZCBleGFjdGx5IG9uZSBtb3JlIG9sZGVyIHdpbmRvdyBhbmQgdmVyaWZ5IHRoZSBwcmVwZW5kIG9yZGVyOyBkbyBOT1RcbiAgLy8gcGFnZSB0byBzZXEgMSDigJQgdGhlIHJlc3luYyBzdGVwIGJlbG93IG5lZWRzIGEgYm91bmRlZCBhcHBsaWVkIHJhbmdlIHNvIHRoZVxuICAvLyBzZWNvbmQgYnVyc3Qgb3BlbnMgYSByZWFsIGdhcC5cbiAgZXhwZWN0KG9sZGVyQm9keS5yZWFjaGVkQWZ0ZXJTZXEpLnRvQmUoZmFsc2UpO1xuICBhd2FpdCBleHBlY3QobG9hZEVhcmxpZXIpLnRvQmVWaXNpYmxlKCk7XG5cbiAgLy8gLS0tIERldGVybWluaXN0aWMgYm91bmRlZCByZXN5bmMgZ2FwIC0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tXG4gIC8vIERldGVybWluaXNtIGlzIGVudGlyZWx5IGNsaWVudC1zaWRlOiB0aGUgZm9sbG93IFdlYlNvY2tldCB3cmFwcGVyIChhZGRlZFxuICAvLyB2aWEgYWRkSW5pdFNjcmlwdCBhdCBsb2dpbikgZHJvcHMgZXZlcnkgZnJhbWUgd2hpbGUgX19mb2xsb3dHYXRlIGlzIHNldCxcbiAgLy8gc28gdGhlIGFwcGxpZWQgY3Vyc29yIGNhbm5vdCBjaGFzZSB0aGUgYnVyc3QgcmVnYXJkbGVzcyBvZiBodWIgYnVmZmVyXG4gIC8vIHNpemVzIG9yIHNvY2tldCB0aW1pbmcuIFRoZSBmaXJzdCBsaXZlIGJhdGNoIGFmdGVyIHJlb3BlbmluZyBvcGVucyBhIGdhcFxuICAvLyB0aG91c2FuZHMgb2Ygcm93cyB3aWRlLCB3aGljaCB0aGUgY2xpZW50IGRlc2NlbmRzIHdpdGggYmVmb3JlU2VxLiBTYW1wbGVcbiAgLy8gdGhlIHNlc3Npb24gZWxlbWVudCdzIGpvdXJuYWwgc3RhdGUgaW50byBhIHdpbmRvdyBnbG9iYWwgb24gYSBmYXN0XG4gIC8vIGludGVydmFsICh0aGUgc2FtcGxlciBydW5zIGluIHRoZSBicm93c2VyOyBhIE5vZGUtc2NvcGUgYXJyYXkgd291bGQgYmVcbiAgLy8gdW5kZWZpbmVkIHRoZXJlKS5cbiAgYXdhaXQgcGFnZS5ldmFsdWF0ZSgoKSA9PiB7XG4gICAgY29uc3QgdyA9IHdpbmRvdyBhcyB1bmtub3duIGFzIHtcbiAgICAgIF9fam91cm5hbFN0YXRlcz86IHN0cmluZ1tdO1xuICAgICAgX19qb3VybmFsU3RhdHVzU2FtcGxlcz86IHN0cmluZ1tdO1xuICAgICAgX19mb2xsb3dGcmFtZXM/OiBzdHJpbmdbXTtcbiAgICB9O1xuICAgIHcuX19qb3VybmFsU3RhdGVzID0gW107XG4gICAgdy5fX2pvdXJuYWxTdGF0dXNTYW1wbGVzID0gW107XG4gICAgdy5fX2ZvbGxvd0ZyYW1lcyA9IFtdO1xuICAgIGNvbnN0IHJlY29yZEJhbm5lciA9ICgpID0+IHtcbiAgICAgIGNvbnN0IGJhbm5lciA9IGRvY3VtZW50LnF1ZXJ5U2VsZWN0b3IoXCJbZGF0YS10ZXN0aWQ9J2pvdXJuYWwtYmFubmVyJ11cIik7XG4gICAgICBpZiAoYmFubmVyKSB3Ll9fam91cm5hbFN0YXRlcyEucHVzaChiYW5uZXIuZ2V0QXR0cmlidXRlKFwiZGF0YS1zdGF0ZVwiKSA/PyBcIlwiKTtcbiAgICB9O1xuICAgIG5ldyBNdXRhdGlvbk9ic2VydmVyKHJlY29yZEJhbm5lcikub2JzZXJ2ZShkb2N1bWVudC5ib2R5LCB7XG4gICAgICBhdHRyaWJ1dGVzOiB0cnVlLFxuICAgICAgc3VidHJlZTogdHJ1ZSxcbiAgICAgIGNoaWxkTGlzdDogdHJ1ZSxcbiAgICB9KTtcbiAgICB3aW5kb3cuc2V0SW50ZXJ2YWwoKCkgPT4ge1xuICAgICAgY29uc3Qgc3RhdGUgPSBkb2N1bWVudFxuICAgICAgICAucXVlcnlTZWxlY3RvcjxIVE1MRWxlbWVudD4oXCJbZGF0YS10ZXN0aWQ9J3Nlc3Npb24tcGFnZSddXCIpXG4gICAgICAgID8uZ2V0QXR0cmlidXRlKFwiZGF0YS1qb3VybmFsXCIpO1xuICAgICAgY29uc3Qgc2FtcGxlcyA9IHcuX19qb3VybmFsU3RhdHVzU2FtcGxlcyE7XG4gICAgICBpZiAoc3RhdGUgJiYgc3RhdGUgIT09IHNhbXBsZXNbc2FtcGxlcy5sZW5ndGggLSAxXSkgc2FtcGxlcy5wdXNoKHN0YXRlKTtcbiAgICB9LCA3NSk7XG4gIH0pO1xuXG4gIC8vIEZpbGwtZGVzY2VuZCByZWFkcyBhZnRlciB0aGlzIHBvaW50IGFyZSByZXN5bmMgZmlsbHMsIG5vdCB0aGUgbWFudWFsIGNsaWNrLlxuICBjb25zdCBmaWxsQmVmb3JlID0gYmVmb3JlU2VxUmVxdWVzdHMubGVuZ3RoO1xuICBjb25zdCBhbGxKb3VybmFsUmVxdWVzdHM6IHN0cmluZ1tdID0gW107XG4gIHBhZ2Uub24oXCJyZXF1ZXN0XCIsIChyZXF1ZXN0KSA9PiB7XG4gICAgY29uc3QgdXJsID0gbmV3IFVSTChyZXF1ZXN0LnVybCgpKTtcbiAgICBpZiAodXJsLnBhdGhuYW1lID09PSBgL3YxL2luc3RhbmNlcy8ke2luc3RhbmNlSWR9L2pvdXJuYWxgKSB7XG4gICAgICBhbGxKb3VybmFsUmVxdWVzdHMucHVzaChgJHtyZXF1ZXN0Lm1ldGhvZCgpfSAke3VybC5zZWFyY2h9YCk7XG4gICAgfVxuICB9KTtcblxuICBjb25zdCBiZWZvcmVSZXN5bmNEdXJhYmxlID0gTnVtYmVyKHNlZWRlZC5kdXJhYmxlU2VxKTtcbiAgLy8gR2F0ZSBldmVyeSBmb2xsb3cgZnJhbWUgZHVyaW5nIHRoZSBiaWcgYnVyc3Qgc28gdGhlIGFwcGxpZWQgY3Vyc29yIGNhbm5vdFxuICAvLyBjaGFzZSBpdDsgdGhlIEh1YiBrZWVwcyBvdmVyZmxvd2luZyBhbmQgcmVzeW5jaW5nLCBhbGwgZHJvcHBlZCBjbGllbnQtc2lkZS5cbiAgYXdhaXQgcGFnZS5ldmFsdWF0ZSgoKSA9PiB7XG4gICAgKHdpbmRvdyBhcyB1bmtub3duIGFzIHsgX19mb2xsb3dHYXRlPzogYm9vbGVhbiB9KS5fX2ZvbGxvd0dhdGUgPSB0cnVlO1xuICB9KTtcbiAgYXdhaXQgYnVyc3QoZHJpdmVyUGFnZSwgaW5zdGFuY2VJZCwgQlVSU1RfQ09VTlQpO1xuICBhd2FpdCB3YWl0RHVyYWJsZShkcml2ZXJQYWdlLCBpbnN0YW5jZUlkLCBiZWZvcmVSZXN5bmNEdXJhYmxlICsgQlVSU1RfQ09VTlQpO1xuICAvLyBSZW9wZW4gYW5kIHNlbmQgYSBzbWFsbCBidXJzdC4gSXRzIGxpdmUgZnJhbWVzIGFyZSBub3QgcmVwbGF5ZWQgZnJvbSB0aGVcbiAgLy8gZ2F0ZWQgZ2FwLCBzbyB0aGUgZmlyc3QgZGVsaXZlcmVkIGJhdGNoIHN0YXJ0cyBmYXIgYWJvdmUgdGhlIHBpbm5lZFxuICAvLyBjdXJzb3IgYW5kIHRoZSBjbGllbnQgZGVzY2VuZHMgdGhlIG1pc3Npbmcgd2luZG93cyB3aXRoIGJlZm9yZVNlcS5cbiAgYXdhaXQgcGFnZS5ldmFsdWF0ZSgoKSA9PiB7XG4gICAgKHdpbmRvdyBhcyB1bmtub3duIGFzIHsgX19mb2xsb3dHYXRlPzogYm9vbGVhbiB9KS5fX2ZvbGxvd0dhdGUgPSBmYWxzZTtcbiAgfSk7XG4gIGF3YWl0IGJ1cnN0KGRyaXZlclBhZ2UsIGluc3RhbmNlSWQsIDIwMCk7XG4gIGF3YWl0IHdhaXREdXJhYmxlKGRyaXZlclBhZ2UsIGluc3RhbmNlSWQsIGJlZm9yZVJlc3luY0R1cmFibGUgKyBCVVJTVF9DT1VOVCArIDIwMCArIDEpO1xuICAvLyBUaGUgbmV3ZXN0IGxhYmVsIGlzIGJ1cnN0LXJlbGF0aXZlIGFuZCBsYW5kcyB3aXRoIHRoZSB0cmFpbGluZyBpZGxlIGZyYW1lLlxuICBjb25zdCBmaW5hbFdpbmRvdyA9IGF3YWl0IGpvdXJuYWxKc29uKGRyaXZlclBhZ2UsIGluc3RhbmNlSWQpO1xuICBjb25zdCByZXN5bmNOZXdlc3RMYWJlbCA9IG1heEJ1cnN0TGFiZWwoZmluYWxXaW5kb3cpO1xuXG4gIC8vIFRoZSBjbGllbnQgZGVzY2VuZHMgdGhlIGJvdW5kZWQgcmVzeW5jIHdpbmRvdyB3aXRoIGJlZm9yZVNlcSBhbmQgc2V0dGxlc1xuICAvLyBiYWNrIHRvIGxpdmUgd2l0aCB0aGUgbmV3ZXN0IHR1cm4gcmVuZGVyZWQuXG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwic2Vzc2lvbi1wYWdlXCIpKS50b0hhdmVBdHRyaWJ1dGUoXCJkYXRhLWpvdXJuYWxcIiwgXCJsaXZlXCIsIHtcbiAgICB0aW1lb3V0OiA5MF8wMDAsXG4gIH0pO1xuICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImpvdXJuYWwtYmFubmVyXCIpKS50b0hhdmVDb3VudCgwKTtcbiAgY29uc3QgeyBzYW1wbGVkLCBvYnNlcnZlZEJhbm5lcnMsIGZyYW1lcywgbGl2ZUNvdW50IH0gPSBhd2FpdCBwYWdlLmV2YWx1YXRlKCgpID0+IHtcbiAgICBjb25zdCB3ID0gd2luZG93IGFzIHVua25vd24gYXMge1xuICAgICAgX19qb3VybmFsU3RhdGVzPzogc3RyaW5nW107XG4gICAgICBfX2pvdXJuYWxTdGF0dXNTYW1wbGVzPzogc3RyaW5nW107XG4gICAgICBfX2ZyYW1lTG9nPzogc3RyaW5nW107XG4gICAgICBfX2ZyYW1lQ291bnQ/OiBudW1iZXI7XG4gICAgfTtcbiAgICByZXR1cm4ge1xuICAgICAgc2FtcGxlZDogdy5fX2pvdXJuYWxTdGF0dXNTYW1wbGVzID8/IFtdLFxuICAgICAgb2JzZXJ2ZWRCYW5uZXJzOiB3Ll9fam91cm5hbFN0YXRlcyA/PyBbXSxcbiAgICAgIGZyYW1lczogdy5fX2ZyYW1lTG9nID8/IFtdLFxuICAgICAgbGl2ZUNvdW50OiB3Ll9fZnJhbWVDb3VudCA/PyAwLFxuICAgIH07XG4gIH0pO1xuICAvLyBUaGUgYmFubmVyIHBhc3NlZCB0aHJvdWdoIGdhcC1iYWNrZmlsbCBvbiB0aGUgd2F5IGJhY2sgdG8gbGl2ZS5cbiAgZXhwZWN0KFxuICAgIFsuLi5zYW1wbGVkLCAuLi5vYnNlcnZlZEJhbm5lcnNdLFxuICAgIGBleHBlY3RlZCBnYXAtYmFja2ZpbGw7IGZpbGxSZWFkcz0ke2JlZm9yZVNlcVJlcXVlc3RzLmxlbmd0aCAtIGZpbGxCZWZvcmV9IHNhbXBsZWQ9JHtKU09OLnN0cmluZ2lmeShzYW1wbGVkKX0gYmFubmVycz0ke0pTT04uc3RyaW5naWZ5KG9ic2VydmVkQmFubmVycyl9IGZyYW1lcz0ke0pTT04uc3RyaW5naWZ5KGZyYW1lcyl9IGxpdmU9JHtsaXZlQ291bnR9IGpvdXJuYWw9JHtKU09OLnN0cmluZ2lmeShhbGxKb3VybmFsUmVxdWVzdHMuc2xpY2UoLTEyKSl9YCxcbiAgKS50b0NvbnRhaW4oXCJnYXAtYmFja2ZpbGxcIik7XG4gIC8vIEEgcmVzeW5jIGdhcCBmaWxsIGRlc2NlbmRlZCB3aXRoIGJlZm9yZVNlcSAoYmV5b25kIHRoZSBtYW51YWwgY2xpY2spLlxuICBleHBlY3QoXG4gICAgYmVmb3JlU2VxUmVxdWVzdHMubGVuZ3RoLFxuICAgIGBleHBlY3RlZCBhIGRlc2NlbmRpbmcgZmlsbCByZWFkOyBzYW1wbGVkPSR7SlNPTi5zdHJpbmdpZnkoc2FtcGxlZCl9IGZyYW1lcz0ke0pTT04uc3RyaW5naWZ5KGZyYW1lcyl9IGpvdXJuYWw9JHtKU09OLnN0cmluZ2lmeShhbGxKb3VybmFsUmVxdWVzdHMuc2xpY2UoLTEyKSl9YCxcbiAgKS50b0JlR3JlYXRlclRoYW4oZmlsbEJlZm9yZSk7XG4gIC8vIFRoZSBtYW51YWwgcGFnaW5nIGxlZnQgdGhlIHZpZXdwb3J0IGF0IHRoZSB0b3Agb2YgaGlzdG9yeTsgdGhlIG5ld2VzdCB0dXJuXG4gIC8vIGxpdmVzIGF0IHRoZSB0YWlsLCBzbyBqdW1wIHRoZXJlIGJlZm9yZSBhc3NlcnRpbmcgaXQgcmVuZGVyZWQuXG4gIGNvbnN0IGp1bXBMYXRlc3QgPSBwYWdlLmdldEJ5VGVzdElkKFwianVtcC1sYXRlc3RcIik7XG4gIGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKFxuICAgICAgYXN5bmMgKCkgPT4ge1xuICAgICAgICBpZiAoYXdhaXQganVtcExhdGVzdC5pc1Zpc2libGUoKS5jYXRjaCgoKSA9PiBmYWxzZSkpIHtcbiAgICAgICAgICBhd2FpdCBqdW1wTGF0ZXN0LmNsaWNrKCkuY2F0Y2goKCkgPT4gdW5kZWZpbmVkKTtcbiAgICAgICAgfVxuICAgICAgICByZXR1cm4gdHJhbnNjcmlwdC50ZXh0Q29udGVudCgpO1xuICAgICAgfSxcbiAgICAgIHsgdGltZW91dDogMTVfMDAwLCBpbnRlcnZhbHM6IFsyMDAsIDUwMF0gfSxcbiAgICApXG4gICAgLnRvQ29udGFpbihgam91cm5hbF9idXJzdCBldmVudCAke3Jlc3luY05ld2VzdExhYmVsfWApO1xufSk7XG4iXSwibWFwcGluZ3MiOiJBQUFBLFNBQVNBLE1BQU0sRUFBRUMsSUFBSSxRQUFzRCxrQkFBa0I7QUFDN0YsU0FBU0MsS0FBSyxRQUFRLFlBQVk7O0FBRWxDO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTs7QUFFQUQsSUFBSSxDQUFDRSxRQUFRLENBQUNDLFNBQVMsQ0FBQztFQUFFQyxJQUFJLEVBQUU7QUFBUyxDQUFDLENBQUM7QUFFM0NKLElBQUksQ0FBQ0ssSUFBSSxDQUFDQyxPQUFPLENBQUNDLEdBQUcsQ0FBQ0MsZ0JBQWdCLEtBQUssR0FBRyxFQUFFLGdDQUFnQyxDQUFDO0FBRWpGLE1BQU1DLFdBQVcsR0FBRyxJQUFJOztBQUV4QjtBQUNBLElBQUlDLE1BQXNELEdBQUcsSUFBSTtBQUVqRSxlQUFlQyxpQkFBaUJBLENBQUNDLElBQVUsRUFBRUMsS0FBYSxFQUFFO0VBQzFELE1BQU1ELElBQUksQ0FBQ0UsUUFBUSxDQUFDLE1BQU9DLElBQUksSUFBSztJQUFBLElBQUFDLFdBQUE7SUFDbEMsTUFBTUMsSUFBSSxHQUFHLE1BQU1DLEtBQUssQ0FBQyxXQUFXLEVBQUU7TUFBRUMsV0FBVyxFQUFFO0lBQVUsQ0FBQyxDQUFDO0lBQ2pFLE1BQU1DLElBQUksR0FBSSxNQUFNSCxJQUFJLENBQUNJLElBQUksQ0FBQyxDQUU3QjtJQUNELE1BQU1DLEVBQUUsSUFBQU4sV0FBQSxHQUFHSSxJQUFJLENBQUNHLEtBQUssY0FBQVAsV0FBQSxnQkFBQUEsV0FBQSxHQUFWQSxXQUFBLENBQVlRLElBQUksQ0FBRUMsSUFBSSxJQUFLQSxJQUFJLENBQUNDLE1BQU0sQ0FBQyxjQUFBVixXQUFBLHVCQUF2Q0EsV0FB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