import { expect, test, type Page } from "@playwright/test";
import { setMode } from "./appearanceHelper";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/tabs-1");

function space(page: Page, name: string) {
  return page.getByTestId("spaces-panel").getByTestId("space-select").filter({ hasText: name });
}

/** UO-2a: the Space index lives on /sessions, reached in-app from the sidebar. */
async function toList(page: Page) {
  await page.getByRole("navigation", { name: "主导航" }).getByRole("link", { name: "会话" }).click();
  await expect(page).toHaveURL(/\/sessions$/);
}

/**
 * Publish only generic demo inventory. Mutate the in-browser fixture before
 * rendering evidence; production code and screenshot pixels stay untouched.
 * The evaluate is idempotent (already-generic labels are skipped), so a Vite
 * dev-server optimizer reload that destroys the execution context is simply
 * retried on the fresh context instead of killing the run.
 */
async function applyDemoInventory(page: Page): Promise<string[]> {
  for (let attempt = 0; ; attempt += 1) {
    try {
      return await page.evaluate(async () => {
        const { mockDb } = await import("/src/lib/mock.ts");
        const { hubStore } = await import("/src/lib/store.ts");
        const labels: string[] = [];
        mockDb.hosts.forEach((host: { label: string; hostname?: string }, index: number) => {
          if (!host.label.startsWith("demo-node-")) labels.push(host.label);
          host.label = `demo-node-${index + 1}`;
          host.hostname = host.label;
        });
        mockDb.workspaces.forEach((workspace: { rootPath: string; canonicalRoot: unknown }) => {
          workspace.rootPath = `/workspace/${workspace.rootPath.split("/").pop()}`;
          workspace.canonicalRoot = { state: "known", value: workspace.rootPath };
        });
        // Host labels reach the sidebar through hub.hosts, so reload those too.
        await hubStore.refreshHosts();
        await hubStore.refresh();
        return labels;
      });
    } catch (error) {
      if (attempt >= 2 || !/garbage collected|Execution context was destroyed/.test((error as Error).message)) throw error;
      // The Vite dev optimizer reloads the page once on first dynamic import;
      // wait for the fresh document instead of networkidle (HMR keeps a
      // websocket open, which can make that state slow to settle).
      await page.waitForLoadState("domcontentloaded");
      await page.waitForTimeout(500);
    }
  }
}

/** Committed screenshots are only refreshed on request; a normal run writes
 *  nothing to the tree (the merge gate rejects a dirty worktree). */
const captureEvidence = process.env.REMUDA_EVIDENCE === "1";

