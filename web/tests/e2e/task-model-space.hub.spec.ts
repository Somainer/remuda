import { expect, test, type Page } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * Plan task-model task 7 (t-taskspace), D-050 §7, docs/design/task-model.md
 * §7: the task space vs project space file panels against the fully composed
 * Hub + default enrolled fake Node (no gated fixture — the fake's g2
 * workspaces and the task ledger routes are part of the default harness).
 *
 * The two panels reuse ONE real-time file view with NO new endpoint:
 *  - project space: the unchanged worktree-tree view on the hostId+
 *    workspaceId axis (all `workspace.scm.status` rows);
 *  - task space: the same payload narrowed client-side by the task's owns[]
 *    globs; an empty projection is 「还没有文件」, never a synthesised row;
 *  - non-git/offline/etc. fall through to the contract's §3.6 availability
 *    states in both tabs.
 *
 * The TaskSpacePanel is a real Remuda render. Task 5 later mounts it in the
 * app surface; for now it is mounted from inside the already-loaded Remuda
 * shell through the Vite module graph (a same-page dynamic import — no lab
 * HTML, no route, no second entry is committed): task metadata comes over the
 * existing /v1/tasks + /v1/tasks/{id}/placements routes and the owning task
 * is resolved through the instance's placement rows. Every screenshot pixel
 * is the Remuda component.
 *
 * Generic wording only; synthetic fixtures only; no real models.
 */

test.describe.configure({ mode: "serial" });

const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence");

interface PanelParams {
  host: string;
  ws: string;
  instance: string;
}

/**
 * Mount the real TaskSpacePanel in a full-viewport overlay on whatever
 * Remuda page is loaded post-login. The Vite dev server transforms the
 * source module on demand; tokens, fonts and the device session are already
 * on the page. No file is committed for this harness.
 */
async function mountPanel(page: Page, params: PanelParams) {
  await page.evaluate(async (p) => {
    const w0 = window as unknown as { __taskspaceUnmount?: () => void };
    w0.__taskspaceUnmount?.();
    document
      .querySelectorAll("[data-testid='taskspace-lab-host']")
      .forEach((node) => node.remove());
    const mod = await import("/src/features/tasks/TaskSpacePanel.tsx");
    const host = document.createElement("div");
    host.setAttribute("data-testid", "taskspace-lab-host");
    Object.assign(host.style, {
      position: "fixed",
      inset: "0",
      zIndex: "99999",
      background: "var(--canvas)",
    });
    document.body.appendChild(host);
    const w = window as unknown as {
      __taskspaceUnmount?: () => void;
    };
    w.__taskspaceUnmount = mod.mountTaskSpacePanel(host, {
      hostId: p.host,
      workspaceId: p.ws,
      instanceId: p.instance,
      onBack: () => window.dispatchEvent(new CustomEvent("taskspace-back")),
    });
  }, params as unknown as Record<string, string>);
  await expect(page.getByTestId("files-pane")).toBeVisible({ timeout: 20_000 });
  await expect(page.getByTestId("files-space-tabs")).toBeVisible();
}

async function resolveHost(page: Page): Promise<string> {
  const response = await page.request.get("/v1/hosts");
  expect(response.ok()).toBe(true);
  const body = (await response.json()) as { items?: { id: string; label?: string }[] };
  const host = (body.items ?? []).find((row) => row.label === "e2e-fake-node") ?? body.items?.[0];
  expect(host, "fake node host").toBeTruthy();
  return host!.id;
}

async function createInstance(page: Page, workspaceId: string, hostId: string): Promise<string> {
  const response = await page.request.post("/v1/instances", {
    data: { hostId, workspaceId, kind: "claude", driver: "claude-headless", prompt: "t-taskspace fixture" },
  });
  expect(response.ok(), `create ${workspaceId}: ${await response.text()}`).toBe(true);
  const body = (await response.json()) as { instance: { instanceId: string } };
  return body.instance.instanceId;
}

async function createTaskWithOwns(page: Page, owns: string[]): Promise<{ id: string; title: string }> {
  const suffix = Date.now().toString(36) + Math.random().toString(36).slice(2, 6);
  const projectResponse = await page.request.fetch("/v1/projects", {
    method: "POST",
    data: { name: `taskspace e2e ${suffix}` },
    headers: { "content-type": "application/json" },
  });
  expect(projectResponse.ok(), `create project: ${await projectResponse.text()}`).toBe(true);
  const projectId = ((await projectResponse.json()) as { id: string }).id;
  const title = "任务空间验收";
  const task = await page.request.fetch("/v1/tasks", {
    method: "POST",
    headers: { "content-type": "application/json" },
    data: { projectId, title, intent: "t-taskspace e2e", owns },
  });
  expect(task.ok(), `create task: ${await task.text()}`).toBe(true);
  const createdTask = (await task.json()) as { id: string; title: string };
  createdTasks.push(createdTask.id);
  return createdTask;
}

