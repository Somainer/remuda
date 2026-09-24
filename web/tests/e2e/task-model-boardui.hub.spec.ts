import { expect, test, type Page, type Locator } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * Plan task-model task 6 (t-board-ui), D-050 §5/§9, ui-spec §2.9: the
 * desktop kanban driven against the fully composed Hub + the gated fake
 * Node (HUB_E2E_TASK_BIND=1 in crates/remuda-hub/examples/hub_e2e.rs).
 *
 * Coverage of the seven acceptance items:
 *  1. three columns consume GET /v1/board; archived is a fold behind a
 *     filter toggle, never a fourth/fifth column, and never changes state;
 *  2. a card lists its sessions (glyph + relative time), its footer shows
 *     the SE-nn key, the `default` config chip and 「与 N 个 task 共用」 for
 *     the directory shared by two tasks;
 *  3. a failed card carries the red badge + blockedReason and sits in the
 *     column placement implies (here in-progress), never folded into done;
 *  4. drag legality is precomputed: the pending card reaches in-progress
 *     through the pending→placed→running multi-hop, an unreachable column
 *     is disabled with a reason instead of 4xx-ing the drop, terminal cards
 *     are not draggable;
 *  5. the PATCH is Hub-gated on the dispatch grant: a 403 renders the
 *     refusal — the UI offered the move without assuming agents can never
 *     write;
 *  6. the card detail is a read-only preview (no composer on this surface)
 *     with 预览模式 and a link into the shared workbench /s/:id;
 *  7. compact widths do not clone the board: /board collapses onto /m
 *     (covered by the mobileRoute unit tests; at 1440 the board renders).
 *
 * Generic wording only; the reference kanban product is never named.
 */

test.describe.configure({ mode: "serial" });
// The branded bind workspace and the in-memory reuse directories exist on
// the fake Node only with HUB_E2E_TASK_BIND=1; a default full-suite run
// skips this spec (mirrors task-model-bind.hub.spec.ts / api-route idiom).
test.skip(
  process.env.HUB_E2E_TASK_BIND !== "1",
  "set HUB_E2E_TASK_BIND=1 for the task-model-boardui harness",
);

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

async function shot(page: Page, name: string) {
  // Evidence shots are an explicit opt-in; a default run must never write a
  // PNG (only committed screenshots are Remuda renders taken with
  // REMUDA_EVIDENCE=1). Mirrors m-keybar.hub.spec.ts.
  if (!evidence) return;
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

type TaskDoc = {
  id: string;
  title: string;
  state: string;
  archivedAt?: string | null;
  blockedReason?: string | null;
  workspaceBinding?: { mode: string; worktreeName?: string };
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

type Fixture = {
  suffix: string;
  project: string;
  projectName: string;
  host: string;
  todo: TaskDoc;
  running: TaskDoc;
  done: TaskDoc;
  failed: TaskDoc;
  archived: TaskDoc;
  sharedA: TaskDoc;
  sharedB: TaskDoc;
  instanceId: string;
};

const createdInstances: string[] = [];
let previousMaxInstances: number | undefined;
let patchedHost = "";

const cardIn = (scope: Page | Locator, id: string): Locator =>
  scope.locator(`[data-testid="board-card"][data-task-id="${id}"]`);
const column = (page: Page, name: string): Locator =>
  page.locator(`[data-testid="board-column"][data-column="${name}"]`);

/**
 * Drive the exact HTML5 drag event sequence the browser fires on a real
 * drag: dragstart (source) → dragenter/dragover (column) → drop → dragend,
 * sharing one DataTransfer. Playwright's CDP mouse cannot drive Chromium's
 * native drag controller deterministically — the move after dragstart
 * engages the controller and the next synthetic move stalls — so the events
 * are dispatched directly. They still run the app's real React drag
 * handlers, the per-card legality gate, and the real state-hop PATCHes.
 */
async function dispatchDrag(
  page: Page,
  cardId: string,
  targetColumn: "todo" | "in-progress" | "done",
): Promise<void> {
  await page.evaluate(
    ({ cardId, targetColumn }) =>
      new Promise<void>((resolve) => {
        const dt = new DataTransfer();
        const source = document.querySelector(
          `[data-testid="board-card"][data-task-id="${cardId}"]`,
        )!;
        const target = document.querySelector(
          `[data-testid="board-column"][data-column="${targetColumn}"]`,
        )!;
        const fire = (el: Element, type: string) =>
          el.dispatchEvent(
            new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: dt }),
          );
        fire(source, "dragstart");
        // Let React flush the dragId before the column reads its legality.
        window.setTimeout(() => {
          fire(target, "dragenter");
          fire(target, "dragover");
          window.setTimeout(() => {
            fire(target, "drop");
            if (source.isConnected) fire(source, "dragend");
            resolve();
          }, 60);
        }, 80);
      }),
    { cardId, targetColumn },
  );
}

