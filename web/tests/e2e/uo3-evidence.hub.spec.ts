import { expect, test, type Browser, type BrowserContext, type Page } from "@playwright/test";
import { devices } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * UO-3 acceptance: the `/m` phone shell and home (D-053 / ui-spec §1.3/§4.7).
 *
 * Geometry/interaction assertions run on every invocation; only the committed
 * screenshots need REMUDA_EVIDENCE=1 (390 + 1440, dark + light, Remuda renders
 * only). The phone cases create their own context with an iPhone device
 * descriptor so the same assertions run on the chromium engine and — via
 * `--browser webkit` — the WebKit engine the owner's real iPhone uses,
 * including a simulated soft keyboard around the home search input.
 *
 * Covered:
 *  - at 390 the /m chrome is exactly two 52px header rows plus the 56px (+
 *    safe area) PhoneNav: no spaces chips row and no tab strip;
 *  - the header's Space button opens the shared SpacesDrawer;
 *  - desktop /m still redirects to /sessions;
 *  - a fine pointer at 390 keeps desktop density (44px follows touch, not
 *    width);
 *  - dark and light resolve role tokens at 390 and 1440;
 *  - perf (`commit:HomeList`): while the bottom-bar badge gains pending
 *    interactions the home list does not commit.
 */

const shotDir = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../docs/design/evidence/ui-overhaul",
);
const evidence = process.env.REMUDA_EVIDENCE === "1";
const created: string[] = [];

async function shoot(page: Page, name: string): Promise<void> {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.waitForTimeout(200);
  await writeFile(
    path.join(shotDir, name),
    await page.screenshot({ animations: "disabled" }),
  );
}

async function phoneContext(browser: Browser): Promise<BrowserContext> {
  return browser.newContext({ ...devices["iPhone 13"] });
}

/** Fine pointer at 390: compact layout, desktop density. */
async function finePhoneContext(browser: Browser): Promise<BrowserContext> {
  return browser.newContext({
    viewport: { width: 390, height: 844 },
    hasTouch: false,
    isMobile: false,
    deviceScaleFactor: 1,
  });
}

