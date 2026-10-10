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

// Drives the in-process fake Node (creates e2e-fake-node sessions); never run
// against an external hub that has no fake node.
test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");

type NotifyLab = {
  notify: (input: { severity?: "info" | "blocking"; subject?: string; stage?: string; reason?: string }) => string;
  toast: (text: string) => void;
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
  // r3 item 5: await the selection change (the old fire-and-forget let start
  // run against the previous host, so cleanup deleted the wrong id) and wait
  // until start is enabled FOR THIS HOST before filling/starting.
  await hostPicker.selectOption(host!);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  // r4 item 2: arm the response waiter BEFORE the click and register the new
  // id for cleanup from the create RESPONSE, not only from the post-navigation
  // URL. A slow navigation after start (waitForURL) used to leave the instance
  // untracked and therefore undeleted when the test bailed here.
  const creating = page.waitForResponse(
    (response) =>
      response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
    { timeout: 20_000 },
  );
  await page.getByTestId("new-session-start").click();
  const response = await creating;
  expect(response.ok(), `instance create failed: ${response.status()}`).toBe(true);
  const body = (await response.json().catch(() => null)) as
    | { instance?: { instanceId?: string; id?: string } }
    | null;
  const responseId = body?.instance?.instanceId ?? body?.instance?.id;
  if (responseId && !created.includes(responseId)) created.push(responseId);
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  if (!created.includes(id)) created.push(id);
  return id;
}

test.beforeEach(async ({ page }) => {
  await login(page);
});