async function screenshot(page: Page, name: string, theme: "night" | "ledger") {
  if (!captureEvidence) return;
  const replacedLabels = await applyDemoInventory(page);
  await setMode(page, theme);
  await page.evaluate(() => document.fonts.ready);
  const rendered = await page.locator("body").innerText();
  expect(rendered).not.toMatch(/\/Users\//);
  for (const label of replacedLabels) expect(rendered).not.toContain(label);
  await mkdir(evidence, { recursive: true });
  const png = await page.screenshot({ path: path.join(evidence, `${name}.png`), animations: "disabled", scale: "css" });
  expect(png.byteLength, `${name} must stay below 300 KB`).toBeLessThanOrEqual(300_000);
}

test.describe("desktop", () => {
  // The mobile-webkit project emulates a touch device; this test asserts the
  // hover-driven desktop affordance at 1440px, so disable touch for it.
  test.use({ hasTouch: false });

  test("status and close are distinct, dismissal keeps sessions running, and exited sessions group", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/sessions");
    const panel = page.getByTestId("spaces-panel");
    const strip = page.getByTestId("space-tabs");
    // UO-2a: list routes carry the Space index and no tab strip; the strip is
    // the session page's own chrome.
    await space(page, "sfe-root").click();
    await expect(strip).toHaveCount(0);
    await page.getByTestId("session-row").first().click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(panel).toHaveCount(0);
    await strip.getByRole("tab").first().click();

    // One × per tab, and it is the close control — the status never renders one.
    const active = strip.locator('[data-active="true"]');
    expect(await active.getByTestId("tab-close").count()).toBe(1);
    expect(await active.getByTestId("session-tab").innerText()).not.toContain("×");
    await expect(active.getByTestId("tab-close")).toHaveAccessibleName(/^关闭标签 /);
    // Status is a labelled shape, not a glyph that can be mistaken for close.
    for (const label of ["待处理", "运行中", "空闲", "已退出"]) {
      const dots = page.getByRole("img", { name: label });
      if (await dots.count()) await expect(dots.first()).toHaveAttribute("data-status", /.+/);
    }

    // The active tab carries the brand marker, and so does its Space in the
    // /sessions index (which has no open session to mark).
    await expect(active).toHaveAttribute("data-active", "true");
    await toList(page);
    await expect(space(page, "sfe-root")).toHaveAttribute("aria-pressed", "true");
    await expect(panel.getByTestId("space-session").and(page.locator('[data-active="true"]'))).toHaveCount(0);
    await page.goBack();
    await expect(page).toHaveURL(/\/s\//);
    await screenshot(page, "desktop-dark", "night");
    await screenshot(page, "desktop-light", "ledger");

    // Keyboard focus must be visible. Programmatic focus does not satisfy
    // :focus-visible, so reach the control the way a keyboard user does.
    await active.getByTestId("session-tab").focus();
    await page.keyboard.press("Tab");
    const closeControl = active.getByTestId("tab-close");
    await expect(closeControl).toBeFocused();
    const ring = await closeControl.evaluate((element) => {
      const style = getComputedStyle(element);
      return { matches: element.matches(":focus-visible"), width: style.outlineWidth, style: style.outlineStyle };
    });
    expect(ring.matches, "the close control must show a :focus-visible ring").toBe(true);
    expect(ring.style).not.toBe("none");
    expect(parseFloat(ring.width)).toBeGreaterThan(0);
    await screenshot(page, "desktop-close-focus-dark", "night");

    // Closing a running tab asks first and never stops the session silently.
    const runningTab = strip.getByRole("tab").first();
    const runningRoute = new URL(page.url()).pathname;
    await runningTab.click();
    await strip.locator('[data-active="true"]').getByTestId("tab-close").click();
    const sheet = page.getByTestId("tab-close-sheet");
    await expect(sheet).toBeVisible();
    await expect(sheet).toContainText("仅关闭标签不会停止它");
    await screenshot(page, "desktop-close-sheet-dark", "night");
    await screenshot(page, "desktop-close-sheet-light", "ledger");
    await page.getByTestId("tab-close-keep").click();
    await expect(sheet).toHaveCount(0);
    await expect(strip.getByRole("tab").filter({ hasText: await runningTab.innerText().catch(() => "—") })).toHaveCount(0);

    // The session is still listed in the /sessions index and re-opens its tab on click.
    await toList(page);
    const dismissed = panel.getByTestId("space-session").and(page.locator(`[href="${runningRoute}"]`));
    await expect(dismissed).toHaveCount(1);
    await dismissed.click();
    await expect(page).toHaveURL(new RegExp(`${runningRoute}$`));
    await expect(strip.getByRole("tab", { selected: true })).toHaveCount(1);

    // Exited sessions live in their own collapsed group with 恢复 and 删除.
    await toList(page);
    const group = panel.getByTestId("exited-toggle").first();
    await expect(group).toContainText("已退出");
    await expect(group).toHaveAttribute("aria-expanded", "false");
    await group.click();
    await expect(group).toHaveAttribute("aria-expanded", "true");
    const exited = panel.getByTestId("exited-session").first();
    await expect(exited.getByTestId("exited-resume")).toBeVisible();
    await screenshot(page, "desktop-exited-group-dark", "night");
    await screenshot(page, "desktop-exited-group-light", "ledger");
    await exited.getByTestId("exited-delete").click();
    await expect(page.getByTestId("delete-session-sheet")).toContainText("删除会话及其记录？");
    await screenshot(page, "desktop-delete-sheet-dark", "night");
    await page.getByTestId("delete-session-sheet-cancel").click();
    await expect(page.getByTestId("delete-session-sheet")).toHaveCount(0);
    expect(errors).toEqual([]);
  });

  test("overflow cues stay pinned to the strip edges and the active tab survives narrowing", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/sessions");
    const strip = page.getByTestId("space-tabs");
    // UO-2a: the strip only exists on /s/*. Enter via the list so the Vite dev
    // server has served the whole route graph before the strip is queried.
    await space(page, "sfe-root").click();
    await page.getByTestId("session-row").first().click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(strip).toBeVisible();

    // First exercise cue pinning with a MIDDLE tab active: it sits inside the
    // scrollport at every pan position, so the resize/visibility observer
    // never fights the manual scroll. The trailing-tab resize proof below
    // activates the last tab separately.
    const tabs = strip.getByRole("tab");
    const tabCount = await tabs.count();
    expect(tabCount).toBeGreaterThanOrEqual(3);
    const lastIndex = tabCount - 1;
    const middleIndex = Math.floor(tabCount / 2);
    await tabs.nth(middleIndex).click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(strip.locator('[data-testid="session-tab"]').nth(middleIndex)).toHaveAttribute("aria-selected", "true");

    const geometry = async () => strip.evaluate((element) => {
      const scroller = element;
      const rect = scroller.getBoundingClientRect();
      const cue = (side: "left" | "right") =>
        scroller.parentElement?.querySelector<HTMLElement>(`[data-edge="${side}"]`);
      const active = scroller.querySelector<HTMLElement>('[data-active="true"]');
      const close = active?.querySelector<HTMLElement>('[data-testid="tab-close"]') ?? null;
      const cueRect = (side: "left" | "right") => cue(side)?.getBoundingClientRect() ?? null;
      const activeRect = active?.getBoundingClientRect() ?? null;
      const closeRect = close?.getBoundingClientRect() ?? null;
      return {
        overflow: scroller.scrollWidth > scroller.clientWidth + 1,
        left: rect.left, right: rect.right,
        leftCue: cueRect("left") ? { left: cueRect("left")!.left, right: cueRect("left")!.right } : null,
        rightCue: cueRect("right") ? { left: cueRect("right")!.left, right: cueRect("right")!.right } : null,
        active: activeRect ? { left: activeRect.left, right: activeRect.right } : null,
        close: closeRect ? { left: closeRect.left, right: closeRect.right } : null,
      };
    });

    const cueStates = () => geometry().then((state) => ({
      left: Boolean(state.leftCue),
      right: Boolean(state.rightCue),
    }));

    let g = await geometry();
    expect(g.overflow, "the sfe-root strip overflows at 1440px").toBe(true);

    // Pan the content fully to the trailing edge; the start cue must stay
    // pinned to the visible left edge, not ride along with the scrolled
    // content, and no end cue remains at the very end.
    await strip.evaluate((element) => { element.scrollLeft = element.scrollWidth; element.dispatchEvent(new Event("scroll")); });
    await expect.poll(cueStates).toEqual({ left: true, right: false });
    g = await geometry();
    expect(Math.abs(g.leftCue!.left - g.left)).toBeLessThanOrEqual(1);

    await strip.evaluate((element) => { element.scrollLeft = (element.scrollWidth - element.clientWidth) / 2; element.dispatchEvent(new Event("scroll")); });
    await expect.poll(cueStates).toEqual({ left: true, right: true });
    g = await geometry();
    expect(Math.abs(g.leftCue!.left - g.left)).toBeLessThanOrEqual(1);
    expect(Math.abs(g.rightCue!.right - g.right)).toBeLessThanOrEqual(1);

    // Back to the start: only the end cue, pinned to the right edge.
    await strip.evaluate((element) => { element.scrollLeft = 0; element.dispatchEvent(new Event("scroll")); });
    await expect.poll(cueStates).toEqual({ left: false, right: true });
    g = await geometry();
    expect(Math.abs(g.rightCue!.right - g.right)).toBeLessThanOrEqual(1);

    // Now the trailing-tab resize proof: activate the LAST tab, then narrow.
    await tabs.nth(lastIndex).click();
    await expect(strip.locator('[data-testid="session-tab"]').nth(lastIndex)).toHaveAttribute("aria-selected", "true");
    // Narrow within the DESKTOP breakpoint (≥768px; below it the compact
    // shell correctly renders no strip at all). Each resize already runs the
    // correction, so to prove it is what reveals the trailing tab: pin
    // scrollLeft to 0 — a scroll event alone never re-reveals the active tab.
    await page.setViewportSize({ width: 820, height: 900 });
    await page.waitForTimeout(60);
    g = await geometry();
    expect(g.overflow, "the narrowed strip overflows").toBe(true);
    await strip.evaluate((element) => { element.scrollLeft = 0; element.dispatchEvent(new Event("scroll")); });
    g = await geometry();
    expect(g.close, "the active close control is measurable").toBeTruthy();
    expect(g.close!.right, "with no correction the last tab's × is outside the scrollport").toBeGreaterThan(g.right + 1);
    // A further width change fires the ResizeObserver, which must scroll the
    // active tab — × included — fully back into the scrollport.
    await page.setViewportSize({ width: 780, height: 900 });
    await expect.poll(async () => {
      const state = await geometry();
      if (!state.close) return { dbg: "no close", ...state } as never;
      // Sub-pixel tolerance: the scrollIntoView landing position can round to
      // a fraction of a pixel.
      return state.close.right <= state.right + 1 && state.close.left >= state.left - 1
        ? { inside: true } : { outside: state.close.right - state.right, overflow: state.overflow };
    }, undefined, { message: "last tab × inside after resize" }).toEqual({ inside: true });
    g = await geometry();
    const scrolled = await strip.evaluate((element) => element.scrollLeft);
    expect(scrolled, "the strip panned right to reveal the last tab").toBeGreaterThan(0);
    // Sub-pixel tolerance: the corrected scroll position can land a fraction
    // of a pixel off the integer-rounded edge.
    expect(g.close!.right, "the last tab's × returns inside the scrollport").toBeLessThanOrEqual(g.right + 1);
    expect(g.close!.left, "…and its left edge too").toBeGreaterThanOrEqual(g.left - 1);
    expect(g.active!.left, "active tab clear of the 24px left cue").toBeGreaterThanOrEqual(g.left + 23);
    expect(g.active!.right, "active tab incl. its × clear of the 24px right cue").toBeLessThanOrEqual(g.right - 23);
  });
  test("Space header select and disclosure stretch to the full header hit area", async ({ page, browser }) => {
    const checkHeader = async (target: Page, coarse: boolean, label: string) => {
      const panel = target.getByTestId("spaces-panel");
      const select = panel.getByTestId("space-select").first();
      await expect(select).toBeVisible();
      expect(await target.evaluate(() => matchMedia("(pointer: coarse)").matches)).toBe(coarse);

      // CSS-module class names are hashed, so measure through the rendered
      // structure: the select's parent is the header row, and the disclosure
      // toggle (its labelled 折叠/展开 button) is a sibling.
      const measured = await select.evaluate((element) => {
        const head = element.parentElement as HTMLElement;
        // Header children: disclosure button, then the select button.
        const disclosureButton = head.querySelector<HTMLElement>("button:first-of-type");
        const h = head.getBoundingClientRect();
        const s = element.getBoundingClientRect();
        const d = disclosureButton?.getBoundingClientRect();
        return {
          head: { h: h.height },
          select: { x: s.x, y: s.y, h: s.height },
          disclosure: d ? { h: d.height } : null,
          isDisclosureSibling: disclosureButton?.parentElement === head,
        };
      });
      expect(measured.disclosure && measured.isDisclosureSibling).toBeTruthy();
      const min = coarse ? 43.5 : 31.5;
      expect(measured.select.h, `${label}: select fills the header height`).toBeGreaterThanOrEqual(min);
      expect(measured.disclosure.h, `${label}: disclosure fills the header height`).toBeGreaterThanOrEqual(min);
      expect(measured.select.h, `${label}: select does not exceed the header`).toBeLessThanOrEqual(measured.head.h + 0.5);

      // The very top and bottom interior edges of the header both resolve to
      // the select button — the whole band, not just text-height, is active.
      const hits = await select.evaluate((element) => {
        const rect = element.getBoundingClientRect();
        const at = (y: number) =>
          document.elementFromPoint(rect.x + 30, y)?.closest('[data-testid="space-select"]') !== null;
        return { top: at(rect.y + 1), bottom: at(rect.y + rect.height - 1) };
      });
      expect(hits, `${label}: header top/bottom edges activate the select`).toEqual({ top: true, bottom: true });
    };

    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/sessions");
    await checkHeader(page, false, "desktop");

    // A touch-enabled 1024×1366 tablet still renders the desktop Spaces
    // index, but matches (pointer: coarse) → the 44px header band.
    const tablet = await browser.newContext({
      viewport: { width: 1024, height: 1366 },
      hasTouch: true,
      isMobile: false,
      deviceScaleFactor: 1,
    });
    const tabletPage = await tablet.newPage();
    try {
      await tabletPage.goto("/sessions");
      await checkHeader(tabletPage, true, "coarse tablet");
    } finally {
      await tablet.close();
    }
  });
});

