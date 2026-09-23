import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { seedMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * Appearance on the first frame (visual-system.md §1; c-ltboot / UI task 14).
 *
 * main.tsx calls applyAppearance(readAppearance()) once before React mounts:
 * an explicit stored dark/light choice becomes `data-appearance` on <html>;
 * system (or no stored value) leaves the attribute off and lets the
 * `prefers-color-scheme` block in tokens.css paint. Either way the first
 * frame the app paints is already in the right mode, and nothing flips the
 * attribute afterwards.
 *
 * Pure front-end behaviour: the assertions only touch localStorage, the
 * attribute and computed colours. login() just gives the evidence frames a
 * real rendered workbench.
 */

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

/** [stage, data-appearance, computed <html> background]. */
type TraceEntry = readonly [stage: string, appearance: string | null, background: string];

const LIGHT_CANVAS = "rgb(249, 248, 245)"; // #f9f8f5
const DARK_CANVAS = "rgb(35, 34, 32)"; // #232220

/**
 * Trace what the first app-painted frame uses. The deferred module graph can
 * finish loading after the browser speculatively paints the empty static
 * shell, so "first frame" is the first frame that shows the Remuda UI — not
 * that pre-script blank frame. Every later attribute change is recorded too.
 */
async function traceBoot(page: Page): Promise<void> {
  await page.addInitScript(() => {
    // Runs at document_start, before <html> may exist: nothing below touches
    // document.documentElement synchronously.
    const trace: TraceEntry[] = [];
    const record = (stage: string) => {
      const html = document.documentElement;
      trace.push([stage, html.getAttribute("data-appearance"), getComputedStyle(html).backgroundColor] as const);
    };
    const attach = () => {
      const html = document.documentElement;
      if (!html) {
        requestAnimationFrame(attach);
        return;
      }
      record("element-ready");
      new MutationObserver(() => record("attr-change")).observe(html, {
        attributes: true,
        attributeFilter: ["data-appearance"],
      });
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
    (window as unknown as { __bootTrace?: TraceEntry[] }).__bootTrace = trace;
  });
}

async function readTrace(page: Page): Promise<TraceEntry[]> {
  return page.evaluate(() => (window as unknown as { __bootTrace?: TraceEntry[] }).__bootTrace ?? []);
}

async function assertBoot(
  page: Page,
  route: string,
  expected: { appearance: "light" | "dark" | null; background: string },
): Promise<void> {
  // A fresh document load (the reload scenario), never a client navigation.
  await page.goto(route);
  await page.waitForFunction(() =>
    ((window as unknown as { __bootTrace?: TraceEntry[] }).__bootTrace ?? []).some(([s]) => s === "app-first-frame"),
  );
  // Let the app settle: route effects, auth, the settings mount effect.
  await page.waitForTimeout(500);

  const trace = await readTrace(page);
  const dump = JSON.stringify(trace);
  const first = trace.findIndex(([stage]) => stage === "app-first-frame");
  expect(first, `${route}: boot committed within the probe window (trace=${dump})`).toBeGreaterThanOrEqual(0);
  // The static HTML carries no mode attribute; the boot script sets it.
  expect(trace.find(([stage]) => stage === "element-ready")?.[1], `${route}: static HTML (trace=${dump})`).toBeNull();

  const [, appearance, background] = trace[first]!;
  expect(appearance, `${route}: data-appearance on the first app frame (trace=${dump})`).toBe(expected.appearance);
  expect(background, `${route}: <html> background on the first app frame`).toBe(expected.background);

  // Nothing flips it after the first frame.
  const later = trace.slice(first + 1).filter(([stage]) => stage === "attr-change");
  expect(later, `${route}: attribute changes after the first frame (trace=${dump})`).toEqual([]);
  expect(await page.evaluate(() => document.documentElement.getAttribute("data-appearance"))).toBe(expected.appearance);
}

test.describe("appearance applied at boot", () => {
  for (const stored of ["light", "ledger"] as const) {
    test(`a stored ${stored} is light on the first frame of every route, without visiting settings`, async ({
      page,
    }) => {
      await page.emulateMedia({ colorScheme: "dark" });
      await page.setViewportSize({ width: 1440, height: 900 });
      await seedMode(page, stored);
      await traceBoot(page);
      await login(page);

      for (const route of ["/", "/sessions", "/approvals", "/m"]) {
        await assertBoot(page, route, { appearance: "light", background: LIGHT_CANVAS });
        await expect(page, `${route}: settings route never entered`).not.toHaveURL(/\/settings(?:[/?]|$)/);
        await expect(page.getByTestId("settings-page")).toHaveCount(0);
      }
    });
  }

  test("a stored dark wins over a light system", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "light" });
    await seedMode(page, "dark");
    await traceBoot(page);
    await assertBoot(page, "/sessions", { appearance: "dark", background: DARK_CANVAS });
  });

  test("system mode on a light system paints the light canvas with no attribute", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "light" });
    await seedMode(page, "system");
    await traceBoot(page);
    await assertBoot(page, "/sessions", { appearance: null, background: LIGHT_CANVAS });
  });

  test("no stored value follows the system: a dark system paints dark", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "dark" });
    await traceBoot(page);
    await assertBoot(page, "/sessions", { appearance: null, background: DARK_CANVAS });
  });

  test("an illegal stored value falls back to system (route-independent, no login)", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "light" });
    await seedMode(page, "midnight");
    await traceBoot(page);
    // /sessions bounces an unauthenticated load to /login; the attribute is
    // settled before the router/auth gate runs, whatever route it lands on.
    await assertBoot(page, "/sessions", { appearance: null, background: LIGHT_CANVAS });
  });

  test("evidence: light workbench on the first frame at 1440 and 390", async ({ page }) => {
    test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture the committed screenshots");
    await page.emulateMedia({ reducedMotion: "reduce" });
    await seedMode(page, "light");
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

/**
 * Reduced motion (visual-system.md §7): tokens.css stops every animation and
 * transition except on elements that opt in with data-motion="essential".
 * Read the live animation list rather than the stylesheet, after hovering and
 * focusing controls that normally transition.
 */
test.describe("reduced motion", () => {
  async function movingOutsideEssential(page: Page): Promise<string[]> {
    return page.evaluate(() =>
      document
        .getAnimations()
        .filter((animation) => animation.playState === "running")
        .map((animation) => {
          const target = (animation.effect as KeyframeEffect | null)?.target ?? null;
          if (!target || target.closest("[data-motion='essential']")) return null;
          const name =
            animation instanceof CSSAnimation
              ? animation.animationName
              : animation instanceof CSSTransition
                ? `transition:${animation.transitionProperty}`
                : "script";
          return `${target.tagName.toLowerCase()}.${String(target.className).slice(0, 40)} ${name}`;
        })
        .filter((entry): entry is string => entry !== null),
    );
  }

  for (const mode of ["dark", "light"] as const) {
    test(`only data-motion=essential moves under reduced motion (${mode})`, async ({ page }) => {
      await page.emulateMedia({ colorScheme: mode, reducedMotion: "reduce" });
      await page.setViewportSize({ width: 1440, height: 900 });
      await seedMode(page, mode);
      await login(page);

      for (const route of ["/sessions", "/approvals", "/settings"]) {
        await page.goto(route);
        await page.waitForLoadState("networkidle").catch(() => undefined);
        const buttons = page.locator("button:visible");
        const count = Math.min(await buttons.count(), 6);
        for (let index = 0; index < count; index += 1) {
          await buttons.nth(index).hover().catch(() => undefined);
          await buttons.nth(index).focus().catch(() => undefined);
          expect(await movingOutsideEssential(page), `${route} after hovering control ${index}`).toEqual([]);
        }
        expect(await movingOutsideEssential(page), `${route} at rest`).toEqual([]);
      }
    });
  }
});