async function makeFixture(page: Page): Promise<Fixture> {
  const suffix = Date.now().toString(36);
  const projectName = `tboardui ${suffix}`;

  const hosts = await apiJson<{ items?: { id?: string; label?: string; maxInstances?: number }[] }>(
    page,
    "GET",
    "/v1/hosts",
  );
  const hostRow = hosts.items?.find((item) => item.label === "e2e-fake-node");
  expect(hostRow?.id, "e2e-fake-node enrolled").toBeTruthy();
  const host = hostRow!.id!;
  patchedHost = host;
  if (previousMaxInstances === undefined) previousMaxInstances = hostRow.maxInstances ?? 8;
  await apiJson(page, "PATCH", `/v1/hosts/${host}`, { maxInstances: 64 });

  // The bind fixture announces one branded workspace (the legacy wsp_e2e
  // label cannot be a project member); discover it rather than guessing.
  const workspaceList = await apiJson<{ workspaces: { workspaceId: string }[] }>(
    page,
    "GET",
    `/v1/hosts/${host}/workspaces`,
  );
  const branded = /^wsp_[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
  const wsp = workspaceList.workspaces.find((entry) => branded.test(entry.workspaceId))?.workspaceId;
  expect(wsp, "branded bind workspace announced").toBeTruthy();

  const project = (await apiJson<{ id: string }>(page, "POST", "/v1/projects", { name: projectName })).id;
  await apiJson(page, "POST", `/v1/projects/${project}/members`, {
    hostId: host,
    workspaceId: wsp,
    role: "build",
  });

  const create = (label: string, binding?: Record<string, unknown>): Promise<TaskDoc> =>
    apiJson<TaskDoc>(page, "POST", "/v1/tasks", {
      projectId: project,
      title: `bui ${label} ${suffix}`,
      intent: `e2e board-ui intent ${label}`,
      ...(binding ? { workspaceBinding: binding } : {}),
    });
  const setState = (task: TaskDoc, state: string, reason?: string) =>
    apiJson(page, "PATCH", `/v1/tasks/${task.id}`, { state, ...(reason ? { reason } : {}) });

  // One card per column.
  const todo = await create("todo");
  const running = await create("running");
  await setState(running, "placed");
  await setState(running, "running");

  const done = await create("done");
  await setState(done, "placed");
  await setState(done, "running");
  await setState(done, "done");

  // Failed mid-flight: a dispatch placement (→ placed + placement ref),
  // then failed with an owner-facing reason → in-progress with a red badge.
  const failed = await create("failed");
  await apiJson(page, "POST", `/v1/tasks/${failed.id}/placements`, {
    kind: "dispatch",
    model: "e2e/auto",
    branch: `wt/e2e/${suffix}`,
  });
  await setState(failed, "failed", "worker exited 42");

  // One archived card (state untouched).
  const archived = await create("archived");
  await apiJson<TaskDoc>(page, "POST", `/v1/tasks/${archived.id}/archive`, {});

  // One directory shared by two tasks (reuse sibling on the bind fixture).
  const sharedBinding = { mode: "reuse", hostId: host, workspaceId: wsp, worktreeName: "agent-one" };
  const sharedA = await create("share-a", sharedBinding);
  const sharedB = await create("share-b", sharedBinding);

  // One live session for the to-do card (normal launch on the default e2e
  // workspace; the task needs no binding to aggregate a session).
  const launched = await apiJson<{ instance?: { instanceId?: string; id?: string } }>(
    page,
    "POST",
    "/v1/instances",
    {
      hostId: host,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      taskId: todo.id,
      prompt: `BUI board session ${suffix}`,
    },
  );
  const instanceId = launched.instance?.instanceId ?? launched.instance?.id;
  expect(instanceId).toBeTruthy();
  createdInstances.push(instanceId!);

  return {
    suffix,
    project,
    projectName,
    host,
    todo,
    running,
    done,
    failed,
    archived,
    sharedA,
    sharedB,
    instanceId: instanceId!,
  };
}

test.afterAll(async ({ browser }) => {
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    await login(page);
    for (const id of createdInstances.splice(0)) {
      await page.request.fetch(`/v1/instances/${id}?force=1`, { method: "DELETE" }).catch(() => undefined);
    }
    if (patchedHost && previousMaxInstances !== undefined) {
      await page.request
        .patch(`/v1/hosts/${patchedHost}`, { data: { maxInstances: previousMaxInstances } })
        .catch(() => undefined);
    }
  } finally {
    await context.close();
  }
});