async function createSession(page: Page, prompt: string): Promise<string> {
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

async function cleanup(context: BrowserContext): Promise<void> {
  const page = await context.newPage();
  try {
    await login(page, "e2e-uo3-cleanup");
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  } finally {
    await page.close();
  }
}

test("390 iPhone: /m chrome is 52+52 with no chips row and no tab strip, and PhoneNav is 56", async ({
  browser,
}) => {
  const context = await phoneContext(browser);
  const page = await context.newPage();
  try {
    await login(page);
    await createSession(page, "UO-3 chrome blocked");
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    await expect(page.getByTestId("phone-inbox-badge")).toHaveText("1", { timeout: 15_000 });
    await expect(page.getByTestId("home-row")).toHaveCount(1);

    const head = page.locator('[data-testid="home-list"] > header');
    await expect(head).toBeVisible();
    const headBox = await head.boundingBox();
    expect(headBox).toBeTruthy();
    expect(Math.round(headBox!.height), "home head is 52 + 52").toBe(104);
    // The header's two div rows are the title row and the toolbar (the order
    // control's own div is nested inside the toolbar).
    const titleRow = head.locator("div").first();
    const titleRowBox = await titleRow.boundingBox();
    expect(titleRowBox).toBeTruthy();
    expect(Math.round(titleRowBox!.height), "title row height").toBe(52);
    const toolbarBox = await page.getByTestId("home-search").boundingBox();
    expect(toolbarBox).toBeTruthy();
    // The search sits in the second 52px band: below the title row's bottom
    // edge and never past the head's 104px total.
    expect(toolbarBox!.y, "search starts inside the second band").toBeGreaterThanOrEqual(
      titleRowBox!.y + 52 - 0.5,
    );
    expect(toolbarBox!.y + toolbarBox!.height).toBeLessThanOrEqual(
      titleRowBox!.y + 104 + 0.5,
    );

    const search = page.getByTestId("home-search");
    const spaceTrigger = page.getByTestId("spaces-drawer-open");
    // Text controls paint their full 44px height on a coarse pointer.
    for (const target of [search, spaceTrigger]) {
      const box = await target.boundingBox();
      expect(box, "primary control rendered").toBeTruthy();
      expect(box!.height, "44px visible on a coarse pointer").toBeGreaterThanOrEqual(43.5);
      expect(box!.width, "control inside the viewport").toBeGreaterThanOrEqual(44);
      expect(box!.x).toBeGreaterThanOrEqual(0);
      expect(box!.x + box!.width).toBeLessThanOrEqual(390);
    }

    // The ordering control is the shared §8.2 segmented control: 26px visible
    // items whose ::after hot zone reaches the 44px touch band.
    for (const id of ["home-order-clock", "home-order-list"] as const) {
      const item = page.getByTestId(id);
      const box = await item.boundingBox();
      expect(box).toBeTruthy();
      expect(Math.round(box!.height), "seg item visible height").toBe(26);
      // Nine pixels above and below the painted item still belong to it:
      // the vertical-only ::after reserves the 44px band without widening
      // sideways into its neighbour.
      const x = box!.x + box!.width / 2;
      for (const y of [box!.y - 8.5, box!.y + box!.height + 8.5]) {
        const hitTestId = await page.evaluate(
          (point) => document.elementFromPoint(point.x, point.y)?.closest("button")?.dataset.testid ?? null,
          { x, y },
        );
        expect(hitTestId, `44px hot band for ${id}`).toBe(id);
      }
    }

    // No spaces chips strip and no tab strip on /m (D-053 list-route rule).
    await expect(page.getByTestId("space-chip")).toHaveCount(0);
    await expect(page.getByTestId("space-tabs")).toHaveCount(0);

    // PhoneNav is the 56px home bar + safe area. Its box adds the 1px top
    // border outside the grid track, so measure the content band (the token
    // budget) and keep the whole bar inside the viewport.
    const bar = page.getByRole("navigation", { name: "手机底栏" });
    const barGeom = await bar.evaluate((el) => {
      const rect = el.getBoundingClientRect();
      const style = getComputedStyle(el);
      return {
        total: rect.height,
        border: parseFloat(style.borderTopWidth),
        safeBottom: parseFloat(style.paddingBottom),
        bottom: rect.bottom,
        token: parseFloat(
          getComputedStyle(document.documentElement).getPropertyValue("--phone-nav-h"),
        ),
      };
    });
    expect(Math.round(barGeom.safeBottom), "device profile exposes no safe-area inset").toBe(0);
    expect(Math.round(barGeom.token)).toBe(56);
    expect(
      Math.round(barGeom.total - barGeom.border - barGeom.safeBottom),
      "bar content band is the 56px phone-nav token",
    ).toBe(56);
    expect(barGeom.bottom).toBeLessThanOrEqual(844 + 0.5);

    // The header Space button opens the same SpacesDrawer the session header
    // chip uses; closing returns focus to the opener.
    await page.getByTestId("spaces-drawer-open").click();
    const drawer = page.getByTestId("spaces-drawer");
    await expect(drawer).toBeVisible();
    await expect(page.getByTestId("spaces-panel")).toBeVisible();
    await page.getByRole("button", { name: "关闭空间面板" }).click();
    await expect(drawer).toHaveCount(0);
    await expect(page.getByTestId("spaces-drawer-open")).toBeFocused();

    await shoot(page, "UO-3-home-390-dark.png");
  } finally {
    await cleanup(context);
    await context.close();
  }
});

test("390 with a fine pointer keeps desktop density: controls stay 32px", async ({ browser }) => {
  const context = await finePhoneContext(browser);
  const page = await context.newPage();
  try {
    await login(page);
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    // The compact layout still applies…
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("space-chip")).toHaveCount(0);
    // …but hit areas follow pointer:coarse, not width (ui-spec §3.4): the
    // text control stays at the 32px fine-pointer height, and the segmented
    // ordering items keep their 26px visible shape (no ::after hot zone).
    const search = page.getByTestId("home-search");
    expect(Math.round((await search.boundingBox())!.height)).toBe(32);
    expect(Math.round((await page.getByTestId("home-order-clock").boundingBox())!.height)).toBe(26);
    expect(Math.round((await page.getByTestId("spaces-drawer-open").boundingBox())!.height)).toBe(32);
  } finally {
    await context.close();
  }
});

