import { expect, test, type Page, type Locator } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * Plan task-model task 5 (t-tasklist), D-050 §9 / ui-spec §2.9: the task
 * list grouped by project + git branch at desktop 1440 and the /m phone task
 * layer at 390.
 *
 * Coverage of the seven acceptance items:
 *  1. project groups whose header carries the buildSpaces() blocked count;
 *  2. children nested under their parent (▸ N), covered by the split tasks;
 *  3. the cross-project 需要你 first group (pending interaction + owner
 *     blocked reason), distinct from the raw blocked count;
 *  4. derived per-project SE-nn keys rendered instead of the bare task id;
 *  5. the detail panel renders mandate / title / blocked reason;
 *  6. archived tasks fold into their own trailing group, rows show the
 *     session count and the session link opens the shared /s/:id;
 *  7. the /m home layers tasks under project without a second transcript —
 *     tapping a task row opens the same /s/:id session route.
 *
 * No gated fake-node fixture is needed: tasks/projects are plain Hub APIs
 * and the session is a normal launch on the default e2e workspace (the fake
 * node parks it on a pending approval, which is exactly the attention
 * signal). Generic wording only; no reference-product names.
 */

test.describe.configure({ mode: "serial" });

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

const createdInstances: string[] = [];

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

type TaskDoc = {
  id: string;
  title: string;
  parentTaskId?: string | null;
  archivedAt?: string | null;
};

async function apiJson<T>(page: Page, method: string, pathName: string, body?: unknown): Promise<T> {
  const response = await page.request.fetch(pathName, {
    method,
    data: body,
    headers: body ? { "content-type": "application/json" } : undefined,
  });
  const text = await response.text();
  expect(response.ok(), `${method} ${pathName} → ${response.status()}: ${text}`).toBe(true);
  return (text ? JSON.parse(text) : null) as T;
}

async function apiStatus(
  page: Page,
  method: string,
  pathName: string,
  body?: unknown,
): Promise<number> {
  const response = await page.request.fetch(pathName, {
    method,
    data: body,
    headers: body ? { "content-type": "application/json" } : undefined,
  });
  return response.status();
}

type Fixture = {
  suffix: string;
  project: string;
  host: string;
  parent: TaskDoc;
  child1: TaskDoc;
  child2: TaskDoc;
  blocked: TaskDoc;
  running: TaskDoc;
  archived: TaskDoc;
  instanceId: string;
};

async function fakeHostId(page: Page): Promise<string> {
  const body = await apiJson<{ items?: { id?: string; label?: string }[] }>(
    page,
    "GET",
    "/v1/hosts",
  );
  const host = (body.items ?? []).find((item) => item.label === "e2e-fake-node");
  expect(host?.id, "e2e-fake-node enrolled").toBeTruthy();
  return host!.id!;
}