// The fake Node parks a create-time approval; answering it lets the first turn
// finish so the composer returns to the idle 发送 (composer-send) control.
// Same answer shape and durability wait as effort-sync.hub.spec.ts.
async function clearApprovals(page: Page, instanceId: string) {
  await page.evaluate(async (id) => {
    const listPending = async () => {
      const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
      return (body.items ?? []).filter(
        (item: { instanceId?: string; state?: string }) =>
          item.instanceId === id && item.state === "pending",
      );
    };
    const deadline = Date.now() + 10_000;
    let mine = await listPending();
    while (mine.length === 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      mine = await listPending();
    }
    for (const item of mine) {
      const optionId = item.request?.options?.[0]?.id;
      if (!optionId) continue;
      await fetch(`/v1/interactions/${item.interactionId ?? item.id}/answer`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          answer: {
            kind: "approval",
            optionId,
            inputDigest: item.request?.inputDigest ?? "",
          },
        }),
      });
    }
    let remaining = await listPending();
    while (remaining.length > 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      remaining = await listPending();
    }
  }, instanceId);
}

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

  test("390: after stacking usage, the permission, effort and model rows each still respond (RC2 mobile regression)", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    const id = await createSession(page, "composer popover sheet rows");
    // Apply the launch model catalog before opening the sheet (mirrors
    // ux-modelsync): otherwise the gateway model rows are not in the list yet.
    await page.reload();
    await expect(page.getByTestId("composer-input")).toBeVisible({ timeout: 20_000 });
    await injectRollup(page);

    // Every option row dispatches instance.configure, with a different payload
    // branch per row. Match the branch so a click on the wrong (or a dead) row
    // can never satisfy the assertion.
    const configure = (branch: (payload: { permissionMode?: unknown; effort?: unknown; model?: unknown }) => boolean) =>
      page.waitForRequest((request) => {
        if (
          request.method() !== "POST" ||
          !request.url().endsWith(`/v1/instances/${id}/commands`) ||
          request.postDataJSON()?.operation !== "instance.configure"
        ) {
          return false;
        }
        return branch(request.postDataJSON()?.payload ?? {});
      });

    const openSheetAndStackUsage = async () => {
      await optionsTrigger(page).click();
      const sheet = page.getByTestId("composer-options-sheet");
      await expect(sheet).toBeVisible();
      // The context row renders despite the hover plumbing.
      await expect(sheet.getByTestId("context-chip")).toBeVisible();
      // Stack the usage card, then return to the sheet through the card's OWN
      // close (it leaves the sheet open and returns focus to the context chip):
      // the round-trip the RC2 regression left broken.
      await sheet.getByTestId("context-chip").click();
      await expect(page.getByTestId("context-usage-popover")).toBeVisible();
      await page.getByTestId("context-usage-close").click();
      await expect(page.getByTestId("context-usage-popover")).toHaveCount(0);
      await expect(page.getByTestId("composer-options-sheet")).toBeVisible();
      return page.getByTestId("composer-options-sheet");
    };

    // (1) Permission: a REAL pick on a live permission row posts the mode.
    let sheet = await openSheetAndStackUsage();
    const permReady = configure((payload) => payload.permissionMode != null);
    const permRow = sheet.locator("[data-testid^='permission-option-']:not([disabled])").first();
    await expect(permRow).toBeVisible();
    await permRow.click();
    await permReady;

    // (2) Effort: reopen, open the tier list, pick a non-selected tier.
    sheet = await openSheetAndStackUsage();
    await sheet.getByTestId("effort-open-list").click();
    await expect(sheet.getByTestId("effort-list")).toBeVisible();
    const effortReady = configure((payload) => payload.effort != null && payload.model == null);
    const tier = sheet
      .getByTestId("effort-list")
      .locator("[data-testid^='effort-tier-']:not([disabled])[data-selected='0']")
      .first();
    await expect(tier).toBeVisible();
    await tier.click();
    await effortReady;

    // (3) Model: the tier pick only closed the inner list, the options SHEET
    // is still open. Reopen the list and pick a gateway model row; it posts
    // the model id and the click closes the list. Use a row other than auto.
    await sheet.getByTestId("effort-open-list").click();
    await expect(sheet.getByTestId("effort-list")).toBeVisible();
    const modelReady = configure((payload) => typeof payload.model === "string" && payload.model.length > 0);
    const modelRow = sheet
      .getByTestId("effort-list")
      .locator("[data-testid='model-option-fast']:not([disabled])")
      .first();
    await expect(modelRow).toBeVisible();
    await modelRow.click();
    await modelReady;
    await expect(sheet.getByTestId("effort-list")).toHaveCount(0);
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


test.describe("overlay z tiers keep the annotation dock under real scrims (r3 item 1)", () => {
  test("390: with the options sheet open, a hit over ＋加批注 lands on the sheet, not the dock", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await createSession(page, "composer popover dock under sheet");
    const add = page.getByTestId("annotation-add");
    await expect(add).toBeVisible();
    const box = (await add.boundingBox())!;
    const point = { x: box.x + box.width / 2, y: box.y + box.height / 2 };

    await optionsTrigger(page).click();
    await expect(page.getByTestId("composer-options-sheet")).toBeVisible();

    // The round-2 regression: the dock (z 50) painted above the literal-40
    // sheet scrim, so this tap opened the annotation panel UNDER the sheet.
    const hit = await page.evaluate((p) => {
      const el = document.elementFromPoint(p.x, p.y);
      return {
        dock: el?.closest("[data-testid='annotation-add']") != null,
        sheet: el?.closest("[data-variant='sheet']") != null,
        tag: (el as HTMLElement | null)?.dataset.testid ?? el?.tagName ?? "",
      };
    }, point);
    expect(hit.dock, `hit resolved to the annotation dock (${hit.tag})`).toBe(false);
    expect(hit.sheet).toBe(true);
    if (process.env.REMUDA_EVIDENCE === "1") {
      await page.screenshot({ path: "test-results/composerpop-r3-dock-under-sheet-390.png", animations: "disabled" });
    }

    // A real tap at that point must NOT open the annotation panel.
    await page.mouse.click(point.x, point.y);
    await expect(page.getByTestId("annotation-panel")).toHaveCount(0);
  });

  test("390: the stacked usage card paints strictly above the options sheet", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await createSession(page, "composer popover stacked tier");
    await injectRollup(page);
    await optionsTrigger(page).click();
    await expect(page.getByTestId("composer-options-sheet")).toBeVisible();
    await page.getByTestId("context-chip").click();
    const usage = page.getByTestId("context-usage-popover");
    await expect(usage).toHaveAttribute("data-mobile", "1");

    // Numeric tier guarantee: 61 (--z-sheet-stacked) over 60 (--z-sheet).
    // Equal values used to let the later-sibling options sheet cover the card.
    const tiers = await page.evaluate(() => {
      const z = (sel: string) => {
        const el = document.querySelector<HTMLElement>(sel);
        return el ? parseInt(getComputedStyle(el).zIndex || "0", 10) : NaN;
      };
      return {
        usage: z("[data-testid='context-usage-popover']"),
        scrim: z("[data-variant='sheet']"),
      };
    });
    expect(tiers.usage).toBeGreaterThan(tiers.scrim);

    // And the geometry guarantee: a hit at the stacked card's own center
    // reaches the card, not the options sheet behind it.
    const box = (await usage.boundingBox())!;
    const hitIsUsage = await page.evaluate((p) => {
      const el = document.elementFromPoint(p.x, p.y);
      return el?.closest("[data-testid='context-usage-popover']") != null;
    }, { x: box.x + box.width / 2, y: box.y + box.height / 2 });
    expect(hitIsUsage).toBe(true);
    if (process.env.REMUDA_EVIDENCE === "1") {
      await page.screenshot({ path: "test-results/composerpop-r3-stacked-usage-390.png", animations: "disabled" });
    }
  });
});


