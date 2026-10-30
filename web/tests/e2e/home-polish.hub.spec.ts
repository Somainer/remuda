import {
  expect,
  request as apiRequest,
  test,
  devices,
  type APIRequestContext,
  type Browser,
  type BrowserContext,
  type Locator,
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
/** The hook approval the fake Node raises for a `hook-approval` prompt
 *  carries this exact option set (crates/remuda-hub/examples/hub_e2e.rs,
 *  fake_hook_approval): the session-wide grant label is the long one. */
const LONG_OPTION = "始终允许 (acceptEdits)";
const HOOK_CARD_BODY = "/tmp/hook-approval.txt";
/** A Space name far wider than the phone's header button or drawer row. */
const LONG_SPACE = "项目-用于检查超长空间名称在手机宽度下的省略与完整标题-第二段";
/** The opt-in `hook-approval long-option` fixture's session-wide grant label:
 *  wider than the 390px card's content box, so it can only be readable if
 *  the inline card wraps it instead of clipping. */
const LONG_SCOPE = "始终允许本次会话内对该工作目录下所有文件的编辑与写入操作 (acceptEdits)";

type Seed = { blockedId: string; exitedId: string };

type Session = { storage: StorageState; request: APIRequestContext; seed: Seed; hostId: string };

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
      hostId,
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
 * text node and cannot wrap. The btnAllowWrap exemption (authorization
 * scopes / path targets) is allowed to wrap and is skipped here.
 */
async function singleLineOffenders(page: Page): Promise<Offender[]> {
  return page.evaluate(() => {
    const SELECTOR = 'button, [role="button"], a[data-testid^="phone-nav-"], a[class*="btn" i]';
    const offenders: { testid: string; text: string; lines: string; box: string }[] = [];
    for (const el of Array.from(new Set(document.querySelectorAll(SELECTOR)))) {
      if (el.getAttribute("aria-hidden") === "true") continue;
      if (Array.from(el.classList).some((name) => name.includes("btnAllowWrap"))) continue;
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

type TruncatedUnlabeled = { testid: string; text: string };

/**
 * D-061: a button that IS truncated must carry the full label in `title` or
 * `aria-label`. The exemption (btnAllowWrap — authorization scopes and
 * filesystem targets) wraps instead of truncating, so it is skipped.
 *
 * Coverage (r3): button-like anchors count too (`a[class*="btn"]` catches
 * `.btn` links and composed classes), and every inner label span is measured
 * on its own — a PhoneNav `.label` clips while its wrapping `.item` container
 * does not.
 */
async function truncatedWithoutFullLabel(page: Page): Promise<TruncatedUnlabeled[]> {
  return page.evaluate(() => {
    const SELECTOR =
      'button, [role="button"], a[data-testid^="phone-nav-"], a[class*="btn" i]';
    const norm = (value: string) => value.replace(/\s+/g, " ").trim();
    const exempt = (el: Element) =>
      Array.from(el.classList).some((name) => name.includes("btnAllowWrap"));
    const out: { testid: string; text: string }[] = [];
    for (const el of Array.from(new Set(document.querySelectorAll(SELECTOR)))) {
      const style = getComputedStyle(el);
      if (style.display === "none" || style.visibility === "hidden") continue;
      const box = el.getBoundingClientRect();
      if (box.width < 1 || box.height < 1) continue;
      if (exempt(el)) continue; // the authorization-scope / path-target exemption
      const clipped: string[] = [];
      if (el.scrollWidth > el.clientWidth + 1) clipped.push(norm(el.textContent ?? ""));
      for (const span of Array.from(el.querySelectorAll("span"))) {
        if (span.scrollWidth > span.clientWidth + 1) clipped.push(norm(span.textContent ?? ""));
      }
      if (clipped.length === 0) continue;
      const sources = [
        el.getAttribute("title"),
        el.getAttribute("aria-label"),
        ...Array.from(el.querySelectorAll("[title], [aria-label]")).flatMap((node) => [
          node.getAttribute("title"),
          node.getAttribute("aria-label"),
        ]),
      ]
        .filter((value): value is string => Boolean(value))
        .map(norm);
      const missing = clipped.filter((text) => text && !sources.some((source) => source.includes(text)));
      if (missing.length > 0) {
        out.push({
          testid: el.getAttribute("data-testid") ?? el.tagName.toLowerCase(),
          text: missing.map((text) => text.slice(0, 40)).join(" | "),
        });
      }
    }
    return out;
  });
}

/** An exempt button (authorization scope / path target) must show its whole
 *  label: it wraps, is not ellipsized, and nothing overflows its box. */
async function expectFullyReadable(button: Locator, label: string): Promise<void> {
  await expect(button).toBeVisible();
  await expect(button).toHaveText(label);
  const geometry = await button.evaluate((el) => {
    const style = getComputedStyle(el);
    return {
      scrollWidth: el.scrollWidth,
      clientWidth: el.clientWidth,
      whiteSpace: style.whiteSpace,
      textOverflow: style.textOverflow,
      textRect: (() => {
        const range = document.createRange();
        range.selectNodeContents(el);
        const rect = range.getBoundingClientRect();
        const box = el.getBoundingClientRect();
        return { left: rect.left - box.left, right: rect.right - box.right };
      })(),
    };
  });
  expect(
    geometry.scrollWidth,
    `${label}: label fits (scrollWidth ${geometry.scrollWidth} > clientWidth ${geometry.clientWidth})`,
  ).toBeLessThanOrEqual(geometry.clientWidth + 1);
  expect(geometry.textRect.left, `${label}: text starts inside the button`).toBeGreaterThanOrEqual(-1);
  expect(geometry.textRect.right, `${label}: text ends inside the button`).toBeLessThanOrEqual(1);
  expect(geometry.whiteSpace, `${label}: wraps instead of truncating`).toBe("normal");
  expect(geometry.textOverflow, `${label}: is not ellipsized`).not.toBe("ellipsis");
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
          expect(await truncatedWithoutFullLabel(page), "/m truncated buttons carry their full text").toEqual([]);
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
          expect(
            await truncatedWithoutFullLabel(page),
            "/sessions truncated buttons carry their full text",
          ).toEqual([]);
          await expectNoHorizontalOverflow(page);
        }
      } finally {
        await context?.close();
      }
    });
  }
}

