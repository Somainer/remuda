import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Evidence capture for workbench batch A (P0-1 + the overlay contract).
 *
 * Synthetic inventory only: three invented hosts, invented directories and
 * invented session titles, built in the browser. Nothing here comes from a real
 * deployment, a real model run or a personal path.
 */
const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence");

async function applyFixture(page: Page) {
  await page.evaluate(async () => {
    const { mockDb } = await import("/src/lib/mock.ts");
    const { hubStore } = await import("/src/lib/store.ts");
    // Drop any remembered Space/tab from the default inventory, so the panel
    // cannot keep showing a host this fixture just replaced.
    localStorage.removeItem("remuda.spaces.v1");
    const stamp = { revision: "1", createdAt: new Date(0).toISOString(), updatedAt: new Date(0).toISOString() };
    const known = (value: string) => ({ state: "known", value });
    mockDb.hosts.length = 0;
    for (const host of [
      { id: "hst_a", label: "demo-node-1" },
      { id: "hst_b", label: "demo-node-2" },
      { id: "hst_c", label: "demo-node-3" },
    ]) {
      mockDb.hosts.push({
        ...stamp, id: host.id, label: host.label, ownerPrincipalId: "prn_demo", state: "online",
        transport: { mode: "outbound-wss", endpointRef: "obj_demo" },
      } as never);
    }
    mockDb.workspaces.length = 0;
    for (const workspace of [
      { id: "wsp_a1", hostId: "hst_a", label: "payments", root: "/home/dev/projects/payments" },
      { id: "wsp_b1", hostId: "hst_b", label: "payments", root: "/srv/build/payments" },
      { id: "wsp_c1", hostId: "hst_c", label: "ledger", root: "/home/dev/projects/ledger" },
    ]) {
      mockDb.workspaces.push({
        ...stamp, id: workspace.id, hostId: workspace.hostId, label: workspace.label, rootPath: workspace.root,
        writePolicy: "workspace-write", canonicalRoot: known(workspace.root),
      } as never);
    }
    const template = mockDb.instances[0];
    mockDb.instances.length = 0;
    mockDb.titles.clear();
    for (const row of [
      { id: "ins_pay_a", hostId: "hst_a", workspaceId: "wsp_a1", title: "对账任务 A", activity: "idle" },
      { id: "ins_pay_a2", hostId: "hst_a", workspaceId: "wsp_a1", title: "退款回归", activity: "waiting-interaction" },
      { id: "ins_pay_b", hostId: "hst_b", workspaceId: "wsp_b1", title: "对账任务 B", activity: "working" },
      { id: "ins_led_c", hostId: "hst_c", workspaceId: "wsp_c1", title: "账本导出", activity: "idle" },
    ]) {
      mockDb.instances.push({
        ...template, ...stamp, id: row.id, hostId: row.hostId, workspaceId: row.workspaceId,
        kind: "claude", lifecycle: "ready", connectivity: "connected", activity: known(row.activity),
        parent: null, activeRunIds: [],
      } as never);
      mockDb.titles.set(row.id, row.title);
    }
    mockDb.interactions.length = 0;
    // `refresh()` reloads instances only; hosts and workspaces come from here.
    await hubStore.refreshHosts();
    await hubStore.refresh();
    // Space prefs carry remembered names and a selected id from the default
    // inventory; reload them so the panel re-derives from this fixture.
    const { spaceStore, SPACES_PREFS_KEY } = await import("/src/features/spaces/store.ts");
    localStorage.removeItem(SPACES_PREFS_KEY);
    spaceStore.reload();
  });
  await expect(page.getByTestId("session-list")).toBeVisible();
}

/**
 * Go to a URL with the synthetic inventory in place.
 *
 * The Space panel remembers a selection in `localStorage`, and `mockDb` is
 * rebuilt by the module on every document load. So: clear the remembered
 * selection before the app boots, then apply the fixture once the page is up.
 */
async function visit(page: Page, url: string) {
  await page.addInitScript(() => localStorage.removeItem("remuda.spaces.v1"));
  await page.goto(url);
  await applyFixture(page);
}

async function shot(page: Page, name: string) {
  await page.evaluate(() => document.fonts.ready);
  const rendered = await page.locator("body").innerText();
  // The fixture is synthetic; assert none of the default mock inventory — whose
  // host labels and home paths are not publishable — survived into the frame.
  // Matching the fixture's own vocabulary keeps this free of any real identifier.
  for (const leaked of ["devbox", "sfe-root", "valhalla", "/Users/", "/home/devuser"]) {
    expect(rendered, `${name} must not show default mock inventory`).not.toContain(leaked);
  }
  expect(rendered).toContain("demo-node-");
  await mkdir(evidence, { recursive: true });
  const png = await page.screenshot({ path: path.join(evidence, `${name}.png`), animations: "disabled", scale: "css" });
  expect(png.byteLength, `${name} must stay below 300 KB`).toBeLessThanOrEqual(300_000);
}

test("workbench A filter evidence at 1440 / 768 / 390", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });

  await page.setViewportSize({ width: 1440, height: 900 });
  await visit(page, "/sessions");
  await shot(page, "workbench-a-filters-1-toolbar-1440");

  await page.getByTestId("session-filter-open").click();
  await expect(page.getByTestId("session-filter-panel")).toBeVisible();
  await shot(page, "workbench-a-filters-1-popover-1440");
  await page.keyboard.press("Escape");

  // Global scope offers the host and directory conditions a fixed Space cannot.
  await page.getByTestId("session-scope-all").click();
  await expect(page.getByTestId("session-scope")).toHaveAttribute("data-scope", "all");
  await page.getByTestId("session-filter-open").click();
  await shot(page, "workbench-a-filters-1-global-popover-1440");
  await page.keyboard.press("Escape");

  // Filtered to zero: the state that must not read as data loss.
  await page.getByTestId("session-search").fill("没有任何会话叫这个名字");
  await expect(page.getByTestId("session-no-matches")).toBeVisible();
  await shot(page, "workbench-a-filters-1-zero-1440");

  await page.setViewportSize({ width: 768, height: 1024 });
  await visit(page, "/sessions");
  await shot(page, "workbench-a-filters-1-toolbar-768");
  await page.getByTestId("session-filter-open").click();
  await expect(page.getByTestId("session-filter-panel")).toBeVisible();
  await shot(page, "workbench-a-filters-1-panel-768");

  await page.setViewportSize({ width: 390, height: 844 });
  await visit(page, "/sessions");
  await shot(page, "workbench-a-filters-1-toolbar-390");
  await page.getByTestId("session-filter-open").click();
  const sheet = page.getByTestId("session-filter-panel");
  await expect(sheet).toHaveAttribute("data-variant", "sheet");
  await shot(page, "workbench-a-filters-1-sheet-390");
});
