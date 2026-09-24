import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * UO-5 / D-053 reading column, against the fake Node.
 *
 * Asserts the column measure (720 / 504 / 358 at 1440 / 768 / 390), the
 * desktop default that folds a settled tool to one quiet line while a failure
 * stays open, a streaming row that keeps its height when the caret comes and
 * goes, and that both system appearances resolve. Screenshots are committed
 * evidence only under REMUDA_EVIDENCE=1 (390 and 1440, dark and light).
 */

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = path.join(here, "../../../docs/design/evidence");

async function shot(page: Page, name: string): Promise<void> {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

const RICH_PROMPT = [
  "阅读列样张：中文与 English prose 混排，正文保持一个安静的行宽，作者与节奏区分你我，而不是气泡。",
  "The reading column keeps a calm rhythm; settled tools collapse into one quiet line.",
  "",
  "## 小结",
  "",
  "- 第一项，带 `inline code`",
  "- 第二项",
  "  - 嵌套的一项",
  "",
  "> 引用：失败、提问与审批始终可见。",
  "",
  "| 宽度 | Measure |",
  "| --- | --- |",
  "| 1440 | 720 |",
  "| 390 | 358 |",
  "",
  "```ts",
  "const measure = 720;",
  "```",
].join("\n");

test.describe.configure({ mode: "serial" });

async function patchMaxInstances(page: Page, value: number): Promise<void> {
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

async function forceDeleteAllInstances(page: Page): Promise<void> {
  await page.evaluate(async () => {
    const list = await fetch("/v1/instances", { credentials: "include" });
    const body = (await list.json()) as { items?: { instanceId?: string }[] };
    await Promise.all(
      (body.items ?? []).map((instance) =>
        fetch(`/v1/instances/${instance.instanceId}?force=1`, {
          method: "DELETE",
          credentials: "include",
        }).catch(() => undefined),
      ),
    );
  });
}

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  await login(page);
  await patchMaxInstances(page, 24);
  await page.close();
});

test.afterAll(async ({ browser }) => {
  const page = await browser.newPage();
  await login(page);
  await forceDeleteAllInstances(page).catch(() => undefined);
  await patchMaxInstances(page, 8);
  await page.close();
});

async function answerPending(page: Page, instanceId: string): Promise<void> {
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
          const mine = (body.items ?? []).filter(
            (item) => item.instanceId === id && item.state === "pending",
          );
          for (const item of mine) {
            const optionId = item.request?.options?.[0]?.id;
            if (!optionId) continue;
            await fetch(`/v1/interactions/${item.id}/answer`, {
              method: "POST",
              credentials: "include",
              headers: { "content-type": "application/json" },
              body: JSON.stringify({
                answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" },
              }),
            });
          }
          return mine.length;
        }, instanceId),
      { timeout: 20_000 },
    )
    .toBe(0);
}

async function createSession(page: Page): Promise<string> {
  await forceDeleteAllInstances(page);
  await page.goto("/sessions/new");
  await expect(page.getByTestId("new-session-host")).toContainText("e2e-fake-node", {
    timeout: 20_000,
  });
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0);
  await page.getByTestId("new-session-prompt").fill("uo5 reading column");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await answerPending(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  return instanceId;
}

async function send(page: Page, prompt: string): Promise<void> {
  await page.getByTestId("composer-input").fill(prompt);
  await page.getByTestId("composer-send").click();
}

/** Content-box width of the reading column that holds the rows. */
async function measure(page: Page): Promise<number> {
  return page.getByTestId("transcript-row").first().evaluate((row) => {
    const list = row.parentElement as HTMLElement;
    return Math.round(parseFloat(getComputedStyle(list).width));
  });
}

/**
 * Hold every follow frame after a stream opens so the streaming state is on
 * screen long enough to measure; order is preserved.
 */
async function holdStreams(page: Page, ms = 1500): Promise<() => void> {
  const flushes: (() => void)[] = [];
  await page.routeWebSocket(/\/v1\/follow/, (ws) => {
    const server = ws.connectToServer();
    let holdUntil = 0;
    let queue: (string | Buffer)[] = [];
    const flush = () => {
      holdUntil = 0;
      for (const frame of queue) ws.send(frame);
      queue = [];
    };
    flushes.push(flush);
    server.onMessage((frame) => {
      if (Date.now() < holdUntil) {
        queue.push(frame);
        return;
      }
      ws.send(frame);
      if (typeof frame === "string" && frame.includes('"operation":"open"') && frame.includes('"stream-')) {
        holdUntil = Date.now() + ms;
        // An infinite hold lasts until the test calls the returned release.
        if (Number.isFinite(ms)) setTimeout(flush, ms);
      }
    });
    ws.onMessage((frame) => server.send(frame));
  });
  return () => {
    for (const flush of flushes) flush();
  };
}