test.describe("phone", () => {
  // Touch emulation is what makes (pointer: coarse) / (hover: none) match, so
  // the 44px hit-area rules exercise the layout the way a real phone sees it.
  test.use({ hasTouch: true, isMobile: true });

  test("compact renders no strip and no chips; the header trigger opens the SpacesDrawer", async ({
    page,
  }) => {
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.setViewportSize({ width: 400, height: 860 });

    // UO-3/D-053: the list route /m owns no chips row and no tab strip.
    await page.goto("/m");
    expect(
      await page.evaluate(() => matchMedia("(pointer: coarse)").matches),
      "the phone run must match the coarse-pointer rules",
    ).toBe(true);
    await expect(page.getByTestId("space-tabs")).toHaveCount(0);
    await expect(page.getByTestId("spaces-chips")).toHaveCount(0);
    await expect(page.getByTestId("space-chip")).toHaveCount(0);
    const trigger = page.getByTestId("spaces-drawer-open");
    await expect(trigger).toBeVisible();

    // The trigger itself owns the 44px coarse hit area.
    const triggerBox = await trigger.boundingBox();
    expect(triggerBox).toBeTruthy();
    expect(triggerBox!.height).toBeGreaterThanOrEqual(43.5);
    expect(triggerBox!.width).toBeGreaterThanOrEqual(43.5);

    await screenshot(page, "phone-dark", "night");
    await screenshot(page, "phone-light", "ledger");

    // The same SpacesPanel the desktop index shows lives in the drawer, with
    // QuickFind, and switching Space closes the drawer.
    await trigger.click();
    const drawer = page.getByTestId("spaces-drawer");
    await expect(drawer).toBeVisible();
    await expect(page.getByTestId("spaces-panel")).toBeVisible();
    await screenshot(page, "phone-drawer-dark", "night");
    for (const row of await page
      .getByTestId("spaces-panel")
      .getByTestId("space-select")
      .all()) {
      const box = await row.boundingBox();
      expect(box).toBeTruthy();
      expect(box!.height, "drawer Space row reaches the 44px touch band").toBeGreaterThanOrEqual(43.5);
    }
    await space(page, "x-codexdrv").click();
    await expect(drawer).toHaveCount(0);

    // The compact session route (/s/:id) keeps the single header chip that
    // opens this same drawer; it still renders no strip.
    await expect(page.getByTestId("space-tabs")).toHaveCount(0);
    await expect(page.getByTestId("space-chip")).toHaveCount(0);
    const headerChips = page.locator("header").getByTestId("spaces-chips");
    await expect(headerChips).toHaveCount(1);
    await expect(headerChips.getByTestId("spaces-drawer-open")).toBeVisible();

    const dimensions = await page.evaluate(() => ({
      width: window.innerWidth,
      content: document.documentElement.scrollWidth,
    }));
    expect(dimensions.content).toBeLessThanOrEqual(dimensions.width);
    expect(errors).toEqual([]);
  });

  test("adjacent short Space names are both reachable inside the drawer", async ({ page }) => {
    await page.setViewportSize({ width: 400, height: 860 });
    // Load the app first (the mock is an in-browser adapter), then add TWO
    // adjacent one-letter Spaces to the fixture and refresh the store. The
    // evaluate is retry-safe for the Vite optimizer reload.
    await page.goto("/m");
    await page.getByTestId("spaces-drawer-open").waitFor();
    for (let attempt = 0; ; attempt += 1) {
      try {
        await page.evaluate(async () => {
          const { mockDb } = await import("/src/lib/mock.ts");
          for (const letter of ["y", "z"] as const) {
            if (
              !mockDb.workspaces.some((workspace: { rootPath?: string }) => workspace.rootPath === `/workspace/${letter}`)
            ) {
              mockDb.workspaces.push({
                ...mockDb.workspaces[0],
                id: `wsp_zzshort_${letter}`,
                label: letter,
                rootPath: `/workspace/${letter}`,
                canonicalRoot: { state: "known", value: `/workspace/${letter}` },
              });
            }
          }
          // refreshHosts (not refresh) rebuilds the workspace list.
          await (await import("/src/lib/store.ts")).hubStore.refreshHosts();
        });
        break;
      } catch (error) {
        if (
          attempt >= 2 ||
          !/garbage collected|Execution context was destroyed|Failed to resolve module/.test((error as Error).message)
        )
          throw error;
        await page.waitForLoadState("domcontentloaded");
        await page.waitForTimeout(500);
      }
    }
    expect(await page.evaluate(() => matchMedia("(pointer: coarse)").matches)).toBe(true);

    // The two trailing Spaces are stacked full-width rows in the drawer; each
    // owns the 44px band and taps resolve to the row itself, never its
    // neighbour. The row label is a one-letter space name followed by the
    // live/blocked count, so anchor on the letter-then-digit text.
    await page.getByTestId("spaces-drawer-open").click();
    await expect(page.getByTestId("spaces-drawer")).toBeVisible();
    const rows = page.getByTestId("spaces-panel").getByTestId("space-select");
    const row = (letter: string) => rows.filter({ hasText: new RegExp(`^${letter}\\d`) }).first();
    const yRow = row("y");
    const zRow = row("z");
    await expect(yRow).toBeVisible();
    await expect(zRow).toBeVisible();
    const yBox = await yRow.boundingBox();
    const zBox = await zRow.boundingBox();
    expect(yBox && zBox).toBeTruthy();
    // Stacked vertically: no shared band, and both reach the touch height.
    expect(Math.min(yBox!.height, zBox!.height)).toBeGreaterThanOrEqual(43.5);
    expect(zBox!.y).toBeGreaterThanOrEqual(yBox!.y + yBox!.height - 0.5);

    // The rows are appended at the END of the drawer list and may sit below
    // its scroll fold — use locator clicks (Playwright scrolls them in and
    // re-checks actionability) rather than raw coordinates into a possibly
    // off-screen point. Empty Spaces navigate to the index (bounced back to
    // /m) and remount the home header, so reopen the drawer per tap and
    // query the freshly rendered pressed state.
    await zRow.scrollIntoViewIfNeeded();
    await zRow.click();
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("spaces-drawer")).toHaveCount(0);
    await page.getByTestId("spaces-drawer-open").click();
    await expect(row("z")).toHaveAttribute("aria-pressed", "true");
    await row("y").scrollIntoViewIfNeeded();
    await row("y").click();
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("spaces-drawer")).toHaveCount(0);
    await page.getByTestId("spaces-drawer-open").click();
    await expect(row("y")).toHaveAttribute("aria-pressed", "true");
  });
});
