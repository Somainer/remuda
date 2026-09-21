import { describe, expect, it } from "vitest";
import { mockDb } from "../../lib/mock";
import { known, type Id } from "../../types/wire";
import type { Instance } from "../../types/instance";
import type { Task } from "../../types/generated";
import type { Interaction } from "../../types/interaction";
import { buildSpaces, defaultSpacePrefs, spaceKey } from "../spaces/store";
import {
  ARCHIVE_GROUP_ID,
  ATTENTION_GROUP_ID,
  buildTaskGroups,
  taskAwaitsHuman,
  taskNextStep,
  taskSpaceId,
  type TaskListGroup,
  type TaskRow,
  type TaskRowsInput,
} from "./taskRows";

/** Minimal task fixture; only the ledger fields the projection reads. */
let createdCounter = 0;
function task(id: string, patch: Partial<Task> = {}): Task {
  const stamp = new Date(Date.UTC(2026, 8, 21, 8, createdCounter++)).toISOString();
  return {
    id: `tsk_${id}`,
    revision: "1",
    createdAt: stamp,
    updatedAt: "2026-09-21T09:00:00.000Z",
    projectId: "prj_a",
    title: `task ${id}`,
    mandate: { chain: [] },
    class: "implement",
    state: "pending",
    ...patch,
  } as Task;
}

function session(id: string, taskId: string | null, patch: Partial<Instance> = {}): Instance {
  return {
    ...mockDb.instances[0],
    id: id as Id,
    hostId: "host-a",
    workspaceId: "wsp-a",
    lifecycle: "ready",
    connectivity: "connected",
    activity: known("idle"),
    updatedAt: "2026-09-21T09:00:00.000Z",
    parent: null,
    taskId: taskId as Id | null,
    ...patch,
  } as Instance;
}

function blockedSession(id: string, taskId: string | null): Instance {
  return session(id, taskId, { activity: known("waiting-interaction") });
}

function pendingInteraction(instanceId: string): Pick<Interaction, "instanceId" | "state"> {
  return { instanceId: instanceId as Id, state: "pending" };
}

const workspaces = [
  {
    id: "wsp-a",
    hostId: "host-a",
    label: "alpha",
    rootPath: "/srv/alpha",
    revision: "1",
    createdAt: "",
    updatedAt: "",
    writePolicy: "default" as const,
    canonicalRoot: known("/srv/alpha"),
  },
  {
    id: "wsp-b",
    hostId: "host-a",
    label: "beta",
    rootPath: "/srv/beta",
    revision: "1",
    createdAt: "",
    updatedAt: "",
    writePolicy: "default" as const,
    canonicalRoot: known("/srv/beta"),
  },
];

function build(
  tasks: Task[],
  instances: Instance[] = [],
  interactions: Pick<Interaction, "instanceId" | "state">[] = [],
  extra: Partial<TaskRowsInput> = {},
) {
  const spaces = buildSpaces(workspaces, instances, defaultSpacePrefs());
  return buildTaskGroups({
    tasks,
    instances,
    interactions,
    spaces,
    projectName: (projectId) => (projectId === "prj_a" ? "alpha" : projectId === "prj_b" ? "beta" : projectId),
    branchOfSpace: (spaceId) =>
      spaceId === spaceKey("host-a", "wsp-a") ? "feat/workbench-g2" : "main",
    ...extra,
  });
}

function group(
  groups: ReturnType<typeof buildTaskGroups>,
  kind: string,
  id?: string,
): TaskListGroup {
  const found = groups.filter((group) => group.kind === kind && (id === undefined || group.id === id));
  if (found.length !== 1) throw new Error(`expected one ${kind} group, got ${found.length}`);
  return found[0];
}

function flattenRows(rows: TaskRow[]): TaskRow[] {
  return rows.flatMap((row) => [row, ...flattenRows(row.children)]);
}

function flattenIds(rows: TaskRow[]): string[] {
  return flattenRows(rows).map((row) => row.id);
}

