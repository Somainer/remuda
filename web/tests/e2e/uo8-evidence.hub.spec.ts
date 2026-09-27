import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * UO-8 evidence (D-050 §2.9, D-053): the desktop kanban at 1440 — six card
 * states (failed badge, amber 需要你, landed sha7, muted 尚未合入, derived
 * next step, archived fold) with the read-only preview drawer open — and the
 * compact surface at 390, where /board collapses onto the /m task layer.
 * Both render in dark and light.
 *
 * A default run skips everything; REMUDA_EVIDENCE=1 writes
 * docs/design/evidence/ui-overhaul/UO-8-*.png. Only Remuda itself is
 * captured (the gate rule).
 */

test.skip(!process.env.REMUDA_EVIDENCE, "set REMUDA_EVIDENCE=1 to capture the committed screenshots");
test.describe.configure({ mode: "serial" });

const shotDir = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../docs/design/evidence/ui-overhaul",
);
const MODES = ["dark", "light"] as const;
type Mode = (typeof MODES)[number];

type TaskDoc = { id: string; title: string; state: string };

const createdInstances: string[] = [];

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

async function shoot(page: Page, name: string, mode: Mode): Promise<void> {
  await setMode(page, mode);
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.waitForTimeout(250);
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({
    path: path.join(shotDir, name),
    animations: "disabled",
  });
}

async function makeFixture(page: Page): Promise<{ project: string; needsYou: TaskDoc }> {
  const suffix = Date.now().toString(36);
  const hosts = await apiJson<{ items?: { id?: string; label?: string }[] }>(
    page,
    "GET",
    "/v1/hosts",
  );
  const host = hosts.items?.find((item) => item.label === "e2e-fake-node")?.id;
  expect(host, "e2e-fake-node enrolled").toBeTruthy();
  await apiJson(page, "PATCH", `/v1/hosts/${host}`, { maxInstances: 64 }).catch(() => undefined);

  const project = (await apiJson<{ id: string }>(page, "POST", "/v1/projects", {
    name: `uo8 evidence ${suffix}`,
  })).id;

  const create = (label: string) =>
    apiJson<TaskDoc>(page, "POST", "/v1/tasks", {
      projectId: project,
      title: `看板 ${label} ${suffix}`,
      intent: `UO-8 evidence ${label}`,
    });
  const setState = (task: TaskDoc, state: string, reason?: string) =>
    apiJson(page, "PATCH", `/v1/tasks/${task.id}`, { state, ...(reason ? { reason } : {}) });

  // Needs you: a session parked on a pending approval.
  const needsYou = await create("需要你");
  const launched = await apiJson<{ instance?: { instanceId?: string; id?: string } }>(
    page,
    "POST",
    "/v1/instances",
    {
      hostId: host,
      workspaceId: "wsp_e2e",
      kind: "claude",
      driver: "claude-print",
      taskId: needsYou.id,
      prompt: `UO8 mhome-blocked approval gate ${suffix}`,
    },
  );
  const instanceId = launched.instance?.instanceId ?? launched.instance?.id;
  expect(instanceId).toBeTruthy();
  createdInstances.push(instanceId!);
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

  // Failure pre-dispatch: badge in the to-do column.
  const failed = await create("失败");
  await setState(failed, "failed", "supply exhausted");

  // Running, then done-without-land and landed.
  const running = await create("进行中");
  await setState(running, "placed");
  await setState(running, "running");

  const unlanded = await create("已完成未合入");
  await setState(unlanded, "placed");
  await setState(unlanded, "running");
  await setState(unlanded, "done");

  const landed = await create("已合入");
  await setState(landed, "placed");
  await setState(landed, "running");
  await setState(landed, "done");
  await apiJson(page, "POST", `/v1/tasks/${landed.id}/land`, { sha: "a1b2c3d4e5f6a7b8" });

  // Archived: folded, state untouched.
  const archived = await create("已归档");
  await apiJson(page, "POST", `/v1/tasks/${archived.id}/archive`, {});

  // A plain pending task keeps the 待派发 next-step state on the board.
  await create("待派发");

  return { project, needsYou };
}

test.afterAll(async ({ browser }) => {
  if (createdInstances.length === 0) return;
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    await login(page);
    for (const id of createdInstances.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  } finally {
    await context.close();
  }
});

test("UO-8 board at 1440 and the collapsed /m task layer at 390, dark and light", async ({ page }) => {
  test.setTimeout(180_000);
  await login(page);
  const fx = await makeFixture(page);

  await page.emulateMedia({ reducedMotion: "reduce" });

  // ── 1440: desktop board with the preview drawer open ──────────────────
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto(`/board?project=${fx.project}`);
  await expect(page.getByTestId("board-page")).toBeVisible();
  await expect(page.getByTestId("board-column")).toHaveCount(3);

  // Open the archive fold and the needs-you preview before shooting.
  await page.getByTestId("board-archive-toggle").check();
  await expect(page.getByTestId("board-archive-fold")).toBeVisible();
  await page
    .locator('[data-testid="board-card"][data-task-id="' + fx.needsYou.id + '"]')
    .getByTestId("board-card-open")
    .click();
  await expect(page.getByRole("dialog", { name: "任务预览" })).toBeVisible();

  for (const mode of MODES) {
    await shoot(page, `UO-8-board-${mode}-1440.png`, mode);
  }

  // ── 390: /board collapses to the /m task layer (no three-column clone) ─
  await page.keyboard.press("Escape");
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`/board?project=${fx.project}`);
  await page.waitForURL(/\/m(?:\?|$)/);
  await expect(page.getByTestId("home-list")).toBeVisible();
  await expect(page.getByTestId("home-tasks")).toBeVisible();
  await expect(page.getByTestId("board-page")).toHaveCount(0);

  for (const mode of MODES) {
    await shoot(page, `UO-8-home-${mode}-390.png`, mode);
  }
});
