import { expect, test, type Browser, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * UO-6a: the session page skeleton (ui-spec §2.1 header, §2.2 dock, keyboard
 * band).
 *
 * Gated:
 *  - a 323px keyboard band with a pending card keeps the body >= 40% of the
 *    band (129px) and the card area >= 44px, with the WHOLE composer (input
 *    and control bar) inside the band;
 *  - an ended session mounts the EndedBar in the composer's place;
 *  - (REMUDA_PERF=1) a live idle session page body does not commit
 *    (commit:SessionPageBody under ?profile=1 stays at zero across a quiet
 *    window — a count, not a time; descendant commits are logged apart).
 *
 * Evidence (REMUDA_EVIDENCE=1 only): live / idle / ended / ⋯ open / run
 * details open, dark and light, plus the keyboard-open band (WebKit). Inside
 * the repo (test-results/evidence) only 390 and 1440 are written; the extra
 * widths (768, 360, 320) are written only when REMUDA_SHOT_DIR names an
 * external directory, which then receives every shot.
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const externalDir = process.env.REMUDA_SHOT_DIR;
const shotDir = externalDir ?? path.join(here, "../../test-results/evidence");
const IN_REPO_WIDTHS = [390, 1440];

const created: string[] = [];

async function shot(page: Page, name: string, width: number) {
  if (!externalDir && !IN_REPO_WIDTHS.includes(width)) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

/** The transcript has replaced 「加载 snapshot…」. */
async function transcriptReady(page: Page) {
  await expect(page.getByTestId("loading-snapshot")).toHaveCount(0, { timeout: 20_000 });
  await expect(page.getByTestId("transcript")).toBeVisible({ timeout: 20_000 });
}

/**
 * A fake-node claude session; "exit agent" prompts end on their own. The id
 * is registered from the create response at once, so a navigation failure
 * after the create cannot leak the instance.
 */
async function createSession(page: Page, prompt: string, into: string[] = created): Promise<string> {
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
  const creating = page.waitForResponse(
    (response) =>
      response.request().method() === "POST" && new URL(response.url()).pathname === "/v1/instances",
  );
  await page.getByTestId("new-session-start").click();
  const res = await creating;
  const body = await res.text();
  expect(res.ok(), `instance create failed: ${res.status()} ${body}`).toBe(true);
  const id = (JSON.parse(body) as { instance?: { instanceId?: string } }).instance?.instanceId as string;
  expect(id).toBeTruthy();
  into.push(id);
  await expect(page).toHaveURL(new RegExp(`/s/${id}`), { timeout: 20_000 });
  return id;
}

/** Answer every pending approval of one instance until its card unmounts. */
async function clearApprovals(page: Page, instanceId: string) {
  await expect
    .poll(
      async () => {
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
        return pending + (await page.getByTestId("approval-card").count());
      },
      { timeout: 20_000 },
    )
    .toBe(0);
}

/** iOS-exact soft keyboard: the visual viewport shrinks and scrolls by kb. */
async function raiseKeyboard(page: Page, kb: number) {
  await page.evaluate((KB) => {
    const vv = window.visualViewport!;
    const h = window.innerHeight - KB;
    Object.defineProperty(vv, "height", { configurable: true, get: () => h });
    Object.defineProperty(vv, "offsetTop", { configurable: true, get: () => KB });
    window.scrollTo(0, KB);
    vv.dispatchEvent(new Event("resize"));
    vv.dispatchEvent(new Event("scroll"));
    window.dispatchEvent(new Event("resize"));
  }, kb);
  await expect(page.locator("html")).toHaveAttribute("data-keyboard", "1");
}

async function bandBox(page: Page, testid: string) {
  return page.evaluate((id) => {
    const el = document.querySelector(`[data-testid='${id}']`);
    if (!el) return null;
    const r = el.getBoundingClientRect();
    const vv = window.visualViewport!;
    return {
      top: r.top,
      bottom: r.bottom,
      height: r.height,
      bandTop: vv.offsetTop,
      bandBottom: vv.offsetTop + vv.height,
      band: vv.height,
    };
  }, testid);
}

test.afterEach(async ({ page }) => {
  for (const id of created.splice(0)) {
    await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
  }
});

test.describe("390 phone", () => {
  // iPhone 15: 852 tall; a 529px keyboard leaves the 323px band of ui-spec §2.
  test.use({ viewport: { width: 393, height: 852 }, hasTouch: true, isMobile: true });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test("a 323px keyboard band keeps the body >= 40% and the pending-card area >= 44px", async ({ page }) => {
    const id = await createSession(page, "uo6a keyboard band");
    await page.goto(`/s/${id}/structured`);
    await transcriptReady(page);
    // The fake node parks a new session on a pending approval.
    await expect(page.getByTestId("approval-card")).toBeVisible({ timeout: 20_000 });
    await expect(page.getByTestId("pending-area")).toBeVisible();

    await raiseKeyboard(page, 852 - 323);
    await expect(page.getByTestId("live-status-strip")).toBeHidden();
    await expect(page.getByTestId("session-more-open")).toBeVisible();

    const body = await bandBox(page, "session-body");
    const cards = await bandBox(page, "pending-area");
    const composer = await bandBox(page, "composer");
    const bar = await bandBox(page, "composer-bar");
    expect(body && cards && composer && bar, "body, cards and composer mounted").toBeTruthy();
    console.log(
      `UO6A band=323 body=${Math.round(body!.height)} cards=${Math.round(cards!.height)}` +
        ` composer=${Math.round(composer!.top)}..${Math.round(composer!.bottom)} (${Math.round(composer!.height)})` +
        ` composer-bar=${Math.round(bar!.height)} band=${Math.round(body!.bandTop)}..${Math.round(body!.bandBottom)}`,
    );
    expect(Math.round(body!.band)).toBe(323);
    expect(body!.height, `body ${body!.height}px`).toBeGreaterThanOrEqual(body!.band * 0.4 - 1);
    expect(cards!.height, `cards ${cards!.height}px`).toBeGreaterThanOrEqual(44);
    // The whole composer — input AND control bar — sits inside the band.
    expect(composer!.top, "composer top in the band").toBeGreaterThanOrEqual(composer!.bandTop - 1);
    expect(composer!.bottom, "composer bottom in the band").toBeLessThanOrEqual(composer!.bandBottom + 1);
    expect(bar!.bottom, "control bar bottom in the band").toBeLessThanOrEqual(bar!.bandBottom + 1);
    // One row: the bar is no taller than its tallest (44px) control.
    expect(bar!.height, `composer-bar ${bar!.height}px`).toBeLessThanOrEqual(45);
  });

  test("an ended session swaps the composer for the EndedBar with Resume", async ({ page }) => {
    const id = await createSession(page, "UO6A_ENDED mhome-exit agent");
    await page.goto(`/s/${id}/structured`);
    const sessionPage = page.getByTestId("session-page");
    await expect(sessionPage).toHaveAttribute("data-status", "exited", { timeout: 20_000 });
    await expect(page.getByTestId("ended-bar")).toBeVisible();
    await expect(page.getByTestId("resume-control")).toBeVisible();
    await expect(page.getByTestId("composer")).toHaveCount(0);
  });

  test("a live idle session page body does not commit", async ({ page }) => {
    // Perf only: an idle page body must not commit on a clock or on the
    // store's periodic refresh. commit:SessionPageBody is the page function
    // itself; commit:SessionPage is the Profiler over the whole subtree
    // (LiveStatusStrip's clock and the like), logged apart. Store emits are
    // logged next to them so a commit can be traced to its source.
    test.skip(process.env.REMUDA_PERF !== "1", "set REMUDA_PERF=1 for the idle commit probe");
    const id = await createSession(page, "UO6A_IDLE live idle");
    await page.goto(`/s/${id}/structured?profile=1`);
    await transcriptReady(page);
    await clearApprovals(page, id);
    await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
    await expect(page.getByTestId("session-page")).not.toHaveAttribute("data-status", "exited");
    const commitTimes = (kind: string) =>
      page.evaluate(
        (k) =>
          (
            (window as unknown as { __remudaPerf?: { getReport: () => { probes: { kind: string; at: number }[] } } })
              .__remudaPerf?.getReport().probes ?? []
          )
            .filter((probe) => probe.kind === k)
            .map((probe) => Math.round(probe.at)),
        kind,
      );
    // Let the approval answer and its journal round land first.
    await page.waitForTimeout(2_000);
    await page.evaluate(async () => {
      const { hubStore } = await import("/src/lib/store.ts");
      const store = hubStore as unknown as { emit: (patch: object) => void };
      const w = window as unknown as { __uo6aEmits: string[] };
      w.__uo6aEmits = [];
      const emit = store.emit.bind(hubStore);
      store.emit = (patch: object) => {
        w.__uo6aEmits.push(`${Math.round(performance.now())}:${Object.keys(patch).join(",")}`);
        emit(patch);
      };
    });
    const bodyBefore = (await commitTimes("commit:SessionPageBody")).length;
    const treeBefore = (await commitTimes("commit:SessionPage")).length;
    await page.waitForTimeout(3_000);
    const body = await commitTimes("commit:SessionPageBody");
    const tree = await commitTimes("commit:SessionPage");
    const emits = await page.evaluate(() => (window as unknown as { __uo6aEmits: string[] }).__uo6aEmits);
    await expect(page.getByTestId("session-page")).not.toHaveAttribute("data-status", "exited");
    console.log(
      `UO6A idle live window=3s body-commits=${body.length - bodyBefore} at=${body.slice(bodyBefore).join(",")}` +
        ` subtree-commits=${tree.length - treeBefore} at=${tree.slice(treeBefore).join(",")}` +
        ` emits=${emits.length} ${emits.join(" ")}`,
    );
    expect(body.length - bodyBefore).toBe(0);
  });
});

test.describe("1440 desktop", () => {
  test.use({ viewport: { width: 1440, height: 900 } });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  /** A dock child's box against the transcript's reading column. */
  async function columnFit(page: Page, testid: string) {
    return page.evaluate((id) => {
      const el = document.querySelector(`[data-testid='${id}']`)!.getBoundingClientRect();
      const list = document.querySelector("[data-testid='transcript-scroller']")!.getBoundingClientRect();
      return { width: el.width, centre: el.left + el.width / 2, column: list.left + list.width / 2 };
    }, testid);
  }

  test("dock children and the EndedBar follow the centred reading column", async ({ page }) => {
    // ui-spec §4.2: width min(100% - 2 gutter, --read-measure) = 720 at 1440.
    const live = await createSession(page, "uo6a column live");
    await page.goto(`/s/${live}/structured`);
    await transcriptReady(page);
    await expect(page.getByTestId("approval-card")).toBeVisible({ timeout: 20_000 });
    for (const id of ["composer", "pending-area"]) {
      const fit = await columnFit(page, id);
      expect(Math.round(fit.width), `${id} width`).toBe(720);
      expect(Math.abs(fit.centre - fit.column), `${id} centred`).toBeLessThanOrEqual(1);
    }

    const ended = await createSession(page, "UO6A_COLUMN mhome-exit agent");
    await page.goto(`/s/${ended}/structured`);
    await transcriptReady(page);
    await expect(page.getByTestId("ended-bar")).toBeVisible({ timeout: 20_000 });
    const fit = await columnFit(page, "ended-bar");
    expect(Math.round(fit.width), "ended-bar width").toBe(720);
    expect(Math.abs(fit.centre - fit.column), "ended-bar centred").toBeLessThanOrEqual(1);
  });
});

/** Evidence: every screen at one width and scheme. */
async function captureScreens(browser: Browser, width: number, scheme: "dark" | "light") {
  const phone = width < 768;
  const context = await browser.newContext({
    baseURL: test.info().project.use.baseURL,
    viewport: { width, height: phone ? 844 : width >= 1440 ? 900 : 1024 },
    colorScheme: scheme,
    hasTouch: phone,
    isMobile: phone,
  });
  const page = await context.newPage();
  const ids: string[] = [];
  try {
    await login(page);
    const live = await createSession(page, `uo6a evidence live ${width}`, ids);
    await page.goto(`/s/${live}/structured`);
    await transcriptReady(page);
    await expect(page.getByTestId("approval-card")).toBeVisible({ timeout: 20_000 });
    await shot(page, `uo6a-live-${width}-${scheme}.png`, width);
    if (phone && externalDir) {
      for (const narrow of [360, 320]) {
        await page.setViewportSize({ width: narrow, height: 844 });
        await shot(page, `uo6a-live-${narrow}-${scheme}.png`, narrow);
      }
      await page.setViewportSize({ width, height: 844 });
    }
    await page.getByTestId("session-more-open").click();
    await shot(page, `uo6a-more-open-${width}-${scheme}.png`, width);
    await page.getByTestId("run-details-summary").click();
    await expect(page.getByTestId("run-details")).toBeVisible();
    await shot(page, `uo6a-run-details-${width}-${scheme}.png`, width);
    // Run details persist per device; close them again for the next screens.
    await page.getByTestId("session-more-open").click();
    await page.getByTestId("run-details-summary").click();

    await clearApprovals(page, live);
    await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
    await shot(page, `uo6a-idle-${width}-${scheme}.png`, width);

    const ended = await createSession(page, `UO6A_SHOT mhome-exit agent ${width}`, ids);
    await page.goto(`/s/${ended}/structured`);
    await transcriptReady(page);
    await expect(page.getByTestId("ended-bar")).toBeVisible({ timeout: 20_000 });
    await shot(page, `uo6a-ended-${width}-${scheme}.png`, width);
  } finally {
    for (const id of ids) await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    await context.close();
  }
}

test("evidence shots: live / idle / ended / ⋯ / run details", async ({ browser }) => {
  test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture screenshots");
  test.setTimeout(480_000);
  // 768 (and 360/320 inside the 390 pass) only reach an external directory.
  const widths = externalDir ? [1440, 768, 390] : IN_REPO_WIDTHS;
  for (const width of widths)
    for (const scheme of ["dark", "light"] as const) await captureScreens(browser, width, scheme);
});

test("evidence shots: keyboard band at 390 (WebKit iPhone)", async ({ playwright }) => {
  test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture screenshots");
  test.setTimeout(180_000);
  let webkit: Browser;
  try {
    webkit = await playwright.webkit.launch();
  } catch {
    test.skip(true, "WebKit is not installed here");
    return;
  }
  try {
    for (const scheme of ["dark", "light"] as const) {
      // The iPhone descriptor (WebKit, touch, DPR 3) at a forced 390x844 so
      // the shot is the 390 width it is named for.
      const context = await webkit.newContext({
        ...playwright.devices["iPhone 15"],
        viewport: { width: 390, height: 844 },
        baseURL: test.info().project.use.baseURL,
        colorScheme: scheme,
      });
      const page = await context.newPage();
      const ids: string[] = [];
      try {
        await login(page);
        const id = await createSession(page, "uo6a evidence keyboard", ids);
        await page.goto(`/s/${id}/structured`);
        await transcriptReady(page);
        await expect(page.getByTestId("approval-card")).toBeVisible({ timeout: 20_000 });
        await raiseKeyboard(page, 844 - 323);
        await shot(page, `uo6a-keyboard-390-${scheme}.png`, 390);
      } finally {
        for (const id of ids) await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
        await context.close();
      }
    }
  } finally {
    await webkit.close();
  }
});
