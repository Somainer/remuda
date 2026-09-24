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
  capabilities: { longTasks: boolean; jsHeap: boolean };
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
  /**
   * Long-task metrics. null means the engine cannot measure this (e.g. WebKit
   * has no Long Tasks API) — never a measured zero.
   */
  longTaskCount: number | null;
  longTasksPerMinute: number | null;
  totalBlockingTimeMs: number | null;
  worstLongTask: {
    durationMs: number;
    regionLabel: string | null;
    regionStackTop: string | null;
    container: string | null;
  } | null;
  /** Long tasks with no instrumented region on the stack — reported, not guessed. */
  unattributedLongTasks: number | null;
  /** Tasks per instrumented region label (attribution histogram). */
  regionHistogram: Record<string, number> | null;
  /**
   * Instrumented cost within the scenario window, whether or not it crossed
   * the 50ms long-task threshold: call count, total synchronous ms and worst
   * ms per label. This is what separates "expensive but never blocking" from
   * "main-thread blocker". Region timing uses performance.now() and works on
   * every engine, so it is never null.
   */
  regionTimings: Record<string, { calls: number; totalMs: number; maxMs: number }>;
  /** null when the engine exposes no JS heap API (non-Chromium), not zero. */
  peakJsHeapBytes: number | null;
  terminalRenderer: string | null;
  terminalContextLosses: number;
  /**
   * Scenario E (UO-8): board-card commit probes. An unchanged 5s poll tick
   * must commit zero cards; changing one task commits exactly that card.
   */
  boardCard?: {
    tasks: number;
    pendingInteractions: number;
    initialMountCommits: number;
    initialUpdateCommits: number;
    initialCommitTotalMs: number;
    initialCommitMaxMs: number;
    quietTickCommits: number;
    /** Quiet-window commits not explained by a relative-time label crossing. */
    quietUnexplainedCommits: number;
    changedTickCommits: number;
    /** Update commits attributed to the one PATCHed card (must be exactly 1). */
    changedCardCommits: number;
  };
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
 * Poll window.performance.memory (Chromium only). readPeakHeap returns null
 * on engines without the API so the JSON says "unsupported" rather than 0.
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
  return page.evaluate(() => {
    const hasMemory =
      "memory" in performance &&
      (performance as unknown as { memory?: object }).memory != null;
    if (!hasMemory) return null;
    return (window as unknown as { __perfHeapMax?: number }).__perfHeapMax ?? 0;
  });
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
  // No Long Tasks API on this engine (e.g. WebKit): emit null, not 0, so an
  // unsupported metric is never mistaken for a measured zero.
  const longTasksSupported = report.capabilities?.longTasks ?? true;
  return {
    engine,
    engineUserAgent: report.userAgent,
    scenario,
    wallMs,
    target,
    longTaskCount: longTasksSupported ? tasks.length : null,
    longTasksPerMinute: longTasksSupported
      ? wallMs > 0
        ? (tasks.length / wallMs) * 60_000
        : 0
      : null,
    totalBlockingTimeMs: longTasksSupported ? totalBlockingTimeMs : null,
    worstLongTask: longTasksSupported ? worst : null,
    unattributedLongTasks: longTasksSupported ? (histogram["(unattributed)"] ?? 0) : null,
    regionHistogram: longTasksSupported ? histogram : null,
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
    // Long tasks are null (unsupported), not zero, on engines without the API.
    if (result.longTaskCount != null) expect(result.longTaskCount).toBeGreaterThanOrEqual(0);
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

/**
 * Scenario E (UO-8): 80 board tasks, 40 with a pending human interaction.
 * The follower opens the real `/board` projection with ?profile=1 and the
 * `commit:BoardCard` probe proves incremental rendering: an unchanged poll
 * tick commits zero cards (cards are memoized on their render signature);
 * changing one task's state commits exactly one card on the next tick.
 */
const E_TASKS = 80;
const E_PENDING = 40;

type BoardCardCommit = { phase: string; actualDuration: number; cardId?: string };

const cardIn = (page: Page, id: string) =>
  page.locator(`[data-testid="board-card"][data-task-id="${id}"]`);

function boardCardCommits(report: RemudaPerfReport): BoardCardCommit[] {
  return report.probes
    .filter((probe) => probe.kind === "commit:BoardCard")
    .map((probe) => (probe.value ?? {}) as BoardCardCommit);
}

const cardIdOf = (commit: BoardCardCommit): string => commit.cardId ?? "";

test("E: 80 board tasks with 40 pending interactions — unchanged ticks commit no cards", async ({
  page,
  browser,
}) => {
  test.setTimeout(300_000);
  await installHeapSampler(page);
  await ensureLogin(page, "e2e-perf");

  const driver = await browser.newPage();
  await ensureLogin(driver, "e2e-perf");
  try {
    await patchMaxInstances(driver, Math.max(E_TASKS, 80));

    const fixture = await driver.evaluate(
      async ({ total, pending }) => {
        const auth = { credentials: "include" as RequestCredentials };
        const hosts = (await (await fetch("/v1/hosts", auth)).json()) as {
          items?: { hostId?: string }[];
        };
        const hostId = hosts.items?.find((host) => host.hostId)?.hostId;
        if (!hostId) throw new Error("fake node host missing");

        const post = async (pathName: string, body: unknown) => {
          const response = await fetch(pathName, {
            method: "POST",
            credentials: "include",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(body),
          });
          if (!response.ok) throw new Error(`${pathName} -> ${response.status}`);
          return response.json();
        };
        const patch = async (pathName: string, body: unknown) => {
          const response = await fetch(pathName, {
            method: "PATCH",
            credentials: "include",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(body),
          });
          if (!response.ok) throw new Error(`${pathName} -> ${response.status}`);
        };

        const stamp = Date.now().toString(36);
        const project = (await post("/v1/projects", { name: `perf E ${stamp}` })) as {
          id: string;
        };
        const taskIds: string[] = [];
        for (let i = 0; i < total; i += 1) {
          const task = (await post("/v1/tasks", {
            projectId: project.id,
            title: `perf E ${i} ${stamp}`,
            intent: "perf scenario E board load",
          })) as { id: string };
          taskIds.push(task.id);
        }
        // One parked approval per first N task: each is a distinct instance
        // owned by its task, exactly like the production attention signal.
        const instanceIds: string[] = [];
        for (let i = 0; i < pending; i += 1) {
          const launched = (await post("/v1/instances", {
            hostId,
            workspaceId: "wsp_e2e",
            kind: "claude",
            driver: "claude-print",
            taskId: taskIds[i],
            prompt: `UO8 perf mhome-blocked approval gate ${i} ${stamp}`,
          })) as { instance: { instanceId: string } };
          instanceIds.push(launched.instance.instanceId);
        }
        // One plain task advances placed → it stays in the to-do column but
        // its render signature flips, isolating a single-card commit.
        await patch(`/v1/tasks/${taskIds[pending + 1]}`, { state: "placed" });
        return { projectId: project.id, taskIds, instanceIds };
      },
      { total: E_TASKS, pending: E_PENDING },
    );

    // Wait until all 40 approvals are actually pending.
    await expect
      .poll(
        () =>
          driver.evaluate(async (ids) => {
            const list = (await (await fetch("/v1/interactions", { credentials: "include" })).json()) as {
              items?: { instanceId?: string; state?: string }[];
            };
            const owned = new Set(ids);
            return (list.items ?? []).filter(
              (item) => item.state === "pending" && owned.has(item.instanceId),
            ).length;
          }, fixture.instanceIds),
        { timeout: 120_000, intervals: [1_000, 2_000] },
      )
      .toBeGreaterThanOrEqual(E_PENDING);

    await page.goto(`/board?project=${fixture.projectId}&profile=1`);
    await expect(page.getByTestId("board-card")).toHaveCount(E_TASKS, { timeout: 60_000 });
    await expect
      .poll(
        () => page.locator('[data-testid="board-signal"][data-kind="needs-human"]').count(),
        { timeout: 60_000, intervals: [1_000, 2_000] },
      )
      .toBeGreaterThanOrEqual(E_PENDING);
    // Let the mount-time data streams settle before measuring tick commits.
    await page.waitForTimeout(7_000);

    // Initial load: record every BoardCard commit since the page mounted.
    const initial = boardCardCommits(await getReport(page));
    const initialPhases = initial.reduce(
      (acc, commit) => {
        acc[commit.phase] = (acc[commit.phase] ?? 0) + 1;
        return acc;
      },
      {} as Record<string, number>,
    );

    // Quiet window: one full /v1/board poll tick with nothing changing. A
    // card MAY commit when a relative-time label crosses a boundary (45s /
    // minute) — that is the one allowed reason; read every card's visible
    // session-time labels before/after and require that every commit be a
    // card whose label actually changed (and no other card committed).
    const cardTimeLabels = async (): Promise<Record<string, string>> =>
      page.evaluate(() => {
        const out: Record<string, string> = {};
        document
          .querySelectorAll<HTMLElement>('[data-testid="board-card"]')
          .forEach((card) => {
            const id = card.getAttribute("data-task-id") ?? "";
            const times = [...card.querySelectorAll<HTMLElement>('[data-testid="board-session"]')]
              .map((row) => row.querySelector("span:last-child")?.textContent?.trim() ?? "")
              .join(",");
            out[id] = times;
          });
        return out;
      });

    await page.evaluate(() => window.__remudaPerf?.reset());
    await markScenario(page, "E-quiet-tick");
    const labelsBefore = await cardTimeLabels();
    await page.waitForTimeout(7_000);
    const labelsAfter = await cardTimeLabels();
    const labelFlipIds = new Set(
      Object.keys(labelsAfter).filter((id) => labelsAfter[id] !== labelsBefore[id]),
    );
    const quietCommitsAll = boardCardCommits(await getReport(page));
    const quietCommits = quietCommitsAll.length;
    const quietUnexplained = quietCommitsAll.filter(
      (commit) => !labelFlipIds.has(cardIdOf(commit)),
    );
    expect(
      quietUnexplained.length,
      "an unchanged tick commits nothing except cards whose time label changed",
    ).toBe(0);
    await markScenario(page, null);

    // One task changes state. Open the measurement window BEFORE the PATCH,
    // check the response, then wait for that exact card's new state rather
    // than counting commits, and attribute the commit to the card id.
    const changedTaskId = fixture.taskIds[E_PENDING + 2];
    const labelsBeforeChange = await cardTimeLabels();
    await page.evaluate(() => window.__remudaPerf?.reset());
    await markScenario(page, "E-board");

    const patchStatus = await driver.evaluate(async (id) => {
      const response = await fetch(`/v1/tasks/${id}`, {
        method: "PATCH",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ state: "placed" }),
      });
      return response.status;
    }, changedTaskId);
    expect(patchStatus, "the state change PATCH succeeds").toBe(200);

    // Wait for THIS card to paint its new state (the poll projects placed).
    await expect
      .poll(
        () =>
          cardIn(page, changedTaskId).getAttribute("data-state"),
        { timeout: 20_000, intervals: [500, 1_000] },
      )
      .toBe("placed");
    // Let the tick settle so concurrent label flips are captured.
    await page.waitForTimeout(1_000);

    const labelsAfterChange = await cardTimeLabels();
    const changeLabelFlipIds = new Set(
      Object.keys(labelsAfterChange).filter(
        (id) => labelsAfterChange[id] !== labelsBeforeChange[id],
      ),
    );
    const changeReport = await getReport(page);
    const changedCommits = boardCardCommits(changeReport).filter(
      (commit) => commit.phase === "update",
    );
    const targetCommits = changedCommits.filter(
      (commit) => cardIdOf(commit) === changedTaskId,
    );
    const unexplainedCommits = changedCommits.filter(
      (commit) => cardIdOf(commit) !== changedTaskId && !changeLabelFlipIds.has(cardIdOf(commit)),
    );

    // The changed card commits exactly once; any other commit must be a
    // crossing relative-time label, never another card's data.
    if (targetCommits.length !== 1 || unexplainedCommits.length !== 0) {
      console.log(
        "SCENARIO_E_DEBUG " +
          JSON.stringify({
            changedTaskId,
            changedCommitValues: changedCommits,
            changeLabelFlipIds: [...changeLabelFlipIds],
          }),
      );
    }
    expect(targetCommits.length, "the changed card commits exactly once").toBe(1);
    expect(
      unexplainedCommits.length,
      "no card commits except the changed one and time-label crossings",
    ).toBe(0);

    const result = summarise(
      changeReport,
      browser.browserType().name(),
      "E-board",
      { tasks: E_TASKS, pendingInteractions: E_PENDING },
      await readPeakHeap(page),
    );
    result.boardCard = {
      tasks: E_TASKS,
      pendingInteractions: E_PENDING,
      initialMountCommits: initialPhases.mount ?? 0,
      initialUpdateCommits: initialPhases.update ?? 0,
      initialCommitTotalMs: Math.round(initial.reduce((sum, c) => sum + c.actualDuration, 0) * 100) / 100,
      initialCommitMaxMs: Math.round(Math.max(0, ...initial.map((c) => c.actualDuration)) * 100) / 100,
      quietTickCommits: quietCommits,
      quietUnexplainedCommits: quietUnexplained.length,
      changedTickCommits: changedCommits.length,
      changedCardCommits: targetCommits.length,
    };
    summaries.push(result);
    console.log(JSON.stringify(result, null, 2));
  } finally {
    await driver.close();
    await cleanupPage(browser);
  }
});
