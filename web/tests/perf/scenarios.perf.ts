import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Browser, type Page } from "@playwright/test";
import { login } from "../e2e/hub-auth";

/**
 * c-perfaudit reproducible load scenarios — MEASUREMENT ONLY.
 *
 * Not part of any gate (own testDir ./tests/perf, own config
 * playwright.perf.config.ts, own pnpm script). The fake Node's perf sentinels
 * exist only when it was started with HUB_E2E_PERF=1, which the perf config
 * sets; without it every test skips.
 *
 * Scenarios (targets are floors from the perf-audit brief):
 *   A long transcript stream — 2000 journal events / 600 tool calls at
 *     20 batches/s while the reader scrolls up and down;
 *   B terminal flood — 5000 output lines at high rate, then scrollback paging;
 *   C interaction flood — 100 pending approvals, inbox scrolling.
 *
 * Every scenario writes a JSON summary into tests/perf/results/ with the
 * engine name taken from Playwright's browserName (chromium / webkit), so the
 * same script produces comparable numbers on Linux Chromium here and macOS
 * Chromium + WebKit on the owner machine.
 */

test.describe.configure({ mode: "serial" });

test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");
test.skip(
  process.env.HUB_E2E_PERF !== "1",
  "perf sentinels need the fake Node started with HUB_E2E_PERF=1 (use playwright.perf.config.ts / pnpm test:perf)",
);

/** Scenario floors (brief: 2000 events / 500 tools, 5000 lines, 100 pending). */
const A_EVENTS = 2000;
const A_BATCHES_PER_SEC = 20;
const B_LINES = 5000;
const B_LINES_PER_FRAME = 25;
const B_FRAMES_PER_SEC = 30;
const C_PENDING = 100;

const RESULTS_DIR = join(dirname(fileURLToPath(import.meta.url)), "results");

type RemudaPerfReport = {
  startedAt: number;
  userAgent: string;
  scenario: string | null;
  scenarios: { name: string; start: number; end: number | null }[];
  longTasks: {
    startTime: number;
    duration: number;
    scenario: string | null;
    region: { label: string; stackTop: string | null } | null;
    container: string | null;
  }[];
  regions: { label: string; start: number; end: number; stackTop: string | null }[];
  probes: { kind: string; value: unknown; at: number }[];
};

type ScenarioResult = {
  engine: string;
  engineUserAgent: string;
  scenario: string;
  wallMs: number;
  target: Record<string, number>;
  longTaskCount: number;
  longTasksPerMinute: number;
  totalBlockingTimeMs: number;
  worstLongTask: {
    durationMs: number;
    regionLabel: string | null;
    regionStackTop: string | null;
    container: string | null;
  } | null;
  /** Long tasks with no instrumented region on the stack — reported, not guessed. */
  unattributedLongTasks: number;
  /** Tasks per instrumented region label (attribution histogram). */
  regionHistogram: Record<string, number>;
  /**
   * Instrumented cost within the scenario window, whether or not it crossed
   * the 50ms long-task threshold: call count, total synchronous ms and worst
   * ms per label. This is what separates "expensive but never blocking" from
   * "main-thread blocker".
   */
  regionTimings: Record<string, { calls: number; totalMs: number; maxMs: number }>;
  peakJsHeapBytes: number | null;
  terminalRenderer: string | null;
  terminalContextLosses: number;
};

const summaries: ScenarioResult[] = [];

async function patchMaxInstances(page: Page, value: number) {
  await page.evaluate(async (next) => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    const body = (await list.json()) as { items?: { hostId?: string }[] };
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
      items?: { instanceId?: string }[];
    };
    await Promise.all(
      (body.items ?? [])
        .filter((instance) => instance.instanceId)
        .map((instance) =>
          fetch(`/v1/instances/${instance.instanceId}?force=1`, {
            method: "DELETE",
            credentials: "include",
          }).catch(() => undefined),
        ),
    );
  });
}

