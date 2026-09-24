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

test("a streaming row keeps its height when the caret comes and goes", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  // Hold every follow frame after the stream opens so the streaming state is
  // on screen long enough to measure; order is preserved.
  await page.routeWebSocket(/\/v1\/follow/, (ws) => {
    const server = ws.connectToServer();
    let holdUntil = 0;
    let queue: (string | Buffer)[] = [];
    const flush = () => {
      for (const frame of queue) ws.send(frame);
      queue = [];
    };
    server.onMessage((frame) => {
      if (Date.now() < holdUntil) {
        queue.push(frame);
        return;
      }
      ws.send(frame);
      if (typeof frame === "string" && frame.includes('"operation":"open"') && frame.includes('"stream-')) {
        holdUntil = Date.now() + 1500;
        setTimeout(flush, 1500);
      }
    });
    ws.onMessage((frame) => server.send(frame));
  });
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

let instanceId = "";

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
