import { devices, expect, test, type Browser, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * home-glance-strip (home.md §1 ①②, §2.1, §2.2, §2.5).
 *
 * Gated here:
 *  - at 390 the sticky head is the 52px title row plus the 28px ① row and
 *    measures at most 80px; ① lives INSIDE that sticky element;
 *  - ① counts a real pending approval the fake Node parks ("mhome-blocked")
 *    as 「⚠N 需要你」 and hides the zero segments;
 *  - ② sits under ① and collapses to one [N/M 主机在线 ›] chip while every
 *    host is online; ★ belongs to home-main-extras and is absent;
 *  - at 1280 ① is inside the PageHeader, ② is a 32px band whose right end is
 *    the 「空间 ▸」 toggle, and the SpacesPanel starts collapsed (§2.5) and
 *    opens/closes from that toggle with the same panel testid.
 *
 * No millisecond thresholds: the only time-like bound is Playwright's own
 * wait for an eventual condition, never an assertion about elapsed time.
 *
 * Self-skipping: needs the in-process fake Node (`e2e-fake-node`). Evidence
 * screenshots are written only when REMUDA_EVIDENCE=1.
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

/** The fake Node enrols shortly after the Hub boots; poll before skipping. */
async function needsFakeNode(page: Page): Promise<boolean> {
  return expect
    .poll(
      async () => {
        try {
          const response = await page.request.get("/v1/hosts");
          if (!response.ok()) return false;
          const body = (await response.json()) as { items?: { label?: string }[] };
          return (body.items ?? []).some((row) => row.label === "e2e-fake-node");
        } catch {
          return false;
        }
      },
      { timeout: 15_000, intervals: [250, 500, 1_000] },
    )
    .toBe(true)
    .then(
      () => true,
      () => false,
    );
}

async function phoneContext(browser: Browser, baseURL: string) {
  const context = await browser.newContext({ ...devices["iPhone 13"], baseURL });
  await login(await context.newPage(), "e2e-hglance-phone");
  return context;
}

const created: string[] = [];

/**
 * Delete this spec's sessions and WAIT until the Hub no longer serves them:
 * the suite shares one fake Node, and a row that survived the next spec's
 * setup changes its group counts (m-home asserts exact dots).
 */
async function cleanup(request: import("@playwright/test").APIRequestContext) {
  for (const id of created.splice(0)) {
    await request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    await expect
      .poll(async () => (await request.get(`/v1/instances/${id}`)).status(), { timeout: 15_000 })
      .toBe(404);
  }
}

/** The shared fake Node seats at most 8 live instances; wait for a free slot. */
async function waitForSeat(page: Page): Promise<void> {
  await expect
    .poll(
      async () => {
        const response = await page.request.get("/v1/hosts");
        if (!response.ok()) return true;
        const body = (await response.json()) as {
          items?: { id?: string; label?: string; maxInstances?: number }[];
        };
        const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node");
        if (!host?.id) return true;
        const instancesResponse = await page.request.get("/v1/instances");
        if (!instancesResponse.ok()) return true;
        const rows = ((await instancesResponse.json()) as {
          items?: { hostId?: string; lifecycle?: string }[];
        }).items ?? [];
        const live = rows.filter(
          (row) => row.hostId === host.id && !["exited", "failed", "closed"].includes(row.lifecycle ?? ""),
        ).length;
        return live < (host.maxInstances ?? 8);
      },
      { timeout: 30_000 },
    )
    .toBe(true);
}

/** A session on the fake Node; the prompt carries its scripted behaviour. */
async function createSession(page: Page, prompt: string): Promise<string> {
  await waitForSeat(page);
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await page.getByTestId("new-session-kind-claude").click();
  const workspacePicker = page.getByTestId("new-session-workspace");
  await expect(workspacePicker.locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await workspacePicker.selectOption("wsp_e2e");
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

/** A session the fake Node parks on a pending approval (m-home's fixture). */
function createBlockedSession(page: Page): Promise<string> {
  return createSession(page, "HGLANCE_BLOCKED mhome-blocked approval gate");
}

/** A session the fake Node keeps in native `working` (rank 3). */
function createWorkingSession(page: Page): Promise<string> {
  return createSession(page, "HGLANCE_WORKING keep running");
}

/**
 * The host list the Hub serving this page reports (the same rule the strip
 * uses: `online === true`, state online, or state enrolled). The suite shares
 * ONE Hub, so earlier specs may leave offline or extra hosts behind; the
 * strip assertions must follow this state instead of assuming our own hosts.
 */
async function hubHosts(page: Page): Promise<{ total: number; online: number }> {
  const response = await page.request.get("/v1/hosts");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as {
    items?: { state?: string; online?: boolean; label?: string }[];
  };
  const items = body.items ?? [];
  const online = items.filter(
    (host) => host.online === true || host.state === "online" || host.state === "enrolled",
  ).length;
  return { total: items.length, online };
}

/** More rows than the reduced viewport holds, so a scroll proof can bite. */
async function seedRows(page: Page, count: number): Promise<void> {
  for (let index = 0; index < count; index += 1) {
    await createWorkingSession(page);
  }
}

/**
 * Park `target` outside the scroller's visible band (above it, else below):
 * the 需要你 tap must be the thing that brings it back. False when the scroller
 * does not overflow or the target is missing — a short list cannot prove it.
 */
async function parkTargetOutOfView(
  page: Page,
  regionSelector: string,
  targetSelector: string,
): Promise<boolean> {
  return page.evaluate(({ region, target }) => {
    const el = document.querySelector(region) as HTMLElement | null;
    let scroller = el?.parentElement ?? null;
    while (scroller && !/(auto|scroll)/.test(getComputedStyle(scroller).overflowY)) {
      scroller = scroller.parentElement;
    }
    const row = document.querySelector(target) as HTMLElement | null;
    if (!scroller || !row || scroller.scrollHeight <= scroller.clientHeight + 4) return false;
    const band = scroller.getBoundingClientRect();
    const rect = row.getBoundingClientRect();
    const rowTop = rect.top - band.top + scroller.scrollTop;
    const max = scroller.scrollHeight - scroller.clientHeight;
    // Just past it, or (when it is last) just above the band.
    let next = rowTop + rect.height + 12;
    if (next > max) next = rowTop - scroller.clientHeight - 12;
    scroller.scrollTop = Math.max(0, Math.min(max, next));
    const after = row.getBoundingClientRect();
    const visibleTop = Math.max(0, band.top);
    const visibleBottom = Math.min(window.innerHeight, band.bottom);
    return after.bottom <= visibleTop || after.top >= visibleBottom;
  }, { region: regionSelector, target: targetSelector });
}

/** Scroll the phone/desktop scroller holding `el` to its end; false if short. */
async function scrollAncestorToEnd(page: Page, selector: string): Promise<boolean> {
  return page.evaluate((sel) => {
    const el = document.querySelector(sel) as HTMLElement | null;
    let scroller = el?.parentElement ?? null;
    while (scroller && !/(auto|scroll)/.test(getComputedStyle(scroller).overflowY)) {
      scroller = scroller.parentElement;
    }
    if (!scroller || scroller.scrollHeight <= scroller.clientHeight + 4) return false;
    scroller.scrollTop = scroller.scrollHeight;
    return true;
  }, selector);
}

/**
 * The element's box lies in the ledger scroller's visible band (the phone's
 * scroller ends above PhoneNav, so a window-only check would call a row below
 * the band "visible").
 */
async function inBand(page: Page, regionSelector: string, selector: string): Promise<boolean> {
  return page.evaluate(({ region, target }) => {
    const el = document.querySelector(region) as HTMLElement | null;
    let scroller = el?.parentElement ?? null;
    while (scroller && !/(auto|scroll)/.test(getComputedStyle(scroller).overflowY)) {
      scroller = scroller.parentElement;
    }
    const row = document.querySelector(target);
    if (!scroller || !row) return false;
    const band = scroller.getBoundingClientRect();
    const rect = row.getBoundingClientRect();
    const top = Math.max(0, band.top);
    const bottom = Math.min(window.innerHeight, band.bottom);
    return rect.bottom > top && rect.top < bottom && rect.right > 0 && rect.left < window.innerWidth;
  }, { region: regionSelector, target: selector });
}

test.describe("phone 390", () => {
  test("the sticky head is title + ① at most 80px, and ① counts the pending approval", async ({
    browser,
  }) => {
    const baseURL = test.info().project.use.baseURL;
    const context = await phoneContext(browser, baseURL!);
    try {
      const page = await context.newPage();
      await page.goto("/m");
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");
      await createBlockedSession(page);

      await page.goto("/m");
      const glance = page.getByTestId("home-glance");
      await expect(glance).toBeVisible();
      // §5.2: the pending approval the fake Node parks is one 需要你, so the
      // first segment is the count — never the empty-queue greeting.
      await expect(page.getByTestId("glance-need-you")).toContainText(/\d+ 需要你/, {
        timeout: 20_000,
      });
      await expect(page.getByTestId("glance-greeting")).toHaveCount(0);
      const needYou = Number(
        ((await page.getByTestId("glance-need-you").innerText()).match(/(\d+)\+? 需要你/) ?? [])[1],
      );
      expect(needYou).toBeGreaterThanOrEqual(1);

      // §2.1: one sticky head — title row (52) over ① (28) — and it is 80px
      // at most. Measured geometry, not a timing threshold.
      const geometry = await page.evaluate(() => {
        const head = document.querySelector('[data-testid="home-head"]') as HTMLElement;
        const row = document.querySelector('[data-testid="home-glance"]') as HTMLElement;
        const title = head.firstElementChild as HTMLElement;
        return {
          headHeight: head.getBoundingClientRect().height,
          headPosition: getComputedStyle(head).position,
          glanceHeight: row.getBoundingClientRect().height,
          glanceInsideHead: head.contains(row),
          titleHeight: title.getBoundingClientRect().height,
          glanceBelowTitle:
            row.getBoundingClientRect().top >= title.getBoundingClientRect().bottom - 0.5,
        };
      });
      expect(geometry.headPosition).toBe("sticky");
      expect(geometry.glanceInsideHead, "① sticks together with the title row").toBe(true);
      expect(geometry.glanceBelowTitle).toBe(true);
      expect(Math.round(geometry.titleHeight), "the title row is the 52px mobile head").toBe(52);
      expect(Math.round(geometry.glanceHeight), "① is the 28px count row").toBe(28);
      expect(geometry.headHeight, "the sticky head is 80px at most").toBeLessThanOrEqual(80);

      // ② is a 28px row under the head, scrolling with the page. The shared
      // Hub decides which shape it paints (all online = one summary chip; any
      // offline host = per-host chips with the offline one first), so read the
      // host list and assert exactly that shape.
      const strip = page.getByTestId("host-strip");
      await expect(strip).toBeVisible();
      const hosts = await hubHosts(page);
      if (hosts.total === 0) {
        await expect(page.getByTestId("host-add")).toContainText("添加主机");
      } else if (hosts.online === hosts.total) {
        await expect(page.getByTestId("host-summary")).toContainText(
          `${hosts.online}/${hosts.total} 主机在线 ›`,
        );
        await expect(page.getByTestId("host-chip")).toHaveCount(0);
      } else {
        await expect(page.getByTestId("host-summary")).toHaveCount(0);
        await expect(page.getByTestId("host-chip")).toHaveCount(hosts.total);
        await expect(page.getByTestId("host-chip").first()).toHaveAttribute("data-offline", "1");
      }
      await expect(strip).not.toContainText("★");

      // Deterministic offline shape: add a host of our own (a never-reachable
      // SSH target) and check the strip's offline branch on controlled state.
      const added = await page.request.post("/v1/hosts/ssh", {
        headers: { Origin: new URL(page.url()).origin },
        data: { target: "rc-never@rc-never.invalid", label: "rc-offline", labels: [] },
      });
      expect(added.ok(), `add ssh host: ${added.status()}`).toBe(true);
      const addedHost = (await added.json()) as { id?: string };
      expect(addedHost.id).toBeTruthy();
      try {
        // Wait for the Hub to serve it, then for the strip's 2 s poll to paint.
        await expect
          .poll(async () => {
            const response = await page.request.get("/v1/hosts");
            const items = ((await response.json()) as { items?: { id?: string }[] }).items ?? [];
            return items.some((host) => host.id === addedHost.id);
          }, { timeout: 20_000 })
          .toBe(true);
        await expect
          .poll(async () => page.getByTestId("host-chip").count(), { timeout: 20_000 })
          .toBeGreaterThan(0);
        await expect(page.getByTestId("host-summary")).toHaveCount(0);
        await expect(page.getByTestId("host-chip").first()).toHaveAttribute("data-offline", "1");
        await expect(
          page.locator(`[data-testid="host-chip"][data-host-id="${addedHost.id}"]`),
        ).toContainText("离线");
      } finally {
        await page.request.delete(`/v1/hosts/${addedHost.id}`).catch(() => undefined);
      }
      const stripGeometry = await page.evaluate(() => {
        const stripEl = document.querySelector('[data-testid="host-strip"]') as HTMLElement;
        const head = document.querySelector('[data-testid="home-head"]') as HTMLElement;
        return {
          height: stripEl.getBoundingClientRect().height,
          position: getComputedStyle(stripEl).position,
          belowHead: stripEl.getBoundingClientRect().top >= head.getBoundingClientRect().bottom - 0.5,
        };
      });
      expect(Math.round(stripGeometry.height), "② is the 28px strip").toBe(28);
      expect(stripGeometry.position, "② scrolls with the page").not.toBe("sticky");
      expect(stripGeometry.belowHead).toBe(true);

      // §2.1 order: ② sits directly under ①; the search/order row follows ②.
      const order = await page.evaluate(() => {
        const head = document.querySelector('[data-testid="home-head"]') as HTMLElement;
        const strip = document.querySelector('[data-testid="host-strip"]') as HTMLElement;
        const search = document.querySelector('[data-testid="home-search"]') as HTMLElement;
        return {
          gap: strip.getBoundingClientRect().top - head.getBoundingClientRect().bottom,
          searchAfterStrip: search.getBoundingClientRect().top >= strip.getBoundingClientRect().bottom - 0.5,
        };
      });
      expect(Math.abs(order.gap), "② starts at the head's bottom edge").toBeLessThanOrEqual(1);
      expect(order.searchAfterStrip, "the search row follows ②").toBe(true);

      // The empty state belongs to the ledger slot: below the toolbar, not
      // between ① and ②.
      await page.getByTestId("home-search").fill("zzz-glance-no-match");
      const empty = page.getByTestId("home-empty");
      await expect(empty).toBeVisible();
      const emptyTop = await empty.evaluate((el) => el.getBoundingClientRect().top);
      const searchBottom = await page.getByTestId("home-search").evaluate((el) => el.getBoundingClientRect().bottom);
      expect(emptyTop, "the empty state follows the search row").toBeGreaterThanOrEqual(searchBottom - 0.5);
      await page.getByTestId("home-search").fill("");

      await setMode(page, "light");
      await shot(page, "home-glance-390-light.png");
      await setMode(page, "dark");
      await shot(page, "home-glance-390-dark.png");
    } finally {
      // The suite shares one fake Node and its placement slots: release ours.
      await cleanup(context.request);
      await context.close();
    }
  });

  test("the ① taps narrow the ledger by rank and scroll to the waiting row", async ({ browser }) => {
    const baseURL = test.info().project.use.baseURL;
    const context = await phoneContext(browser, baseURL!);
    try {
      const page = await context.newPage();
      await page.goto("/m");
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");
      await createBlockedSession(page);
      await createWorkingSession(page);
      // Enough rows that the reduced viewport can park the waiting row fully
      // outside the visible band: a tap that "works" only because everything
      // already fits proves nothing. The fake host seats 8 live instances, so
      // six rows (blocked + working + four) leave room under that cap.
      await seedRows(page, 4);
      await page.setViewportSize({ width: 390, height: 340 });
      await page.goto("/m");

      const working = page.getByTestId("glance-working");
      await expect(working).toContainText(/工作中/, { timeout: 20_000 });
      const count = Number(((await working.innerText()).match(/(\d+) 工作中/) ?? [])[1]);
      expect(count).toBeGreaterThanOrEqual(1);
      await working.click();
      await expect(working).toHaveAttribute("aria-pressed", "true");
      // The tapped list equals the count: every row left is rank 3, and there
      // are exactly as many as the segment said.
      await expect
        .poll(async () =>
          page.getByTestId("home-row").evaluateAll((rows) =>
            rows.every((row) => (row as HTMLElement).dataset.rank === "working"),
          ),
        )
        .toBe(true);
      expect(await page.getByTestId("home-row").count()).toBe(count);
      await working.click();
      await expect(working).toHaveAttribute("aria-pressed", "false");
      await expect(page.locator('[data-testid="home-row"][data-rank="blocked"]')).toHaveCount(1);

      // 需要你 scrolls the ledger to the waiting row: the scroller overflows,
      // the row is parked OUT of view, and only the tap brings it back.
      expect(
        await parkTargetOutOfView(page, '[data-testid="home-head"]', '[data-testid="home-row"][data-blocked="1"]'),
      ).toBe(true);
      expect(await inBand(page, '[data-testid="home-head"]', '[data-testid="home-row"][data-blocked="1"]')).toBe(false);
      await page.getByTestId("glance-need-you").click();
      await expect
        .poll(() => inBand(page, '[data-testid="home-head"]', '[data-testid="home-row"][data-blocked="1"]'))
        .toBe(true);
      await shot(page, "home-glance-390-taps.png");
    } finally {
      await cleanup(context.request);
      await context.close();
    }
  });
});

test.describe("desktop 1280", () => {
  test("① sits in the PageHeader, ② is the 32px band, and 空间 ▸ opens the collapsed index", async ({
    browser,
  }) => {
    const baseURL = test.info().project.use.baseURL;
    const context = await browser.newContext({
      baseURL: baseURL!,
      viewport: { width: 1280, height: 900 },
    });
    try {
      const page = await context.newPage();
      await login(page, "e2e-hglance-desktop");
      await page.goto("/sessions");
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");
      await expect(page.getByTestId("session-list")).toBeVisible({ timeout: 20_000 });

      // §2.2: the count line is the PageHeader's right-hand action.
      await expect(page.locator('[data-testid="page-header"] [data-testid="home-glance"]')).toHaveCount(1);
      // §2.2/§2.5: a 32px band under the header, with the toggle at its end.
      const band = await page.evaluate(() => {
        const strip = document.querySelector('[data-testid="host-strip"]') as HTMLElement;
        const header = document.querySelector('[data-testid="page-header"]') as HTMLElement;
        const toggle = document.querySelector('[data-testid="spaces-index-open"]') as HTMLElement;
        return {
          height: strip.getBoundingClientRect().height,
          belowHeader: strip.getBoundingClientRect().top >= header.getBoundingClientRect().bottom - 0.5,
          toggleInside: strip.contains(toggle),
          toggleRightGap:
            strip.getBoundingClientRect().right - toggle.getBoundingClientRect().right,
        };
      });
      expect(Math.round(band.height), "② band is 32px").toBe(32);
      expect(band.belowHeader).toBe(true);
      expect(band.toggleInside, "「空间 ▸」 sits in the band").toBe(true);
      expect(band.toggleRightGap).toBeLessThanOrEqual(48);

      // §2.5: collapsed by default — no index column, no panel node.
      const toggle = page.getByTestId("spaces-index-open");
      await expect(toggle).toHaveAttribute("aria-expanded", "false");
      await expect(toggle).toContainText("空间");
      await expect(page.getByTestId("spaces-panel")).toBeHidden();
      await toggle.click();
      await expect(page.getByTestId("spaces-panel")).toBeVisible();
      await expect(toggle).toHaveAttribute("aria-expanded", "true");
      await shot(page, "home-glance-1280-index.png");
      // The same toggle closes it, and the remembered state is per device.
      await toggle.click();
      await expect(page.getByTestId("spaces-panel")).toBeHidden();
      await page.reload();
      await expect(page.getByTestId("session-list")).toBeVisible({ timeout: 20_000 });
      await expect(page.getByTestId("spaces-panel")).toBeHidden();

      await setMode(page, "light");
      await shot(page, "home-glance-1280-light.png");
      await setMode(page, "dark");
      await shot(page, "home-glance-1280-dark.png");
    } finally {
      await context.close();
    }
  });

  test("the ① taps filter the desktop ledger by rank and scroll to the waiting card", async ({
    browser,
  }) => {
    const baseURL = test.info().project.use.baseURL;
    const context = await browser.newContext({
      baseURL: baseURL!,
      viewport: { width: 1280, height: 900 },
    });
    try {
      const page = await context.newPage();
      await login(page, "e2e-hglance-desktop-taps");
      await page.goto("/sessions");
      if (!(await needsFakeNode(page))) test.skip(true, "needs the in-process fake Node");
      await expect(page.getByTestId("session-list")).toBeVisible({ timeout: 20_000 });
      await createBlockedSession(page);
      await createWorkingSession(page);
      await seedRows(page, 4);
      // The count row is owner-global while /sessions is pinned to the active
      // Space — and the default Space has no sessions, so its list renders the
      // empty state without the scope switch. Open the list in global scope so
      // the filtered rows are comparable with the count.
      await page.setViewportSize({ width: 1280, height: 420 });
      await page.goto("/sessions?scope=all");
      await expect(page.getByTestId("session-list")).toBeVisible({ timeout: 20_000 });
      await expect(page.getByTestId("session-scope")).toHaveAttribute("data-scope", "all");

      const working = page.getByTestId("glance-working");
      await expect(working).toContainText(/工作中/, { timeout: 20_000 });
      const count = Number(((await working.innerText()).match(/(\d+) 工作中/) ?? [])[1]);
      expect(count).toBeGreaterThanOrEqual(1);
      await working.click();
      await expect(working).toHaveAttribute("aria-pressed", "true");
      await expect(page).toHaveURL(/rank=working/);
      // The filter is a removable chip, and the list equals the count.
      await expect(page.getByTestId("session-chip").filter({ hasText: "工作中" })).toBeVisible();
      await expect
        .poll(async () =>
          page.getByTestId("board-card").evaluateAll((cards) =>
            cards.every((card) => (card as HTMLElement).dataset.rank === "working"),
          ),
        )
        .toBe(true);
      expect(await page.getByTestId("board-card").count()).toBe(count);
      await working.click();
      await expect(working).toHaveAttribute("aria-pressed", "false");
      await expect(page).not.toHaveURL(/rank=/);
      await expect(page.locator('[data-testid="board-card"][data-rank="blocked"]')).toHaveCount(1);

      // 需要你 scrolls the desktop list to the blocked card: the list overflows,
      // the card is parked OUT of view, and only the tap brings it back.
      expect(
        await parkTargetOutOfView(page, '[data-testid="session-list"]', '[data-testid="board-card"][data-rank="blocked"]'),
      ).toBe(true);
      expect(await inBand(page, '[data-testid="session-list"]', '[data-testid="board-card"][data-rank="blocked"]')).toBe(false);
      await page.getByTestId("glance-need-you").click();
      await expect
        .poll(() => inBand(page, '[data-testid="session-list"]', '[data-testid="board-card"][data-rank="blocked"]'))
        .toBe(true);
      await shot(page, "home-glance-1280-taps.png");
    } finally {
      await cleanup(context.request);
      await context.close();
    }
  });
});