/**
 * Fresh contexts are normally unauthenticated, but a cold document mount
 * right after a heavy scenario can still be booting when the login form is
 * probed (one observed flake: an already-authed shell at /login). Wait for
 * either side of that race instead of assuming the form exists immediately.
 */
async function ensureLogin(page: Page, name: string) {
  await page.goto("/login");
  const formReady = await page
    .getByTestId("login-page")
    .waitFor({ state: "visible", timeout: 15_000 })
    .then(() => true)
    .catch(() => false);
  if (formReady) {
    await login(page, name);
    return;
  }
  await expect(page.getByTestId("session-list")).toBeVisible({ timeout: 15_000 });
}

test.beforeAll(async ({ browser }) => {
  const setup = await browser.newPage();
  await login(setup, "e2e-perf");
  await patchMaxInstances(setup, 24);
  await setup.close();
});

test.afterAll(async ({ browser }, testInfo) => {
  const setup = await browser.newPage();
  await login(setup, "e2e-perf");
  await patchMaxInstances(setup, 8);
  await forceDeleteAllInstances(setup);
  await setup.close();

  // One JSON file per engine run; paths are relative so the artifact carries
  // no username/home/host.
  const engine = browser.browserType().name();
  mkdirSync(RESULTS_DIR, { recursive: true });
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const file = join(RESULTS_DIR, `perf-${engine}-${stamp}.json`);
  writeFileSync(
    file,
    JSON.stringify(
      {
        engine,
        generatedAt: new Date().toISOString(),
        results: summaries,
      },
      null,
      2,
    ),
  );
  // Mirror onto the test attachment so a CI-style run keeps it as well.
  testInfo.attach(`perf-${engine}.json`, {
    body: JSON.stringify(summaries, null, 2),
    contentType: "application/json",
  });
  console.log(`perf results written: ${file}`);
});

async function cleanupPage(browser: Browser) {
  const page = await browser.newPage();
  try {
    await ensureLogin(page, "e2e-perf");
    await forceDeleteAllInstances(page);
  } finally {
    await page.close();
  }
}

async function hostId(page: Page): Promise<string> {
  const hosts = await page.evaluate(async () => {
    const response = await fetch("/v1/hosts", { credentials: "include" });
    return (await response.json()) as { items?: { hostId?: string }[] };
  });
  const id = hosts.items?.find((host) => host.hostId)?.hostId;
  expect(id, "fake Node host should be enrolled").toBeTruthy();
  return id!;
}

async function createInstance(
  page: Page,
  kind: "claude" | "terminal",
  prompt: string,
): Promise<string> {
  const id = await hostId(page);
  const created = await page.evaluate(
    async ({ host, kind, prompt }) => {
      const response = await fetch("/v1/instances", {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          hostId: host,
          workspaceId: "wsp_e2e",
          kind,
          // claude-print takes the generic approval-card create arm.
          driver: kind === "claude" ? "claude-print" : "shell-pty",
          prompt,
        }),
      });
      return response.json() as Promise<{ instance: { instanceId: string } }>;
    },
    { host: id, kind, prompt },
  );
  return created.instance.instanceId;
}

/** POST a Node command through the Hub (same path as hub specs). */
async function command(page: Page, instanceId: string, operation: string, payload: unknown) {
  const response = await page.request.post(`/v1/instances/${instanceId}/commands`, {
    headers: { Origin: new URL(page.url()).origin },
    data: { operation, payload },
    timeout: 30_000,
  });
  expect(response.ok(), `${operation} -> ${response.status()}`).toBe(true);
}

async function answerCreateApproval(page: Page, instanceId: string) {
  await expect
    .poll(
      async () =>
        page.evaluate(async (id) => {
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
          if (!pending || !optionId) return false;
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
          return true;
        }, instanceId),
      { timeout: 30_000, intervals: [200, 500] },
    )
    .toBe(true);
}

async function waitDurable(page: Page, instanceId: string, min: number) {
  await expect
    .poll(
      async () =>
        page.evaluate(async (id) => {
          const response = await fetch(`/v1/instances/${id}/journal`, { credentials: "include" });
          if (!response.ok) return -1;
          return Number((await response.json()).durableSeq as string);
        }, instanceId),
      { timeout: 120_000, intervals: [500, 1000] },
    )
    .toBeGreaterThanOrEqual(min);
}

