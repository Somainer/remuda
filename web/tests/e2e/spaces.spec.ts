import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Committed evidence is refreshed only on request (REMUDA_EVIDENCE=1); every other run —
// including the merge gate, whose verify-tree step rejects a dirty worktree — writes
// the same screenshots under the gitignored test-results/ instead.
const evidence = process.env.REMUDA_EVIDENCE === "1"
  ? path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/spaces-1")
  : path.join(path.dirname(fileURLToPath(import.meta.url)), "../../test-results/evidence/spaces-1");

function space(page: Page, name: string) {
  return page.getByTestId("spaces-panel").getByTestId("space-select").filter({ hasText: name });
}

function tab(page: Page, title: string) {
  return page.getByTestId("space-tabs").getByRole("tab").filter({ hasText: title });
}

async function screenshot(page: Page, name: string, theme: "night" | "ledger") {
  // Publish only generic demo inventory. Mutate the in-browser fixture before
  // rendering evidence; production code and screenshot pixels stay untouched.
  const replacedLabels = await page.evaluate(async () => {
    const mockModule = "/src/lib/mock.ts";
    const storeModule = "/src/lib/store.ts";
    const { mockDb } = await import(mockModule);
    const { hubStore } = await import(storeModule);
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
  const png = await page.screenshot({ path: path.join(evidence, `mock-${name}.png`), animations: "disabled", scale: "css" });
  expect(png.byteLength, `${name} must stay below 300 KB`).toBeLessThanOrEqual(300_000);
}

test("mock spaces remember tabs, names, order and panel state across desktop and phone layouts", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/sessions");
  const panel = page.getByTestId("spaces-panel");
  const strip = page.getByTestId("space-tabs");
  await space(page, "sfe-root").click();
  await tab(page, "空闲会话").click();
  const firstPath = new URL(page.url()).pathname;
  await expect(tab(page, "空闲会话")).toHaveAttribute("aria-selected", "true");
  await space(page, "x-codexdrv").click();
  await expect(tab(page, "codex-worker")).toHaveAttribute("aria-selected", "true");
  await expect(tab(page, "空闲会话")).toHaveCount(0);
  const otherPath = new URL(page.url()).pathname;
  await space(page, "sfe-root").click();
  await expect(tab(page, "空闲会话")).toHaveAttribute("aria-selected", "true");
  await expect(tab(page, "codex-worker")).toHaveCount(0);
  await page.goto(otherPath);
  await expect(space(page, "x-codexdrv")).toHaveAttribute("aria-pressed", "true");
  await page.goto(firstPath);
  await expect(space(page, "sfe-root")).toHaveAttribute("aria-pressed", "true");
  await expect(tab(page, "空闲会话")).toHaveAttribute("aria-selected", "true");

  await strip.getByRole("tab").first().click();
  await page.keyboard.press("ControlOrMeta+2");
  await expect(strip.getByRole("tab").nth(1)).toHaveAttribute("aria-selected", "true");
  await page.keyboard.press("ControlOrMeta+1");
  await expect(strip.getByRole("tab").first()).toHaveAttribute("aria-selected", "true");
  await page.keyboard.press("ControlOrMeta+]");
  await expect(space(page, "valhalla")).toHaveAttribute("aria-pressed", "true");
  await page.keyboard.press("ControlOrMeta+[");
  try {
    await expect(space(page, "sfe-root")).toHaveAttribute("aria-pressed", "true");
  } catch (error) {
    const state = await page.evaluate(() => ({
      route: window.location.pathname,
      selectedSpaces: [...document.querySelectorAll('[data-testid="space-select"][aria-pressed="true"]')].map((element) => element.textContent),
      focus: { tag: document.activeElement?.tagName, role: document.activeElement?.getAttribute("role"), label: document.activeElement?.getAttribute("aria-label") },
    }));
    await test.info().attach("space-keyboard-state", { body: JSON.stringify(state), contentType: "application/json" });
    throw error;
  }

  await space(page, "sfe-root").click();
  await tab(page, "看 TaskManager spill 这段为啥抖").click();
  await page.getByTestId("panel-toggle").click();
  await expect(panel).toHaveAttribute("data-collapsed", "true");
  await page.reload();
  await expect(panel).toHaveAttribute("data-collapsed", "true");
  await expect(tab(page, "看 TaskManager spill 这段为啥抖")).toHaveAttribute("aria-selected", "true");
  await screenshot(page, "desktop-collapsed-dark", "night");
  await page.getByTestId("panel-toggle").focus();
  await page.keyboard.press("ControlOrMeta+b");
  await expect(panel).toHaveAttribute("data-collapsed", "false");
  await screenshot(page, "desktop-dark", "night");
  await screenshot(page, "desktop-light", "ledger");

  await page.setViewportSize({ width: 400, height: 860 });
  await expect(page.getByTestId("spaces-chips")).toBeVisible();
  await expect(strip).toBeVisible();
  expect(await strip.evaluate((element) => element.scrollWidth > element.clientWidth)).toBe(true);
  await expect(page.getByTestId("spaces-drawer")).toHaveCount(0);
  await screenshot(page, "phone-light", "ledger");
  await screenshot(page, "phone-dark", "night");
  await page.getByTestId("spaces-drawer-open").click();
  await expect(page.getByTestId("spaces-drawer")).toBeVisible();
  await screenshot(page, "phone-drawer-dark", "night");
  await space(page, "x-codexdrv").click();
  await expect(page.getByTestId("spaces-drawer")).toHaveCount(0);
  await expect(tab(page, "codex-worker")).toHaveAttribute("aria-selected", "true");
  await expect(tab(page, "空闲会话")).toHaveCount(0);
  const dimensions = await page.evaluate(() => ({ width: window.innerWidth, content: document.documentElement.scrollWidth }));
  expect(dimensions.content).toBeLessThanOrEqual(dimensions.width);

  await page.setViewportSize({ width: 1440, height: 900 });

  // Status and close are distinct: the tab strip carries exactly one × per tab
  // (close), and the status indicator is never one.
  await space(page, "sfe-root").click();
  const firstTab = strip.getByRole("tab").first();
  await firstTab.click();
  const tabRow = strip.locator('[data-active="true"]');
  expect(await tabRow.getByTestId("tab-close").count()).toBe(1);
  expect(await tabRow.innerText()).not.toContain("×");
  await expect(tabRow.getByTestId("tab-close")).toHaveAccessibleName(/^关闭标签 /);

  await space(page, "x-codexdrv").click();
  await panel.getByRole("button", { name: "重命名", exact: true }).click();
  await panel.getByRole("textbox", { name: "空间名称" }).fill("Code review");
  await panel.getByRole("button", { name: "保存", exact: true }).click();
  await panel.getByRole("button", { name: "下移 Code review", exact: true }).click();
  const order = await panel.getByTestId("space-select").evaluateAll((buttons) => buttons.map((button) => button.getAttribute("data-space-id")));
  await page.reload();
  await expect(space(page, "Code review")).toHaveAttribute("aria-pressed", "true");
  expect(await panel.getByTestId("space-select").evaluateAll((buttons) => buttons.map((button) => button.getAttribute("data-space-id")))).toEqual(order);
  expect(errors).toEqual([]);
});

/**
 * P0-1 §5: the filter conditions live in the URL, so a deep link and a reload
 * must produce the same list, and a Space must say which host it is on when the
 * directory name alone cannot tell two Spaces apart.
 */
test("filter conditions survive a deep link and a reload, and the scope names its host", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.setViewportSize({ width: 1440, height: 900 });

  await page.goto("/sessions");
  const scope = page.getByTestId("session-scope");
  await expect(scope).toHaveAttribute("data-scope", "space");
  // A Space is a host + directory pair, and the scope line spells out both so
  // two Spaces sharing a directory name stay distinguishable (§2.2).
  await expect(scope).toContainText("当前 Space 固定范围");
  const hostInScope = await scope.innerText();
  expect(hostInScope).toMatch(/·/);

  // Deep-link straight into a global text search.
  await page.goto("/sessions?scope=all&q=codex");
  await expect(page.getByTestId("session-scope")).toHaveAttribute("data-scope", "all");
  await expect(page.getByTestId("session-search")).toHaveValue("codex");
  const matched = await page.getByTestId("board-card").count();
  expect(matched).toBeGreaterThan(0);
  await expect(page.getByTestId("session-match-count")).toContainText(`${matched} /`);

  // A reload is the same URL, so it is the same list.
  await page.reload();
  await expect(page.getByTestId("session-search")).toHaveValue("codex");
  await expect(page.getByTestId("board-card")).toHaveCount(matched);

  // Clearing is one explicit action and never touches a session.
  await page.getByTestId("session-clear-filters").click();
  await expect(page.getByTestId("session-search")).toHaveValue("");
  expect(await page.getByTestId("board-card").count()).toBeGreaterThanOrEqual(matched);
  expect(errors).toEqual([]);
});