test.describe("desktop board at 1440 (HUB_E2E_TASK_BIND=1)", () => {
  test.use({ viewport: { width: 1440, height: 900 } });

  test("three columns, sessions, sharing, failed badge, legal drags and the read-only preview", async ({
    page,
  }) => {
    await login(page);
    const fx = await makeFixture(page);

    // The sidebar project rows are the board's project filter (UO-2a, formerly
    // the top-bar switcher); global would show every project on the shared
    // fake hub.
    await page.goto("/board");
    await page.getByTestId("sidebar-project-row").filter({ hasText: fx.projectName }).click();
    await expect(page.getByTestId("board-page")).toBeVisible();

    // Acceptance 1: exactly three work columns consume the projection.
    const columns = page.getByTestId("board-column");
    await expect(columns).toHaveCount(3);
    await expect(column(page, "todo")).toContainText("待办");
    await expect(column(page, "in-progress")).toContainText("进行中");
    await expect(column(page, "done")).toContainText("已完成");

    // One card per column (plus the two sharing cards in to-do).
    await expect(cardIn(column(page, "todo"), fx.todo.id)).toBeVisible();
    await expect(cardIn(column(page, "todo"), fx.sharedA.id)).toBeVisible();
    await expect(cardIn(column(page, "todo"), fx.sharedB.id)).toBeVisible();
    await expect(cardIn(column(page, "in-progress"), fx.running.id)).toBeVisible();
    await expect(cardIn(column(page, "done"), fx.done.id)).toBeVisible();

    // Acceptance 2: SE-nn key (never the bare task id), session row with a
    // link into the shared /s/:id workbench, and the default config chip.
    const todoCard = cardIn(page, fx.todo.id);
    await expect(todoCard).toContainText(/SE-\d{2,}/);
    await expect(todoCard).not.toContainText(/tsk_/);
    const sessionLine = todoCard.getByTestId("board-session");
    await expect(sessionLine).toHaveAttribute("href", `/s/${fx.instanceId}`);
    await expect(todoCard.getByTestId("board-config")).toHaveText("default");

    // Acceptance 2: the shared directory footer on both tasks.
    await expect(cardIn(page, fx.sharedA.id).getByTestId("board-shared")).toHaveText(
      "与 2 个 task 共用",
    );
    await expect(cardIn(page, fx.sharedB.id).getByTestId("board-shared")).toHaveText(
      "与 2 个 task 共用",
    );

    // Acceptance 3: failed card sits in in-progress by its placement and
    // carries the red badge + reason; it is never in the done column.
    const failedCard = cardIn(column(page, "in-progress"), fx.failed.id);
    await expect(failedCard).toBeVisible();
    await expect(failedCard).toHaveAttribute("data-failed", "1");
    await expect(failedCard.getByTestId("board-failed-badge")).toContainText("worker exited 42");
    await expect(cardIn(column(page, "done"), fx.failed.id)).toHaveCount(0);

    // Terminal cards are not draggable at all (no silent 4xx path).
    expect(await cardIn(page, fx.done.id).getAttribute("draggable")).toBe("false");
    expect(await failedCard.getAttribute("draggable")).toBe("false");

    // Archived stays out of the work columns until the filter opens.
    for (const name of ["todo", "in-progress", "done"] as const) {
      await expect(cardIn(column(page, name), fx.archived.id)).toHaveCount(0);
    }
    await page.getByTestId("board-archive-toggle").check();
    const fold = page.getByTestId("board-archive-fold");
    await expect(fold).toBeVisible();
    await expect(cardIn(fold, fx.archived.id)).toBeVisible();

    // Acceptance 4: to-do → in-progress runs the pending→placed→running
    // multi-hop through legal PATCHes.
    await dispatchDrag(page, fx.todo.id, "in-progress");
    await expect
      .poll(
        async () => cardIn(column(page, "in-progress"), fx.todo.id).count(),
        { timeout: 15_000 },
      )
      .toBe(1);
    const moved = await apiJson<TaskDoc>(page, "GET", `/v1/tasks/${fx.todo.id}`);
    expect(moved.state).toBe("running");
    await expect(cardIn(column(page, "todo"), fx.todo.id)).toHaveCount(0);

    // running → done is the single legal hop.
    await dispatchDrag(page, fx.todo.id, "done");
    await expect
      .poll(async () => cardIn(column(page, "done"), fx.todo.id).count(), { timeout: 15_000 })
      .toBe(1);

    // Acceptance 4: an unreachable column is disabled with a reason rather
    // than rejecting the drop. Drive the dataTransfer the browser uses so
    // the dragover wiring runs against the real app.
    const disabled = await page.evaluate(
      async ([sharedId]) => {
        const dt = new DataTransfer();
        const fire = (selector: string, type: string) => {
          const el = document.querySelector(selector)!;
          el.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: dt }));
        };
        const cardSelector = `[data-testid="board-card"][data-task-id="${sharedId}"]`;
        fire(cardSelector, "dragstart");
        // Let React flush the dragstart state before the dragover reads it.
        await new Promise((resolve) => setTimeout(resolve, 100));
        fire('[data-testid="board-column"][data-column="done"]', "dragover");
        await new Promise((resolve) => setTimeout(resolve, 50));
        const col = document.querySelector(
          '[data-testid="board-column"][data-column="done"]',
        )!;
        const reason = col.querySelector('[data-testid="board-drop-reason"]')?.textContent ?? null;
        const result = { drop: col.getAttribute("data-drop"), reason };
        fire(cardSelector, "dragend");
        return result;
      },
      [fx.sharedA.id] as const,
    );
    expect(disabled.drop).toBe("disabled");
    expect(disabled.reason).toBeTruthy();

    // Acceptance 5: the move is Hub-gated. A 403 (e.g. a caller without the
    // dispatch grant) renders the refusal — the board never hid the move
    // based on caller identity.
    await page.route("**/v1/tasks/*", async (route) => {
      if (route.request().method() !== "PATCH") return route.continue();
      await route.fulfill({
        status: 403,
        contentType: "application/json",
        body: JSON.stringify({ error: "dispatch grant required" }),
      });
    });
    await dispatchDrag(page, fx.running.id, "done");
    await expect(page.getByTestId("board-move-error")).toContainText(/403|Dispatch|授权/);
    await page.unroute("**/v1/tasks/*");
    // The refusal left the card where it was.
    await expect(cardIn(column(page, "in-progress"), fx.running.id)).toBeVisible();

    // Acceptance 6: the card detail is an explicit read-only preview with a
    // link into the shared workbench. There is no composer on /board.
    await cardIn(page, fx.todo.id).getByTestId("board-card-open").click();
    await expect(page.getByTestId("board-preview-banner")).toContainText("预览模式");
    await expect(page.getByTestId("board-preview-open")).toHaveAttribute(
      "href",
      `/s/${fx.instanceId}`,
    );
    await expect(page.getByTestId("task-detail-title")).toContainText("bui todo");

    // Evidence: columns with the failed badge in 进行中, the sharing footer
    // in 待办, the archive fold and the preview banner all visible.
    await shot(page, "task-model-6-board-1440.png");
  });
});