test("soft keyboard keeps the home head and search inside the visible band", async ({
  browser,
}) => {
  const context = await phoneContext(browser);
  const page = await context.newPage();
  try {
    await login(page);
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    const search = page.getByTestId("home-search");

    // Playwright cannot raise the platform keyboard: resize visualViewport the
    // way viewport.ts listens for it, then focus and type as a real iPhone
    // session would.
    await search.click();
    await page.evaluate((nextHeight) => {
      const viewport = window.visualViewport;
      if (!viewport) return;
      Object.defineProperty(viewport, "height", { configurable: true, value: nextHeight });
      Object.defineProperty(viewport, "offsetTop", { configurable: true, value: 0 });
      viewport.dispatchEvent(new Event("resize"));
    }, 323);
    await search.fill("echo e2e");
    await expect(search).toHaveValue("echo e2e");

    const band = await page.evaluate(() => ({
      top: Math.round(window.visualViewport.offsetTop),
      bottom: Math.round(window.visualViewport.offsetTop + window.visualViewport.height),
    }));
    // Measure with getBoundingClientRect directly (Playwright's actionability
    // boundingBox can report null while the shrunken visualViewport has the
    // scrolled-into-view input sitting at its band edge).
    const geometry = await page.evaluate(() => {
      const head = document.querySelector('[data-testid="home-list"] > header');
      const input = document.querySelector<HTMLElement>('[data-testid="home-search"]');
      const headRect = head?.getBoundingClientRect();
      const inputRect = input?.getBoundingClientRect();
      const inputStyle = input ? getComputedStyle(input) : null;
      return {
        headTop: headRect ? Math.round(headRect.top) : null,
        searchTop: inputRect ? Math.round(inputRect.top) : null,
        searchBottom: inputRect ? Math.round(inputRect.bottom) : null,
        searchVisible: inputStyle ? inputStyle.display !== "none" && inputStyle.visibility !== "hidden" : false,
      };
    });
    expect(geometry.searchVisible, "search stays mounted and visible under the keyboard").toBe(true);
    expect(geometry.headTop, "head top at band top").toBeGreaterThanOrEqual(band.top - 0.5);
    expect(geometry.searchBottom, "search inside the visible band").toBeLessThanOrEqual(
      band.bottom + 0.5,
    );
    expect(geometry.searchTop).toBeGreaterThanOrEqual(geometry.headTop! + 52 - 0.5);
  } finally {
    await context.close();
  }
});

test("1440 desktop: /m redirects to /sessions and no phone chrome renders", async ({ browser }) => {
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  try {
    await login(page);
    await page.goto("/m");
    await expect(page).toHaveURL(/\/sessions$/);
    await expect(page.getByTestId("session-list")).toBeVisible();
    await expect(page.getByRole("navigation", { name: "手机底栏" })).toHaveCount(0);
    await page.goto("/m/inbox");
    await expect(page).toHaveURL(/\/sessions$/);

    await shoot(page, "UO-3-sessions-1440-dark.png");
  } finally {
    await context.close();
  }
});

test("both appearances resolve role tokens at 390 and 1440", async ({ browser }) => {
  const context = await phoneContext(browser);
  const desktop = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  try {
    const page = await context.newPage();
    await login(page);
    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    for (const mode of ["dark", "light"] as const) {
      await setMode(page, mode);
      const tokens = await page.evaluate(() => {
        const read = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
        return {
          canvas: read("--bg-canvas"),
          surface: read("--bg-surface"),
          strong: read("--fg-strong"),
          divider: read("--divider"),
        };
      });
      // Both modes resolve real values and differ from each other.
      expect(tokens.canvas).toBeTruthy();
      expect(tokens.surface).toBeTruthy();
      expect(tokens.strong).toBeTruthy();
      if (mode === "dark") {
        expect(tokens.canvas.toLowerCase()).toBe("#232220");
      } else {
        expect(tokens.canvas.toLowerCase()).toBe("#f9f8f5");
      }
      await shoot(page, `UO-3-home-390-${mode}.png`);
    }

    const page1440 = await desktop.newPage();
    await login(page1440);
    await page1440.goto("/sessions");
    await expect(page1440.getByTestId("session-list")).toBeVisible();
    for (const mode of ["dark", "light"] as const) {
      await setMode(page1440, mode);
      const canvas = await page1440.evaluate(() =>
        getComputedStyle(document.documentElement).getPropertyValue("--bg-canvas").trim(),
      );
      expect(canvas.toLowerCase()).toBe(mode === "dark" ? "#232220" : "#f9f8f5");
      await shoot(page1440, `UO-3-sessions-1440-${mode}.png`);
    }
  } finally {
    await cleanup(context).catch(() => undefined);
    await desktop.close();
    await context.close();
  }
});

