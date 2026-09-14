import { expect, test, type Page } from "@playwright/test";

/**
 * P0-1 acceptance: filtering, scope and zero results (exploration §5).
 *
 * The fixture is synthetic and built in the browser: three hosts, and the same
 * directory name registered on two of them. Same-name-across-hosts is the case
 * that makes a bare workspace label ambiguous, and it is exactly what the fixed
 * Space scope and the host-qualified chips exist to disambiguate. No real model
 * is involved — mock mode serves this inventory straight out of `mockDb`.
 *
 * `mockDb` is module state inside the page, so every navigation that reloads
 * the document drops it. Seeding is therefore a separate step the caller
 * re-runs after each `goto`/`reload`, rather than something bundled into one.
 */
async function applyFixture(page: Page) {
  await page.evaluate(async () => {
    const { mockDb } = await import("/src/lib/mock.ts");
    const { hubStore } = await import("/src/lib/store.ts");
    const stamp = { revision: "1", createdAt: new Date(0).toISOString(), updatedAt: new Date(0).toISOString() };
    const known = (value: string) => ({ state: "known", value });

    const hosts = [
      { id: "hst_a", label: "demo-node-1" },
      { id: "hst_b", label: "demo-node-2" },
      { id: "hst_c", label: "demo-node-3" },
    ];
    mockDb.hosts.length = 0;
    for (const host of hosts) {
      mockDb.hosts.push({
        ...stamp, id: host.id, label: host.label, ownerPrincipalId: "prn_demo", state: "online",
        transport: { mode: "outbound-wss", endpointRef: "obj_demo" },
      } as never);
    }

    // `payments` exists on demo-node-1 and demo-node-2 under the same name.
    const workspaces = [
      { id: "wsp_a1", hostId: "hst_a", label: "payments", root: "/home/dev/projects/payments" },
      { id: "wsp_b1", hostId: "hst_b", label: "payments", root: "/srv/build/payments" },
      { id: "wsp_c1", hostId: "hst_c", label: "ledger", root: "/home/dev/projects/ledger" },
    ];
    mockDb.workspaces.length = 0;
    for (const workspace of workspaces) {
      mockDb.workspaces.push({
        ...stamp, id: workspace.id, hostId: workspace.hostId, label: workspace.label, rootPath: workspace.root,
        writePolicy: "workspace-write", canonicalRoot: known(workspace.root),
      } as never);
    }

    const template = mockDb.instances[0];
    const rows = [
      { id: "ins_pay_a", hostId: "hst_a", workspaceId: "wsp_a1", title: "对账任务 A", activity: "idle" },
      { id: "ins_pay_a2", hostId: "hst_a", workspaceId: "wsp_a1", title: "退款回归", activity: "waiting-interaction" },
      { id: "ins_pay_b", hostId: "hst_b", workspaceId: "wsp_b1", title: "对账任务 B", activity: "working" },
      { id: "ins_led_c", hostId: "hst_c", workspaceId: "wsp_c1", title: "账本导出", activity: "idle" },
    ];
    mockDb.instances.length = 0;
    mockDb.titles.clear();
    for (const row of rows) {
      mockDb.instances.push({
        ...template, ...stamp, id: row.id, hostId: row.hostId, workspaceId: row.workspaceId,
        kind: "claude", lifecycle: "ready", connectivity: "connected", activity: known(row.activity),
        parent: null, activeRunIds: [],
      } as never);
      mockDb.titles.set(row.id, row.title);
    }
    mockDb.interactions.length = 0;
    await hubStore.refresh();
  });
  await expect(page.getByTestId("session-list")).toBeVisible();
}

/** Navigate, then rebuild the in-page fixture the navigation just discarded. */
async function visit(page: Page, url: string) {
  await page.goto(url);
  await applyFixture(page);
}

/** Select the Space whose sidebar entry carries this name. */
function space(page: Page, name: string) {
  return page.getByTestId("spaces-panel").getByTestId("space-select").filter({ hasText: name });
}

/**
 * Select a Space and come back to the list.
 *
 * Choosing a Space navigates to its selected session, which is the workbench's
 * normal behaviour; the filter toolbar lives on `/sessions`. Returning there
 * keeps the Space selected, so this exercises the switch without asserting
 * anything about the detail route.
 */
async function selectSpace(page: Page, name: string, index = 0) {
  await space(page, name).nth(index).click();
  await page.getByRole("navigation", { name: "主导航" }).getByRole("link").first().click();
  await expect(page.getByTestId("session-list")).toBeVisible();
}

/**
 * Leave the Space and search everything, waiting for the switch to land.
 *
 * The click writes `scope=all` to the URL and the list re-derives from it, so
 * assertions that follow must wait for the derived state rather than the URL.
 */