/** Bind a task to a session via the placement ledger (the wire session set). */
async function placeTaskOnInstance(page: Page, taskId: string, hostId: string, instanceId: string) {
  const response = await page.request.fetch(`/v1/tasks/${taskId}/placements`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    data: { kind: "dispatch", hostId, instanceId, model: "e2e/auto", branch: "wt/taskspace/e2e" },
  });
  expect(response.ok(), `placement: ${await response.text()}`).toBe(true);
}

const created: string[] = [];
const createdTasks: string[] = [];

test.beforeEach(async ({ page }) => {
  await login(page);
  const host = await resolveHost(page);
  const patched = await page.request.patch(`/v1/hosts/${host}`, { data: { maxInstances: 64 } });
  expect(patched.ok(), `raise maxInstances: ${await patched.text()}`).toBe(true);
});

test.afterEach(async ({ page }) => {
  // owns globs are active claims unique across tasks; release ours so the
  // next case (and the full serial suite) can claim the same fixture paths.
  for (const taskId of createdTasks.splice(0)) {
    try {
      await page.request.fetch(`/v1/tasks/${taskId}/own`, {
        method: "DELETE",
        headers: { "content-type": "application/json" },
        data: { paths: [] },
      });
    } catch {
      /* best-effort cleanup */
    }
  }
  for (const instanceId of created.splice(0)) {
    await page.request.delete(`/v1/instances/${instanceId}?force=1`).catch(() => undefined);
  }
});

test("project space shows every worktree change; task space narrows by owns[] and reuses the same diff read", async ({
  page,
}) => {
  const host = await resolveHost(page);
  const instanceId = await createInstance(page, "wsp_e2e", host);
  created.push(instanceId);
  // src/** + notes/** cover two of the three fixture rows; the binary under
  // assets/ is outside the task.
  const task = await createTaskWithOwns(page, ["src/**", "notes/**"]);
  await placeTaskOnInstance(page, task.id, host, instanceId);

  await mountPanel(page, { host, ws: "wsp_e2e", instance: instanceId });

  // Project space: the unchanged worktree tree on the hostId+workspaceId axis.
  await expect(page.getByTestId("files-space-project")).toHaveAttribute("aria-selected", "true");
  await expect(page.getByTestId("files-entry")).toHaveCount(3);
  await expect(page.getByTestId("files-entry").filter({ hasText: "src/main.rs" })).toBeVisible();
  await expect(page.getByTestId("files-entry").filter({ hasText: "notes/todo.md" })).toBeVisible();
  await expect(page.getByTestId("files-entry").filter({ hasText: "assets/logo.bin" })).toBeVisible();

  // Task space: same status payload, filtered to the owns[] scope.
  const requests: string[] = [];
  page.on("request", (request) => {
    const pathname = new URL(request.url()).pathname;
    if (pathname.includes("/changes")) requests.push(pathname);
  });
  await page.getByTestId("files-space-task").click();
  await expect(page.getByTestId("files-space-task")).toHaveAttribute("aria-selected", "true");
  await expect(page.getByTestId("files-entry")).toHaveCount(2);
  await expect(page.getByTestId("files-entry").filter({ hasText: "src/main.rs" })).toBeVisible();
  await expect(page.getByTestId("files-entry").filter({ hasText: "notes/todo.md" })).toBeVisible();
  await expect(page.getByTestId("files-entry").filter({ hasText: "assets/logo.bin" })).toHaveCount(0);
  // No synthesised rows, no second status fetch: filtering is client-side.
  expect(requests.filter((p) => p.endsWith("/changes"))).toEqual([]);

  // The existing diff read is reused unchanged inside the task space.
  await page.getByTestId("files-entry").filter({ hasText: "src/main.rs" }).click();
  await expect(page.getByTestId("files-diff")).toContainText("println!(\"workbench g2\")");
  expect(requests.filter((p) => p.includes("/changes/diff"))).toHaveLength(1);
});