function groups2rows(groups: TaskListGroup[]): TaskRow[] {
  return groups.flatMap((group) => flattenRows(group.rows));
}

describe("project + branch grouping", () => {
  it("groups tasks by project and Space and keeps the buildSpaces blockedCount on the header", () => {
    const t1 = task("a1", { createdAt: "2026-09-21T08:00:00.000Z" });
    // Same project, a second space: never merged with wsp-a (D-024).
    const t2 = task("a2", {
      createdAt: "2026-09-21T08:01:00.000Z",
      workspaceBinding: { mode: "reuse", hostId: "host-a", workspaceId: "wsp-b" },
    });
    // One blocked session for t1: it contributes the space blockedCount.
    const instances = [
      blockedSession("ins_1", t1.id),
      session("ins_2", t2.id, { hostId: "host-a", workspaceId: "wsp-b" }),
    ];
    const groups = build([t1, t2], instances);

    expect(groups.filter((group) => group.kind === "project")).toHaveLength(2);
    const alpha = group(groups, "project", `prj:prj_a:${spaceKey("host-a", "wsp-a")}`);
    const beta = group(groups, "project", `prj:prj_a:${spaceKey("host-a", "wsp-b")}`);
    expect(alpha.project).toBe("alpha");
    expect(alpha.branch).toBe("feat/workbench-g2");
    expect(beta.branch).toBe("main");

    // Acceptance 1: the header count is exactly buildSpaces() blockedCount.
    const spaces = buildSpaces(workspaces, instances, defaultSpacePrefs());
    const alphaSpace = spaces.find((space) => space.id === spaceKey("host-a", "wsp-a"))!;
    expect(alpha.blockedCount).toBe(alphaSpace.blockedCount);
    expect(alpha.blockedCount).toBe(1);
    expect(beta.blockedCount).toBe(0);
  });

  it("sorts projects by name and rows oldest-first, and projects never share an SE sequence", () => {
    const aLate = task("late", { projectId: "prj_a", createdAt: "2026-09-21T09:00:00.000Z" });
    const bFirst = task("bfirst", { projectId: "prj_b", createdAt: "2026-09-21T07:00:00.000Z" });
    const aEarly = task("early", { projectId: "prj_a", createdAt: "2026-09-21T06:00:00.000Z" });
    const groups = build([aLate, bFirst, aEarly]);

    const projectGroups = groups.filter((g) => g.kind === "project");
    expect(projectGroups.map((g) => g.project)).toEqual(["alpha", "beta"]);
    const alpha = group(groups, "project", `prj:prj_a:${"-"}`);
    expect(alpha.rows.map((row) => row.id)).toEqual([aEarly.id, aLate.id]);
    // Per-project sequence: alpha SE-01/SE-02, beta restarts at SE-01.
    expect(alpha.rows.map((row) => row.displayKey)).toEqual(["SE-01", "SE-02"]);
    const beta = group(groups, "project", `prj:prj_b:${"-"}`);
    expect(beta.rows[0].displayKey).toBe("SE-01");
  });
});

