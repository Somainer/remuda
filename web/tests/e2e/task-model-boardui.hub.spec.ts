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
  blocked: TaskDoc;
  landed: TaskDoc;
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

  // A live session parked on a pending approval: the card's 需要你 signal.
  const blocked = await create("needs-you");
  const blockedLaunch = await apiJson<{ instance?: { instanceId?: string; id?: string } }>(
    page,
    "POST",
    "/v1/instances",
    {
      hostId: host,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      taskId: blocked.id,
      prompt: `BUI mhome-blocked approval gate ${suffix}`,
    },
  );
  const blockedInstanceId = blockedLaunch.instance?.instanceId ?? blockedLaunch.instance?.id;
  expect(blockedInstanceId).toBeTruthy();
  createdInstances.push(blockedInstanceId!);
  await expect
    .poll(
      async () => {
        const list = await apiJson<{ items?: { instanceId?: string; state?: string }[] }>(
          page,
          "GET",
          `/v1/interactions?instanceId=${blockedInstanceId}`,
        );
        return (list.items ?? []).some(
          (item) => item.instanceId === blockedInstanceId && item.state === "pending",
        );
      },
      { timeout: 20_000 },
    )
    .toBe(true);

  // A genuinely landed card: done through the state machine, then the
  // gate/land record writes the sha (the only path to 已合入).
  const landed = await create("landed");
  await setState(landed, "placed");
  await setState(landed, "running");
  await setState(landed, "done");
  await apiJson(page, "POST", `/v1/tasks/${landed.id}/land`, {
    sha: "abcdef0123456789abcdef",
  });

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
    blocked,
    landed,
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

  // Shared across the serial tests in this describe (the project poll test
  // only needs a project id to deep-link).
  let fixture: Fixture | null = null;

  test("three columns, card states, legal drags and the overlay preview", async ({ page }) => {
    await login(page);
    const fx = await makeFixture(page);
    fixture = fx;

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

    // UO-8: the six card states. Failed keeps its badge; the rest render the
    // one signal line (needs-you / landed / unlanded / derived next step).
    const blockedCard = cardIn(column(page, "todo"), fx.blocked.id);
    await expect(blockedCard.getByTestId("board-signal")).toHaveAttribute(
      "data-kind",
      "needs-human",
    );
    await expect(blockedCard.getByTestId("board-signal")).toContainText("需要你处理");

    const landedCard = cardIn(column(page, "done"), fx.landed.id);
    await expect(landedCard.getByTestId("board-signal")).toHaveAttribute("data-kind", "landed");
    await expect(landedCard.getByTestId("board-signal")).toContainText("abcdef0");
    await expect(landedCard.getByTestId("board-signal")).toContainText("已合入");

    const doneCard = cardIn(column(page, "done"), fx.done.id);
    await expect(doneCard.getByTestId("board-signal")).toHaveAttribute("data-kind", "unlanded");
    await expect(doneCard.getByTestId("board-signal")).toContainText("尚未合入");

    await expect(
      cardIn(column(page, "in-progress"), fx.running.id).getByTestId("board-signal"),
    ).toHaveAttribute("data-kind", "next-step");
    await expect(
      cardIn(column(page, "todo"), fx.sharedA.id).getByTestId("board-signal"),
    ).toContainText("待派发");

    // UO-8: the board exposes no land entry — 已合入/尚未合入 are status
    // lines, never a button or link. (Scope to the board surface and match
    // the action word, not task titles, which may contain "landed".)
    const boardSurface = page.getByTestId("board-page");
    expect(await boardSurface.locator('[data-testid*="land" i]').count()).toBe(0);
    for (const role of ["button", "link"] as const) {
      expect(
        await boardSurface.getByRole(role, { name: /合入|land\b/i }).count(),
        `no ${role} offers a land action`,
      ).toBe(0);
    }

    // UO-8: three tracks at 1440. Sidebar 248 + rail 272 + 32px gutters leave
    // ~269px per column (the exact pixel depends on the scroller width).
    const widths = await page
      .getByTestId("board-column")
      .evaluateAll((nodes) => nodes.map((node) => (node as HTMLElement).getBoundingClientRect().width));
    expect(widths).toHaveLength(3);
    for (const width of widths) {
      expect(Math.round(width), `column width ${width} ≈ 269`).toBeGreaterThanOrEqual(255);
      expect(Math.round(width)).toBeLessThanOrEqual(285);
    }
    console.log(`UO-8 column widths at 1440: ${widths.map((w) => Math.round(w)).join(", ")}px`);

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

    // Acceptance 6 + UO-8: the detail is an overlay drawer, not a fixed
    // right rail — width min(400px, 100% - 48px), raised surface over a
    // scrim, 预览模式 banner, link into the shared workbench.
    const todoOpen = cardIn(page, fx.todo.id).getByTestId("board-card-open");
    await todoOpen.click();
    const scrim = page.getByTestId("board-preview-scrim");
    const drawer = page.getByRole("dialog", { name: "任务预览" });
    await expect(scrim).toBeVisible();
    await expect(drawer).toBeVisible();
    const drawerBox = await drawer.boundingBox();
    expect(drawerBox).toBeTruthy();
    expect(Math.round(drawerBox!.width)).toBe(400);
    expect(Math.round(drawerBox!.height)).toBeGreaterThan(600);
    // The drawer overlays the board rather than participating in its grid.
    expect(drawerBox!.x).toBeGreaterThan(1440 - 400 - 24);

    await expect(page.getByTestId("board-preview-banner")).toContainText("预览模式");
    await expect(page.getByTestId("board-preview-open")).toHaveAttribute(
      "href",
      `/s/${fx.instanceId}`,
    );
    await expect(page.getByTestId("task-detail-title")).toContainText("bui todo");
    // Focus moved into the drawer while open.
    await expect(drawer).toBeFocused();

    // Esc closes the overlay and returns focus to the originating card.
    await page.keyboard.press("Escape");
    await expect(drawer).toHaveCount(0);
    await expect(scrim).toHaveCount(0);
    await expect
      .poll(
        () =>
          page.evaluate(() => {
            const el = document.activeElement;
            return {
              testid: el?.getAttribute("data-testid"),
              task: el?.closest('[data-testid="board-card"]')?.getAttribute("data-task-id"),
            };
          }),
        { timeout: 2_000 },
      )
      .toEqual({ testid: "board-card-open", task: fx.todo.id });

    // Clicking the scrim also dismisses; the drawer itself stops the click.
    await todoOpen.click();
    await expect(drawer).toBeVisible();
    await scrim.click({ position: { x: 4, y: 4 } });
    await expect(drawer).toHaveCount(0);

    // Evidence: columns with the failed badge in 进行中, the sharing footer
    // in 待办, the archive fold and the preview banner all visible.
    await shot(page, "task-model-6-board-1440.png");
  });

  test("fetches /v1/projects on mount only, never on a poll interval", async ({ page }) => {
    test.setTimeout(75_000);
    await login(page);
    const fx = fixture ?? (await makeFixture(page));
    const requestedAt: number[] = [];
    page.on("request", (request) => {
      if (new URL(request.url()).pathname.endsWith("/v1/projects")) {
        requestedAt.push(Date.now());
      }
    });

    await page.goto(`/board?project=${fx.project}`);
    await expect(page.getByTestId("board-page")).toBeVisible();
    // The mount burst (Shell directory + board rail names).
    await expect.poll(() => requestedAt.length).toBeGreaterThan(0);
    await page.waitForTimeout(2_000);
    const burst = requestedAt.length;

    // Wait past the old 30s project interval; the board's own 5s poll is
    // /v1/board, so the project count must not move.
    await page.waitForTimeout(31_000);
    expect(requestedAt.length).toBe(burst);
  });

  test("preview overlay: covered region is inert, polls do not refocus, move survives, Esc keeps the sidebar menu", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    await login(page);
    const fx = await makeFixture(page);
    await page.goto("/board");
    await page.getByTestId("sidebar-project-row").filter({ hasText: fx.projectName }).click();
    await expect(page.getByTestId("board-page")).toBeVisible();
    // Wait for the first projection to populate the columns (board visibility
    // is true before the first /v1/board arrives); test 1 is slower and hid
    // the race, the isolated focus run exposed it.
    await expect(cardIn(page, fx.todo.id).getByTestId("board-card-open")).toBeVisible();

    const focusInfo = () =>
      page.evaluate(() => {
        const el = document.activeElement as HTMLElement | null;
        return {
          testid: el?.getAttribute("data-testid") ?? null,
          // A board card or a rail task row carries the id.
          task:
            (el?.closest('[data-task-id]') as HTMLElement | null)?.getAttribute("data-task-id") ??
            null,
          column: el?.closest('[data-testid="board-card"]')?.getAttribute("data-column") ?? null,
          inInert: !!el?.closest("[inert]"),
        };
      });
    const COVERED = new Set([
      "board-card-open",
      "board-card-archive",
      "board-session",
      "board-archive-toggle",
      "board-index-open",
    ]);

    // Open the shared pending card's preview.
    await cardIn(page, fx.todo.id).getByTestId("board-card-open").click();
    const drawer = page.getByRole("dialog", { name: "任务预览" });
    await expect(drawer).toBeVisible();
    await expect(drawer).toBeFocused();

    // ── #1 inert covered region + forward/reverse Tab ──────────────────────
    // The covered region cannot be focused even by an explicit .focus().
    expect(
      await page.evaluate(() => {
        const el = document.querySelector<HTMLElement>('[data-testid="board-card-archive"]');
        el?.focus();
        return {
          testid: (document.activeElement as HTMLElement | null)?.getAttribute("data-testid"),
          inert: !!el?.closest("[inert]"),
        };
      }),
    ).toEqual({ testid: null, inert: true });
    // Refocus the drawer for the keyboard walk.
    await drawer.focus();

    // Forward Tab walks the drawer only (never a covered control).
    await page.keyboard.press("Tab");
    let info = await focusInfo();
    expect(info.testid).toBe("board-preview-open");
    expect(COVERED.has(info.testid ?? "")).toBe(false);
    await page.keyboard.press("Tab");
    info = await focusInfo();
    expect(info.testid).toBe("task-detail-open-session");
    expect(info.inInert).toBe(false);

    // Reverse Tab returns through the banner link into the task rail — never
    // into a covered card behind the scrim.
    await page.keyboard.press("Shift+Tab");
    expect((await focusInfo()).testid).toBe("board-preview-open");
    let reachedRail = false;
    for (let i = 0; i < 10; i += 1) {
      await page.keyboard.press("Shift+Tab");
      info = await focusInfo();
      expect(info.inInert, "reverse Tab never enters the covered region").toBe(false);
      expect(
        COVERED.has(info.testid ?? ""),
        `reverse Tab never focuses a covered control (got ${info.testid})`,
      ).toBe(false);
      if (info.testid === "task-row" || info.testid === "task-session-link" || info.testid === "task-search") {
        reachedRail = true;
        break;
      }
    }
    expect(reachedRail, "Shift+Tab from the drawer reaches the task rail").toBe(true);

    // ── #2 a poll tick must not refocus the drawer ─────────────────────────
    await page.focus('[data-testid="task-search"]');
    expect((await focusInfo()).testid).toBe("task-search");
    await page.waitForTimeout(6_000); // ≥ one 5s /v1/board poll
    expect((await focusInfo()).testid).toBe("task-search");

    await page.focus('[data-testid="board-preview-open"]');
    await page.waitForTimeout(6_000);
    expect((await focusInfo()).testid).toBe("board-preview-open");

    // ── #3 Esc restore after the open task moves columns ──────────────────
    await dispatchDrag(page, fx.todo.id, "in-progress");
    await expect
      .poll(
        async () => cardIn(page, fx.todo.id).getAttribute("data-column"),
        { timeout: 15_000 },
      )
      .toBe("in-progress");

    await page.keyboard.press("Escape");
    await expect(drawer).toHaveCount(0);
    await expect
      .poll(focusInfo, { timeout: 2_000 })
      .toEqual(
        expect.objectContaining({
          testid: "board-card-open",
          task: fx.todo.id,
          column: "in-progress",
        }),
      );

    // ── #2 (rail-origin) Esc from a rail-opened preview returns to the RAIL ──
    // Open via a rail row click, not a card button; the restore target must
    // be that rail row even though a card for the task exists on the board.
    const railRowFor = (id: string) =>
      page.locator(`[data-testid="task-row"][data-task-id="${id}"]`);
    await railRowFor(fx.running.id).click();
    await expect(drawer).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(drawer).toHaveCount(0);
    await expect
      .poll(focusInfo, { timeout: 2_000 })
      .toEqual(
        expect.objectContaining({ testid: "task-row", task: fx.running.id }),
      );

    // ── #4 Esc in the drawer must not close the sidebar 管理 menu ──────────
    await page.getByTestId("sidebar-admin").click();
    const adminMenu = page.getByRole("menu", { name: "管理" });
    await expect(adminMenu).toBeVisible();
    // Open a preview from the rail via keyboard (no outside pointerdown that
    // would dismiss the menu): focus a rail row, then Enter.
    await page
      .locator('[data-testid="task-list"] [data-testid="task-row"]')
      .first()
      .evaluate((el) => (el as HTMLElement).focus());
    await page.keyboard.press("Enter");
    await expect(drawer).toBeVisible();
    await expect(adminMenu).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(drawer).toHaveCount(0);
    await expect(adminMenu).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(adminMenu).toHaveCount(0);
  });

  test("preview at 900px covers the folded rail overlay: inert rows and scrim click", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 900, height: 900 });
    await login(page);
    const fx = await makeFixture(page);
    await page.goto(`/board?project=${fx.project}`);
    await expect(page.getByTestId("board-page")).toBeVisible();

    // Below 1024 the rail is folded behind a 清单 button.
    const rail = page.getByTestId("task-list");
    await expect(rail).not.toBeVisible();
    await page.getByTestId("board-index-open").click();
    await expect(rail).toBeVisible();
    expect(await rail.getAttribute("inert")).toBeNull();

    // Open a preview from the rail. The overlay rail is now UNDER the scrim
    // (z-31 < scrim z-40) and must be inert.
    const firstRailRow = page.locator('[data-testid="task-list"] [data-testid="task-row"]').first();
    const railTaskId = (await firstRailRow.getAttribute("data-task-id")) ?? "";
    await firstRailRow.click();
    const drawer = page.getByRole("dialog", { name: "任务预览" });
    await expect(drawer).toBeVisible();
    await expect(rail).toHaveAttribute("inert", "");

    const inRail = async () =>
      page.evaluate(() => {
        const el = document.activeElement;
        return !!el?.closest('[data-testid="task-list"]');
      });

    // Forward and reverse Tab from the drawer never reach the covered rail.
    await drawer.focus();
    for (let i = 0; i < 12; i += 1) {
      await page.keyboard.press("Tab");
      expect(await inRail()).toBe(false);
    }
    for (let i = 0; i < 12; i += 1) {
      await page.keyboard.press("Shift+Tab");
      expect(await inRail()).toBe(false);
    }
    // Even an explicit .focus() cannot enter the inert rail.
    expect(
      await page.evaluate((id) => {
        const el = document.querySelector<HTMLElement>(
          `[data-testid="task-row"][data-task-id="${id}"]`,
        );
        el?.focus();
        return {
          focusedInRail: !!document.activeElement?.closest('[data-testid="task-list"]'),
          inert: !!el?.closest("[inert]"),
        };
      }, railTaskId),
    ).toEqual({ focusedInRail: false, inert: true });

    // A pointer click on the covered rail position hits the scrim and
    // dismisses the preview (it never opens a different task).
    const rowBox = await firstRailRow.boundingBox();
    expect(rowBox).toBeTruthy();
    await page.mouse.click(rowBox!.x + rowBox!.width / 2, rowBox!.y + rowBox!.height / 2);
    await expect(drawer).toHaveCount(0);
    // Focus restored to the rail row that opened the preview (rail origin).
    await expect
      .poll(
        () =>
          page.evaluate(() => ({
            testid: (document.activeElement as HTMLElement | null)?.getAttribute("data-testid"),
            task:
              (document.activeElement as HTMLElement | null)
                ?.closest('[data-testid="task-row"]')
                ?.getAttribute("data-task-id") ?? null,
          })),
        { timeout: 2_000 },
      )
      .toEqual(expect.objectContaining({ testid: "task-row", task: railTaskId }));
  });
});
