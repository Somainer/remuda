import {
  expect,
  request as apiRequest,
  test,
  devices,
  type APIRequestContext,
  type Browser,
  type BrowserContext,
  type Page,
} from "@playwright/test";
import { seedMode } from "./appearanceHelper";
import { login } from "./hub-auth";

type StorageState = Awaited<ReturnType<BrowserContext["storageState"]>>;

/**
 * P0-9 single-line buttons (visual-system.md §8.1).
 *
 * The two home surfaces are asserted at 390, 1280 and 1440 in light and dark.
 * Viewport routing makes them mutually exclusive (resolveLanding,
 * web/src/lib/mobileRoute.ts): at 390 the compact shell renders /m, at 1280
 * and 1440 the desktop shell renders /sessions. Each surface therefore runs
 * at the widths it actually renders at, and the same page-level sweep covers
 * every visible button, link-button and the PhoneNav bar.
 *
 * The stress rows are real fake-Node sessions with a long Chinese title: on
 * /m the blocked row and the exited row (恢复, the shared `.btn`) prove the
 * row geometry and the one-line actions at phone width, and /sessions carries
 * the same long title through its row and filter controls at desktop width.
 *
 * "One line" is measured from the rendered text, not from the painted height:
 * `.btn` has a `min-height` (and a 44px control height on coarse pointers),
 * so a two-line label can sit in a box whose height looks unchanged. A
 * wrapped label always produces a second line box, so each text node's Range
 * must report a single line.
 *
 * Self-skipping: needs the in-process fake Node (`e2e-fake-node`). Under any
 * Hub without it every case skips instead of failing on a missing fixture.
 */

// Not serial: the seed is created once and every case must still run when an
// earlier viewport fails (a failing serial test skips its siblings). The hub
// config pins one worker and no parallelism, so file order is stable.

const LONG_TITLE = "帮我检查一下这个长会话的构建日志并总结所有失败的测试用例以及下一步应该怎么处理";
const BLOCKED_TOKEN = "homepolish-blocked";
const EXITED_TOKEN = "homepolish-exited";

type Seed = { blockedId: string; exitedId: string };

type Session = { storage: StorageState; request: APIRequestContext; seed: Seed };

let session: Session | null = null;
let unavailable = false;

const VIEWPORTS = [
  { width: 390, label: "390" },
  { width: 1280, label: "1280" },
  { width: 1440, label: "1440" },
] as const;

const THEMES = ["light", "dark"] as const;

/** The fake Node registers shortly after the Hub boots; poll instead of
 *  racing it. Returns null when this Hub has no fake Node at all. */