test("a streaming row keeps its height when the caret comes and goes", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await holdStreams(page);
  await login(page);
  await createSession(page);

  // Short enough that both halves sit on one line: any height change would
  // come from the caret itself.
  await send(page, "stream ab");
  const row = page.getByTestId("transcript-row").filter({
    has: page.getByTestId("streaming-cursor"),
  });
  await expect(row).toHaveCount(1, { timeout: 20_000 });
  const streamingHeight = await row.evaluate((el) => el.getBoundingClientRect().height);
  const anchor = await row.getAttribute("data-anchor");
  const done = page.locator(`[data-testid="transcript-row"][data-anchor="${anchor}"]`);
  await expect(done.getByTestId("streaming-cursor")).toHaveCount(0, { timeout: 20_000 });
  await expect(done).toContainText("echo: stream ab");
  const settledHeight = await done.evaluate((el) => el.getBoundingClientRect().height);
  expect(settledHeight - streamingHeight).toBe(0);
});

test("the caret on a long unwrapped code line never widens the transcript", async ({ page }) => {
  await holdStreams(page, 2000);
  await login(page);
  const scroller = page.getByTestId("transcript-scroller");
  const overflow = () => scroller.evaluate((el) => el.scrollWidth - el.clientWidth);
  for (const width of [1440, 390]) {
    await page.setViewportSize({ width, height: width === 390 ? 844 : 900 });
    // A fresh session at each width: the phone shell lands on it directly.
    await createSession(page);
    // The reply splits mid-line, so the streaming half ends inside the
    // unclosed fence, on a ~150 character line wider than the measure.
    await send(page, `stream ${width}\n\`\`\`\n${"x".repeat(300)}\n\`\`\``);
    const row = page.getByTestId("transcript-row").filter({
      has: page.getByTestId("streaming-cursor"),
    });
    await expect(row).toHaveCount(1, { timeout: 20_000 });
    await expect(row.locator("pre")).toHaveCount(1);
    const box = await row.evaluate((el) => {
      const cursor = el.querySelector("[data-testid='streaming-cursor']")!.getBoundingClientRect();
      const pre = el.querySelector("pre")!.getBoundingClientRect();
      return { cursorRight: cursor.right, preRight: pre.right };
    });
    expect(box.cursorRight).toBeLessThanOrEqual(box.preRight + 0.5);
    expect(await overflow()).toBe(0);
    // Scrolling the block re-places the caret without widening anything.
    await row.locator("pre").evaluate((el) => el.scrollBy(200, 0));
    expect(await overflow()).toBe(0);
    const anchor = await row.getAttribute("data-anchor");
    const done = page.locator(`[data-testid="transcript-row"][data-anchor="${anchor}"]`);
    await expect(done.getByTestId("streaming-cursor")).toHaveCount(0, { timeout: 20_000 });
    expect(await overflow()).toBe(0);
    await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  }
});

