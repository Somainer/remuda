import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Plan task-model task 4 (t-board-api), D-050: the read-only board
 * projection against the fully composed Hub (in-process Hub + enrolled fake
 * Node in crates/remuda-hub/examples/hub_e2e.rs). The board never opens a
 * Node RPC itself — this proves the route and projection on the live hub.
 *
 * Coverage:
 * - the eight ledger states project onto 待办/进行中/已完成 plus an archive
 *   group; failed is placed by placement presence, never a fifth column;
 * - archiving stamps archivedAt without changing state (re-archive → 409);
 * - column drags are legal set_task_state hops: the shortcut
 *   pending → running is refused 409 by the state machine while the
 *   pending → placed → running sequence moves the card across columns;
 * - the done column is not the dependency gate: a done-but-unlanded
 *   upstream keeps the edge locked until a real land;
 * - cards carry the display-only per-project SE-nn key.
 *
 * Generic wording only: the reference kanban product is never named.
 */

test.describe.configure({ mode: "serial" });

type TaskDoc = {
  id: string;
  state: string;
  archivedAt?: string | null;
  blockedReason?: string | null;
  boardColumn?: string;
  displayKey?: string;
};

type BoardResponse = {
  project: string | null;
  columns: Record<string, TaskDoc[]>;
};

async function api(page: Page, method: string, path: string, body?: unknown) {
  const response = await page.request.fetch(path, {
    method,
    data: body,
    headers: body ? { "content-type": "application/json" } : undefined,
  });
  return response;
}

async function apiJson<T>(page: Page, method: string, path: string, body?: unknown): Promise<T> {
  const response = await api(page, method, path, body);
  expect(response.ok(), `${method} ${path}: ${await response.text()}`).toBe(true);
  return (await response.json()) as T;
}

async function createTask(page: Page, project: string, title: string): Promise<TaskDoc> {
  return apiJson<TaskDoc>(page, "POST", "/v1/tasks", {
    projectId: project,
    title,
    intent: `e2e board intent: ${title}`,
  });
}

async function setState(page: Page, id: string, state: string, reason?: string) {
  return api(page, "PATCH", `/v1/tasks/${id}`, { state, reason });
}

async function boardFor(page: Page, project: string): Promise<BoardResponse> {
  return apiJson<BoardResponse>(page, "GET", `/v1/board?project=${project}`);
}

function ids(cards: TaskDoc[] | undefined): string[] {
  return (cards ?? []).map((card) => card.id);
}

test("board projects the ledger states onto three columns plus an archive group", async ({
  page,
}) => {
  await login(page);

  const suffix = Date.now().toString(36);
  const project = (
    await apiJson<{ id: string }>(page, "POST", "/v1/projects", {
      name: `board e2e ${suffix}`,
    })
  ).id;

  // To-do states.
  const pending = await createTask(page, project, `pending ${suffix}`);
  const deferred = await createTask(page, project, `deferred ${suffix}`);
  expect((await setState(page, deferred.id, "deferred")).status()).toBe(200);
  const parked = await createTask(page, project, `parked ${suffix}`);
  for (const state of ["placed", "running", "parked"]) {
    expect((await setState(page, parked.id, state)).status()).toBe(200);
  }
  const placed = await createTask(page, project, `placed ${suffix}`);
  expect((await setState(page, placed.id, "placed")).status()).toBe(200);

  // In-progress states.
  const running = await createTask(page, project, `running ${suffix}`);
  for (const state of ["placed", "running"]) {
    expect((await setState(page, running.id, state)).status()).toBe(200);
  }
  const stalled = await createTask(page, project, `stalled ${suffix}`);
  for (const state of ["placed", "running", "stalled"]) {
    expect((await setState(page, stalled.id, state)).status()).toBe(200);
  }

  // Failed before dispatch: no placement → to-do, with a blocked reason.
  const failedEarly = await createTask(page, project, `failed early ${suffix}`);
  expect(
    (await setState(page, failedEarly.id, "failed", "supply exhausted")).status(),
  ).toBe(200);

  // Failed after a dispatch placement row → in-progress, badge + reason.
  const failedMid = await createTask(page, project, `failed mid ${suffix}`);
  await apiJson(page, "POST", `/v1/tasks/${failedMid.id}/placements`, {
    kind: "dispatch",
    model: "e2e/auto",
    branch: `wt/e2e/${suffix}`,
  });
  expect(
    (await setState(page, failedMid.id, "failed", "worker exited 42")).status(),
  ).toBe(200);

  // A done card for the done column.
  const done = await createTask(page, project, `done ${suffix}`);
  for (const state of ["placed", "running", "done"]) {
    expect((await setState(page, done.id, state)).status()).toBe(200);
  }

  const board = await boardFor(page, project);
  expect(board.project).toBe(project);
  const todo = ids(board.columns.todo);
  expect(todo).toContain(pending.id);
  expect(todo).toContain(deferred.id);
  expect(todo).toContain(parked.id);
  expect(todo).toContain(placed.id);
  expect(todo).toContain(failedEarly.id);

  const inProgress = ids(board.columns["in-progress"]);
  expect(inProgress).toContain(running.id);
  expect(inProgress).toContain(stalled.id);
  expect(inProgress).toContain(failedMid.id);

  const midCard = board.columns["in-progress"].find((card) => card.id === failedMid.id)!;
  expect(midCard.state).toBe("failed");
  expect(midCard.boardColumn).toBe("in-progress");
  expect(midCard.blockedReason).toBe("worker exited 42");
  // Failed never folds into the done column.
  expect(ids(board.columns.done)).toEqual([done.id]);

  // The display-only per-project key is present on every card (SE-nn).
  for (const card of [...board.columns.todo, ...board.columns["in-progress"], ...board.columns.done]) {
    expect(card.displayKey).toMatch(/^SE-\d{2,}$/);
    expect(card.boardColumn).toBeTruthy();
  }
});