async function goGlobal(page: Page) {
  await page.getByTestId("session-scope-all").click();
  await expect(page.getByTestId("session-scope")).toHaveAttribute("data-scope", "all");
}

test.describe("P0-1 filters, scope and zero results", () => {
  test.beforeEach(async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await visit(page, "/sessions");
  });

  test("the filter panel opens and closes by click and by keyboard, reporting aria-expanded", async ({ page }) => {
    const trigger = page.getByTestId("session-filter-open");
    const panel = page.getByTestId("session-filter-panel");
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await expect(panel).toHaveCount(0);

    await trigger.click();
    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await expect(panel).toBeVisible();
    await expect(panel).toHaveAttribute("aria-modal", "true");
    await expect(panel).toHaveAccessibleName("筛选会话");
    // Focus is inside the overlay, not left on the page behind it.
    expect(await panel.evaluate((element) => element.contains(document.activeElement))).toBe(true);

    await page.keyboard.press("Escape");
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await expect(panel).toHaveCount(0);
    await expect(trigger).toBeFocused();

    // The same round trip without a mouse.
    await trigger.press("Enter");
    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await expect(panel).toBeVisible();
    await page.getByTestId("session-filter-close").click();
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await expect(trigger).toBeFocused();
  });

  test("Tab stays inside the open panel", async ({ page }) => {
    await page.getByTestId("session-filter-open").click();
    const panel = page.getByTestId("session-filter-panel");
    for (let step = 0; step < 12; step += 1) {
      await page.keyboard.press("Tab");
      expect(
        await panel.evaluate((element) => element.contains(document.activeElement)),
        `focus escaped the overlay after ${step + 1} tabs`,
      ).toBe(true);
    }
  });

  test("a Space fixes the scope and withholds host and directory conditions", async ({ page }) => {
    await selectSpace(page, "payments");
    const scope = page.getByTestId("session-scope");
    await expect(scope).toHaveAttribute("data-scope", "space");
    await expect(scope).toContainText("当前 Space 固定范围");
    // The Space names its host, because two Spaces are called `payments`.
    await expect(scope).toContainText("demo-node-1");

    await page.getByTestId("session-filter-open").click();
    await expect(page.getByTestId("session-filter-hosts")).toHaveCount(0);
    await expect(page.getByTestId("session-filter-workspaces")).toHaveCount(0);
    await expect(page.getByTestId("session-filter-scope-note")).toContainText("已固定主机与工作目录");
  });

  test("same-name directories on different hosts stay distinguishable in global scope", async ({ page }) => {
    await goGlobal(page);
    await expect(page).toHaveURL(/scope=all/);
    await expect(page.getByTestId("board-card")).toHaveCount(4);

    await page.getByTestId("session-filter-open").click();
    const directories = page.getByTestId("session-filter-workspaces");
    // Both `payments` entries are offered, each qualified by its host.
    await expect(directories.getByRole("button", { name: "payments · demo-node-1" })).toBeVisible();
    await expect(directories.getByRole("button", { name: "payments · demo-node-2" })).toBeVisible();
    await expect(directories.getByRole("button", { name: "ledger", exact: true })).toBeVisible();

    await directories.getByRole("button", { name: "payments · demo-node-2" }).click();
    await page.getByTestId("session-filter-close").click();
    // Only the one on demo-node-2, not the same-named directory on demo-node-1.
    await expect(page.getByTestId("board-card")).toHaveCount(1);
    await expect(page.getByTestId("board-card")).toContainText("对账任务 B");
    await expect(page.getByTestId("session-chip").filter({ hasText: "payments" })).toContainText("demo-node-2");
  });

  test("zero results are distinguished from an empty Space and offer both ways back", async ({ page }) => {
    await goGlobal(page);
    await page.getByTestId("session-search").fill("没有任何会话叫这个名字");

    const zero = page.getByTestId("session-no-matches");
    await expect(zero).toBeVisible();
    await expect(zero).toContainText("没有会话符合当前筛选条件");
    // States what exists behind the filter, so this never reads as data loss.
    await expect(zero).toContainText("4 个会话");
    await expect(zero).toContainText("不会创建或关闭任何会话");
    await expect(page.getByTestId("board-card")).toHaveCount(0);

    await page.getByTestId("session-no-matches-clear").click();
    await expect(page.getByTestId("session-no-matches")).toHaveCount(0);
    // Clearing restores the list and creates or closes nothing.
    await expect(page.getByTestId("board-card")).toHaveCount(4);
  });

  test("zero results inside a Space can widen to all spaces", async ({ page }) => {
    await selectSpace(page, "ledger");
    await page.getByTestId("session-search").fill("对账");
    const zero = page.getByTestId("session-no-matches");
    await expect(zero).toBeVisible();
    await page.getByTestId("session-no-matches-all").click();
    // Same text condition, wider scope: the matches were in other Spaces.
    await expect(page.getByTestId("session-scope")).toHaveAttribute("data-scope", "all");
    await expect(page.getByTestId("board-card")).toHaveCount(2);
    await expect(page.getByTestId("session-search")).toHaveValue("对账");
  });

  test("switching Space drops conditions it cannot honour and keeps the ones it can", async ({ page }) => {
    await goGlobal(page);
    await page.getByTestId("session-search").fill("对账");
    await page.getByTestId("session-filter-open").click();
    await page.getByTestId("session-filter-workspaces").getByRole("button", { name: "payments · demo-node-2" }).click();
    await page.getByTestId("session-filter-close").click();
    await expect(page.getByTestId("board-card")).toHaveCount(1);
    expect(new URL(page.url()).searchParams.get("workspace")).toBe("wsp_b1");

    // Return to the Space-fixed scope carrying those conditions. This stays
    // inside the SPA, so the pruning runs as a user would see it rather than on
    // a fresh mount.
    await page.getByTestId("session-scope-space").click();

    // The directory condition belonged to the old scope and cannot apply to a
    // fixed one, so it goes — and the notice says so rather than the list
    // silently narrowing.
    await expect(page.getByTestId("session-scope-notice")).toContainText("已清除不适用的条件");
    await expect(page).not.toHaveURL(/workspace=/);
    await expect(page.getByTestId("session-scope")).toHaveAttribute("data-scope", "space");
    // The text condition still means the same thing here, so it stays, visibly.
    await expect(page.getByTestId("session-search")).toHaveValue("对账");
    await expect(page.getByTestId("session-chip").filter({ hasText: "对账" })).toBeVisible();
  });

  test("a deep link, a refresh and Back all agree on the conditions and the results", async ({ page }) => {
    await visit(page, "/sessions?scope=all&q=%E5%AF%B9%E8%B4%A6&status=idle");
    await expect(page.getByTestId("session-scope")).toHaveAttribute("data-scope", "all");
    await expect(page.getByTestId("session-search")).toHaveValue("对账");
    await expect(page.getByTestId("board-card")).toHaveCount(1);
    await expect(page.getByTestId("board-card")).toContainText("对账任务 A");

    await page.reload();
    await applyFixture(page);
    await expect(page.getByTestId("session-search")).toHaveValue("对账");
    await expect(page.getByTestId("session-scope")).toHaveAttribute("data-scope", "all");
    await expect(page.getByTestId("board-card")).toHaveCount(1);

    // An explicit condition change is one history entry, so Back undoes exactly it.
    await page.getByTestId("session-chip").filter({ hasText: "空闲" }).click();
    await expect(page).not.toHaveURL(/status=idle/);
    await expect(page.getByTestId("board-card")).toHaveCount(2);
    await page.goBack();
    await expect(page).toHaveURL(/status=idle/);
    await expect(page.getByTestId("board-card")).toHaveCount(1);
  });

  test("typing in the search box does not stack one history entry per keystroke", async ({ page }) => {
    await goGlobal(page);
    const search = page.getByTestId("session-search");
    await search.fill("对");
    await search.fill("对账");
    await search.fill("对账任");
    await expect(page).toHaveURL(/q=/);
    // One Back leaves the whole typed query behind rather than replaying it.
    await page.goBack();
    await expect(page.getByTestId("session-search")).toHaveValue("");
  });

  test("the phone layout uses the bottom sheet and keeps the filter controls reachable", async ({ page }) => {
    // The sheet animates in with a transform, which leaves the painted box on a
    // fractional pixel mid-flight. Reduced motion is a real user setting the
    // stylesheet already honours, and it makes the measurement deterministic.
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.setViewportSize({ width: 390, height: 844 });
    const trigger = page.getByTestId("session-filter-open");
    await trigger.click();
    const panel = page.getByTestId("session-filter-panel");
    await expect(panel).toHaveAttribute("data-variant", "sheet");
    await expect(panel).toHaveAttribute("aria-modal", "true");

    // Every control in the sheet meets the project's 44px touch target.
    const targets = await panel.getByRole("button").all();
    for (const target of targets) {
      const box = await target.boundingBox();
      expect(box, "a sheet control must be laid out").not.toBeNull();
      expect(box!.height).toBeGreaterThanOrEqual(44);
    }
    await page.keyboard.press("Escape");
    await expect(panel).toHaveCount(0);
    await expect(trigger).toBeFocused();
  });
});
