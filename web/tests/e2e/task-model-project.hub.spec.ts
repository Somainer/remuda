import { expect, test, type Page } from "@playwright/test";
import { fileURLToPath } from "node:url";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { login } from "./hub-auth";

// Evidence renders are written into docs/design/evidence only for an
// explicit REMUDA_EVIDENCE=1 run; normal runs keep them under test-results.
const evidenceDir =
  process.env.REMUDA_EVIDENCE === "1"
    ? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
    : path.resolve("test-results/evidence");

/**
 * Plan task-model task 8 (t-project-switcher), D-050 §9 / ui-spec §2.9:
 * the web adopts the authoritative Hub Project entity and the top-bar
 * 全局▸project switcher scopes the task list and the board.
 *
 * The fixture is gated: with HUB_E2E_PROJECT_SWITCHER=1 the harness enrolls
 * TWO fake Nodes (crates/remuda-hub/examples/hub_e2e.rs) —
 * `e2e-fake-node` announcing /tmp/remuda-project-a and a second host
 * `e2e-project-host-b` announcing /tmp/remuda-project-b — so one project can
 * have members on two hosts. Without the trigger this file self-skips, as a
 * default full-suite run enrolls only the primary host.
 *
 * Coverage of the four acceptance items:
 * 1. /projects reads Project rows through the generated client
 *    (GET /v1/projects); it is no longer the registered-workspace directory.
 * 2. the switcher selection drives the documented scoped reads:
 *    GET /v1/tasks?project= and GET /v1/board?project= return only that
 *    project; global (no filter) returns every project's rows.
 * 3. Project.members[] map to Spaces through the FULL (hostId, workspaceId)
 *    pair: the two hosts' members stay two distinct Space keys (D-024).
 * 4. the bot channel defaultProject reference still renders verbatim.
 *
 * Generic wording only; no reference-product names. All renders shown are
 * Remuda itself against the fake Node.
 */

test.describe.configure({ mode: "serial" });
// Gated exactly like task-model-bind.hub.spec.ts: the second enrolled host
// exists only when the harness started with HUB_E2E_PROJECT_SWITCHER=1, so a
// default full-suite run reports this file as skipped rather than failing.
test.skip(
  process.env.HUB_E2E_PROJECT_SWITCHER !== "1",
  "set HUB_E2E_PROJECT_SWITCHER=1 for the two-host project-switcher harness",
);

type HostRow = { id: string; label?: string };
type WorkspaceRow = { workspaceId: string; root: string };
type ProjectDoc = {
  id: string;
  name: string;
  members?: { hostId: string; workspaceId: string; role?: string }[];
};
type TaskDoc = { id: string; projectId: string; title: string };
type BoardResponse = {
  project: string | null;
  columns: Record<string, TaskDoc[]>;
};

async function apiJson<T>(page: Page, method: string, path: string, body?: unknown): Promise<T> {
  const response = await page.request.fetch(path, {
    method,
    data: body,
    headers: body ? { "content-type": "application/json" } : undefined,
  });
  const text = await response.text();
  expect(response.ok(), `${method} ${path} → ${response.status()}: ${text}`).toBe(true);
  return (text ? JSON.parse(text) : null) as T;
}

async function waitForHosts(page: Page): Promise<{ hostA: string; hostB: string }> {
  let hostA = "";
  let hostB = "";
  for (let attempt = 0; attempt < 50 && (!hostA || !hostB); attempt += 1) {
    const body = await apiJson<{ items?: HostRow[] }>(page, "GET", "/v1/hosts");
    for (const row of body.items ?? []) {
      if (row.label === "e2e-fake-node") hostA = row.id;
      if (row.label === "e2e-project-host-b") hostB = row.id;
    }
    if (!hostA || !hostB) await page.waitForTimeout(200);
  }
  expect(hostA, "primary e2e-fake-node enrolled").toBeTruthy();
  expect(hostB, "second e2e-project-host-b enrolled").toBeTruthy();
  return { hostA, hostB };
}

async function workspaceAt(page: Page, hostId: string, root: string): Promise<string> {
  const body = await apiJson<{ workspaces: WorkspaceRow[] }>(
    page,
    "GET",
    `/v1/hosts/${hostId}/workspaces`,
  );
  const found = body.workspaces.find((row) => row.root === root);
  expect(found, `branded workspace ${root} on ${hostId}`).toBeTruthy();
  return found!.workspaceId;
}

