import { expect, test, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * Plan task-model task 3 (t-bind), D-050 §2/§3.4: the per-task directory
 * binding against the composed Hub + the gated fake Node
 * (HUB_E2E_TASK_BIND=1 in crates/remuda-hub/examples/hub_e2e.rs).
 *
 * Coverage of the four acceptance items:
 * 1. reuse binds the registered root or an existing remuda-wt sibling; the
 *    launch cwd folds through the existing cwd admission, and an out-of-tree
 *    name is refused exactly as resolve_instance_cwd would refuse it.
 * 2. pool obtains its own wt/<slot>/<task-slug> branch through the lease and
 *    flows to the launch via the existing CreateInstanceBody.worktree field —
 *    no new dispatch wire field.
 * 3. the binding lives in doc_json (workspaceBinding round-trips on the
 *    task); no schema column was added.
 * 4. binding onto an already-leased reuse directory increments refcount and
 *    surfaces the 与 N 个 task 共用 / dir-busy queue; a dirty directory and a
 *    full pool return blocked/deferred and never create a task.
 *
 * Generic wording only; no reference-product names.
 */

test.describe.configure({ mode: "serial" });

type Sharing = {
  dirKey?: string;
  refcount?: number;
  queued?: boolean;
  blocked?: string | null;
};

type CreatedTask = {
  id: string;
  title: string;
  workspaceBinding?: {
    mode: "reuse" | "pool";
    hostId: string;
    workspaceId: string;
    worktreeName?: string;
    branch?: string;
    leaseRefIds?: string[];
  };
  sharing?: Sharing;
};

type InstanceDoc = {
  instanceId?: string;
  id?: string;
  cwd?: string | null;
  worktree?: string | null;
};

async function apiJson<T>(page: Page, method: string, path: string, body?: unknown): Promise<T> {
  const response = await page.request.fetch(path, {
    method,
    data: body,
    headers: body ? { "content-type": "application/json" } : undefined,
  });
  const text = await response.text();
  if (!response.ok()) {
    throw Object.assign(new Error(`${method} ${path} → ${response.status}: ${text}`), {
      status: response.status(),
      body: text,
    });
  }
  return (text ? JSON.parse(text) : null) as T;
}

async function apiStatus(
  page: Page,
  method: string,
  path: string,
  body?: unknown,
): Promise<{ status: number; body: string }> {
  const response = await page.request.fetch(path, {
    method,
    data: body,
    headers: body ? { "content-type": "application/json" } : undefined,
  });
  return { status: response.status(), body: await response.text() };
}

async function hostId(page: Page): Promise<string> {
  const body = await apiJson<{ items?: { id?: string; label?: string }[] }>(
    page,
    "GET",
    "/v1/hosts",
  );
  const host = (body.items ?? []).find((item) => item.label === "e2e-fake-node");
  if (!host?.id) throw new Error("e2e-fake-node missing");
  return host.id;
}

const ROOT_CWD = "/tmp/remuda-e2e";
const SIBLING_CWD = "/tmp/remuda-e2e/remuda-wt/agent-one";
// The fake Node announces one branded workspace (HUB_E2E_TASK_BIND=1); project
// membership rejects the legacy non-branded `wsp_e2e` label, so discover it.
const BRANDED_WSP = /^wsp_[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

test.describe("task directory binding (HUB_E2E_TASK_BIND=1)", () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  let suffix = "";
  let project = "";
  let host = "";
  let wsp = "";
  const createdInstances: string[] = [];

  async function makeProject(page: Page): Promise<void> {
    suffix = Date.now().toString(36);
    host = await hostId(page);
    const workspaceList = await apiJson<{
      workspaces: { workspaceId: string; root: string }[];
    }>(page, "GET", `/v1/hosts/${host}/workspaces`);
    const workspace = workspaceList.workspaces.find((entry) =>
      BRANDED_WSP.test(entry.workspaceId),
    );
    expect(workspace, "branded bind workspace").toBeTruthy();
    wsp = workspace!.workspaceId;
    project = (
      await apiJson<{ id: string }>(page, "POST", "/v1/projects", {
        name: `tbind ${suffix}`,
      })
    ).id;
    await apiJson(page, "POST", `/v1/projects/${project}/members`, {
      hostId: host,
      workspaceId: wsp,
      role: "build",
    });
  }

  async function boundTask(
    page: Page,
    label: string,
    binding: Record<string, unknown>,
  ): Promise<CreatedTask> {
    return apiJson<CreatedTask>(page, "POST", "/v1/tasks", {
      projectId: project,
      title: `${label} ${suffix}`,
      intent: `e2e t-bind intent ${label}`,
      workspaceBinding: binding,
    });
  }

  async function launchCwd(page: Page, taskId: string): Promise<InstanceDoc> {
    const created = await apiJson<{ instance: InstanceDoc }>(page, "POST", "/v1/instances", {
      hostId: host,
      workspaceId: wsp,
      kind: "claude",
      driver: "claude-pty",
      taskId,
      prompt: "t-bind launch smoke",
    });
    const id = created.instance.instanceId ?? created.instance.id;
    expect(id).toBeTruthy();
    createdInstances.push(id!);
    const fetched = await apiJson<InstanceDoc>(page, "GET", `/v1/instances/${id}`);
    return fetched;
  }

  test.afterAll(async ({ browser }) => {
    // Best-effort cleanup of the bound sessions so leases detach.
    const context = await browser.newContext();
    const page = await context.newPage();
    try {
      await login(page);
      for (const id of createdInstances) {
        await page.request
          .fetch(`/v1/instances/${id}?force=1`, { method: "DELETE" })
          .catch(() => undefined);
      }
    } finally {
      await context.close();
    }
  });

  test("reuse root: binding round-trips in doc_json and the session starts at the registered root", async ({
    page,
  }) => {
    await makeProject(page);
    const task = await boundTask(page, "reuse-root", {
      mode: "reuse",
      hostId: host,
      workspaceId: wsp,
    });
    // Acceptance 3: the binding is on the task document (doc_json), root has
    // no worktree name and carries a lease row id.
    expect(task.workspaceBinding).toMatchObject({
      mode: "reuse",
      hostId: host,
      workspaceId: wsp,
    });
    expect(task.workspaceBinding?.worktreeName).toBeUndefined();
    expect(task.workspaceBinding?.leaseRefIds?.length).toBe(1);
    expect(task.sharing?.refcount).toBe(1);

    // Acceptance 1: no cwd override is forwarded for a root binding, so the
    // Hub projects the workspace id exactly as an unbound launch does and the
    // Node applies resolve_instance_cwd to the registered root itself.
    const instance = await launchCwd(page, task.id);
    expect(instance.cwd).toBe(wsp);
  });

  test("reuse sibling: cwd folds to the existing remuda-wt directory", async ({ page }) => {
    await makeProject(page);
    const task = await boundTask(page, "reuse-sibling", {
      mode: "reuse",
      hostId: host,
      workspaceId: wsp,
      worktreeName: "agent-one",
    });
    expect(task.workspaceBinding?.worktreeName).toBe("agent-one");
    const instance = await launchCwd(page, task.id);
    expect(instance.cwd).toBe(SIBLING_CWD);
    // Reuse never flows through the worktree field.
    expect(instance.worktree ?? null).toBeNull();
  });

  test("reuse sharing: a second task on the same directory bumps refcount and is queued 与 N 个 task 共用", async ({
    page,
  }) => {
    await makeProject(page);
    // Dedicated dir: other specs in this serial run keep their own leases, so
    // the refcount here starts at zero on this dir_key.
    const shareDir = "agent-two";
    const first = await boundTask(page, "share-a", {
      mode: "reuse",
      hostId: host,
      workspaceId: wsp,
      worktreeName: shareDir,
    });
    // The first task occupies the directory; its session holds the attach
    // lock while it runs.
    const firstInstance = await launchCwd(page, first.id);
    expect(firstInstance.cwd).toBe(`${ROOT_CWD}/remuda-wt/${shareDir}`);

    const second = await boundTask(page, "share-b", {
      mode: "reuse",
      hostId: host,
      workspaceId: wsp,
      worktreeName: shareDir,
    });
    // Acceptance 4: same dir_key → refcount 2 and an explicit dir-busy queue,
    // never a different directory.
    expect(second.sharing?.dirKey).toBe(shareDir);
    expect(second.sharing?.refcount).toBe(2);
    expect(second.sharing?.queued).toBe(true);
    expect(second.sharing?.blocked ?? "").toContain("dir-busy");

    // Sharing is serial: while the first session is attached, the second
    // task's launch is refused dir-busy instead of running concurrently.
    const blockedLaunch = await apiStatus(page, "POST", "/v1/instances", {
      hostId: host,
      workspaceId: wsp,
      kind: "claude",
      driver: "claude-pty",
      taskId: second.id,
      prompt: "must queue",
    });
    expect(blockedLaunch.status).toBe(409);
    expect(blockedLaunch.body).toMatch(/dir-busy/);

    // Once the holder detaches, the queued task can launch in the same cwd.
    await page.request
      .fetch(`/v1/instances/${firstInstance.instanceId ?? firstInstance.id}?force=1`, {
        method: "DELETE",
      })
      .catch(() => undefined);
    const secondInstance = await launchCwd(page, second.id);
    expect(secondInstance.cwd).toBe(`${ROOT_CWD}/remuda-wt/${shareDir}`);
  });

  test("pool: the lease allocates a slot with its own wt/<slot>/<task> branch and folds into worktree", async ({
    page,
  }) => {
    await makeProject(page);
    const task = await boundTask(page, "pool-alpha", {
      mode: "pool",
      hostId: host,
      workspaceId: wsp,
      worktreeName: "alpha",
    });
    // Acceptance 2: the pool name the operator chose resolves to the Node's
    // concrete slot, and the branch is per-task.
    expect(task.workspaceBinding?.mode).toBe("pool");
    expect(task.workspaceBinding?.worktreeName).toMatch(/^alpha-s\d+$/);
    const slot = task.workspaceBinding?.worktreeName ?? "";
    expect(task.workspaceBinding?.branch).toMatch(
      new RegExp(`^wt/${slot.replace(/\d+$/, "\\d+")}/tsk-`),
    );

    // The launch cwd is the leased slot and it rode the existing worktree
    // field — no new dispatch wire field.
    const instance = await launchCwd(page, task.id);
    expect(instance.cwd).toBe(`${ROOT_CWD}/remuda-wt/${slot}`);
  });

  test("refusals: out-of-tree, dirty directory and a full pool block without creating a task", async ({
    page,
  }) => {
    await makeProject(page);
    // Acceptance 1/4: a traversal segment is rejected with the same shape a
    // resolve_instance_cwd failure would produce.
    const outOfTree = await apiStatus(page, "POST", "/v1/tasks", {
      projectId: project,
      title: `escape ${suffix}`,
      intent: "must not bind outside",
      workspaceBinding: {
        mode: "reuse",
        hostId: host,
        workspaceId: wsp,
        worktreeName: "../escape",
      },
    });
    expect(outOfTree.status).toBe(400);
    expect(outOfTree.body).toMatch(/must match|outside|segment|escape/i);

    // A dirty pool lease refuses (409 blocked); the task is never created.
    const dirty = await apiStatus(page, "POST", "/v1/tasks", {
      projectId: project,
      title: `dirty ${suffix}`,
      intent: "must not reroute",
      workspaceBinding: { mode: "pool", hostId: host, workspaceId: wsp, worktreeName: "dirty" },
    });
    expect(dirty.status).toBe(409);
    expect(dirty.body).toMatch(/dirty/i);

    // A full pool defers (429 SUPPLY_DEFERRED), again with no task row.
    const full = await apiStatus(page, "POST", "/v1/tasks", {
      projectId: project,
      title: `full ${suffix}`,
      intent: "must wait for supply",
      workspaceBinding: { mode: "pool", hostId: host, workspaceId: wsp, worktreeName: "fullpool" },
    });
    expect(full.status).toBe(429);
    expect(full.body).toMatch(/SUPPLY_DEFERRED|full/);

    // The refusals left no half-created tasks behind: the suffix appears on
    // only the tasks that bound successfully elsewhere — none here.
    const list = await apiJson<{ items: CreatedTask[] }>(
      page,
      "GET",
      `/v1/tasks?project=${project}`,
    );
    const refused = list.items.filter(
      (task) => task.title.includes(`dirty ${suffix}`) || task.title.includes(`full ${suffix}`),
    );
    expect(refused).toEqual([]);
  });

  test("a binding outside project membership is refused and dispatch cannot override the bound host", async ({
    page,
  }) => {
    await makeProject(page);
    const other = await apiStatus(page, "POST", "/v1/tasks", {
      projectId: project,
      title: `alien ${suffix}`,
      intent: "wrong space",
      workspaceBinding: {
        mode: "reuse",
        hostId: "hst_not_a_member",
        workspaceId: "wsp_g2_clean",
      },
    });
    expect(other.status).toBe(400);

    const task = await boundTask(page, "pinned", {
      mode: "reuse",
      hostId: host,
      workspaceId: wsp,
    });
    // An explicit conflicting host on launch is a hard conflict, not a
    // silent directory switch (D-035).
    const conflict = await apiStatus(page, "POST", "/v1/instances", {
      hostId: "hst_not_a_member",
      workspaceId: wsp,
      kind: "claude",
      driver: "claude-pty",
      taskId: task.id,
      prompt: "wrong host",
    });
    expect(conflict.status).toBe(409);
    expect(conflict.body).toMatch(/bound to host/);
  });
});