test("the caret stays at the visible end of a highlighted line while the block scrolls", async ({ page }) => {
  // Held until measured: the lazily loaded highlighter lands mid-stream.
  const release = await holdStreams(page, Number.POSITIVE_INFINITY);
  // The turn-end check also catches up over HTTP; hold that too, or the
  // close lands through it before the caret is measured.
  let releaseJournal = () => {};
  const journalHeld = new Promise<void>((done) => {
    releaseJournal = done;
  });
  await page.route(/\/journal\?afterSeq=/, async (route) => {
    await journalHeld;
    await route.continue();
  });
  await login(page);
  await page.setViewportSize({ width: 1440, height: 900 });
  await createSession(page);
  // Language-tagged: highlighting lands after the text commits and swaps the
  // code's text node for spans, under a caret that is already placed.
  await send(page, `stream hl\n\`\`\`ts\n${"const value = 42; ".repeat(30)}\n\`\`\``);
  const streaming = page.getByTestId("transcript-row").filter({ has: page.getByTestId("streaming-cursor") });
  await expect(streaming).toHaveCount(1, { timeout: 20_000 });
  // Pinned by anchor: the same row once the stream closes.
  const anchor = await streaming.getAttribute("data-anchor");
  const row = page.locator(`[data-testid="transcript-row"][data-anchor="${anchor}"]`);
  const pre = row.locator("pre");
  await expect(pre.locator("code span").first()).toBeAttached({ timeout: 10_000 });
  const measure = () =>
    row.evaluate((el) => {
      const node = el.querySelector<HTMLElement>("[data-testid='streaming-cursor']");
      if (!node) return null;
      const cursor = node.getBoundingClientRect();
      const box = el.querySelector("pre")!;
      const pre = box.getBoundingClientRect();
      return {
        cursor: { left: cursor.left, right: cursor.right, top: cursor.top, bottom: cursor.bottom },
        pre: { right: pre.left + box.clientLeft + box.clientWidth, top: pre.top, bottom: pre.bottom },
        margin: getComputedStyle(node).marginTop,
      };
    });
  for (const dx of [0, 200, 400]) {
    await pre.evaluate((el, x) => el.scrollTo(x, 0), dx);
    // One frame for the capturing scroll listener to re-place.
    await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => done(null))));
    const box = await measure();
    expect(box,`still streaming at ${page.url()}`).not.toBeNull();
    const { cursor, pre: code, margin } = box!;
    // The line is wider than the block, so its end is past the right edge:
    // the caret is clamped to the visible end, inside the block vertically.
    expect(margin).toBe("0px");
    expect(cursor.right).toBeLessThanOrEqual(code.right + 0.5);
    expect(cursor.right).toBeGreaterThan(code.right - 2);
    expect(cursor.top).toBeGreaterThanOrEqual(code.top);
    expect(cursor.bottom).toBeLessThanOrEqual(code.bottom);
  }
  release();
  releaseJournal();
  await expect(row.getByTestId("streaming-cursor")).toHaveCount(0, { timeout: 20_000 });
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
});

let instanceId = "";

test("a row above the viewport that grows does not move the text being read", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await login(page);
  await createSession(page);
  for (let i = 0; i < 14; i += 1) {
    await send(page, `锚点 ${i}\n\n${"一段用来撑高的正文。".repeat(30)}`);
    await expect(page.getByTestId("message").filter({ hasText: `echo: 锚点 ${i}` })).toHaveCount(1, {
      timeout: 20_000,
    });
  }
  const scroller = page.getByTestId("transcript-scroller");
  // Read from the middle, away from the bottom follow.
  await scroller.evaluate((el) => {
    el.scrollTop = Math.round((el.scrollHeight - el.clientHeight) / 2);
  });
  await page.waitForTimeout(300);
  const firstVisible = () =>
    scroller.evaluate((el) => {
      const top = el.getBoundingClientRect().top;
      const rows = [...el.querySelectorAll<HTMLElement>("[data-anchor]")];
      const index = rows.findIndex((row) => row.getBoundingClientRect().bottom > top);
      return {
        id: rows[index]?.dataset.anchor ?? "",
        top: rows[index] ? rows[index].getBoundingClientRect().top - top : 0,
        above: index > 0 ? (rows[index - 1].dataset.anchor ?? "") : "",
      };
    });
  const before = await firstVisible();
  expect(before.above, "a mounted row sits above the viewport").not.toBe("");
  // Grow that row by 200px after first paint, the way a late result would.
  await scroller.evaluate((el, id) => {
    const row = el.querySelector<HTMLElement>(`[data-anchor="${CSS.escape(id)}"]`)!;
    const grow = document.createElement("div");
    grow.style.height = "200px";
    row.firstElementChild!.appendChild(grow);
  }, before.above);
  await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  const after = await firstVisible();
  expect(after.id).toBe(before.id);
  expect(Math.abs(after.top - before.top)).toBeLessThanOrEqual(1);
});