async function makeFixture(page: Page): Promise<Fixture> {
  const suffix = Date.now().toString(36);
  const host = await fakeHostId(page);
  const project = (
    await apiJson<{ id: string }>(page, "POST", "/v1/projects", {
      name: `tlist ${suffix}`,
    })
  ).id;
  // No project membership rows: the fixture's tasks carry no workspace
  // binding, and a project with no members places on every enrolled host.
  // The default wsp_e2e label is a legacy non-UUID workspace id, so it
  // cannot be added as a member (the t-bind spec instead uses its gated
  // branded UUID workspace).

  const create = async (label: string): Promise<TaskDoc> =>
    apiJson<TaskDoc>(page, "POST", "/v1/tasks", {
      projectId: project,
      title: `tlist ${label} ${suffix}`,
      intent: `e2e task-list intent ${label} ${suffix}`,
    });

  const parent = await create("parent");
  const child1 = await apiJson<TaskDoc>(page, "POST", `/v1/tasks/${parent.id}/split`, {
    title: `tlist child one ${suffix}`,
    intent: `child one intent ${suffix}`,
  });
  const child2 = await apiJson<TaskDoc>(page, "POST", `/v1/tasks/${parent.id}/split`, {
    title: `tlist child two ${suffix}`,
    intent: `child two intent ${suffix}`,
  });
  expect(child1.parentTaskId).toBe(parent.id);
  expect(child2.parentTaskId).toBe(parent.id);

  // Failed pre-dispatch with an owner-facing reason: needs-a-human without
  // an interaction (distinct from the raw session blocked count).
  const blocked = await create("blocked");
  expect(
    await apiStatus(page, "PATCH", `/v1/tasks/${blocked.id}`, {
      state: "failed",
      reason: `tlist supply blocked ${suffix}`,
    }),
  ).toBe(200);

  const running = await create("running");
  expect(await apiStatus(page, "PATCH", `/v1/tasks/${running.id}`, { state: "placed" })).toBe(200);
  expect(await apiStatus(page, "PATCH", `/v1/tasks/${running.id}`, { state: "running" })).toBe(200);

  const archived = await create("archived");
  const archivedDoc = await apiJson<TaskDoc>(page, "POST", `/v1/tasks/${archived.id}/archive`, {});
  expect(archivedDoc.archivedAt).toBeTruthy();

  // A live session on the parent task, parked on the fake node's pending
  // approval: one session for the row count plus the interaction-inbox
  // attention signal.
  const launched = await apiJson<{ instance?: { instanceId?: string; id?: string } }>(
    page,
    "POST",
    "/v1/instances",
    {
      hostId: host,
      workspaceId: "wsp_e2e",
      kind: "claude",
      // The fake node scripts its pending approval on the print driver path
      // (the pty branch only reports ready); this mirrors how the m-home UI
      // spec parks a session on an approval.
      driver: "claude-print",
      taskId: parent.id,
      // The sentinel both parks the turn on the approval card and flips the
      // fake node's native status to blocked (what buildSpaces() counts);
      // a plain approval prompt would read working.
      prompt: `TLIST mhome-blocked approval gate ${suffix}`,
    },
  );
  const instanceId = launched.instance?.instanceId ?? launched.instance?.id;
  expect(instanceId).toBeTruthy();
  createdInstances.push(instanceId!);

  // Wait until the approval interaction is actually pending for this
  // session — the attention group projects from the interaction inbox.
  await expect
    .poll(
      async () => {
        const list = await apiJson<{ items?: { instanceId?: string; state?: string }[] }>(
          page,
          "GET",
          `/v1/interactions?instanceId=${instanceId}`,
        );
        return (list.items ?? []).some(
          (item) => item.instanceId === instanceId && item.state === "pending",
        );
      },
      { timeout: 20_000 },
    )
    .toBe(true);

  return {
    suffix,
    project,
    host,
    parent,
    child1,
    child2,
    blocked,
    running,
    archived,
    instanceId: instanceId!,
  };
}

test.afterAll(async ({ browser }) => {
  if (createdInstances.length === 0) return;
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    await login(page);
    for (const id of createdInstances.splice(0)) {
      await page.request
        .fetch(`/v1/instances/${id}?force=1`, { method: "DELETE" })
        .catch(() => undefined);
    }
  } finally {
    await context.close();
  }
});

const rowIn = (scope: Locator | Page, id: string): Locator =>
  scope.locator(`[data-testid="task-row"][data-task-id="${id}"]`);