test("an empty task-space projection says 还没有文件 without synthesising entries", async ({ page }) => {
  const host = await resolveHost(page);
  const instanceId = await createInstance(page, "wsp_e2e", host);
  created.push(instanceId);
  // owns that none of the three fixture rows fall inside.
  const task = await createTaskWithOwns(page, ["docs/**"]);
  await placeTaskOnInstance(page, task.id, host, instanceId);

  await mountPanel(page, { host, ws: "wsp_e2e", instance: instanceId });
  await page.getByTestId("files-space-task").click();

  await expect(page.getByTestId("files-task-empty")).toBeVisible();
  await expect(page.getByTestId("files-task-empty")).toContainText("还没有文件");
  await expect(page.getByTestId("files-entry")).toHaveCount(0);
  // The project space still has the real rows.
  await page.getByTestId("files-space-project").click();
  await expect(page.getByTestId("files-entry")).toHaveCount(3);
});

test("non-git and offline availability states pass through into the task space unchanged", async ({
  page,
}) => {
  const host = await resolveHost(page);

  // Non-git workspace: 不支持 in the project space AND the task space — the
  // task filter never fakes a baseline for an unavailable workspace.
  const nogitInstance = await createInstance(page, "wsp_g2_nogit", host);
  created.push(nogitInstance);
  const nogitTask = await createTaskWithOwns(page, ["src/**"]);
  await placeTaskOnInstance(page, nogitTask.id, host, nogitInstance);

  await mountPanel(page, { host, ws: "wsp_g2_nogit", instance: nogitInstance });
  await expect(page.getByTestId("files-unsupported")).toBeVisible();
  await page.getByTestId("files-space-task").click();
  await expect(page.getByTestId("files-unsupported")).toBeVisible();
  await expect(page.getByTestId("files-task-empty")).toHaveCount(0);

  // Offline: the status proxy answers 409 HOST_OFFLINE; task metadata is
  // unaffected. Both tabs land on the distinct 离线 state. Distinct owns from
  // the non-git task above — claims are unique across active tasks.
  const liveInstance = await createInstance(page, "wsp_e2e", host);
  created.push(liveInstance);
  const liveTask = await createTaskWithOwns(page, ["notes/**"]);
  await placeTaskOnInstance(page, liveTask.id, host, liveInstance);

  await page.route(/\/changes(?:\?|$)/, async (route) => {
    await route.fulfill({
      status: 409,
      contentType: "application/json",
      body: JSON.stringify({ code: "HOST_OFFLINE", error: "host is offline" }),
    });
  });

  await mountPanel(page, { host, ws: "wsp_e2e", instance: liveInstance });
  await expect(page.getByTestId("files-offline")).toBeVisible();
  await page.getByTestId("files-space-task").click();
  await expect(page.getByTestId("files-offline")).toBeVisible();
});

test.describe("evidence renders (Remuda only)", () => {
  test.use({ viewport: { width: 1440, height: 900 } });

  test("1440: project space and task space panels", async ({ page }) => {
    const host = await resolveHost(page);
    const instanceId = await createInstance(page, "wsp_e2e", host);
    created.push(instanceId);
    const task = await createTaskWithOwns(page, ["src/**", "notes/**"]);
    await placeTaskOnInstance(page, task.id, host, instanceId);

    await mountPanel(page, { host, ws: "wsp_e2e", instance: instanceId });
    await page.waitForTimeout(300);
    await page.screenshot({
      path: path.join(evidence, "task-model-7-project-1440.png"),
      animations: "disabled",
    });

    await page.getByTestId("files-space-task").click();
    await expect(page.getByTestId("files-entry")).toHaveCount(2);
    await page.waitForTimeout(300);
    await page.screenshot({
      path: path.join(evidence, "task-model-7-task-1440.png"),
      animations: "disabled",
    });
  });
});

test.describe("mobile 390px", () => {
  test.use({ viewport: { width: 390, height: 780 } });

  test("390: task space panel stays in the single shared files surface", async ({ page }) => {
    const host = await resolveHost(page);
    const instanceId = await createInstance(page, "wsp_e2e", host);
    created.push(instanceId);
    const task = await createTaskWithOwns(page, ["src/**"]);
    await placeTaskOnInstance(page, task.id, host, instanceId);

    await mountPanel(page, { host, ws: "wsp_e2e", instance: instanceId });
    await page.getByTestId("files-space-task").click();
    await expect(page.getByTestId("files-entry")).toHaveCount(1);
    await expect(page.getByTestId("files-entry").filter({ hasText: "src/main.rs" })).toBeVisible();
    await page.waitForTimeout(300);
    await page.screenshot({
      path: path.join(evidence, "task-model-7-task-390.png"),
      animations: "disabled",
    });
  });
});
