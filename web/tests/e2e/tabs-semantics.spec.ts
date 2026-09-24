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
          workspace.rootPath = `/home/dev/projects/${workspace.rootPath.split("/").pop()}`;
          workspace.canonicalRoot = { state: "known", value: workspace.rootPath };
        });
        // Host labels reach the sidebar through hub.hosts, so reload those too.
        await hubStore.refreshHosts();
        await hubStore.refresh();
        return labels;
      });
    } catch (error) {
      if (attempt >= 1 || !/garbage collected|Execution context was destroyed/.test((error as Error).message)) throw error;
      await page.waitForLoadState("networkidle");
    }
  }
}

async function screenshot(page: Page, name: string, theme: "night" | "ledger") {
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
});

test.describe("phone", () => {
  // Touch emulation is what makes (pointer: coarse) / (hover: none) match, so
  // the hover-free close affordance is exercised as a phone really sees it.
  test.use({ hasTouch: true, isMobile: true });

  test("a long press reveals close on a phone, and the sheet fits 400px", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.setViewportSize({ width: 400, height: 860 });
    // UO-2a/D-049: the phone strip lives ONLY on the compact home; compact
    // /s/:id folds switching into the header chip. The long-press gesture is
    // therefore exercised on /m, against the strip itself — no tab click
    // (which would navigate to /s/:id and unmount the strip).
    await page.goto("/m");
    const strip = page.getByTestId("space-tabs");
    expect(await page.evaluate(() => matchMedia("(pointer: coarse)").matches),
      "the phone run must match the coarse-pointer rules").toBe(true);
    await expect(strip.getByRole("tab").first()).toBeVisible();
    await expect(page.getByTestId("spaces-chips")).toBeVisible();
    const phoneTab = strip.locator('[data-testid="session-tab"]').first().locator("..");
    const close = phoneTab.getByTestId("tab-close");

    // Without a hover to reveal it, × must not sit next to the status shape.
    await expect(phoneTab).not.toHaveAttribute("data-revealed", "true");
    expect(await close.evaluate((element) => getComputedStyle(element).opacity)).toBe("0");
    await screenshot(page, "phone-dark", "night");
    await screenshot(page, "phone-light", "ledger");

    // A long press reveals it. The component listens to the same touch-event
    // sequence a phone sends; dispatch it directly on the tab row (a trusted
    // tap would activate the tab and navigate away).
    const box = await phoneTab.boundingBox();
    expect(box).not.toBeNull();
    await phoneTab.dispatchEvent("touchstart", {
      touches: [{ identifier: 1, clientX: box!.x + 20, clientY: box!.y + box!.height / 2 }],
    });
    await expect(phoneTab).toHaveAttribute("data-revealed", "true", { timeout: 3000 });
    await phoneTab.dispatchEvent("touchend", { touches: [], changedTouches: [] });
    expect(await close.evaluate((element) => getComputedStyle(element).opacity)).toBe("1");
    await screenshot(page, "phone-close-revealed-dark", "night");
    await close.click();
    await expect(page.getByTestId("tab-close-sheet")).toBeVisible();
    await screenshot(page, "phone-close-sheet-dark", "night");
    await screenshot(page, "phone-close-sheet-light", "ledger");
    await page.getByTestId("tab-close-sheet-cancel").click();
    // The sheet dismiss keeps us on the compact home, where the opener lives.
    await expect(page).toHaveURL(/\/m$/);
    await page.getByTestId("spaces-drawer-open").click();
    await expect(page.getByTestId("spaces-drawer")).toBeVisible();
    await screenshot(page, "phone-drawer-dark", "night");

    const dimensions = await page.evaluate(() => ({ width: window.innerWidth, content: document.documentElement.scrollWidth }));
    expect(dimensions.content).toBeLessThanOrEqual(dimensions.width);
    expect(errors).toEqual([]);
  });
});