function boardTaskIds(board: BoardResponse): string[] {
  return Object.values(board.columns).flat().map((card) => card.id);
}

test.describe("Project entity + top-bar project switcher (HUB_E2E_PROJECT_SWITCHER=1)", () => {
  const suffix = Date.now().toString(36);
  let hostA = "";
  let wspA = "";
  let hostB = "";
  let wspB = "";
  let crossHostProject: ProjectDoc;
  let otherProject: ProjectDoc;
  let crossTask: TaskDoc;
  let otherTask: TaskDoc;

  test.beforeAll(async ({ browser }) => {
    // API-only setup in its own context; the UI tests log in separately.
    const context = await browser.newContext();
    const page = await context.newPage();
    await login(page);
    const hosts = await waitForHosts(page);
    hostA = hosts.hostA;
    hostB = hosts.hostB;
    wspA = await workspaceAt(page, hostA, "/tmp/remuda-project-a");
    wspB = await workspaceAt(page, hostB, "/tmp/remuda-project-b");

    // Project 1: members span the two fake hosts (the D-024 entity).
    crossHostProject = await apiJson<ProjectDoc>(page, "POST", "/v1/projects", {
      name: `跨机项目 ${suffix}`,
      members: [
        { hostId: hostA, workspaceId: wspA, role: "primary" },
        { hostId: hostB, workspaceId: wspB, role: "build" },
      ],
    });
    // Project 2: one member on the primary host only.
    otherProject = await apiJson<ProjectDoc>(page, "POST", "/v1/projects", {
      name: `单机项目 ${suffix}`,
      members: [{ hostId: hostA, workspaceId: wspA }],
    });

    crossTask = await apiJson<TaskDoc>(page, "POST", "/v1/tasks", {
      projectId: crossHostProject.id,
      title: `cross-host task ${suffix}`,
      intent: "e2e project-switcher cross host",
    });
    otherTask = await apiJson<TaskDoc>(page, "POST", "/v1/tasks", {
      projectId: otherProject.id,
      title: `single-host task ${suffix}`,
      intent: "e2e project-switcher single host",
    });
    await context.close();
  });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test("the projects page reads Project entities and no longer lists workspaces", async ({ page }) => {
    await page.goto("/projects");
    const row = page.getByTestId("project-row").filter({ hasText: crossHostProject.name });
    await expect(row).toBeVisible();
    // Project metadata: two members on two hosts, not workspace rows.
    await expect(row).toContainText("2 个成员");
    await expect(row).toContainText("2 台主机");
    await expect(row).toContainText("e2e-fake-node");
    await expect(row).toContainText("e2e-project-host-b");

    // The old stub linked every registered workspace; none of the legacy
    // workspace directory may leak into the Project page.
    await expect(page.locator("body")).not.toContainText("/tmp/remuda-e2e");
    const links = await page.getByTestId("project-row").evaluateAll((nodes) =>
      nodes.map((node) => (node as HTMLAnchorElement).getAttribute("href")),
    );
    expect(links).toContain(`/projects/${crossHostProject.id}`);
    expect(links.some((href) => href === `/projects/${wspA}`)).toBe(false);
  });

  test("members map to Spaces through the full (hostId, workspaceId) pair — two hosts stay two keys (D-024)", async ({
    page,
  }) => {
    // A deep link mounts the detail route but must NOT silently rescope the
    // global filter (scope changes only come from an explicit row click or
    // switcher action): the stored selection stays absent.
    await page.goto(`/projects/${crossHostProject.id}`);
    expect(await page.evaluate(() => localStorage.getItem("remuda.project-filter.v1"))).toBeNull();
    const memberRows = page.getByTestId("project-member-row");
    await expect(memberRows).toHaveCount(2);
    const keys = await memberRows.evaluateAll((nodes) =>
      nodes.map((node) => node.getAttribute("data-space-key")),
    );
    expect(keys).toContain(JSON.stringify([hostA, wspA]));
    expect(keys).toContain(JSON.stringify([hostB, wspB]));
    // Same project across two hosts never collapses into one Space key.
    expect(new Set(keys).size).toBe(2);
    // Both registered members resolve to their actual roots; the bare-pair
    // fallback ("工作区尚未注册") is not used for either. Rows render the host
    // label, so scope by it to keep the assertions strict.
    const rowA = memberRows.filter({ hasText: "e2e-fake-node" });
    const rowB = memberRows.filter({ hasText: "e2e-project-host-b" });
    await expect(rowA).toContainText("/tmp/remuda-project-a");
    await expect(rowB).toContainText("/tmp/remuda-project-b");
    await expect(rowA).not.toContainText("工作区尚未注册");
    await expect(rowB).not.toContainText("工作区尚未注册");
  });

  test("opening a project from the directory is an explicit scope action (filter set; detail shows the project)", async ({
    page,
  }) => {
    await page.goto("/projects");
    const row = page.getByTestId("project-row").filter({ hasText: crossHostProject.name });
    await row.click();
    await expect(page).toHaveURL(new RegExp(`/projects/${crossHostProject.id}$`));
    // The explicit row click sets the scope, unlike a deep-link mount.
    expect(await page.evaluate(() => localStorage.getItem("remuda.project-filter.v1"))).toBe(
      crossHostProject.id,
    );
    // The sidebar project rows (UO-2a, formerly the top-bar switcher) and any
    // page-header switcher name the open project.
    await expect(
      page.getByTestId("sidebar-project-row").and(page.locator(`[data-project-id="${crossHostProject.id}"]`)),
    ).toHaveAttribute("aria-pressed", "true");
    for (const select of await page.getByTestId("project-switcher").all()) {
      await expect(select).toHaveValue(crossHostProject.id);
    }
  });

  test("the switcher scopes the board and the task list by project id; global shows everything", async ({
    page,
  }) => {
    await page.goto("/projects");
    await expect(page.getByTestId("project-row").filter({ hasText: crossHostProject.name })).toBeVisible();

    // Global baseline: the documented unscoped reads return both projects'
    // rows (the task list and board surfaces issue exactly these reads).
    const globalTasks = await apiJson<{ items: TaskDoc[] }>(page, "GET", "/v1/tasks");
    expect(globalTasks.items.map((t) => t.id)).toEqual(
      expect.arrayContaining([crossTask.id, otherTask.id]),
    );
    const globalBoard = await apiJson<BoardResponse>(page, "GET", "/v1/board");
    expect(globalBoard.project).toBeNull();
    expect(boardTaskIds(globalBoard)).toEqual(
      expect.arrayContaining([crossTask.id, otherTask.id]),
    );

    // Select the single-host project in the sidebar (UO-2a: the project rows
    // replace the top-bar switcher; a row sets the global scope and opens the
    // board).
    const projectRow = (id: string) =>
      page.getByTestId("sidebar-project-row").and(page.locator(`[data-project-id="${id}"]`));
    const boardRead = (id: string | null, target: Page = page) =>
      target.waitForRequest((request) => new URL(request.url()).pathname === "/v1/board" && new URL(request.url()).searchParams.get("project") === id);
    // ui-spec §1.1: the row lands on /board?project={id}, so history and a
    // copied link keep the project.
    await projectRow(crossHostProject.id).click();
    await expect(page).toHaveURL(new RegExp(`/board\\?project=${crossHostProject.id}$`));
    await projectRow(otherProject.id).click();
    await expect(page).toHaveURL(new RegExp(`/board\\?project=${otherProject.id}$`));
    const backRead = boardRead(crossHostProject.id);
    await page.goBack();
    await expect(page).toHaveURL(new RegExp(`/board\\?project=${crossHostProject.id}$`));
    await backRead;
    await page.goForward();
    await expect(page).toHaveURL(new RegExp(`/board\\?project=${otherProject.id}$`));

    // A copied link opens the same scope in a browser with no stored filter.
    const copied = await page.context().browser()!.newPage();
    try {
      await login(copied);
      expect(await copied.evaluate(() => localStorage.getItem("remuda.project-filter.v1"))).toBeNull();
      const copiedRead = boardRead(otherProject.id, copied);
      await copied.goto(page.url());
      await copiedRead;
    } finally {
      await copied.close();
    }

    // The selection is the device-local filter surfaces read.
    expect(await page.evaluate(() => localStorage.getItem("remuda.project-filter.v1"))).toBe(
      otherProject.id,
    );

    // Scoped reads return only the selected project's task/board rows.
    const scopedTasks = await apiJson<{ items: TaskDoc[] }>(
      page,
      "GET",
      `/v1/tasks?project=${otherProject.id}`,
    );
    const scopedTaskIds = scopedTasks.items.map((t) => t.id);
    expect(scopedTaskIds).toContain(otherTask.id);
    expect(scopedTaskIds).not.toContain(crossTask.id);

    const scopedBoard = await apiJson<BoardResponse>(
      page,
      "GET",
      `/v1/board?project=${otherProject.id}`,
    );
    expect(scopedBoard.project).toBe(otherProject.id);
    const scopedBoardIds = boardTaskIds(scopedBoard);
    expect(scopedBoardIds).toContain(otherTask.id);
    expect(scopedBoardIds).not.toContain(crossTask.id);

    // The projects surface itself honours the scope: only that project row.
    await page.getByTestId("sidebar-projects-link").click();
    await expect(page.getByTestId("project-row")).toHaveCount(1);
    await expect(page.getByTestId("project-row")).toContainText(otherProject.name);

    // Back to global: filter cleared and both projects show again.
    await projectRow("").click();
    await expect(page).toHaveURL(/\/board$/);
    await expect.poll(() => page.evaluate(() => localStorage.getItem("remuda.project-filter.v1"))).toBeNull();
    await page.getByTestId("sidebar-projects-link").click();
    await expect(page.getByTestId("project-row")).toHaveCount(2);
    const backToGlobal = await apiJson<BoardResponse>(page, "GET", "/v1/board");
    expect(backToGlobal.project).toBeNull();
    expect(boardTaskIds(backToGlobal)).toEqual(
      expect.arrayContaining([crossTask.id, otherTask.id]),
    );

    // The URL is the only scope: 全局 → A → back lands on the bare /board and
    // both the board read and the sidebar go back to 全局; forward is A again.
    await projectRow("").click();
    await expect(page).toHaveURL(/\/board$/);
    await projectRow(crossHostProject.id).click();
    await expect(page).toHaveURL(new RegExp(`/board\\?project=${crossHostProject.id}$`));
    const globalRead = boardRead(null);
    await page.goBack();
    await expect(page).toHaveURL(/\/board$/);
    await globalRead;
    await expect(projectRow("")).toHaveAttribute("aria-pressed", "true");
    await expect(projectRow(crossHostProject.id)).toHaveAttribute("aria-pressed", "false");
    await expect.poll(() => page.evaluate(() => localStorage.getItem("remuda.project-filter.v1"))).toBeNull();
    const forwardRead = boardRead(crossHostProject.id);
    await page.goForward();
    await expect(page).toHaveURL(new RegExp(`/board\\?project=${crossHostProject.id}$`));
    await forwardRead;
    await expect(projectRow(crossHostProject.id)).toHaveAttribute("aria-pressed", "true");
  });

  test("the bot channel defaultProject reference stays a valid display reference", async ({ page }) => {
    // The fixture channel's defaultProject is an independently seeded string;
    // it must render verbatim on the channel surface even though no Project
    // with that id is in the Hub directory (the reference is never blanked).
    await page.goto("/bots/feishu");
    await expect(page.getByTestId("bot-defaults")).toContainText("sfe-root");
  });

  for (const width of [390, 1440]) {
    test(`evidence render: projects directory and the cross-host project at ${width}px`, async ({
      page,
    }) => {
      test.skip(process.env.REMUDA_EVIDENCE !== "1", "set REMUDA_EVIDENCE=1 to capture evidence");
      await page.setViewportSize({ width, height: 1000 });
      await page.goto("/projects");
      await expect(
        page.getByTestId("project-row").filter({ hasText: crossHostProject.name }),
      ).toBeVisible();
      await mkdir(evidenceDir, { recursive: true });
      await page.screenshot({
        path: path.join(evidenceDir, `task-model-8-projects-${width}.png`),
        animations: "disabled",
      });

      // Enter via the explicit row click (not a deep-link goto) so the scope
      // is the open project and the switchers name it in the evidence render.
      await page
        .getByTestId("project-row")
        .filter({ hasText: crossHostProject.name })
        .click();
      await expect(page).toHaveURL(new RegExp(`/projects/${crossHostProject.id}$`));
      await expect(page.getByTestId("project-member-row")).toHaveCount(2);
      await page.screenshot({
        path: path.join(evidenceDir, `task-model-8-members-${width}.png`),
        animations: "disabled",
      });
    });
  }
});
