import { expect, test, devices, webkit, type Browser, type BrowserContext, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-composerpop: the context chip's hover-close timer must not kill the
 * effort/permission menus (RC2), the chip opens an empty-state card with no
 * rollup (RC3), the stacked mobile usage sheet closes with its parent (RC4),
 * and menus stay hit-testable above the notification stack (item 8).
 *
 * Drives the in-process fake node. Fake-node sessions carry no usage rollup
 * (the current live shape), which is exactly the RC3 empty case. The RC4
 * rollup cases inject one through the window.__usageLab seam.
 */
test.describe.configure({ mode: "serial" });

type NotifyLab = {
  notify: (input: { severity?: "info" | "blocking"; subject?: string; stage?: string; reason?: string }) => string;
  dismissAllBlocking: () => void;
};

/** Minimal Hub-computed rollup shape (contextUsage.ts UsageRollup). */
const INJECTED_ROLLUP = {
  contextUsedTokens: 12_000,
  contextWindowTokens: 200_000,
  contextPct: 6,
  sessionInputTokens: 12_000,
  sessionOutputTokens: 340,
  cacheReadTokens: 9_000,
  cacheCreationTokens: 120,
  turns: 1,
  tpmIn60s: 0,
  tpmOut60s: 0,
  tpmIn5m: 0,
  tpmOut5m: 0,
  lastTurnAt: Date.now(),
};

async function injectRollup(page: Page) {
  await page.waitForFunction(
    () => (window as unknown as { __usageLab?: unknown }).__usageLab != null,
    undefined,
    { timeout: 10_000 },
  );
  await page.evaluate((rollup) => {
    const lab = (window as unknown as { __usageLab?: { setRollup: (r: unknown) => void } }).__usageLab;
    if (!lab) throw new Error("__usageLab seam missing");
    lab.setRollup(rollup);
  }, INJECTED_ROLLUP);
  // No chip assertion here: on phones the chip mounts only when the options
  // sheet opens.
}

/** The options-sheet trigger keeps a stable attr whether or not effort caps
 *  collapse it into the model-effort-chip testid. */
const optionsTrigger = (page: Page) => page.locator("[data-options-trigger='1']");

/** The sheet scrim (the panel inside it is role=dialog). */
const sheetScrim = (page: Page) => page.locator("[data-variant='sheet'][role='presentation']");

const created: string[] = [];

test.afterEach(async ({ page }) => {
  // Guaranteed cleanup even when an assertion throws (serial suite).
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => {});
  }
});

async function createSession(page: Page, prompt: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const host = await hostPicker.locator("option").filter({ hasText: "e2e-fake-node" }).getAttribute("value");
  expect(host).toBeTruthy();
  hostPicker.selectOption(host!);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

test.beforeEach(async ({ page }) => {
  await login(page);
});

test.describe("context chip does not close other menus (RC2)", () => {
  test("crossing the context chip keeps the permission menu open and topmost", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await createSession(page, "composer popover permission");
    await page.clock.install();
    await page.getByTestId("permission-chip").click();
    const permMenu = page.getByTestId("permission-menu");
    await expect(permMenu).toBeVisible();

    // Move across the context chip and dwell, then on to the lower-left row.
    const chipBox = (await page.getByTestId("context-chip").boundingBox())!;
    await page.mouse.move(chipBox.x + 4, chipBox.y + chipBox.height / 2, { steps: 8 });
    const row = permMenu.locator("[data-testid^='permission-option-']:not([disabled])").last();
    const rowBox = (await row.boundingBox())!;
    await page.mouse.move(rowBox.x + 8, rowBox.y + rowBox.height - 6, { steps: 12 });
    await page.clock.runFor("00:00:30");

    await expect(permMenu).toBeVisible();
    // The bottom row is hit-testable on the menu, not dismissed.
    await row.click({ trial: true });
    await expect(permMenu).toBeVisible();
  });

  test("resting on the context chip then clicking effort keeps effort open", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await createSession(page, "composer popover effort");
    await page.clock.install();
    // Leaving the chip (moving straight to effort) schedules its hover-close;
    // on the buggy build toggle() did not cancel that timer, so opening effort
    // here was undone ~140 ms later. No clock advance before the click.
    await page.getByTestId("context-chip").hover();
    await page.getByTestId("model-effort-chip").click();
    const panel = page.getByTestId("effort-slider-panel");
    await expect(panel).toBeVisible();
    await page.clock.runFor("00:00:30");
    await expect(panel).toBeVisible();
  });
});