test("archiving moves the card to the archive group without changing state", async ({ page }) => {
  await login(page);

  const suffix = Date.now().toString(36);
  const project = (
    await apiJson<{ id: string }>(page, "POST", "/v1/projects", {
      name: `board archive e2e ${suffix}`,
    })
  ).id;
  const task = await createTask(page, project, `archive me ${suffix}`);
  for (const state of ["placed", "running", "stalled"]) {
    expect((await setState(page, task.id, state)).status()).toBe(200);
  }

  const archived = await apiJson<TaskDoc>(page, "POST", `/v1/tasks/${task.id}/archive`);
  expect(archived.state).toBe("stalled", "archiving never moves the ledger state");
  expect(archived.archivedAt).toBeTruthy();

  const board = await boardFor(page, project);
  expect(ids(board.columns.archived)).toContain(task.id);
  expect(ids(board.columns["in-progress"])).not.toContain(task.id);
  const card = board.columns.archived.find((item) => item.id === task.id)!;
  expect(card.state).toBe("stalled");

  // The flag is one-way: a second archive is a conflict, not an overwrite.
  const again = await api(page, "POST", `/v1/tasks/${task.id}/archive`);
  expect(again.status()).toBe(409);
});

test("column drags reuse legal state hops and reject illegal shortcuts whole", async ({
  page,
}) => {
  await login(page);

  const suffix = Date.now().toString(36);
  const project = (
    await apiJson<{ id: string }>(page, "POST", "/v1/projects", {
      name: `board moves e2e ${suffix}`,
    })
  ).id;
  const pending = await createTask(page, project, `multi hop ${suffix}`);

  // The column-to-column shortcut skips the legal edge: the state machine
  // refuses it (whole-move rejection — no partial state is written).
  const shortcut = await setState(page, pending.id, "running");
  expect(shortcut.status()).toBe(409);
  let board = await boardFor(page, project);
  expect(ids(board.columns.todo)).toContain(pending.id);

  // The legal multi-hop sequence moves the card to-do → in-progress.
  expect((await setState(page, pending.id, "placed")).status()).toBe(200);
  expect((await setState(page, pending.id, "running")).status()).toBe(200);
  board = await boardFor(page, project);
  expect(ids(board.columns["in-progress"])).toContain(pending.id);
  expect(ids(board.columns.todo)).not.toContain(pending.id);

  // No stalled→done edge exists: a stalled card completes only through the
  // stalled→running→done multi-hop. The shortcut is refused, then both legal
  // hops land it in the done column.
  const stalled = await createTask(page, project, `stalled hop ${suffix}`);
  for (const state of ["placed", "running", "stalled"]) {
    expect((await setState(page, stalled.id, state)).status()).toBe(200);
  }
  expect((await setState(page, stalled.id, "done")).status()).toBe(409);
  expect((await setState(page, stalled.id, "running")).status()).toBe(200);
  expect((await setState(page, stalled.id, "done")).status()).toBe(200);
  board = await boardFor(page, project);
  expect(ids(board.columns.done)).toContain(stalled.id);

  // Terminal states cannot be dragged: failed is refused any transition.
  const failed = await createTask(page, project, `terminal ${suffix}`);
  expect((await setState(page, failed.id, "failed")).status()).toBe(200);
  expect((await setState(page, failed.id, "running")).status()).toBe(409);
});

test("the done column never unlocks dependencies; only a landed sha does", async ({ page }) => {
  await login(page);

  const suffix = Date.now().toString(36);
  const project = (
    await apiJson<{ id: string }>(page, "POST", "/v1/projects", {
      name: `board deps e2e ${suffix}`,
    })
  ).id;
  const dep = await createTask(page, project, `api first ${suffix}`);
  const dependent = await apiJson<TaskDoc>(page, "POST", "/v1/tasks", {
    projectId: project,
    title: `cli second ${suffix}`,
    intent: "needs the api",
    deps: [{ taskId: dep.id, note: "API first" }],
  });

  // Mark the upstream done but never land: the card sits in the done column
  // while the dependent's edge stays locked.
  for (const state of ["placed", "running", "done"]) {
    expect((await setState(page, dep.id, state)).status()).toBe(200);
  }
  const board = await boardFor(page, project);
  expect(ids(board.columns.done)).toContain(dep.id);
  const view = await apiJson<{ lockedDeps: string[] }>(
    page,
    "GET",
    `/v1/tasks/${dependent.id}`,
  );
  expect(view.lockedDeps).toEqual([dep.id]);

  // The real gate/land step is the only unlock.
  const landed = await apiJson<TaskDoc>(page, "POST", `/v1/tasks/${dep.id}/land`, {
    sha: "abcdef0123456789",
  });
  expect(landed.state).toBe("done");
  const after = await apiJson<{ lockedDeps: string[] }>(
    page,
    "GET",
    `/v1/tasks/${dependent.id}`,
  );
  expect(after.lockedDeps).toEqual([]);
});