test.describe("notification stack clears the phone home bar at 390 (r2/r3 item 2)", () => {
  const clear = (page: Page) =>
    page.evaluate(() => {
      (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab?.dismissAllBlocking();
    });

  test("a blocking notice never covers the bottom PhoneNav buttons on a home route", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    // login() lands a compact viewport on /m where PhoneNav renders (home
    // route); no session is needed.
    await login(page);
    await expect(page.getByTestId("phone-nav-home")).toBeVisible();
    await page.evaluate(() => {
      const lab = (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab;
      lab?.notify({ severity: "blocking", subject: "standing one", stage: "standing error" });
    });
    await expect(page.getByText("standing error")).toBeVisible();

    // Each bottom nav button is hit-testable at its own center — the stack
    // must clear the 56px home bar (+ safe area), not park over it.
    for (const id of ["phone-nav-home", "phone-nav-new", "phone-nav-more"]) {
      const btn = page.getByTestId(id);
      const box = (await btn.boundingBox())!;
      const hit = await page.evaluate((p) => {
        const el = document.elementFromPoint(p.x, p.y);
        return el?.closest(`[data-testid='${p.id}']`) != null;
      }, { id, x: box.x + box.width / 2, y: box.y + box.height / 2 });
      expect(hit, `${id} covered by the notification stack`).toBe(true);
    }

    // A REAL tap on 更多 reaches the button (toggles its menu) rather than the
    // standing notice.
    await page.getByTestId("phone-nav-more").click();
    await expect(page.getByTestId("phone-nav-more")).toHaveAttribute("aria-expanded", "true");

    // /m mounts PhoneNav, so the shell stamps data-phone-nav and the stack is
    // lifted by 56px (--phone-nav-h) + safe-bottom + space-3.
    const shell = page.locator("[data-compact]");
    await expect(shell).toHaveAttribute("data-phone-nav", "1");
    const bottomOffset = await page.evaluate(() => {
      const stack = document.querySelector<HTMLElement>("[data-testid='blocking-errors']")?.parentElement;
      return stack ? window.innerHeight - stack.getBoundingClientRect().bottom : NaN;
    });
    expect(bottomOffset).toBeCloseTo(68, 0);

    await clear(page);
  });

  // r4 item 1: a legacy hubStore.toast (bridged to the info strip) raised
  // FIRST, then a standing blocker — the taller stack the fix must keep clear
  // of the whole session dock.
  const raiseToastThenBlocker = (page: Page) =>
    page.evaluate(() => {
      const lab = (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab;
      lab?.toast("已保存更改");
      lab?.notify({ severity: "blocking", subject: "standing one", stage: "standing error" });
    });

  // The stack's clearance of the viewport bottom must equal the measured
  // session-dock height plus the 12px gap (the shell pads no bottom safe area;
  // the dock reaches the band edge). The dock's async strips
  // (LiveStatusStrip/TaskTrack/notifications) can keep growing for a frame or
  // two after mount, so poll until the ResizeObserver-published variable has
  // converged with the laid-out dock AND the browser has placed the stack one
  // gap above it — a one-shot read can see a stale variable mid-settle.
  const expectStackClearsDock = async (page: Page) => {
    await page.waitForFunction(
      () => {
        const stack = document.querySelector<HTMLElement>("[data-testid='blocking-errors']")?.parentElement;
        const docks = document.querySelectorAll<HTMLElement>("[data-testid='session-dock']");
        const dock = docks[0];
        if (!stack || docks.length !== 1 || !dock) return false;
        const bottom = Math.round(window.innerHeight - stack.getBoundingClientRect().bottom);
        const dockH = Math.round(dock.offsetHeight);
        const varVal =
          parseFloat(
            getComputedStyle(document.documentElement).getPropertyValue("--session-dock-h"),
          ) || NaN;
        return dockH > 0 && Math.abs(varVal - dockH) <= 1 && Math.abs(bottom - (dockH + 12)) <= 1;
      },
      null,
      { timeout: 10_000 },
    );
  };

  test("390 /s/:id: a blocking notice lifts over the whole dock, so options and send stay real-clickable", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    const id = await createSession(page, "composer popover notify session");
    // Clear the fake Node's create-time approval so the first turn ends and the
    // primary returns to idle 发送 (composer-send, not 排队).
    await clearApprovals(page, id);
    // PhoneNav is not mounted on /s/* (r3 item 2) and the session layout owns
    // the lift. waitForURL resolves on the history update before React commits
    // the route, so assert on the auto-retrying locator, not a one-shot read.
    const shell = page.locator("[data-compact]");
    await expect(shell).not.toHaveAttribute("data-phone-nav", "1");
    await expect(shell).toHaveAttribute("data-layout", "session");
    await expect(page.getByTestId("composer-input")).toBeVisible();
    await expect(page.getByTestId("composer-send")).toBeVisible({ timeout: 20_000 });

    // Raise the notice BEFORE touching any composer control (the r4 ordering).
    await raiseToastThenBlocker(page);
    await expect(page.getByText("standing error")).toBeVisible();
    await expectStackClearsDock(page);

    // A REAL tap on the collapsed options trigger opens the sheet — on the
    // buggy build the blocker parked over this control and swallowed the tap.
    await optionsTrigger(page).click({ trial: false });
    await expect(page.getByTestId("composer-options-sheet")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("composer-options-sheet")).toHaveCount(0);

    // A REAL send: fill and tap the primary button; the instance.send command
    // must be dispatched (the tap reached composer-send, not the blocker).
    await page.getByTestId("composer-input").fill("notice up send");
    const sent = page.waitForRequest(
      (request) =>
        request.method() === "POST" &&
        request.url().endsWith(`/v1/instances/${id}/commands`) &&
        request.postDataJSON()?.operation === "instance.send",
      { timeout: 10_000 },
    );
    await page.getByTestId("composer-send").click();
    await sent;

    if (process.env.REMUDA_EVIDENCE === "1") {
      await page.screenshot({ path: "test-results/composerpop-r4-notify-session-390.png", animations: "disabled" });
    }
    await clear(page);
  });

  test("1440 /s/:id: a blocking notice leaves the effort and permission chips real-clickable", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await createSession(page, "composer popover notify chips");
    await expect(page.getByTestId("composer-input")).toBeVisible();

    // Raise FIRST (toast + blocker), then open each menu — the r4 ordering.
    await raiseToastThenBlocker(page);
    await expect(page.getByText("standing error")).toBeVisible();
    await expectStackClearsDock(page);

    await page.getByTestId("permission-chip").click();
    await expect(page.getByTestId("permission-menu")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("permission-menu")).toHaveCount(0);

    await page.getByTestId("model-effort-chip").click();
    await expect(page.getByTestId("effort-slider-panel")).toBeVisible();

    if (process.env.REMUDA_EVIDENCE === "1") {
      await page.screenshot({ path: "test-results/composerpop-r4-notify-chips-1440.png", animations: "disabled" });
    }
    await clear(page);
  });
});