describe("parent / child nesting", () => {
  it("nests children under their parent inside the group with a child count", () => {
    const parent = task("parent", { title: "适配 provider", createdAt: "2026-09-21T08:00:00.000Z" });
    const child1 = task("c1", {
      parentTaskId: parent.id,
      title: "子任务一",
      createdAt: "2026-09-21T08:05:00.000Z",
    });
    const child2 = task("c2", {
      parentTaskId: child1.id,
      title: "孙任务",
      createdAt: "2026-09-21T08:06:00.000Z",
    });
    const groups = build([child2, child1, parent]);

    const projectGroup = group(groups, "project");
    expect(projectGroup.rows).toHaveLength(1);
    const root = projectGroup.rows[0];
    expect(root.id).toBe(parent.id);
    expect(root.depth).toBe(0);
    expect(root.childCount).toBe(1);
    expect(root.children).toHaveLength(1);
    const nested = root.children[0];
    expect(nested.id).toBe(child1.id);
    expect(nested.depth).toBe(1);
    expect(nested.childCount).toBe(1);
    expect(nested.children[0].id).toBe(child2.id);
    expect(nested.children[0].depth).toBe(2);
    expect(nested.children[0].childCount).toBe(0);
    // The group count includes nested children.
    expect(projectGroup.count).toBe(3);
    expect(flattenIds(projectGroup.rows)).toEqual([parent.id, child1.id, child2.id]);
  });

  it("renders a dangling parent reference and an archived child as top-level", () => {
    const orphan = task("orphan", { parentTaskId: "tsk_missing" });
    const parent = task("p");
    const archivedChild = task("ac", { parentTaskId: parent.id, archivedAt: "2026-09-21T10:00:00Z" });
    const groups = build([orphan, archivedChild, parent]);

    const active = group(groups, "project");
    expect(active.rows.map((row) => row.id).sort()).toEqual([orphan.id, parent.id]);
    expect(active.rows.every((row) => row.childCount === 0)).toBe(true);
    const archived = group(groups, "archived");
    expect(archived.rows.map((row) => row.id)).toEqual([archivedChild.id]);
  });
});

describe("需要你 attention group", () => {
  it("pins tasks with a pending interaction or a blocked reason first, without dropping the project row", () => {
    const awaiting = task("awaiting");
    const ownerBlocked = task("owner", { state: "failed", blockedReason: "supply exhausted" });
    const quiet = task("quiet");
    const groups = build(
      [quiet, ownerBlocked, awaiting],
      [session("ins_a", awaiting.id)],
      [pendingInteraction("ins_a")],
    );

    expect(groups[0].kind).toBe("attention");
    expect(groups[0].id).toBe(ATTENTION_GROUP_ID);
    expect(groups[0].count).toBe(2);
    expect(flattenIds(groups[0].rows).sort()).toEqual([awaiting.id, ownerBlocked.id]);

    // Pinned tasks still appear in their project groups. A task with a
    // session buckets onto that Space; ledger-only tasks share the unattached
    // bucket, which is never merged across Spaces (D-024).
    const activeRows = groups
      .filter((g) => g.kind === "project")
      .flatMap((g) => flattenIds(g.rows))
      .sort();
    expect(activeRows).toEqual([awaiting.id, ownerBlocked.id, quiet.id]);
  });

  it("is distinct from the raw blocked count: a blocked session without an interaction or task reason stays out of attention", () => {
    // The task itself carries no blocked reason; the session is blocked on
    // something the task ledger does not attribute (no pending interaction).
    const t = task("t");
    const groups = build([t], [blockedSession("ins_b", t.id)], []);

    expect(groups.some((g) => g.kind === "attention")).toBe(false);
    const active = group(groups, "project");
    // …yet the project header still carries buildSpaces() raw blockedCount.
    expect(active.blockedCount).toBe(1);
    expect(flattenIds(active.rows)).toEqual([t.id]);
  });

  it("never pulls archived tasks into attention", () => {
    const archived = task("gone", {
      archivedAt: "2026-09-21T10:00:00Z",
      blockedReason: "still broken",
    });
    const groups = build([archived], [], [pendingInteraction("ins_x")]);
    expect(groups.map((g) => g.kind)).toEqual(["archived"]);
  });

  it("exposes the pure predicate for the phone layer", () => {
    expect(taskAwaitsHuman(task("x"), ["ins_p"], new Set(["ins_p"]))).toBe(true);
    expect(taskAwaitsHuman(task("y", { blockedReason: "  " }), ["ins_q"], new Set())).toBe(false);
    expect(taskAwaitsHuman(task("z", { blockedReason: "dir-busy" }), [], new Set())).toBe(true);
  });
});

