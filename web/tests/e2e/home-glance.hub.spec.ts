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

/** A session the fake Node parks on a pending approval (m-home's fixture). */
async function createBlockedSession(page: Page): Promise<string> {
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
  await page.getByTestId("new-session-prompt").fill("HGLANCE_BLOCKED mhome-blocked approval gate");
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
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

      // ② is a 28px row under the head, scrolling with the page; every host is
      // online, so it is the one summary chip.
      const strip = page.getByTestId("host-strip");
      await expect(strip).toBeVisible();
      await expect(page.getByTestId("host-summary")).toContainText("主机在线 ›");
      await expect(page.getByTestId("host-chip")).toHaveCount(0);
      await expect(strip).not.toContainText("★");
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

      await setMode(page, "light");
      await shot(page, "home-glance-390-light.png");
      await setMode(page, "dark");
      await shot(page, "home-glance-390-dark.png");
    } finally {
      // The suite shares one fake Node and its placement slots: release ours.
      for (const id of created.splice(0)) {
        await context.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
      }
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
      await expect(page.getByTestId("spaces-panel")).toHaveCount(0);
      await toggle.click();
      await expect(page.getByTestId("spaces-panel")).toBeVisible();
      await expect(toggle).toHaveAttribute("aria-expanded", "true");
      await shot(page, "home-glance-1280-index.png");
      // The same toggle closes it, and the remembered state is per device.
      await toggle.click();
      await expect(page.getByTestId("spaces-panel")).toHaveCount(0);
      await page.reload();
      await expect(page.getByTestId("session-list")).toBeVisible({ timeout: 20_000 });
      await expect(page.getByTestId("spaces-panel")).toHaveCount(0);

      await setMode(page, "light");
      await shot(page, "home-glance-1280-light.png");
      await setMode(page, "dark");
      await shot(page, "home-glance-1280-dark.png");
    } finally {
      await context.close();
    }
  });
});