/**
 * D-061 r2: buttons whose label states an authorization scope (approval
 * options) or a filesystem target (the Space name) are exempt from the
 * single-line clip. The hook approval's 始终允许 (acceptEdits) is wider than a
 * compact card row third: it must stay fully readable, and a truncated Space
 * name must carry the full name in its title.
 */
test("390 light: long approval options and a long Space name stay readable", async ({ browser }) => {
  const baseURL = test.info().project.use.baseURL;
  expect(baseURL, "project baseURL").toBeTruthy();
  const prepared = await prepare(browser, baseURL!);
  test.skip(!prepared, "needs the in-process fake Node (e2e-fake-node)");

  const approvalCreate = await prepared!.request.post("/v1/instances", {
    data: {
      hostId: prepared!.hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      title: "homepolish hook approval",
      prompt: `hook-approval ${Date.now().toString(36)}`,
    },
  });
  expect(approvalCreate.ok(), `create hook approval: ${approvalCreate.status()}`).toBe(true);
  const approvalBody = (await approvalCreate.json()) as { instance?: { instanceId?: string; id?: string } };
  const approvalId = approvalBody.instance?.instanceId ?? approvalBody.instance?.id;
  expect(approvalId, "hook approval instance").toBeTruthy();

  const context = await browser.newContext({
    ...devices["iPhone 13"],
    baseURL: baseURL!,
    storageState: prepared!.storage,
  });
  try {
    const page = await context.newPage();
    await seedMode(page, "light");

    // ---- the compact approval card: full option labels, never clipped ----
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible({ timeout: 20_000 });
    const card = page.getByTestId("approval-row").filter({ hasText: HOOK_CARD_BODY });
    await expect(card).toBeVisible({ timeout: 20_000 });
    await expectFullyReadable(card.getByRole("button", { name: LONG_OPTION }), LONG_OPTION);
    await expectFullyReadable(card.getByRole("button", { name: "允许一次" }), "允许一次");
    await expectFullyReadable(card.getByRole("button", { name: "拒绝" }), "拒绝");
    // Exempt labels never truncate, so the general title rule has nothing to
    // report on this card either.
    expect(await truncatedWithoutFullLabel(page), "/m/inbox truncated buttons carry their full text").toEqual(
      [],
    );

    // ---- a long Space name: the header button ellipsizes, title keeps all ----
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible({ timeout: 20_000 });
    await page.getByTestId("spaces-drawer-open").click();
    const drawer = page.getByTestId("spaces-drawer");
    await expect(drawer).toBeVisible();
    await drawer.getByRole("button", { name: "重命名" }).click();
    await drawer.getByLabel("空间名称").fill(LONG_SPACE);
    await drawer.getByRole("button", { name: "保存" }).click();

    const spaceRow = drawer.getByTestId("space-select").filter({ hasText: LONG_SPACE });
    await expect(spaceRow).toHaveCount(1);
    // The drawer row clips the name (it is genuinely too wide) but its title
    // carries the whole name for a long-press / hover read.
    expect(await spaceRow.getAttribute("title")).toContain(LONG_SPACE);
    const nameClipped = await spaceRow.evaluate((el) => {
      const name = el.querySelector("span");
      return name ? name.scrollWidth > name.clientWidth + 1 : false;
    });
    expect(nameClipped, "the long Space name really is wider than the drawer row").toBe(true);

    // Select the renamed Space: with no remembered selection the home shows
    // the first Space by name, and the rename moved this one out of first
    // place. Selecting it closes the drawer and lands back on /m.
    await spaceRow.click();
    await page.waitForURL((url) => url.pathname === "/m" || url.pathname.startsWith("/s/"), {
      timeout: 20_000,
    });
    if (new URL(page.url()).pathname !== "/m") await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible({ timeout: 20_000 });
    const header = page.getByTestId("spaces-drawer-open");
    await expect(header).toHaveAttribute("title", LONG_SPACE);
    expect(await truncatedWithoutFullLabel(page), "the header Space button carries its full name").toEqual(
      [],
    );
    await expectNoHorizontalOverflow(page);
  } finally {
    await context.close();
    await prepared!.request.delete(`/v1/instances/${approvalId}?force=1`).catch(() => undefined);
  }
});