describe("archive fold", () => {
  it("folds every archived task into one trailing 已归档 group absent from active groups", () => {
    const live = task("live");
    const old = task("old", { archivedAt: "2026-09-21T10:00:00Z" });
    const groups = build([old, live]);

    const archived = group(groups, "archived");
    expect(archived.id).toBe(ARCHIVE_GROUP_ID);
    expect(archived.count).toBe(1);
    expect(flattenIds(archived.rows)).toEqual([old.id]);
    const active = group(groups, "project");
    expect(flattenIds(active.rows)).toEqual([live.id]);
    expect(groups[groups.length - 1].kind).toBe("archived");
  });
});

describe("SE-nn display key", () => {
  it("renders a derived per-project key instead of the bare task id", () => {
    const t = task("opaque", { id: "tsk_opq" });
    const groups = build([t]);
    expect(groups[0].rows[0].displayKey).toMatch(/^SE-\d{2,}$/);
    expect(groups[0].rows[0].displayKey).not.toContain("tsk_");
    expect(groups[0].rows[0].id).toBe("tsk_opq");
  });
});

describe("sessions per task", () => {
  it("counts sessions, prefers the placement session, and resolves the bound Space", () => {
    const t = task("placed", {
      placement: { instanceId: "ins_2" },
    });
    const groups = build(
      [t],
      [
        session("ins_1", t.id, { lifecycle: "exited" }),
        session("ins_2", t.id),
      ],
      [],
    );
    const row = group(groups, "project").rows[0];
    expect(row.sessionCount).toBe(2);
    expect(row.sessionIds.sort()).toEqual(["ins_1", "ins_2"]);
    expect(row.primarySessionId).toBe("ins_2");
    expect(row.spaceId).toBe(spaceKey("host-a", "wsp-a"));
  });

  it("falls back to the binding space and stays unattached without either", () => {
    const bound = task("bound", {
      workspaceBinding: { mode: "reuse", hostId: "host-a", workspaceId: "wsp-b" },
    });
    const loose = task("loose");
    expect(taskSpaceId(bound, [])).toBe(spaceKey("host-a", "wsp-b"));
    const byId = new Map(
      groups2rows(build([bound, loose])).map((row) => [row.id, row.spaceId]),
    );
    expect(byId.get(bound.id)).toBe(spaceKey("host-a", "wsp-b"));
    expect(byId.get(loose.id)).toBeNull();
  });
});

describe("next step line", () => {
  it("puts the human wait first, then the reason, then the ledger phrase", () => {
    const awaited = task("n1", { blockedReason: "x" });
    expect(taskNextStep({ task: awaited, needsHuman: true, blocked: true, sessionCount: 0 })).toBe(
      "需要你处理",
    );
    const failed = task("n2", { state: "failed", blockedReason: "worker exited 42" });
    expect(taskNextStep({ task: failed, needsHuman: false, blocked: true, sessionCount: 0 })).toBe(
      "worker exited 42",
    );
    const unlaunched = task("n3", { state: "pending" });
    expect(taskNextStep({ task: unlaunched, needsHuman: false, blocked: false, sessionCount: 0 })).toBe(
      "待派发",
    );
    const running = task("n4", { state: "running" });
    expect(taskNextStep({ task: running, needsHuman: false, blocked: false, sessionCount: 1 })).toBe(
      "进行中",
    );
  });
});

describe("search", () => {
  it("narrows rows by title, SE key and blocked reason", () => {
    const spill = task("spill", { title: "修 spill", blockedReason: "leaked handle" });
    const other = task("other", { title: "整理 imports" });
    const byTitle = build([spill, other], [], [], { query: "spill" });
    expect(flattenIds(group(byTitle, "project").rows)).toEqual([spill.id]);
    const byReason = build([spill, other], [], [], { query: "handle" });
    expect(flattenIds(group(byReason, "project").rows)).toEqual([spill.id]);
    const byKey = build([spill, other], [], [], { query: "SE-01" });
    // Both tasks are SE-01 of different projects? No — same project, so the
    // key search matches the first sequence only.
    expect(flattenIds(group(byKey, "project").rows)).toEqual([spill.id]);
    expect(build([spill, other], [], [], { query: "不存在的关键词" }).some((g) => g.rows.length)).toBe(false);
  });
});
