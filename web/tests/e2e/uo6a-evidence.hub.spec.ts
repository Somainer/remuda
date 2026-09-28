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
 *    band (129px) and the card area >= 44px, with the composer in the band;
 *  - an ended session mounts the EndedBar in the composer's place;
 *  - an idle session page does not commit (commit:SessionPage under
 *    ?profile=1 stays at zero across a quiet window — a count, not a time).
 *
 * Evidence (REMUDA_EVIDENCE=1 only): live / idle / ended / ⋯ open / run
 * details open at 1440, 768 and 390, the header at 360 and 320, dark and
 * light, plus the keyboard-open band at 390. Shots go to REMUDA_SHOT_DIR
 * (default test-results/evidence).
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = process.env.REMUDA_SHOT_DIR ?? path.join(here, "../../test-results/evidence");

const created: string[] = [];

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

/** A fake-node claude session; "exit agent" prompts end on their own. */
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
  const id = new URL(page.url()).pathname.split("/")[2]!;
  created.push(id);
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
    // The fake node parks a new session on a pending approval.
    await expect(page.getByTestId("approval-card")).toBeVisible({ timeout: 20_000 });
    await expect(page.getByTestId("pending-area")).toBeVisible();

    await raiseKeyboard(page, 852 - 323);
    await expect(page.getByTestId("live-status-strip")).toBeHidden();
    await expect(page.getByTestId("session-more-open")).toBeVisible();

    const body = await bandBox(page, "session-body");
    const cards = await bandBox(page, "pending-area");
    const input = await bandBox(page, "composer-input");
    const bar = await bandBox(page, "composer-bar");
    expect(body && cards && input && bar, "body, cards and composer mounted").toBeTruthy();
    console.log(
      `UO6A band=323 body=${Math.round(body!.height)} cards=${Math.round(cards!.height)}` +
        ` composer-bar=${Math.round(bar!.height)} bar-overflow=${Math.max(0, Math.round(bar!.bottom - bar!.bandBottom))}`,
    );
    expect(Math.round(body!.band)).toBe(323);
    expect(body!.height, `body ${body!.height}px`).toBeGreaterThanOrEqual(body!.band * 0.4 - 1);
    expect(cards!.height, `cards ${cards!.height}px`).toBeGreaterThanOrEqual(44);
    // The text box being typed into stays in the band. The control bar under
    // it is the compact composer's own row (D-042 budgets it at 56px); its
    // height is logged above, not asserted here.
    expect(input!.top).toBeGreaterThanOrEqual(input!.bandTop - 1);
    expect(input!.bottom).toBeLessThanOrEqual(input!.bandBottom + 1);
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

  test("an idle session page does not commit", async ({ page }) => {
    // Perf only: an idle page must not commit on a clock. The store's
    // periodic refresh emits are logged next to the commits so a commit can
    // be traced to its source.
    test.skip(process.env.REMUDA_PERF !== "1", "set REMUDA_PERF=1 for the idle commit probe");
    const id = await createSession(page, "UO6A_IDLE mhome-exit agent");
    await page.goto(`/s/${id}/structured?profile=1`);
    await expect(page.getByTestId("session-page")).toHaveAttribute("data-status", "exited", {
      timeout: 20_000,
    });
    await expect(page.getByTestId("ended-bar")).toBeVisible();
    const commitTimes = () =>
      page.evaluate(() =>
        (
          (window as unknown as { __remudaPerf?: { getReport: () => { probes: { kind: string; at: number }[] } } })
            .__remudaPerf?.getReport().probes ?? []
        )
          .filter((probe) => probe.kind === "commit:SessionPage")
          .map((probe) => Math.round(probe.at)),
      );
    // Let the mount and the first journal/hydration round land first.
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
    const before = (await commitTimes()).length;
    await page.waitForTimeout(3_000);
    const after = await commitTimes();
    const emits = await page.evaluate(() => (window as unknown as { __uo6aEmits: string[] }).__uo6aEmits);
    console.log(
      `UO6A idle commit:SessionPage window=3s commits=${after.length - before}`,
      `at=${after.slice(before).join(",")} emits=${emits.join(" ")}`,
    );
    expect(after.length - before).toBe(0);
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
    const live = await createSession(page, `uo6a evidence live ${width}`);
    ids.push(live);
    await page.goto(`/s/${live}/structured`);
    await expect(page.getByTestId("approval-card")).toBeVisible({ timeout: 20_000 });
    await shot(page, `uo6a-live-${width}-${scheme}.png`);
    if (phone) {
      for (const narrow of [360, 320]) {
        await page.setViewportSize({ width: narrow, height: 844 });
        await shot(page, `uo6a-live-${narrow}-${scheme}.png`);
      }
      await page.setViewportSize({ width, height: 844 });
    }
    await page.getByTestId("session-more-open").click();
    await shot(page, `uo6a-more-open-${width}-${scheme}.png`);
    await page.getByTestId("run-details-summary").click();
    await expect(page.getByTestId("run-details")).toBeVisible();
    await shot(page, `uo6a-run-details-${width}-${scheme}.png`);
    // Run details persist per device; close them again for the next screens.
    await page.getByTestId("session-more-open").click();
    await page.getByTestId("run-details-summary").click();

    await clearApprovals(page, live);
    await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
    await shot(page, `uo6a-idle-${width}-${scheme}.png`);

    const ended = await createSession(page, `UO6A_SHOT mhome-exit agent ${width}`);
    ids.push(ended);
    await page.goto(`/s/${ended}/structured`);
    await expect(page.getByTestId("ended-bar")).toBeVisible({ timeout: 20_000 });
    await shot(page, `uo6a-ended-${width}-${scheme}.png`);
  } finally {
    for (const id of ids) await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    await context.close();
  }
}

test("evidence shots: live / idle / ended / ⋯ / run details", async ({ browser }) => {
  test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture screenshots");
  test.setTimeout(480_000);
  for (const width of [1440, 768, 390])
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
      const context = await webkit.newContext({
        ...playwright.devices["iPhone 15"],
        baseURL: test.info().project.use.baseURL,
        colorScheme: scheme,
      });
      const page = await context.newPage();
      let id = "";
      try {
        await login(page);
        id = await createSession(page, "uo6a evidence keyboard");
        await page.goto(`/s/${id}/structured`);
        await expect(page.getByTestId("approval-card")).toBeVisible({ timeout: 20_000 });
        const { height } = page.viewportSize()!;
        await raiseKeyboard(page, height - 323);
        await shot(page, `uo6a-keyboard-390-${scheme}.png`);
      } finally {
        if (id) await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
        await context.close();
      }
    }
  } finally {
    await webkit.close();
  }
});