test.describe("notification stack does not cover menus (item 8)", () => {
  const clear = (page: Page) =>
    page.evaluate(() => {
      (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab?.dismissAllBlocking();
    });

  // The permission menu is flush full-width option buttons, so the only inert
  // chrome its top band offers is the popover's 7px padding; the effort panel
  // has inert chrome (plain divs, the aria-hidden bolt) throughout. Rather
  // than guessing how many rows reach which band, raise distinct standing
  // errors (distinct subjects = distinct keys, BLOCKING_LIMIT 5) until the
  // menu∩standing-error intersection actually contains an inert probe point.
  const raiseBlockersUntilInertOverlap = async (page: Page, menuTestId: string) => {
    const interactive =
      'button,input,select,textarea,a[href],[role="button"],[role="slider"],[role="switch"],[tabindex]';
    for (let n = 1; n <= 5; n++) {
      // Raise the nth distinct standing error (distinct subject = distinct
      // key; BLOCKING_LIMIT 5) and keep the legacy hubStore.toast fresh (2.4 s
      // TTL), then wait for the React commit before measuring.
      await page.evaluate((index) => {
        const lab = (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab;
        lab?.toast("已保存更改");
        lab?.notify({
          severity: "blocking",
          subject: `standing ${index}`,
          stage: "standing error",
        });
      }, n);
      await page.waitForFunction(
        (count) => document.querySelectorAll("[data-testid='blocking-error']").length === count,
        n,
      );
      const hasInertOverlap = await page.evaluate(
        ({ testId, interactive }) => {
          const rectOf = (el: Element) => {
            const r = el.getBoundingClientRect();
            return { x: r.x, y: r.y, w: r.width, h: r.height };
          };
          const menu = document.querySelector<HTMLElement>(`[data-testid='${testId}']`);
          const blocker = document.querySelector<HTMLElement>("[data-testid='blocking-errors']");
          if (!menu || !blocker) return false;
          const a = rectOf(menu);
          const b = rectOf(blocker);
          const region = {
            x: Math.max(a.x, b.x),
            y: Math.max(a.y, b.y),
            w: Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x),
            h: Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y),
          };
          if (region.w < 8 || region.h < 8) return false;
          for (let yy = region.y + 2; yy < region.y + region.h - 2; yy += 2) {
            for (let xx = region.x + 2; xx < region.x + region.w - 2; xx += 2) {
              const el = document.elementFromPoint(xx, yy);
              if (
                el?.closest(`[data-testid='${testId}']`) &&
                !(el as HTMLElement).closest(interactive)
              ) {
                return true;
              }
            }
          }
          return false;
        },
        { testId: menuTestId, interactive },
      );
      if (hasInertOverlap) return;
    }
    throw new Error(`standing-error stack never produced an inert overlap with ${menuTestId}`);
  };

  test("open menus stay hit-testable above a blocking notification", async ({ page }) => {
    // The standing-error stack is bottom-CENTER and the menus open upward
    // from the control bar; with the dock at its real (short) height the
    // popups overlap the stack. Base build: .stack z-30 with default pointer
    // events, popover z-5: the overlap hit resolves to the stack and stack
    // pointer-events is "auto". Fixed: popover above the stack, .stack
    // pointer-events:none with only .blocking auto — the hit lands on the
    // menu.
    // Find a REAL-clickable INERT point — panel/menu chrome, never a control
    // (whose own click would close or change something). When useBlocker is
    // set the point is constrained to the menu∩standing-error intersection:
    // elementFromPoint resolving inside the menu THERE proves both z-order
    // and that the click reaches the menu, and a real click at that point must
    // leave the menu open.
    const inertPoint = (menuTestId: string, useBlocker: boolean) =>
      page.evaluate(
        ({ testId, withBlocker }) => {
          const rectOf = (el: Element) => {
            const r = el.getBoundingClientRect();
            return { x: r.x, y: r.y, w: r.width, h: r.height };
          };
          const menu = document.querySelector<HTMLElement>(`[data-testid='${testId}']`);
          if (!menu) return { ok: false as const, reason: "missing menu" };
          const a = rectOf(menu);
          let region = a;
          if (withBlocker) {
            const blocker = document.querySelector<HTMLElement>("[data-testid='blocking-errors']");
            if (!blocker) return { ok: false as const, reason: "missing blocker" };
            const b = rectOf(blocker);
            const x1 = Math.max(a.x, b.x);
            const y1 = Math.max(a.y, b.y);
            const x2 = Math.min(a.x + a.w, b.x + b.w);
            const y2 = Math.min(a.y + a.h, b.y + b.h);
            if (x2 - x1 < 8 || y2 - y1 < 8) return { ok: false as const, reason: "no overlap" };
            region = { x: x1, y: y1, w: x2 - x1, h: y2 - y1 };
          }
          const interactive =
            'button,input,select,textarea,a[href],[role="button"],[role="slider"],[role="switch"],[tabindex]';
          for (let yy = region.y + 4; yy < region.y + region.h - 4; yy += 4) {
            for (let xx = region.x + 4; xx < region.x + region.w - 4; xx += 4) {
              const el = document.elementFromPoint(xx, yy);
              if (el?.closest(`[data-testid='${testId}']`) && !(el as HTMLElement).closest(interactive)) {
                return { ok: true as const, x: xx, y: yy };
              }
            }
          }
          return { ok: false as const, reason: "no inert point" };
        },
        { testId: menuTestId, withBlocker: useBlocker },
      );

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
    const id = await createSession(page, "composer popover zorder");
    // Answer the create-time approval FIRST: while it stands it lives inside
    // the measured session dock and hoists the stack hundreds of px up, out
    // of the menus' geometry — the intersection probes below would then find
    // no overlap (the r4 "separated" assertion passed only via that lift).
    await clearApprovals(page, id);
    await expect(page.getByTestId("composer-input")).toBeEnabled();
    // Wait for the answered approval card to unmount AND for the
    // ResizeObserver-published --session-dock-h to converge with the now
    // shorter dock. The server answer lands immediately, but the client store
    // learns it on its ~2 s poll, so the card (and the stale, taller anchor)
    // outlives clearApprovals by a couple of seconds; probing against the
    // stale anchor parks the stack above the menus with no intersection.
    await expect(page.getByTestId("approval-card")).toHaveCount(0, { timeout: 15_000 });
    await page.waitForFunction(
      () => {
        const dock = document.querySelector<HTMLElement>("[data-testid='session-dock']");
        if (!dock) return false;
        const varVal =
          parseFloat(
            getComputedStyle(document.documentElement).getPropertyValue("--session-dock-h"),
          ) || NaN;
        return dock.offsetHeight > 0 && Math.abs(varVal - dock.offsetHeight) <= 1;
      },
      null,
      { timeout: 10_000 },
    );

    // Permission menu: open FIRST, THEN raise the blockers until they reach
    // the menu's top chrome band — the r5 ordering. A REAL click on an INERT
    // point inside the menu∩standing-error intersection is delivered to the
    // menu and keeps it open while the standing errors are up.
    await page.getByTestId("permission-chip").click();
    const permMenu = page.getByTestId("permission-menu");
    await expect(permMenu).toBeVisible();
    await raiseBlockersUntilInertOverlap(page, "permission-menu");
    await expect(page.getByText("standing error").first()).toBeVisible();
    const permPoint = await inertPoint("permission-menu", true);
    expect(permPoint.ok, permPoint.ok ? "" : permPoint.reason).toBe(true);
    if (permPoint.ok) {
      await page.mouse.click(permPoint.x!, permPoint.y!);
      await expect(permMenu).toBeVisible();
    }
    await clear(page);
    await page.keyboard.press("Escape");
    await expect(permMenu).toHaveCount(0);

    // Effort panel: same ordering — open, then raise. With the approval card
    // gone the panel DOES overlap the centered stack; z-order keeps the panel
    // on top.
    await page.getByTestId("model-effort-chip").click();
    const sliderPanel = page.getByTestId("effort-slider-panel");
    await expect(sliderPanel).toBeVisible();
    await raiseBlockersUntilInertOverlap(page, "effort-slider-panel");
    await expect(page.getByText("standing error").first()).toBeVisible();

    // Token guarantees (fail deterministically on base).
    const styles = await styleGuarantees(page);
    expect(styles).not.toBeNull();
    expect(styles!.stackPointerEvents).toBe("none");
    expect(styles!.blockingPointerEvents).toBe("auto");
    expect(styles!.panelZ).toBeGreaterThan(styles!.stackZ);

    // Prove the panel is interactive INSIDE its overlap with the standing
    // stack: a REAL click on inert panel chrome at an intersection point
    // reaches the panel and leaves it open while the standing error is up.
    const effortPoint = await inertPoint("effort-slider-panel", true);
    expect(effortPoint.ok, effortPoint.ok ? "" : effortPoint.reason).toBe(true);
    if (effortPoint.ok) {
      await page.mouse.click(effortPoint.x!, effortPoint.y!);
      await expect(sliderPanel).toBeVisible();
    }
    if (process.env.REMUDA_EVIDENCE === "1") {
      // 1440 desktop evidence paired with the 390 shots in the r3 item-1 suite.
      await page.screenshot({ path: "test-results/composerpop-r5-menu-above-notify-1440.png", animations: "disabled" });
    }
    await clear(page);
  });
});