test.describe("context chip empty state (RC3)", () => {
  test("desktop click opens an explanatory empty-state card with no rollup", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await createSession(page, "composer popover empty desktop");
    await expect(page.getByTestId("context-chip")).toContainText("—");
    await page.getByTestId("context-chip").click();
    await expect(page.getByTestId("context-usage-empty-note")).toBeVisible();
  });

  test("desktop hover opens the empty-state card with no rollup", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await createSession(page, "composer popover empty hover");
    await page.getByTestId("context-chip").hover();
    await expect(page.getByTestId("context-usage-empty-note")).toBeVisible();
  });

  test("mobile 390: tapping the context row in the sheet shows the explanation", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await createSession(page, "composer popover empty mobile");
    await optionsTrigger(page).click();
    await page.getByTestId("context-chip").click();
    await expect(page.getByTestId("context-usage-empty-note")).toBeVisible();
  });
});

test.describe("stacked mobile usage sheet closes with its parent (RC4)", () => {
  test("390: one scrim tap dismisses stacked usage together with the options sheet", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await createSession(page, "composer popover sheet scrim");
    await injectRollup(page);
    await optionsTrigger(page).click();
    const sheet = page.getByTestId("composer-options-sheet");
    await expect(sheet).toBeVisible();
    await page.getByTestId("context-chip").click();
    await expect(page.getByTestId("context-usage-popover")).toHaveAttribute("data-mobile", "1");

    // One tap on the scrim strip above the stacked usage sheet must close
    // usage WITH the options sheet — no orphaned popover left floating.
    await sheetScrim(page).click({ position: { x: 6, y: 6 } });
    await expect(page.getByTestId("context-usage-popover")).toHaveCount(0);
    await expect(sheet).toHaveCount(0);

    // The composer trigger is hit-testable again (nothing covers it).
    const trigger = optionsTrigger(page);
    const box = (await trigger.boundingBox())!;
    const hitIsTrigger = await page.evaluate((p) => {
      const el = document.elementFromPoint(p.x, p.y);
      return Boolean(el && el.matches("[data-options-trigger='1']"));
    }, { x: box.x + box.width / 2, y: box.y + box.height / 2 });
    expect(hitIsTrigger).toBe(true);
  });

  test("700x850: a single Escape leaves no stacked usage sheet", async ({ page }) => {
    await page.setViewportSize({ width: 700, height: 850 });
    await createSession(page, "composer popover sheet escape");
    await injectRollup(page);
    await optionsTrigger(page).click();
    await expect(page.getByTestId("composer-options-sheet")).toBeVisible();
    await page.getByTestId("context-chip").click();
    await expect(page.getByTestId("context-usage-popover")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("context-usage-popover")).toHaveCount(0);
    await expect(page.getByTestId("composer-options-sheet")).toHaveCount(0);
  });

  test("390: the other option rows in the sheet stay usable (RC2 mobile regression)", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await createSession(page, "composer popover sheet rows");
    await injectRollup(page);
    await optionsTrigger(page).click();
    const sheet = page.getByTestId("composer-options-sheet");
    await expect(sheet).toBeVisible();
    // The context row renders and stays reachable despite the hover plumbing.
    await expect(sheet.getByTestId("context-chip")).toBeVisible();
    // Stacking usage and closing it with the scrim leaves the sheet usable.
    await sheet.getByTestId("context-chip").click();
    await expect(page.getByTestId("context-usage-popover")).toBeVisible();
    await sheetScrim(page).click({ position: { x: 6, y: 6 } });
    await expect(page.getByTestId("context-usage-popover")).toHaveCount(0);
  });

  test("iPhone/WebKit: scrim tap dismisses the stacked usage sheet", async () => {
    // Real-engine phone: connects to the jammy run-server
    // (PW_TEST_CONNECT_WS_ENDPOINT, e.g. ws://127.0.0.1:3177) or launches the
    // bundled WebKit; skips on hosts with neither. Same self-contained
    // pattern as uo6b-evidence.hub.spec.ts.
    const endpoint = process.env.PW_TEST_CONNECT_WS_ENDPOINT;
    let browser: Browser;
    if (endpoint) {
      browser = await webkit.connect(endpoint);
    } else {
      let bundled = true;
      const { access } = await import("node:fs/promises");
      await access(webkit.executablePath()).catch(() => {
        bundled = false;
      });
      test.skip(!bundled, "needs WebKit (PW_TEST_CONNECT_WS_ENDPOINT or a bundled webkit build)");
      browser = await webkit.launch();
    }
    const baseURL = test.info().project.use.baseURL;
    if (!baseURL) throw new Error("iPhone/WebKit case needs a configured project baseURL");
    const context: BrowserContext = await browser.newContext({ ...devices["iPhone 13"], baseURL });
    const page: Page = await context.newPage();
    let id = "";
    try {
      await login(page, "e2e-composerpop-iphone");
      await page.setViewportSize({ width: 390, height: 844 });
      id = await createSession(page, "composer popover sheet webkit");
      await injectRollup(page);
      await optionsTrigger(page).click();
      await expect(page.getByTestId("composer-options-sheet")).toBeVisible();
      await page.getByTestId("context-chip").click();
      await expect(page.getByTestId("context-usage-popover")).toHaveAttribute("data-mobile", "1");
      await sheetScrim(page).click({ position: { x: 6, y: 6 } });
      await expect(page.getByTestId("context-usage-popover")).toHaveCount(0);
      await expect(page.getByTestId("composer-options-sheet")).toHaveCount(0);
    } finally {
      if (id) await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => {});
      await context.close();
      await browser.close();
    }
  });
});