async function getReport(page: Page): Promise<RemudaPerfReport> {
  return page.evaluate(() => {
    const api = (window as unknown as { __remudaPerf?: { getReport: () => RemudaPerfReport } })
      .__remudaPerf;
    if (!api) throw new Error("profiler missing — page was not loaded with ?profile=1");
    return api.getReport();
  });
}

async function markScenario(page: Page, name: string | null) {
  await page.evaluate((n) => {
    (window as unknown as { __remudaPerf?: { markScenario: (n: string | null) => void } })
      .__remudaPerf?.markScenario(n);
  }, name);
}

/**
 * Poll window.performance.memory (Chromium only); WebKit leaves the peak null
 * and the JSON says so instead of inventing a number.
 */
async function installHeapSampler(page: Page) {
  await page.addInitScript(() => {
    const w = window as unknown as { __perfHeapMax?: number; __perfHeap?: ReturnType<typeof setInterval> };
    w.__perfHeapMax = 0;
    const sample = () => {
      const memory = (performance as unknown as { memory?: { usedJSHeapSize: number } }).memory;
      if (memory) w.__perfHeapMax = Math.max(w.__perfHeapMax ?? 0, memory.usedJSHeapSize);
    };
    w.__perfHeap = setInterval(sample, 200);
  });
}

async function readPeakHeap(page: Page): Promise<number | null> {
  return page.evaluate(() => (window as unknown as { __perfHeapMax?: number }).__perfHeapMax ?? null);
}

function summarise(
  report: RemudaPerfReport,
  engine: string,
  scenario: string,
  target: Record<string, number>,
  peakHeap: number | null,
): ScenarioResult {
  const interval = report.scenarios.find((s) => s.name === scenario);
  const wallMs = interval && interval.end ? interval.end - interval.start : 0;
  const tasks = report.longTasks.filter((task) => task.scenario === scenario);
  const totalBlockingTimeMs = tasks.reduce((sum, task) => sum + Math.max(0, task.duration - 50), 0);
  const worst = tasks.reduce<ScenarioResult["worstLongTask"]>(
    (best, task) =>
      !best || task.duration > best.durationMs
        ? {
            durationMs: task.duration,
            regionLabel: task.region?.label ?? null,
            regionStackTop: task.region?.stackTop ?? null,
            container: task.container,
          }
        : best,
    null,
  );
  const histogram: Record<string, number> = {};
  for (const task of tasks) {
    const label = task.region?.label ?? "(unattributed)";
    histogram[label] = (histogram[label] ?? 0) + 1;
  }
  const intervalStart = interval?.start ?? 0;
  const intervalEnd = interval?.end ?? Number.POSITIVE_INFINITY;
  const timings: ScenarioResult["regionTimings"] = {};
  for (const region of report.regions) {
    if (region.start < intervalStart || region.start > intervalEnd) continue;
    const ms = region.end - region.start;
    const entry = (timings[region.label] ??= { calls: 0, totalMs: 0, maxMs: 0 });
    entry.calls += 1;
    entry.totalMs += ms;
    entry.maxMs = Math.max(entry.maxMs, ms);
  }
  const rendererProbes = report.probes.filter((probe) => probe.kind === "terminal-renderer");
  return {
    engine,
    engineUserAgent: report.userAgent,
    scenario,
    wallMs,
    target,
    longTaskCount: tasks.length,
    longTasksPerMinute: wallMs > 0 ? (tasks.length / wallMs) * 60_000 : 0,
    totalBlockingTimeMs,
    worstLongTask: worst,
    unattributedLongTasks: histogram["(unattributed)"] ?? 0,
    regionHistogram: histogram,
    regionTimings: timings,
    peakJsHeapBytes: peakHeap,
    terminalRenderer: rendererProbes.length
      ? (rendererProbes[rendererProbes.length - 1]!.value as string)
      : null,
    terminalContextLosses: report.probes.filter(
      (probe) => probe.kind === "terminal-renderer-context-loss",
    ).length,
  };
}

