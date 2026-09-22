import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * c-ltboot / UI task 14 (D-053): the stored theme is applied before React
 * mounts (main.tsx calls applyTheme(readTheme()) once at boot), so the
 * choice holds on the FIRST FRAME of every route — the operator no longer
 * has to open the settings page to see a light theme after a reload.
 *
 * Pure front-end behaviour: the assertions only touch localStorage and the
 * <html data-theme> attribute. No fake-node triggers are involved; the
 * login() helper just gives the evidence frames a real rendered workbench.
 */

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

type ThemeTraceEntry = readonly [stage: string, theme: string | null];

/**
 * Seed the theme before any page script runs AND trace what data-theme the
 * first app-painted frame uses. index.html ships the static fallback
 * data-theme="night"; main.tsx must overwrite it before React mounts, so by
 * the time #root first holds rendered content the committed value is the
 * stored choice. (The deferred module graph can finish loading after the
 * browser speculatively paints the empty static shell, so "first frame"
 * here is the first frame that actually shows the Remuda UI — not that
 * pre-script blank frame.)
 */
async function seedTheme(page: Page, value: string): Promise<void> {
  await page.addInitScript((raw) => {
    // Runs at document_start, before any page script — and, in this
    // Chromium, before <html> itself exists, so nothing below may touch
    // document.documentElement synchronously.
    try {
      localStorage.setItem("runtime.theme.v1", raw);
    } catch {
      /* storage unavailable: the boot path must take its default instead */
    }
    const trace: ThemeTraceEntry[] = [];
    const record = (stage: string) =>
      trace.push([stage, document.documentElement.getAttribute("data-theme")] as const);

    const attach = () => {
      const html = document.documentElement;
      if (!html) {
        requestAnimationFrame(attach);
        return;
      }
      record("element-ready");
      new MutationObserver(() => record("attr-change")).observe(html, {
        attributes: true,
        attributeFilter: ["data-theme"],
      });
      // Poll per frame until React has committed into #root, then sample the
      // value that frame is about to paint. Give up after ~10 s so a hung
      // boot fails the test instead of looping forever.
      let frames = 0;
      const probe = () => {
        const root = document.getElementById("root");
        if (root && root.childElementCount > 0) {
          record("app-first-frame");
          return;
        }
        if (frames++ > 600) {
          record("probe-timeout");
          return;
        }
        requestAnimationFrame(probe);
      };
      requestAnimationFrame(probe);
    };
    requestAnimationFrame(attach);
    (window as unknown as { __themeTrace?: ThemeTraceEntry[] }).__themeTrace = trace;
  }, value);
}

async function readTrace(page: Page): Promise<ThemeTraceEntry[]> {
  return page.evaluate(
    () => (window as unknown as { __themeTrace?: ThemeTraceEntry[] }).__themeTrace ?? [],
  );
}

async function assertBootTheme(page: Page, route: string, expected: "night" | "ledger"): Promise<void> {
  // A fresh document load (the reload scenario), never a client navigation.
  await page.goto(route);

  const trace = await readTrace(page);
  const appFrameIdx = trace.findIndex(([stage]) => stage === "app-first-frame");
  expect(appFrameIdx, `${route}: boot committed within the probe window (trace=${JSON.stringify(trace)})`).toBeGreaterThanOrEqual(0);
  // The attribute the parser handed over (before page scripts ran) really is
  // night — so the ledger assertion proves the boot script changed it rather
  // than a default coincidentally matching.
  expect(
    trace.find(([stage]) => stage === "element-ready")?.[1],
    `${route}: static HTML fallback before page scripts (trace=${JSON.stringify(trace)})`,
  ).toBe("night");
  const ledgerSwitchIdx = trace.findIndex(
    ([stage, theme]) => stage === "attr-change" && theme === "ledger",
  );
  if (expected === "ledger") {
    // main.tsx applied the stored choice before React committed the route…
    expect(ledgerSwitchIdx, `${route}: switched to ledger before app paint (trace=${JSON.stringify(trace)})`).toBeGreaterThanOrEqual(0);
    expect(ledgerSwitchIdx).toBeLessThan(appFrameIdx);
  } else {
    // Invalid/missing storage: nothing ever promotes the attribute to ledger.
    expect(ledgerSwitchIdx, `${route}: no ledger switch for an invalid stored value`).toBe(-1);
  }
  // …so the first painted app frame already carries the stored choice.
  expect(trace[appFrameIdx]![1], `${route}: theme on the frame the app first paints`).toBe(expected);
  // And it still holds after the app settled.
  expect(
    await page.evaluate(() => document.documentElement.dataset.theme),
    `${route}: data-theme after load`,
  ).toBe(expected);
}

test.describe("theme applied at boot", () => {
  test("the stored ledger theme is on the first frame of every route, without visiting settings", async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await seedTheme(page, "ledger");
    await login(page);

    for (const route of ["/", "/sessions", "/approvals", "/m"]) {
      await assertBootTheme(page, route, "ledger");
      // The choice is applied app-wide; the settings page is never opened.
      await expect(page, `${route}: settings route never entered`).not.toHaveURL(/\/settings(?:[/?]|$)/);
      await expect(page.getByTestId("settings-page")).toHaveCount(0);
    }
  });

  test("an illegal stored value falls back to night at boot (route-independent, no login)", async ({
    page,
  }) => {
    await seedTheme(page, "midnight");
    // /sessions bounces an unauthenticated load to /login; data-theme is
    // stamped before the router/auth gate runs, so it is asserted regardless
    // of the page the route settles on.
    await assertBootTheme(page, "/sessions", "night");
  });

  test("evidence: ledger workbench on the first frame at 1440 and 390", async ({ page }) => {
    test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture the committed screenshots");
    await page.emulateMedia({ reducedMotion: "reduce" });
    await seedTheme(page, "ledger");
    await page.setViewportSize({ width: 1440, height: 900 });
    await login(page);

    await page.goto("/sessions");
    await expect(page.getByTestId("session-list")).toBeVisible();
    await mkdir(shotDir, { recursive: true });
    await page.screenshot({
      path: path.join(shotDir, "ui-upgrade-14-sessions-1440.png"),
      animations: "disabled",
    });

    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/m");
    await expect(page).toHaveURL(/\/m$/);
    await expect(page.getByTestId("home-list")).toBeVisible();
    await page.screenshot({
      path: path.join(shotDir, "ui-upgrade-14-home-390.png"),
      animations: "disabled",
    });
  });
});