test("after a jump past the mounted window, a row that grows above does not move the text", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await login(page);
  await createSession(page);
  // Tall rows, each measured as it arrives at the followed bottom.
  for (let i = 0; i < 14; i += 1) {
    await send(page, `跳转 ${i}\n\n${"一段用来撑高的正文。".repeat(160)}`);
    await expect(page.getByTestId("message").filter({ hasText: `echo: 跳转 ${i}` })).toHaveCount(1, {
      timeout: 20_000,
    });
  }
  const scroller = page.getByTestId("transcript-scroller");
  // One assignment several viewports up: the scroll event samples while the
  // old window is still mounted, so no mounted row is in view yet.
  const jump = await scroller.evaluate((el) => {
    const from = el.scrollTop;
    const mounted = [...el.querySelectorAll<HTMLElement>("[data-anchor]")].map((row) => row.dataset.anchor);
    el.scrollTop = 1200;
    return { from, to: el.scrollTop, mounted };
  });
  expect(jump.from - jump.to, "the jump spans several viewports").toBeGreaterThan(900 * 4);
  // Let the window commit the new range, and nothing else scroll.
  await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  const firstVisible = () =>
    scroller.evaluate((el) => {
      const top = el.getBoundingClientRect().top;
      const rows = [...el.querySelectorAll<HTMLElement>("[data-anchor]")];
      const index = rows.findIndex((row) => row.getBoundingClientRect().bottom > top);
      return {
        id: rows[index]?.dataset.anchor ?? "",
        top: rows[index] ? rows[index].getBoundingClientRect().top - top : 0,
        above: index > 0 ? (rows[index - 1].dataset.anchor ?? "") : "",
      };
    });
  const before = await firstVisible();
  expect(jump.mounted, "the jump lands beyond the mounted window").not.toContain(before.id);
  expect(before.above, "a mounted row sits above the viewport").not.toBe("");
  await scroller.evaluate((el, id) => {
    const row = el.querySelector<HTMLElement>(`[data-anchor="${CSS.escape(id)}"]`)!;
    const grow = document.createElement("div");
    grow.style.height = "200px";
    row.firstElementChild!.appendChild(grow);
  }, before.above);
  await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  const after = await firstVisible();
  expect(after.id).toBe(before.id);
  expect(Math.abs(after.top - before.top)).toBeLessThanOrEqual(1);
});

test("the reading column holds 720 / 504 / 358 and folds settled tools on desktop", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await login(page);
  instanceId = await createSession(page);

  await send(page, RICH_PROMPT);
  await expect(page.getByTestId("message").filter({ hasText: "echo: 阅读列样张" })).toHaveCount(1, {
    timeout: 20_000,
  });
  await send(page, "toolfold settle");
  const settled = page.getByTestId("tool-card").first();
  await expect(settled).toBeVisible({ timeout: 20_000 });
  // D-053: a settled card is one quiet line at desktop width by default.
  await expect(settled).toHaveAttribute("data-folded", "1", { timeout: 20_000 });
  await send(page, "workflow card fail");
  await expect(page.getByTestId("tool-card").filter({ hasText: /fail|失败/ }).first()).toBeVisible({
    timeout: 20_000,
  });

  for (const [width, expected] of [
    [1440, 720],
    [768, 504],
    [390, 358],
  ] as const) {
    await page.setViewportSize({ width, height: width === 390 ? 844 : 900 });
    await expect.poll(() => measure(page)).toBe(expected);
  }
});

for (const scheme of ["dark", "light"] as const) {
  test(`the reading column renders in the system ${scheme} appearance`, async ({ page }) => {
    test.skip(!instanceId, "needs the session from the first test");
    await page.emulateMedia({ colorScheme: scheme });
    await login(page);
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: width === 390 ? 844 : 900 });
      await page.goto(`/s/${instanceId}`);
      const transcript = page.getByTestId("transcript");
      await expect(transcript).toContainText("echo: 阅读列样张", { timeout: 20_000 });
      // The page resolves the system appearance: body text is light on a dark
      // ground in dark mode and dark on a light ground in light mode.
      const luminance = await page.evaluate(() => {
        const parse = (value: string) => {
          const [r, g, b] = (value.match(/[\d.]+/g) ?? ["0", "0", "0"]).map(Number);
          return 0.2126 * r + 0.7152 * g + 0.0722 * b;
        };
        const body = getComputedStyle(document.body);
        const row = document.querySelector("[data-testid='message']") as HTMLElement;
        return { ground: parse(body.backgroundColor), text: parse(getComputedStyle(row).color) };
      });
      if (scheme === "dark") expect(luminance.text).toBeGreaterThan(luminance.ground);
      else expect(luminance.text).toBeLessThan(luminance.ground);
      // Frame the rendered reply (prose, list, quote, table, code) and the
      // tool lines under it.
      await page
        .getByTestId("message")
        .filter({ hasText: "echo: 阅读列样张" })
        .evaluate((el) => el.scrollIntoView({ block: "start" }));
      await shot(page, `uo5-reading-column-${width}-${scheme}.png`);
    }
  });
}