test("A: long transcript streaming at 20 batches/s while scrolling", async ({ page, browser }) => {
  test.setTimeout(180_000);
  await installHeapSampler(page);
  await ensureLogin(page, "e2e-perf");

  const driver = await browser.newPage();
  await ensureLogin(driver, "e2e-perf");
  try {
    const instanceId = await createInstance(driver, "claude", "perf scenario A");
    await answerCreateApproval(driver, instanceId);

    // Follower mounts before the flood so live follow frames are measured.
    await page.goto(`/s/${instanceId}/structured?profile=1`);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-journal", "live", {
      timeout: 30_000,
    });

    // Oscillate the transcript scroller for the whole window: top ↔ bottom
    // every 120 ms, like a reader chasing live output while reviewing history.
    await page.evaluate(() => {
      const w = window as unknown as { __perfScroll?: ReturnType<typeof setInterval> };
      let toBottom = false;
      w.__perfScroll = setInterval(() => {
        const el = document.querySelector<HTMLElement>("[data-testid='transcript-scroller']");
        if (!el) return;
        toBottom = !toBottom;
        el.scrollTop = toBottom ? el.scrollHeight : 0;
      }, 120);
    });

    await markScenario(page, "A-long-transcript");
    await command(driver, instanceId, "instance.send", {
      prompt: `__perf_transcript__:${A_EVENTS}:${A_BATCHES_PER_SEC}`,
    });
    // 2000 events in 10-event batches = 200 appends; the last assistant
    // MESSAGE in the batch layout (k = 2,3,6,7) is global 1997 — 1998/1999
    // are the final tool_call/tool_result pair. User message and statuses add
    // a handful of seqs before them.
    await waitDurable(driver, instanceId, A_EVENTS + 2);
    await expect(page.getByTestId("transcript")).toContainText(
      `perf transcript event ${A_EVENTS - 3}`,
      { timeout: 120_000 },
    );
    // Let the final frames paint and the scroll settle.
    await page.waitForTimeout(1500);
    await markScenario(page, null);
    await page.evaluate(() => {
      const w = window as unknown as { __perfScroll?: ReturnType<typeof setInterval> };
      if (w.__perfScroll) clearInterval(w.__perfScroll);
      w.__perfScroll = undefined;
    });

    const result = summarise(
      await getReport(page),
      browser.browserType().name(),
      "A-long-transcript",
      { events: A_EVENTS, toolCalls: 600, batchesPerSec: A_BATCHES_PER_SEC },
      await readPeakHeap(page),
    );
    summaries.push(result);
    console.log(JSON.stringify(result, null, 2));

    // Sanity floors: the scenario actually drove the target load. Numbers are
    // REPORTED, not asserted against performance thresholds — this task
    // measures, it does not fix.
    expect(result.wallMs).toBeGreaterThan(0);
    expect(result.longTaskCount).toBeGreaterThanOrEqual(0);
  } finally {
    await driver.close();
    await cleanupPage(browser);
  }
});