async function fakeHostId(request: APIRequestContext): Promise<string | null> {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    const response = await request.get("/v1/hosts");
    if (response.ok()) {
      const body = (await response.json()) as { items?: { hostId?: string; id?: string; label?: string }[] };
      const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node");
      const id = host?.hostId ?? host?.id;
      if (id) return id;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  return null;
}

async function createSession(
  request: APIRequestContext,
  hostId: string,
  fields: { title: string; prompt: string },
): Promise<string> {
  const response = await request.post("/v1/instances", {
    data: {
      hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      title: fields.title,
      name: fields.title,
      prompt: fields.prompt,
    },
  });
  expect(response.ok(), `create ${fields.title}: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance?: { instanceId?: string; id?: string } };
  const id = body.instance?.instanceId ?? body.instance?.id;
  expect(id, "created instance id").toBeTruthy();
  return id!;
}

/** One login, then one storage state reused by every viewport/theme context. */
async function prepare(browser: Browser, baseURL: string): Promise<Session | null> {
  if (session) return session;
  if (unavailable) return null;
  const context = await browser.newContext({ baseURL });
  const page = await context.newPage();
  try {
    await login(page, "e2e-homepolish");
    const hostId = await fakeHostId(page.request);
    if (!hostId) {
      unavailable = true;
      return null;
    }
    const blockedId = await createSession(page.request, hostId, {
      title: LONG_TITLE,
      prompt: `${BLOCKED_TOKEN} mhome-blocked`,
    });
    const exitedId = await createSession(page.request, hostId, {
      title: `${LONG_TITLE}（已结束）`,
      prompt: `${EXITED_TOKEN} mhome-exit`,
    });
    const storage = await context.storageState();
    session = {
      storage,
      // The login context closes after this; a standalone API context keeps
      // the cookie jar so afterAll can delete the seeded instances.
      request: await apiRequest.newContext({ baseURL, storageState: storage }),
      seed: { blockedId, exitedId },
    };
    return session;
  } finally {
    await context.close();
  }
}

test.afterAll(async () => {
  if (!session) return;
  for (const id of [session.seed.blockedId, session.seed.exitedId]) {
    await session.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
  await session.request.dispose().catch(() => undefined);
});

type Offender = { testid: string; text: string; lines: string; box: string };

/**
 * Every rendered button must paint each text node on one line box. Nodes of a
 * hidden subtree report no rects and are skipped; icon-only buttons have no
 * text node and cannot wrap.
 */
async function singleLineOffenders(page: Page): Promise<Offender[]> {
  return page.evaluate(() => {
    const SELECTOR = 'button, [role="button"], a[data-testid^="phone-nav-"]';
    const offenders: { testid: string; text: string; lines: string; box: string }[] = [];
    for (const el of Array.from(new Set(document.querySelectorAll(SELECTOR)))) {
      if (el.getAttribute("aria-hidden") === "true") continue;
      const style = getComputedStyle(el);
      if (style.display === "none" || style.visibility === "hidden") continue;
      const box = el.getBoundingClientRect();
      if (box.width < 1 || box.height < 1) continue;
      const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
      let text = "";
      const lines: string[] = [];
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        const content = (node as Text).data;
        if (!content.trim()) continue;
        const range = document.createRange();
        range.selectNodeContents(node);
        const rects = [...range.getClientRects()].filter((rect) => rect.width > 0.5 && rect.height > 0.5);
        if (rects.length === 0) continue; // a display:none subtree inside the button
        text += content.trim();
        // A contiguous text node makes one inline box per line; distinct tops
        // mean it wrapped onto a second line.
        const tops = [...new Set(rects.map((rect) => Math.round(rect.top)))];
        if (tops.length > 1) lines.push(JSON.stringify(tops));
      }
      if (!text.trim()) continue;
      if (lines.length > 0) {
        offenders.push({
          testid: el.getAttribute("data-testid") ?? el.tagName.toLowerCase(),
          text: text.slice(0, 40),
          lines: lines.join(" "),
          box: `${Math.round(box.width)}x${Math.round(box.height)}`,
        });
      }
    }
    return offenders;
  });
}

/** The long title must be on screen (the stress is real) and the page must
 *  not scroll sideways because of it (P0-9 "do not overflow"). */
async function expectNoHorizontalOverflow(page: Page): Promise<void> {
  const overflow = await page.evaluate(() => {
    const root = document.documentElement;
    return { scroll: root.scrollWidth, client: root.clientWidth };
  });
  expect(
    overflow.scroll,
    `page overflows horizontally: scrollWidth ${overflow.scroll} > clientWidth ${overflow.client}`,
  ).toBeLessThanOrEqual(overflow.client + 1);
}

for (const theme of THEMES) {
  for (const viewport of VIEWPORTS) {
    test(`${viewport.label}px ${theme}: every visible button on /m and /sessions is one line`, async ({
      browser,
    }) => {
      const baseURL = test.info().project.use.baseURL;
      expect(baseURL, "project baseURL").toBeTruthy();
      const prepared = await prepare(browser, baseURL!);
      test.skip(!prepared, "needs the in-process fake Node (e2e-fake-node)");

      let context: BrowserContext | null = null;
      try {
        context =
          viewport.width === 390
            ? await browser.newContext({ ...devices["iPhone 13"], baseURL: baseURL!, storageState: prepared!.storage })
            : await browser.newContext({
                viewport: { width: viewport.width, height: 900 },
                baseURL: baseURL!,
                storageState: prepared!.storage,
              });
        const page = await context.newPage();
        await seedMode(page, theme);

        // ---- the surface this width actually renders ----
        // resolveLanding (web/src/lib/mobileRoute.ts) makes /m and /sessions
        // mutually exclusive by viewport: compact phones get /m only
        // (`/sessions` -> `/m`), desktops get /sessions only (`/m*` ->
        // `/sessions`). So 390 asserts the phone home and the desktop widths
        // assert the session list; both carry the same filter/action controls.
        if (viewport.width === 390) {
          await page.goto("/m");
          await expect(page.getByTestId("home-list")).toBeVisible({ timeout: 20_000 });
          expect(await page.evaluate(() => document.documentElement.dataset.appearance)).toBe(theme);
          // Both seeded rows are on screen (the long title is the stress) and
          // the exited row carries the shared `.btn` action.
          const rows = page.getByTestId("home-row").filter({ hasText: LONG_TITLE });
          await expect(rows).toHaveCount(2, { timeout: 20_000 });
          await expect(
            page.locator('[data-testid="home-row"][data-status="blocked"]').filter({ hasText: LONG_TITLE }),
          ).toHaveCount(1);
          await expect(
            page.locator('[data-testid="home-row"][data-status="exited"]').filter({ hasText: LONG_TITLE }),
          ).toHaveCount(1);
          await expect(page.getByTestId("home-resume").first()).toBeVisible();

          expect(await singleLineOffenders(page), "/m buttons render one line").toEqual([]);
          await expectNoHorizontalOverflow(page);
        } else {
          await page.goto("/sessions");
          await expect(page.getByTestId("session-list")).toBeVisible({ timeout: 20_000 });
          expect(await page.evaluate(() => document.documentElement.dataset.appearance)).toBe(theme);
          // /sessions is pinned to the active Space and the fake Node
          // registers many workspaces; the default one has no sessions, so
          // select the seeded Space the way the sidebar does. With a
          // non-empty list the toolbar, rows and their actions all render.
          const row = page.getByTestId("session-row").filter({ hasText: LONG_TITLE });
          if (!(await row.first().isVisible().catch(() => false))) {
            await page
              .locator('[data-testid="space-select"][title^="remuda-e2e ·"]')
              .first()
              .click();
          }
          await expect(row.first()).toBeVisible({ timeout: 20_000 });
          await expect(page.getByTestId("session-filter-open")).toBeVisible();

          expect(await singleLineOffenders(page), "/sessions buttons render one line").toEqual([]);
          await expectNoHorizontalOverflow(page);
        }
      } finally {
        await context?.close();
      }
    });
  }
}