/**
 * D-061 r3: the same exemption on the session-pinned card above the composer.
 * `hook-approval long-option` makes the fake Node emit a session-wide grant
 * label wider than the 390px card, so a nowrap button would hide the scope.
 */
test("390 light: the session-pinned approval card wraps a long scope label", async ({ browser }) => {
  const baseURL = test.info().project.use.baseURL;
  expect(baseURL, "project baseURL").toBeTruthy();
  const prepared = await prepare(browser, baseURL!);
  test.skip(!prepared, "needs the in-process fake Node (e2e-fake-node)");

  const create = await prepared!.request.post("/v1/instances", {
    data: {
      hostId: prepared!.hostId,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      title: "homepolish long scope",
      prompt: `hook-approval long-option ${Date.now().toString(36)}`,
    },
  });
  expect(create.ok(), `create long-scope approval: ${create.status()}`).toBe(true);
  const body = (await create.json()) as { instance?: { instanceId?: string; id?: string } };
  const instanceId = body.instance?.instanceId ?? body.instance?.id;
  expect(instanceId, "long-scope instance").toBeTruthy();

  const context = await browser.newContext({
    ...devices["iPhone 13"],
    baseURL: baseURL!,
    storageState: prepared!.storage,
  });
  try {
    const page = await context.newPage();
    await seedMode(page, "light");
    await page.goto(`/s/${instanceId}`);
    const card = page.getByTestId("approval-card");
    await expect(card).toBeVisible({ timeout: 20_000 });
    await expectFullyReadable(card.getByRole("button", { name: LONG_SCOPE }), LONG_SCOPE);
    await expectFullyReadable(card.getByRole("button", { name: "拒绝" }), "拒绝");
    expect(await truncatedWithoutFullLabel(page), "no truncated card option").toEqual([]);
    await expectNoHorizontalOverflow(page);
  } finally {
    await context.close();
    await prepared!.request.delete(`/v1/instances/${instanceId}?force=1`).catch(() => undefined);
  }
});

/**
 * D-061 r3: the truncation scan must see what the product actually renders —
 * link-buttons (e.g. the `去回答` anchor, `openLink`) and labels that clip on
 * an inner span while their container wraps (the PhoneNav `.label`). This is
 * a self-test of the assertion: two synthetic controls stand in for a case
 * the fixture pages do not currently produce.
 */
test("the truncation scan sees link-buttons and inner label spans", async ({ browser }) => {
  const baseURL = test.info().project.use.baseURL;
  expect(baseURL, "project baseURL").toBeTruthy();
  const prepared = await prepare(browser, baseURL!);
  test.skip(!prepared, "needs the in-process fake Node (e2e-fake-node)");

  const context = await browser.newContext({
    ...devices["iPhone 13"],
    baseURL: baseURL!,
    storageState: prepared!.storage,
  });
  try {
    const page = await context.newPage();
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible({ timeout: 20_000 });
    await page.evaluate(() => {
      const host = document.createElement("div");
      host.id = "synthetic-truncation";
      host.style.cssText = "position:fixed;left:0;top:0;width:140px;z-index:99999";
      host.innerHTML = [
        '<a class="fakeBtn" data-testid="synthetic-link-btn" href="/m"',
        ' style="display:inline-block;max-width:60px;overflow:hidden;white-space:nowrap;',
        'text-overflow:ellipsis">一个很长的链接按钮文字</a>',
        '<button type="button" data-testid="synthetic-spanned"',
        ' style="display:inline-flex;white-space:normal;max-width:60px">',
        '<span style="display:inline-block;max-width:40px;overflow:hidden;white-space:nowrap;',
        'text-overflow:ellipsis">另一个很长的内部标签</span></button>',
      ].join("");
      document.body.append(host);
    });
    const offenders = await truncatedWithoutFullLabel(page);
    const ids = offenders.map((entry) => entry.testid);
    expect(ids, "a truncated anchor styled as a button is caught").toContain("synthetic-link-btn");
    expect(ids, "a truncated label span inside a wrapping container is caught").toContain(
      "synthetic-spanned",
    );
    await page.evaluate(() => document.getElementById("synthetic-truncation")?.remove());
    expect(await truncatedWithoutFullLabel(page), "nothing synthetic is left").toEqual([]);
  } finally {
    await context.close();
  }
});