test.describe("notification stack clears the tty bottom chrome (r5 item 2)", () => {
  // Two standing blockers (distinct subjects = distinct keys): tall enough to
  // cover the phone key bar on the pre-fix 12px anchor.
  const raiseTwo = (page: Page) =>
    page.evaluate(() => {
      const lab = (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab;
      lab?.notify({ severity: "blocking", subject: "standing one", stage: "standing error" });
      lab?.notify({ severity: "blocking", subject: "standing two", stage: "standing error" });
    });
  const clear = (page: Page) =>
    page.evaluate(() => {
      (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab?.dismissAllBlocking();
    });

  async function createTerminal(page: Page): Promise<string> {
    const response = await page.request.get("/v1/hosts");
    expect(response.ok()).toBe(true);
    const body = (await response.json()) as { items?: { id?: string; label?: string }[] };
    const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
    expect(host?.id, "fake node host").toBeTruthy();
    const createdRes = await page.request.post("/v1/instances", {
      data: {
        hostId: host!.id,
        workspaceId: "wsp_e2e",
        kind: "terminal",
        driver: "shell-pty",
        prompt: "composer popover tty chrome",
      },
    });
    expect(
      createdRes.ok(),
      `create terminal: ${createdRes.status()} ${await createdRes.text()}`,
    ).toBe(true);
    const createdBody = (await createdRes.json()) as {
      instance: { instanceId?: string; id?: string };
    };
    // Register for the suite afterEach the instant the id is known: on a
    // test timeout Playwright tears the request context down before the
    // test's own finally runs, so its DELETE fails silently and the terminal
    // instance leaks into the shared hub (tripping host maxInstances later).
    // The finally delete stays as the fast path (its 404 here is swallowed).
    const id = createdBody.instance.instanceId ?? createdBody.instance.id!;
    if (!created.includes(id)) created.push(id);
    return id;
  }

  async function gotoTty(page: Page, id: string) {
    await page.goto(`/s/${id}/tty`);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "tty");
    await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 20_000 });
    await expect(page.locator(".xterm")).toBeVisible();
  }

  // The ResizeObserver-published --session-dock-h must equal the measured tty
  // bottom chrome (local input dock / phone key bar) and the stack must sit one
  // 12px gap above it — same contract as the structured session dock.
  const expectStackClearsChrome = async (page: Page) => {
    await page.waitForFunction(
      () => {
        const stack = document.querySelector<HTMLElement>(
          "[data-testid='blocking-errors']",
        )?.parentElement;
        const chrome = document.querySelector<HTMLElement>("[data-testid='tty-bottom-chrome']");
        if (!stack || !chrome) return false;
        const bottom = Math.round(window.innerHeight - stack.getBoundingClientRect().bottom);
        const chromeH = Math.round(chrome.getBoundingClientRect().height);
        const varVal =
          parseFloat(
            getComputedStyle(document.documentElement).getPropertyValue("--session-dock-h"),
          ) || NaN;
        return chromeH > 0 && Math.abs(varVal - chromeH) <= 1 && Math.abs(bottom - (chromeH + 12)) <= 2;
      },
      null,
      { timeout: 10_000 },
    );
  };

  /** Center hit-test: the element at the centre must own the point (not the
   *  standing notice), and return the centre for a REAL tap. */
  const centerPoint = async (page: Page, testId: string) => {
    const node = page.getByTestId(testId).first();
    await node.evaluate((el) => el.scrollIntoView({ block: "nearest", inline: "center" }));
    const box = (await node.boundingBox())!;
    const point = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
    const hit = await page.evaluate(
      (p) =>
        document
          .elementFromPoint(p.x, p.y)
          ?.closest(`[data-testid='${p.id}']`)
          ?.getAttribute("data-testid") === p.id,
      { id: testId, ...point },
    );
    expect(hit, `${testId} covered by the notification stack`).toBe(true);
    return point;
  };

  test.describe("390px compact", () => {
    // Same touch context shape as m-keybar.hub: coarse pointer so both the
    // local input dock and the phone key bar mount (the full bottom chrome).
    test.use({ viewport: { width: 390, height: 844 }, hasTouch: true });

    test("a blocking notice lifts over the phone key bar, so tty keys stay real-tappable", async ({
      page,
    }) => {
      let id = "";
      try {
        id = await createTerminal(page);
        await gotoTty(page, id);

        // Raise the standing notices BEFORE touching the bar (the r5
        // ordering): on the pre-fix build the stack dropped to a 12px anchor
        // and parked across the nine-key row.
        await raiseTwo(page);
        await expect(page.getByText("standing error").first()).toBeVisible();
        await expectStackClearsChrome(page);

        // A REAL tap on 键 reaches the nine-key row and reveals the raw-key
        // strip (pre-fix: the blocker swallowed it).
        const keyboardPoint = await centerPoint(page, "phone-key-keyboard");
        await page.mouse.click(keyboardPoint.x, keyboardPoint.y);
        const rawBar = page.getByTestId("tty-keybar");
        await expect(rawBar).toBeVisible();
        // The expanded strip grows the measured chrome; re-assert clearance.
        await expectStackClearsChrome(page);

        // Hit-test and REAL-tap a tty-key-* button in the expanded row. Ctrl
        // is sticky, so aria-pressed proves the tap reached the key rather
        // than the standing notice.
        const ctrlPoint = await centerPoint(page, "tty-key-ctrl");
        await page.mouse.click(ctrlPoint.x, ctrlPoint.y);
        await expect(page.getByTestId("tty-key-ctrl")).toHaveAttribute("aria-pressed", "true");

        if (process.env.REMUDA_EVIDENCE === "1") {
          await page.screenshot({
            path: "test-results/composerpop-r5-tty-notify-390.png",
            animations: "disabled",
          });
        }
      } finally {
        await clear(page);
        if (id) await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
      }
    });
  });

  test("1440 /s/:id/tty: a blocking notice leaves the local input dock clickable", async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    let id = "";
    try {
      id = await createTerminal(page);
      await gotoTty(page, id);

      // Desktop defaults to 直连 (the local dock is unmounted); switch to the
      // 本地输入 mode so the tty-dock input is part of the bottom chrome.
      await page.getByRole("button", { name: "本地输入" }).click();
      await expect(page.getByTestId("tty-dock")).toBeVisible();
      await expect(page.getByTestId("tty-mode-pill")).toHaveText("keys");

      await raiseTwo(page);
      await expect(page.getByText("standing error").first()).toBeVisible();
      await expectStackClearsChrome(page);

      const input = page.locator("[data-testid='tty-dock'] input[aria-label='本地输入']");
      const box = (await input.boundingBox())!;
      const point = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
      const hit = await page.evaluate(
        (p) => document.elementFromPoint(p.x, p.y)?.closest("input[aria-label='本地输入']") != null,
        point,
      );
      expect(hit, "tty-dock input covered by the notification stack").toBe(true);
      // A REAL click focuses the input; typing lands in it.
      await page.mouse.click(point.x, point.y);
      await page.keyboard.type("ls -la");
      await expect(input).toHaveValue("ls -la");

      if (process.env.REMUDA_EVIDENCE === "1") {
        await page.screenshot({
          path: "test-results/composerpop-r5-tty-notify-1440.png",
          animations: "disabled",
        });
      }
    } finally {
      await clear(page);
      if (id) await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });
});

