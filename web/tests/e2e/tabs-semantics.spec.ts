import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/tabs-1");

function space(page: Page, name: string) {
  return page.getByTestId("spaces-panel").getByTestId("space-select").filter({ hasText: name });
}

async function screenshot(page: Page, name: string, theme: "night" | "ledger") {
  // Publish only generic demo inventory. Mutate the in-browser fixture before
  // rendering evidence; production code and screenshot pixels stay untouched.
  const replacedLabels = await page.evaluate(async () => {
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
    await hubStore.refresh();
    return labels;
  });
  await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
  await page.evaluate(() => document.fonts.ready);
  const rendered = await page.locator("body").innerText();
  expect(rendered).not.toMatch(/\/Users\//);
  for (const label of replacedLabels) expect(rendered).not.toContain(label);
  await mkdir(evidence, { recursive: true });
  const png = await page.screenshot({ path: path.join(evidence, `${name}.png`), animations: "disabled", scale: "css" });
  expect(png.byteLength, `${name} must stay below 300 KB`).toBeLessThanOrEqual(300_000);
}

test("status and close are distinct, dismissal keeps sessions running, and exited sessions group", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/sessions");
  const panel = page.getByTestId("spaces-panel");
  const strip = page.getByTestId("space-tabs");
  await space(page, "sfe-root").click();
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

  // The active tab and the active sidebar rows both carry the brand marker.
  await expect(active).toHaveAttribute("data-active", "true");
  await expect(panel.getByTestId("space-session").and(page.locator('[data-active="true"]'))).toHaveCount(1);
  await screenshot(page, "desktop-dark", "night");
  await screenshot(page, "desktop-light", "ledger");

  // Keyboard focus on the close control stays visible.
  await active.getByTestId("tab-close").focus();
  await expect(active.getByTestId("tab-close")).toBeFocused();
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

  // The session is still listed in the sidebar and re-opens its tab on click.
  const dismissed = panel.getByTestId("space-session").and(page.locator(`[href="${runningRoute}"]`));
  await expect(dismissed).toHaveCount(1);
  await dismissed.click();
  await expect(page).toHaveURL(new RegExp(`${runningRoute}$`));
  await expect(strip.getByRole("tab", { selected: true })).toHaveCount(1);

  // Exited sessions live in their own collapsed group with 恢复 and 删除.
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

  // 400px: the same semantics, with the close control revealed by a long press.
  await page.setViewportSize({ width: 400, height: 860 });
  await expect(page.getByTestId("spaces-chips")).toBeVisible();
  await screenshot(page, "phone-dark", "night");
  await screenshot(page, "phone-light", "ledger");
  const phoneTab = strip.locator('[data-active="true"]');
  await phoneTab.dispatchEvent("touchstart", { touches: [{ clientX: 40, clientY: 20 }] });
  await expect(phoneTab).toHaveAttribute("data-revealed", "true", { timeout: 3000 });
  await phoneTab.dispatchEvent("touchend", { touches: [] });
  await screenshot(page, "phone-close-revealed-dark", "night");
  await phoneTab.getByTestId("tab-close").click();
  await expect(page.getByTestId("tab-close-sheet")).toBeVisible();
  await screenshot(page, "phone-close-sheet-dark", "night");
  await screenshot(page, "phone-close-sheet-light", "ledger");
  await page.getByTestId("tab-close-sheet-cancel").click();
  await page.getByTestId("spaces-drawer-open").click();
  await expect(page.getByTestId("spaces-drawer")).toBeVisible();
  await screenshot(page, "phone-drawer-dark", "night");

  const dimensions = await page.evaluate(() => ({ width: window.innerWidth, content: document.documentElement.scrollWidth }));
  expect(dimensions.content).toBeLessThanOrEqual(dimensions.width);
  expect(errors).toEqual([]);
});
