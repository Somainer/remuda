import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * m-sessionfold (D-049, ui-spec §1.3 / §4.7): the compact /s/:id* chrome
 * budget — exactly one app-level top bar and one bottom strip, the body at
 * least 60% of the viewport, no capability loss.
 *
 * 390px (structured and tty): the SpaceTabs row, the full spaces strip and
 * the app bottom navigation bar do not render; the header space chip still
 * opens spaces-drawer-open; view-switch + Stop stay in the header owning
 * their 44px hit-zone corners (same geometry-probe style as
 * ux-touchhit.hub.spec.ts); the transcript action chips sit behind the
 * ⋯ trigger until expanded, testids unchanged.
 *
 * 1440px: regression only — tabs, inline chips, host chip all still there.
 */
test.describe.configure({ mode: "serial" });

const TOUCH = 44;
const HALF = TOUCH / 2;
/** ui-spec §4.7: body >= 60% of the viewport with the composer collapsed. */
const MIN_RATIO = 0.6;

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

const created: string[] = [];

async function shot(page: Page, name: string) {
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function resolveHost(page: Page): Promise<string> {
  const response = await page.request.get("/v1/hosts");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { items?: { id?: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  expect(host?.id, "fake node host").toBeTruthy();
  return host!.id!;
}

async function createInstance(page: Page, kind: "claude" | "terminal"): Promise<string> {
  const hostId = await resolveHost(page);
  const response = await page.request.post("/v1/instances", {
    data: { hostId, workspaceId: "wsp_e2e", kind, driver: "shell-pty", prompt: "m chrome body budget" },
  });
  expect(response.ok(), `create ${kind} instance: ${response.status()} ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId?: string; id?: string } };
  const id = body.instance.instanceId ?? body.instance.id;
  expect(id).toBeTruthy();
  created.push(id!);
  return id!;
}

/**
 * The terminal kind through the new-session sheet (same flow as
 * ux-ttymode.hub.spec.ts): the fake harness keeps the instance in
 * "requested/starting" while the tty lab attaches and paints anyway.
 */
async function createTerminalViaSheet(page: Page): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await hostPicker.selectOption(hostId!);
  await page.getByTestId("new-session-kind-terminal").click();
  await expect(page.getByTestId("new-session-start")).toBeEnabled();
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/")[2]!;
  created.push(id);
  return id;
}

/** Resolve pending launch approvals until none remain SERVER-SIDE AND the
 *  ApprovalCard has unmounted in the UI: the fake harness can raise a second
 *  approval right after the first answer, and a single zero reading races
 *  it. The 60% body budget is measured with the composer collapsed and no
 *  interaction surface in the dock. */
async function clearApprovals(page: Page, instanceId: string) {
  const deadline = Date.now() + 20_000;
  for (;;) {
    const pending = await page.evaluate(async (id) => {
      const res = await fetch("/v1/interactions", { credentials: "include" });
      const items = ((await res.json()).items ?? []) as {
        instanceId?: string;
        state?: string;
        id: string;
        interactionId?: string;
        request?: { inputDigest?: string; options?: { id: string }[] };
      }[];
      const mine = items.filter((item) => item.instanceId === id && item.state === "pending");
      await Promise.all(
        mine.map(async (item) => {
          const optionId = item.request?.options?.[0]?.id;
          if (!optionId) return;
          await fetch(`/v1/interactions/${item.interactionId ?? item.id}/answer`, {
            method: "POST",
            credentials: "include",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({
              answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" },
            }),
          });
        }),
      );
      return mine.length;
    }, instanceId);
    const cards = await page.getByTestId("approval-card").count();
    if (pending === 0 && cards === 0) break;
    expect(Date.now() < deadline, "approvals clear within 20s").toBe(true);
    await page.waitForTimeout(300);
  }
}

async function forceDeleteAllInstances(page: Page) {
  await page.evaluate(async () => {
    const body = (await (await fetch("/v1/instances", { credentials: "include" })).json()) as {
      items?: { instanceId?: string; id?: string; lifecycle?: string }[];
    };
    await Promise.all(
      (body.items ?? [])
        .filter((i) => i.lifecycle !== "exited" && i.lifecycle !== "failed" && i.lifecycle !== "closed")
        .map((i) =>
          fetch(`/v1/instances/${i.instanceId ?? i.id}?force=1`, {
            method: "DELETE",
            credentials: "include",
          }).catch(() => undefined),
        ),
    );
  });
}

async function patchCap(page: Page, value: number): Promise<number> {
  const response = await page.request.get("/v1/hosts");
  const body = (await response.json()) as {
    items?: { id?: string; maxInstances?: number; label?: string }[];
  };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  const previous = host?.maxInstances ?? 8;
  if (host?.id && previous < value) {
    await page.request.patch(`/v1/hosts/${host.id}`, { data: { maxInstances: value } });
  }
  return previous;
}

type Ratio = { top: number; height: number; viewport: number; ratio: number };

async function measure(page: Page, selector: string, label: string): Promise<Ratio> {
  const result = await page.evaluate((sel) => {
    const el = document.querySelector<HTMLElement>(sel);
    if (!el) throw new Error(`measure target missing: ${sel}`);
    const rect = el.getBoundingClientRect();
    if (rect.height === 0) throw new Error(`measure target zero-height: ${sel}`);
    return {
      top: rect.top,
      height: rect.height,
      viewport: window.innerHeight,
      ratio: rect.height / window.innerHeight,
    };
  }, selector);
  console.log(
    `MCHROME ${label} top=${Math.round(result.top)} height=${Math.round(result.height)} vh=${result.viewport} ratio=${result.ratio.toFixed(3)}`,
  );
  return result;
}

/** The compact session route renders none of the folded-away app chrome. */
async function assertFoldedChromeAbsent(page: Page) {
  // Exactly one APP-LEVEL top bar — the session header. (The tty view has
  // its own view-level toolbar <header>, which is not app chrome.)
  await expect(page.locator("[data-testid='session-page'] > header")).toHaveCount(1);
  await expect(page.getByTestId("space-tabs")).toHaveCount(0);
  await expect(page.getByRole("navigation", { name: "手机底栏" })).toHaveCount(0);
  // The strip variant (a div.chips row of per-space buttons) is gone; the
  // D-040 header chip keeps the wrapper testid as a <span>.
  await expect(page.locator("div[data-testid='spaces-chips']")).toHaveCount(0);
  await expect(page.getByTestId("space-chip")).toHaveCount(0);
  const headerChips = page.locator("header").locator("span[data-testid='spaces-chips']");
  await expect(headerChips).toHaveCount(1);
  await expect(headerChips).toBeVisible();
}

async function mark(locator: Locator, owner: string) {
  await locator.evaluate((el, value) => el.setAttribute("data-mchrome-owner", value), owner);
}

/**
 * Geometry probe (ux-touchhit style): the control's visible box stays inside
 * the viewport and elementFromPoint at all four corners of a centred 44×44
 * hit zone resolves to the control itself. Bounding-box width alone cannot
 * catch a covered hot zone or a missing ::after.
 */
async function assertHitTarget(page: Page, target: Locator, owner: string): Promise<void> {
  await expect(target).toBeVisible();
  await mark(target, owner);
  await page.evaluate(({ owner: name, half }) => {
    const el = document.querySelector<HTMLElement>(`[data-mchrome-owner="${name}"]`);
    if (!el) throw new Error(`${name}: marker missing`);
    const rect = el.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) throw new Error(`${name}: zero-size box`);
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const inside = (x: number, y: number) => x >= 0 && y >= 0 && x <= vw && y <= vh;
    if (!inside(rect.left, rect.top) || !inside(rect.right, rect.bottom)) {
      throw new Error(`${name}: visual box outside ${vw}x${vh}`);
    }
    const cx = rect.left + rect.width / 2;
    const cy = rect.top + rect.height / 2;
    for (const [corner, x, y] of [
      ["tl", cx - half + 0.5, cy - half + 0.5],
      ["tr", cx + half - 0.5, cy - half + 0.5],
      ["bl", cx - half + 0.5, cy + half - 0.5],
      ["br", cx + half - 0.5, cy + half - 0.5],
    ] as const) {
      if (!inside(x, y)) throw new Error(`${name}: ${corner} hot corner outside ${vw}x${vh}`);
      const hit = document
        .elementFromPoint(x, y)
        ?.closest("[data-mchrome-owner]")
        ?.getAttribute("data-mchrome-owner");
      if (hit !== name) throw new Error(`${name}: ${corner} resolved to "${hit ?? "none"}"`);
    }
  }, { owner, half: HALF });
}

test.describe("390px compact session route", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test.beforeAll(async ({ browser }) => {
    const setup = await browser.newPage();
    await login(setup);
    await patchCap(setup, 24);
    await setup.close();
  });

  test.afterAll(async ({ browser }) => {
    const cleanup = await browser.newPage();
    await login(cleanup);
    await forceDeleteAllInstances(cleanup).catch(() => undefined);
    await patchCap(cleanup, 8).catch(() => undefined);
    await cleanup.close();
  });

  test("structured: one top bar, no tabs/app-bottom-bar, body >= 60%, switch+Stop keep 44px hits, toolbar folds", async ({
    page,
  }) => {
    const instanceId = await createInstance(page, "claude");
    await page.goto(`/s/${instanceId}/structured`);
    const sessionPage = page.getByTestId("session-page");
    await expect(sessionPage).toHaveAttribute("data-view", "structured");
    await expect(sessionPage).toHaveAttribute("data-lifecycle", "running", { timeout: 20_000 });
    await expect(page.getByTestId("composer")).toBeVisible();
    await clearApprovals(page, instanceId);

    await assertFoldedChromeAbsent(page);

    // Capability preserved: the header chip opens the same spaces drawer.
    await page.getByTestId("spaces-drawer-open").click();
    await expect(page.getByTestId("spaces-drawer")).toBeVisible();
    await page.getByRole("button", { name: "关闭空间面板" }).click();
    await expect(page.getByTestId("spaces-drawer")).toHaveCount(0);

    // view-switch + Stop stay in the header with full 44px hit geometry.
    await assertHitTarget(page, page.getByTestId("view-switch-tty"), "seg-tty");
    await assertHitTarget(page, page.getByTestId("view-switch-structured"), "seg-struct");
    await assertHitTarget(page, page.getByRole("button", { name: "Stop" }), "stop");

    // D-049 transcript fold: trigger only, chips unmounted; expand → same
    // components with the same testids.
    const tools = page.getByTestId("transcript-tools-open");
    await expect(tools).toBeVisible();
    await expect(page.getByTestId("collapse-all")).toHaveCount(0);
    await expect(page.getByTestId("transcript-search-open")).toHaveCount(0);
    await tools.click();
    await expect(page.getByTestId("collapse-all")).toBeVisible();
    await page.getByTestId("transcript-search-open").click();
    await expect(page.getByTestId("transcript-search-input")).toBeVisible();

    // No horizontal overflow at the folded width.
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390);

    // Reopen the chips for a clean (pre-search) evidence shot.
    await page.getByTestId("transcript-search-close").click();
    await page.emulateMedia({ reducedMotion: "reduce" });
    await shot(page, "mobile-ui-5-structured-390.png");

    const measured = await measure(page, '[data-testid="session-body"]', "structured session-body");
    expect(measured.ratio, "structured body ratio >= 0.60").toBeGreaterThanOrEqual(MIN_RATIO);
  });

  test("tty: same folded chrome, xterm container >= 60%, switch stays in the header", async ({ page }) => {
    const instanceId = await createTerminalViaSheet(page);
    await page.goto(`/s/${instanceId}/tty`);
    const sessionPage = page.getByTestId("session-page");
    await expect(sessionPage).toHaveAttribute("data-view", "tty");
    // The harness tty lab attaches while lifecycle stays "requested"; xterm
    // readiness, not lifecycle, gates the geometry measurement.
    await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 20_000 });
    await expect(page.locator(".xterm")).toBeVisible();

    await assertFoldedChromeAbsent(page);
    await assertHitTarget(page, page.getByTestId("view-switch-tty"), "seg-tty");
    await assertHitTarget(page, page.getByTestId("view-switch-structured"), "seg-struct");
    await assertHitTarget(page, page.getByRole("button", { name: "Stop" }), "stop");

    await page.emulateMedia({ reducedMotion: "reduce" });
    await shot(page, "mobile-ui-5-terminal-390.png");

    // The xterm container is the terminal "body"; same 60% budget.
    const xterm = await measure(page, ".xterm", "tty xterm");
    expect(xterm.ratio, "xterm container ratio >= 0.60").toBeGreaterThanOrEqual(MIN_RATIO);
    // session-body wraps it and must clear the budget itself too.
    const body = await measure(page, '[data-testid="session-body"]', "tty session-body");
    expect(body.ratio).toBeGreaterThanOrEqual(MIN_RATIO);
  });
});

test.describe("1440px desktop regression", () => {
  test.beforeEach(async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test.afterAll(async ({ browser }) => {
    const cleanup = await browser.newPage();
    await cleanup.setViewportSize({ width: 1440, height: 900 });
    await login(cleanup);
    await forceDeleteAllInstances(cleanup).catch(() => undefined);
    await patchCap(cleanup, 8).catch(() => undefined);
    await cleanup.close();
  });

  test("session route keeps tabs, inline transcript chips and the desktop chrome", async ({ page }) => {
    const instanceId = await createInstance(page, "claude");
    await page.goto(`/s/${instanceId}/structured`);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-view", "structured");
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-lifecycle", "running", {
      timeout: 20_000,
    });
    await clearApprovals(page, instanceId);

    // Desktop keeps the tab strip and the mobile phone bar stays display:none.
    await expect(page.getByTestId("space-tabs")).toBeVisible();
    await expect(page.getByRole("navigation", { name: "手机底栏" })).toBeHidden();

    // Toolbar chips inline — no ⋯ fold on desktop; header keeps host chip.
    await expect(page.getByTestId("transcript-tools-open")).toHaveCount(0);
    await expect(page.getByTestId("collapse-all")).toBeVisible();
    await expect(page.getByTestId("transcript-search-open")).toBeVisible();
    await expect(page.getByTestId("session-host")).toBeVisible();

    const measured = await measure(page, '[data-testid="session-body"]', "1440 session-body");
    expect(measured.ratio).toBeGreaterThanOrEqual(MIN_RATIO);
  });
});