test.describe("notification stack keeps the safe-area offset without measured chrome (r6 item 1)", () => {
  // /s/:id/events mounts neither the structured dock nor the tty bottom
  // chrome (and the snapshot-loading state mounts neither either), so the
  // height-publishing hook REMOVES --session-dock-h. The pre-fix session
  // rule fell back to 0px and parked a blocking item inside a notched
  // phone's home-indicator zone; it must fall back to --safe-bottom like
  // the base rule instead.
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true });

  test("390 /s/:id/events: the stack bottom is safe-bottom + 12", async ({ page }) => {
    const id = await createSession(page, "composer popover events safe area");
    await page.goto(`/s/${id}/events`);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "events");

    // No measured chrome on the events route → the hook unpublished the var.
    await expect(page.getByTestId("session-dock")).toHaveCount(0);
    await expect(page.getByTestId("tty-bottom-chrome")).toHaveCount(0);
    await page.waitForFunction(
      () =>
        getComputedStyle(document.documentElement).getPropertyValue("--session-dock-h").trim() ===
        "",
      null,
      { timeout: 10_000 },
    );

    // env(safe-area-inset-bottom) cannot be emulated; stamp the derived
    // token directly (a notched phone is ~34 px).
    await page.evaluate(() => {
      document.documentElement.style.setProperty("--safe-bottom", "34px");
    });

    await page.evaluate(() => {
      const lab = (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab;
      lab?.notify({ severity: "blocking", subject: "standing one", stage: "standing error" });
    });
    try {
      await expect(page.getByText("standing error").first()).toBeVisible();
      // 34 px inset + the 12 px (--space-3) anchor gap.
      await page.waitForFunction(
        (expected) => {
          const stack = document.querySelector<HTMLElement>(
            "[data-testid='blocking-errors']",
          )?.parentElement;
          if (!stack) return false;
          const bottom = Math.round(window.innerHeight - stack.getBoundingClientRect().bottom);
          return Math.abs(bottom - expected) <= 2;
        },
        46,
        { timeout: 10_000 },
      );
    } finally {
      await page.evaluate(() => {
        (window as unknown as { __notifyLab?: NotifyLab }).__notifyLab?.dismissAllBlocking();
      });
    }
  });
});