test("B: 5000-line terminal flood then scrollback paging", async ({ page, browser }) => {
  test.setTimeout(180_000);
  await installHeapSampler(page);
  await ensureLogin(page, "e2e-perf");

  const driver = await browser.newPage();
  await ensureLogin(driver, "e2e-perf");
  try {
    const instanceId = await createInstance(driver, "terminal", "perf scenario B");

    await page.goto(`/s/${instanceId}/tty?profile=1`);
    await expect(page.locator("[data-tty-lab='1']")).toBeVisible();
    await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 30_000 });

    await markScenario(page, "B-terminal-flood");
    const sentinel = `__perf_tty__:${B_LINES}:${B_LINES_PER_FRAME}:${B_FRAMES_PER_SEC}`;
    await command(driver, instanceId, "tty.write", {
      dataBase64: Buffer.from(`${sentinel}\r`).toString("base64"),
      source: "ui",
    });
    // The preview <pre> keeps the newest 4000 chars, where the final line lands.
    await expect(page.getByTestId("tty-raw-tail")).toContainText(
      `perf flood line ${String(B_LINES).padStart(5, "0")}`,
      { timeout: 120_000 },
    );

    // Scrollback paging: the brief's "then page up and down". Wheel over the
    // terminal host, alternating direction, several rounds.
    const host = page.locator("[data-tty-lab='1'] .xterm").first();
    const box = await host.boundingBox();
    expect(box).toBeTruthy();
    for (let round = 0; round < 3; round += 1) {
      for (let i = 0; i < 8; i += 1) {
        await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
        await page.mouse.wheel(0, -500);
        await page.waitForTimeout(60);
      }
      for (let i = 0; i < 8; i += 1) {
        await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
        await page.mouse.wheel(0, 500);
        await page.waitForTimeout(60);
      }
    }
    await page.waitForTimeout(1000);
    await markScenario(page, null);

    const result = summarise(
      await getReport(page),
      browser.browserType().name(),
      "B-terminal-flood",
      {
        lines: B_LINES,
        linesPerFrame: B_LINES_PER_FRAME,
        framesPerSec: B_FRAMES_PER_SEC,
      },
      await readPeakHeap(page),
    );
    summaries.push(result);
    console.log(JSON.stringify(result, null, 2));

    // The renderer that ACTUALLY took effect (webgl / canvas / dom).
    expect(result.terminalRenderer).toMatch(/^(webgl|canvas|dom)$/);
    const attr = await page.locator("[data-tty-lab='1']").getAttribute("data-tty-renderer");
    expect(attr).toBe(result.terminalRenderer);
  } finally {
    await driver.close();
    await cleanupPage(browser);
  }
});

test("C: 100 pending interactions with inbox scrolling", async ({ page, browser }) => {
  test.setTimeout(180_000);
  await installHeapSampler(page);
  await ensureLogin(page, "e2e-perf");

  const driver = await browser.newPage();
  await ensureLogin(driver, "e2e-perf");
  try {
    const instanceId = await createInstance(driver, "claude", "perf scenario C");
    await answerCreateApproval(driver, instanceId);
    await command(driver, instanceId, "instance.send", {
      prompt: `__perf_interactions__:${C_PENDING}`,
    });

    await page.goto("/approvals?profile=1");
    await expect(page.getByTestId("approvals-page")).toBeVisible();
    await markScenario(page, "C-interaction-flood");
    await expect.poll(async () => page.getByTestId("approval-row").count(), {
      timeout: 30_000,
      intervals: [500, 1000],
    }).toBeGreaterThanOrEqual(C_PENDING);

    // Inbox scroll: the page scrolls inside its overflow:auto body, not the
    // window. Page to bottom and back in rounds while the 2s interaction.list
    // poll keeps re-rendering the list.
    const scrollInbox = (top: number) =>
      page.evaluate((target) => {
        const root = document.querySelector<HTMLElement>("[data-testid='approvals-page']");
        // Deepest element that actually scrolls (the CSS-module .body wrapper).
        const scroller = root
          ? Array.from(root.querySelectorAll<HTMLElement>("*")).find(
              (el) => el.scrollHeight > el.clientHeight + 8,
            ) ?? root
          : document.body;
        scroller.scrollTop = target;
      }, top);
    for (let round = 0; round < 4; round += 1) {
      await scrollInbox(Number.MAX_SAFE_INTEGER);
      await page.waitForTimeout(600);
      await scrollInbox(0);
      await page.waitForTimeout(600);
    }
    // Cover at least one 2-second interaction.list poll while scrolling.
    await page.waitForTimeout(2500);
    await markScenario(page, null);

    const result = summarise(
      await getReport(page),
      browser.browserType().name(),
      "C-interaction-flood",
      { pending: C_PENDING },
      await readPeakHeap(page),
    );
    summaries.push(result);
    console.log(JSON.stringify(result, null, 2));
  } finally {
    await driver.close();
    await cleanupPage(browser);
  }
});