test.describe("task list at 1440 desktop", () => {
  test.use({ viewport: { width: 1440, height: 900 } });

  test("groups, nesting, attention, SE keys, detail and archive fold", async ({ page }) => {
    await login(page);
    const fx = await makeFixture(page);

    await page.goto(`/board?project=${fx.project}`);
    await expect(page.getByTestId("task-list")).toBeVisible();

    const groups = page.getByTestId("task-group");
    await expect(groups.first()).toBeVisible();

    // Acceptance 4: every row renders a derived SE-nn key, never the bare id.
    await expect(page.getByTestId("task-groups")).toContainText(/SE-\d{2,}/);
    await expect(page.getByTestId("task-groups")).not.toContainText(/tsk_/);

    // Acceptance 3: 需要你 is the first group and holds both the
    // interaction-parked parent and the owner-blocked failure.
    const attention = groups.first();
    await expect(attention).toHaveAttribute("data-kind", "attention");
    await expect(attention).toContainText("需要你");
    await expect(rowIn(attention, fx.blocked.id)).toHaveAttribute("data-needs-human", "1");
    await expect(rowIn(attention, fx.parent.id)).toHaveAttribute("data-needs-human", "1");

    // Acceptance 1: project groups follow, keyed project + Space + branch.
    // The parent's group is the wsp_e2e Space (one shared directory never
    // merges with another, D-024) and its header carries a buildSpaces()
    // blocked count; the launched pending approval contributes at least one.
    const parentProjectGroup = groups
      .filter({ has: rowIn(page, fx.child1.id) })
      .first();
    await expect(parentProjectGroup).toHaveAttribute("data-kind", "project");
    const blockedBadge = parentProjectGroup.locator("[data-testid=task-group-blocked]");
    await expect(blockedBadge).toContainText(/\d+\s*待处理/);
    await expect
      .poll(async () => parseInt((await blockedBadge.textContent()) ?? "0", 10), {
        timeout: 10_000,
      })
      .toBeGreaterThanOrEqual(1);

    // Acceptance 2: children nest under the parent with a ▸ N count.
    await expect(rowIn(parentProjectGroup, fx.parent.id)).toHaveAttribute("data-depth", "0");
    await expect(rowIn(parentProjectGroup, fx.parent.id)).toHaveAttribute(
      "data-has-children",
      "1",
    );
    await expect(
      rowIn(parentProjectGroup, fx.parent.id).locator("[data-testid=task-child-count]"),
    ).toHaveText("▸ 2");
    await expect(rowIn(parentProjectGroup, fx.child1.id)).toHaveAttribute("data-depth", "1");
    await expect(rowIn(parentProjectGroup, fx.child2.id)).toHaveAttribute("data-depth", "1");

    // Acceptance 6: the row carries its session count and a /s/:id link.
    await expect(rowIn(parentProjectGroup, fx.parent.id)).toContainText("1 会话");
    await expect(
      rowIn(parentProjectGroup, fx.parent.id).getByTestId("task-session-link"),
    ).toHaveAttribute("href", `/s/${fx.instanceId}`);

    // Acceptance 6: archived folds into its own trailing group and never
    // appears in an active group.
    await expect.poll(async () => groups.count()).toBeGreaterThan(1);
    const total = await groups.count();
    const archivedGroup = groups.nth(total - 1);
    await expect(archivedGroup).toHaveAttribute("data-kind", "archived");
    await expect(archivedGroup).toContainText("已归档");
    await expect(archivedGroup).toContainText(fx.archived.title);
    for (let i = 0; i < total - 1; i++) {
      await expect(groups.nth(i)).not.toContainText(fx.archived.title);
    }

    // Acceptance 5: the detail panel renders title, blocked reason, mandate.
    await rowIn(page, fx.blocked.id).first().click();
    await expect(page.getByTestId("task-detail-title")).toHaveText(fx.blocked.title);
    await expect(page.getByTestId("task-detail-blocked")).toContainText(
      `tlist supply blocked ${fx.suffix}`,
    );
    await expect(page.getByTestId("task-detail")).toContainText("Mandate");

    // A task with sessions offers the shared workbench surface.
    await rowIn(page, fx.parent.id).first().click();
    await expect(page.getByTestId("task-detail-open-session")).toHaveAttribute(
      "href",
      `/s/${fx.instanceId}`,
    );

    // The task search narrows the list.
    await page.getByTestId("task-search").fill("tlist-nonexistent-zzz");
    await expect(page.getByTestId("task-list-empty")).toBeVisible();
    await page.getByTestId("task-search").fill("");

    await shot(page, "task-model-5-board-1440.png");
  });
});

test.describe("task layer at 390 phone (/m)", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test("stacks the task groups on the home and opens the shared session", async ({ page }) => {
    await login(page);
    const fx = await makeFixture(page);

    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    const layer = page.getByTestId("home-tasks");
    await expect(layer).toBeVisible();

    // The layer is the same read-only projection: 需要你 first.
    const taskGroups = layer.getByTestId("task-group");
    await expect(taskGroups.first()).toHaveAttribute("data-kind", "attention");
    await expect(taskGroups.first()).toContainText(fx.blocked.title);

    // Parent/child nesting + SE-nn key, no bare task id.
    await expect(rowIn(layer, fx.parent.id).first()).toHaveAttribute("data-depth", "0");
    await expect(rowIn(layer, fx.child1.id).first()).toHaveAttribute("data-depth", "1");
    await expect(layer).toContainText(/SE-\d{2,}/);
    await expect(layer).not.toContainText(/tsk_/);

    // Archive folds away inside the layer too.
    await expect.poll(async () => taskGroups.count()).toBeGreaterThan(1);
    const count = await taskGroups.count();
    await expect(taskGroups.nth(count - 1)).toHaveAttribute("data-kind", "archived");

    await shot(page, "task-model-5-home-390.png");

    // Acceptance 7: tapping a task row opens the shared /s/:id — no second
    // transcript route exists.
    await rowIn(layer, fx.parent.id).first().click();
    await page.waitForURL(new RegExp(`/s/${fx.instanceId}`), { timeout: 15_000 });
  });
});