test.describe("notification stack does not cover menus (item 8)", () => {
  const raiseTwo = (page: Page) =>
    page.evaluate(() => {
      const lab = (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab;
      // Distinct subjects = distinct keys, so both stack (taller stack).
      lab?.notify({ severity: "blocking", subject: "standing one", stage: "standing error" });
      lab?.notify({ severity: "blocking", subject: "standing two", stage: "standing error" });
    });
  const clear = (page: Page) =>
    page.evaluate(() => {
      (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab?.dismissAllBlocking();
    });

  test("open menus stay hit-testable above a blocking notification", async ({ page }) => {
    // The standing-error stack is bottom-CENTER; the right-anchored
    // permission menu never meets it at 1440, but the left-anchored effort
    // panel does. Two blockers make that overlap tall enough to probe.
    //
    // Base build: .stack z-30 with default pointer events, popover z-5:
    // the overlap hit resolves to the stack and stack pointer-events is
    // "auto". Fixed: popover z-40, .stack pointer-events:none, only
    // .blocking auto — the hit lands on the panel.
    const probeOverlap = (menuTestId: string) =>
      page.evaluate((testId) => {
        const rectOf = (el: Element) => {
          const r = el.getBoundingClientRect();
          return { x: r.x, y: r.y, w: r.width, h: r.height };
        };
        const menu = document.querySelector<HTMLElement>(`[data-testid='${testId}']`);
        const blocker = document.querySelector<HTMLElement>("[data-testid='blocking-errors']");
        if (!menu || !blocker) return { ok: false as const, reason: "missing nodes" };
        const a = rectOf(menu);
        const b = rectOf(blocker);
        const x1 = Math.max(a.x, b.x);
        const y1 = Math.max(a.y, b.y);
        const x2 = Math.min(a.x + a.w, b.x + b.w);
        const y2 = Math.min(a.y + a.h, b.y + b.h);
        if (x2 - x1 < 8 || y2 - y1 < 8) return { ok: false as const, reason: "no overlap" };
        const x = x1 + (x2 - x1) / 2;
        const y = y2 - 12; // 12 px above the panel bottom, deep in both boxes
        const el = document.elementFromPoint(x, y);
        return { ok: true as const, onMenu: !!el?.closest(`[data-testid='${testId}']`), x, y };
      }, menuTestId);

    const styleGuarantees = (page: Page) =>
      page.evaluate(() => {
        const blockingEl = document.querySelector<HTMLElement>("[data-testid='blocking-errors']");
        // The .stack container is the blocker's parent (info-toasts is not
        // rendered when only blockers exist).
        const stackEl = blockingEl?.parentElement;
        const panelEl = document.querySelector<HTMLElement>("[data-testid='effort-menu']");
        if (!stackEl || !blockingEl || !panelEl) return null;
        return {
          stackPointerEvents: getComputedStyle(stackEl).pointerEvents,
          blockingPointerEvents: getComputedStyle(blockingEl).pointerEvents,
          stackZ: parseInt(getComputedStyle(stackEl).zIndex || "0", 10),
          panelZ: parseInt(getComputedStyle(panelEl).zIndex || "0", 10),
        };
      });

    await page.setViewportSize({ width: 1440, height: 900 });
    await createSession(page, "composer popover zorder");

    // Permission menu bottom row: the hit stays on the menu and a trial
    // click does not close it while standing errors are up.
    await page.getByTestId("permission-chip").click();
    const permMenu = page.getByTestId("permission-menu");
    await expect(permMenu).toBeVisible();
    await raiseTwo(page);
    await expect(page.getByText("standing one")).toBeVisible();
    const permRow = permMenu.locator("[data-testid^='permission-option-']:not([disabled])").first();
    const permBox = (await permRow.boundingBox())!;
    const onPermRow = await page.evaluate((b) => {
      const el = document.elementFromPoint(b.x + b.width / 2, b.y + b.height / 2);
      return !!el?.closest("[data-testid='permission-menu']");
    }, permBox);
    expect(onPermRow).toBe(true);
    await permRow.click({ trial: true });
    await expect(permMenu).toBeVisible();
    await clear(page);
    await page.keyboard.press("Escape");

    // Effort panel overlaps the centered stack: open first, then raise.
    await page.getByTestId("model-effort-chip").click();
    const sliderPanel = page.getByTestId("effort-slider-panel");
    await expect(sliderPanel).toBeVisible();
    await raiseTwo(page);
    await expect(page.getByText("standing one")).toBeVisible();

    // Token guarantees (fail deterministically on base).
    const styles = await styleGuarantees(page);
    expect(styles).not.toBeNull();
    expect(styles!.stackPointerEvents).toBe("none");
    expect(styles!.blockingPointerEvents).toBe("auto");
    expect(styles!.panelZ).toBeGreaterThan(styles!.stackZ);

    // The overlap hit lands on the panel, not the standing error, and the
    // point is clickable without dismissing the panel.
    const hit = await probeOverlap("effort-slider-panel");
    expect(hit.ok, hit.ok ? "" : hit.reason).toBe(true);
    if (hit.ok) {
      expect(hit.onMenu).toBe(true);
      const box = (await sliderPanel.boundingBox())!;
      await sliderPanel.click({ position: { x: hit.x! - box.x, y: hit.y! - box.y }, trial: true });
      await expect(sliderPanel).toBeVisible();
    }
    await clear(page);
  });
});