test("perf: gaining pending bottom-bar interactions never commits HomeList", async ({
  browser,
}) => {
  const context = await phoneContext(browser);
  const page = await context.newPage();
  try {
    await login(page);
    // One blocked session (the create flow parks on a pending approval).
    await createSession(page, "UO-3 perf badge");
    await page.goto("/m?profile=1");
    await expect(page.getByTestId("home-list")).toBeVisible();
    const badge = page.getByTestId("phone-inbox-badge");
    await expect(badge).toHaveText("1", { timeout: 15_000 });
    await expect(page.getByTestId("home-row")).toHaveCount(1);
    // Wait for the live git-branch hydration to finish so its one legitimate
    // commit cannot land inside the badge-only measurement window.
    await expect(page.getByTestId("home-group")).toContainText("feat/workbench-g2", {
      timeout: 15_000,
    });

    // Freeze every data source EXCEPT the interactions list for the window:
    //  - /v1/instances is replayed verbatim from one captured snapshot, so a
    //    refresh only changes the interactions slice;
    //  - the 2.5s row-hydration timers (journal phrases, /screen) are no-oped
    //    on the store instance, so their emits can't enter the window;
    //  - the task ledger polls its own /v1/tasks in a sibling component whose
    //    commits are outside the HomeList subtree the probe wraps.
    const snapshot = await page.evaluate(async () => {
      const response = await fetch("/v1/instances", { credentials: "include" });
      return { status: response.status, body: await response.text() };
    });
    await page.route("**/v1/instances", (route) =>
      route.fulfill({ status: snapshot.status, body: snapshot.body, contentType: "application/json" }),
    );
    await page.evaluate(async () => {
      const { hubStore } = await import("/src/lib/store.ts");
      hubStore.refreshScreens = () => Promise.resolve();
      hubStore.hydrateRowSummaries = () => Promise.resolve();
    });

    type CommitProbeEntry = { kind: string };
    const homeCommits = async () =>
      (
        await page.evaluate(
          () =>
            (window as unknown as { __remudaPerf?: { getReport: () => { probes: CommitProbeEntry[] } } })
              .__remudaPerf?.getReport().probes ?? [],
        )
      ).filter((probe) => probe.kind === "commit:HomeList").length;

    // The interactions list gets `extra` extra pending rows per response.
    let extra = 0;
    await page.route("**/v1/interactions", async (route) => {
      if (route.request().method() !== "GET") {
        await route.continue();
        return;
      }
      const response = await route.fetch();
      const body = (await response.json()) as { items?: Array<Record<string, unknown>> };
      const items = [...(body.items ?? [])];
      const first = items.find((item) => item.state === "pending");
      if (first && extra > 0) {
        for (let n = 0; n < extra; n += 1) {
          const suffix = `_synth_${items.length}_${n}`;
          items.push({
            ...first,
            id: `${String(first.id ?? "int")}${suffix}`,
            interactionId: first.interactionId
              ? `${String(first.interactionId)}${suffix}`
              : undefined,
          });
        }
      }
      await route.fulfill({ response, json: { ...body, items } });
    });

    // Let one post-freeze refresh cycle (and the task ledger poll it crosses)
    // settle so the baseline contains every legitimate current-state commit.
    await page.evaluate(async () => {
      const { hubStore } = await import("/src/lib/store.ts");
      await hubStore.refresh();
    });
    await expect(badge).toHaveText("1");
    await page.waitForTimeout(600);
    const baseline = await homeCommits();
    expect(baseline, "HomeList mounted under the profiler").toBeGreaterThan(0);

    // Badge climbs 1→2→3→4 while the single blocked row is pixel-identical;
    // the cached HomeList slice bails out every time. The 2s poll between
    // driven refreshes is covered too: replay the frozen instance snapshot
    // three extra times and assert the counter never drifts.
    for (const next of [2, 3, 4]) {
      extra = next - 1;
      await page.evaluate(async () => {
        const { hubStore } = await import("/src/lib/store.ts");
        await hubStore.refresh();
      });
      await expect(badge).toHaveText(String(next), { timeout: 5_000 });
      await expect(page.getByTestId("home-row")).toHaveCount(1);
      await page.waitForTimeout(2200);
      expect(await homeCommits(), `badge ${next - 1}→${next} (+ one 2s poll) does not commit HomeList`).toBe(
        baseline,
      );
    }
  } finally {
    await cleanup(context);
    await context.close();
  }
});
