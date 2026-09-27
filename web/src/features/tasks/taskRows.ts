/**
 * Pure derivation for the task list (plan task-model task 5, D-050 §9 /
 * ui-spec §2.9).
 *
 * The Hub task ledger (eight states) and `buildSpaces()` stay authoritative;
 * this module only projects them into the operator's work list:
 *
 *  - a first **需要你 · N** attention group: tasks with a pending human
 *    interaction on one of their sessions, or a blocked reason waiting on
 *    the owner. It is an attention/inbox aggregation (tied to the
 *    interaction inbox), never the raw space `blockedCount`;
 *  - then **project + git branch** groups. A group key is
 *    `(projectId, Space id)` — the Space key `(hostId, workspaceId)` is not
 *    relaxed (D-024); the branch is display-only. A group header carries the
 *    exact `buildSpaces().blockedCount` for its space;
 *  - children nest under their `parentTaskId` parent inside the same group,
 *    with a direct-child count on the parent row;
 *  - archived tasks fold into one final **已归档 · N** group and never appear
 *    in an active group;
 *  - every row carries the display-only per-project `SE-nn` key
 *    (`boardColumns.displayKeysByTaskId`, derived at projection time, no
 *    schema change), never the bare task id.
 *
 * No React, no fetch: every rule below is unit-tested in taskRows.test.ts.
 */
import type { Task } from "../../types/generated";
import type { Interaction } from "../../types/interaction";
import { spaceKey, type Space } from "../spaces/store";
import { displayKeysByTaskId } from "./boardColumns";

/** First, cross-project attention group id (ui-spec §2.9 需要你). */
export const ATTENTION_GROUP_ID = "needs-you";
/** Final fold for archived tasks; never a fifth board column. */
export const ARCHIVE_GROUP_ID = "archived";

export type TaskGroupKind = "attention" | "project" | "archived";

/**
 * Structural session slice this projection needs. The normalized Instance
 * carries these fields (including its Hub `taskId` membership); tests pass
 * minimal fixtures.
 */
export type TaskSessionLike = {
  id: string;
  taskId?: string | null;
  hostId?: string | null;
  workspaceId?: string | null;
  lifecycle?: string;
};

export type TaskRow = {
  task: Task;
  id: string;
  /** Display-only per-project sequence, e.g. `SE-03` (never the bare id). */
  displayKey: string;
  /** 0 for a top-level row; direct children render one level deeper. */
  depth: number;
  /** Direct children nested under this row in the same bucket. */
  childCount: number;
  /** Sessions whose `instances.task_id` points at this task. */
  sessionIds: string[];
  sessionCount: number;
  /** Where the row's session link goes: placement first, then a live row. */
  primarySessionId: string | null;
  /** buildSpaces() Space id the task is attached to, or null when unattached. */
  spaceId: string | null;
  /** Git branch label for the group header; null when the Node gave none. */
  branch: string | null;
  /** Sits in the 需要你 group: pending human interaction or owner-blocked. */
  needsHuman: boolean;
  /** Carries a failure/blocked reason the row must surface. */
  blocked: boolean;
  children: TaskRow[];
};

export type TaskListGroup = {
  id: string;
  kind: TaskGroupKind;
  projectId: string | null;
  spaceId: string | null;
  project: string;
  branch: string | null;
  /**
   * Always the matching `buildSpaces()` space blockedCount for project
   * groups (0 for the attention/archive groups), even while a search hides
   * rows — the header reports the space, not the filtered slice.
   */
  blockedCount: number;
  /** Total tasks in the group, including nested children. */
  count: number;
  /** Top-level rows; children hang off `row.children`. */
  rows: TaskRow[];
};

export type TaskRowsInput = {
  tasks: readonly Task[];
  instances?: readonly TaskSessionLike[];
  interactions?: readonly Pick<Interaction, "instanceId" | "state">[];
  /** buildSpaces() output; supplies the authoritative blocked counts. */
  spaces?: readonly Space[];
  projectName?: (projectId: string) => string | null | undefined;
  /** Live git branch per Space id, the same hydration the phone home uses. */
  branchOfSpace?: (spaceId: string) => string | null | undefined;
  /** Free-text search across SE key / title / reason / project. */
  query?: string;
};

/**
 * A task waits on a human when one of its sessions has a pending interaction
 * (the interaction inbox queue) or it carries a blocked reason waiting on
 * the owner. Deliberately does NOT read the space blockedCount: that raw
 * count counts blocked *sessions* for a whole directory, while the 需要你
 * group is the per-task attention projection (ui-spec §2.9).
 */
export function taskAwaitsHuman(
  task: Pick<Task, "blockedReason">,
  sessionIds: readonly string[],
  pendingInstanceIds: ReadonlySet<string>,
): boolean {
  if (task.blockedReason?.trim()) return true;
  return sessionIds.some((id) => pendingInstanceIds.has(id));
}

/**
 * Resolve the buildSpaces() Space id a task attaches to: the space of its
 * sessions first (the `instances.task_id` membership), then its recorded
 * directory binding. A child task that has neither inherits its parent's
 * space — a delegation runs in the parent task's directory, so the family
 * must stay inside one project+branch group. Returns null for an
 * unattached ledger task with no placed ancestor.
 */
export function taskSpaceId(
  task: Pick<Task, "workspaceBinding" | "parentTaskId">,
  sessions: readonly Pick<TaskSessionLike, "hostId" | "workspaceId">[],
  parentById?: ReadonlyMap<string, Pick<Task, "workspaceBinding" | "parentTaskId">>,
  sessionsByTask?: ReadonlyMap<string, readonly Pick<TaskSessionLike, "hostId" | "workspaceId">[]>,
): string | null {
  for (const session of sessions) {
    if (session.hostId && session.workspaceId) {
      return spaceKey(session.hostId, session.workspaceId);
    }
  }
  const binding = task.workspaceBinding;
  if (binding?.hostId && binding?.workspaceId) {
    return spaceKey(binding.hostId, binding.workspaceId);
  }
  // Inherit the nearest placed ancestor's space; guard against cycles.
  const seen = new Set<string>();
  let parentId = task.parentTaskId ?? null;
  while (parentId && !seen.has(parentId)) {
    seen.add(parentId);
    const parent = parentById?.get(parentId);
    if (!parent) break;
    for (const session of sessionsByTask?.get(parentId) ?? []) {
      if (session.hostId && session.workspaceId) {
        return spaceKey(session.hostId, session.workspaceId);
      }
    }
    if (parent.workspaceBinding?.hostId && parent.workspaceBinding?.workspaceId) {
      return spaceKey(parent.workspaceBinding.hostId, parent.workspaceBinding.workspaceId);
    }
    parentId = parent.parentTaskId ?? null;
  }
  return null;
}

function byCreatedThenId(a: Task, b: Task): number {
  if (a.createdAt !== b.createdAt) return a.createdAt < b.createdAt ? -1 : 1;
  return a.id.localeCompare(b.id);
}

function primarySession(task: Task, sessions: readonly TaskSessionLike[]): string | null {
  const ids = sessions.map((session) => session.id);
  const placed = task.placement?.instanceId;
  if (placed && ids.includes(placed)) return placed;
  const live = sessions.find((session) => session.lifecycle !== "exited" && session.lifecycle !== "failed");
  return (live ?? sessions[0])?.id ?? null;
}

type Prepared = {
  task: Task;
  id: string;
  displayKey: string;
  sessionIds: string[];
  sessionCount: number;
  primarySessionId: string | null;
  spaceId: string | null;
  branch: string | null;
  needsHuman: boolean;
  blocked: boolean;
  children: Prepared[];
};

/**
 * Nest the prepared rows by `parentTaskId`. A parent only collects children
 * that share the same bucket (a child never follows its parent into the
 * archive or the attention group on its own); a dangling parent reference
 * renders top-level. Both levels are ordered oldest-first so the derived
 * SE-nn sequence reads top to bottom.
 */
function buildForest(flat: Prepared[]): TaskRow[] {
  const byId = new Map<string, Prepared>();
  for (const row of [...flat].sort((a, b) => byCreatedThenId(a.task, b.task))) {
    byId.set(row.id, { ...row, children: [] });
  }
  const roots: Prepared[] = [];
  for (const row of byId.values()) {
    const parentId = row.task.parentTaskId ?? null;
    if (parentId && byId.has(parentId)) {
      byId.get(parentId)!.children.push(row);
    } else {
      roots.push(row);
    }
  }
  const materialize = (node: Prepared, depth: number): TaskRow => ({
    ...node,
    depth,
    childCount: node.children.length,
    children: [...node.children]
      .sort((a, b) => byCreatedThenId(a.task, b.task))
      .map((child) => materialize(child, depth + 1)),
  });
  return [...roots]
    .sort((a, b) => byCreatedThenId(a.task, b.task))
    .map((row) => materialize(row, 0));
}

function countRows(rows: readonly TaskRow[]): number {
  return rows.reduce((sum, row) => sum + 1 + countRows(row.children), 0);
}

function flatten(rows: readonly TaskRow[]): TaskRow[] {
  return rows.flatMap((row) => [row, ...flatten(row.children)]);
}

/** One-line state label per ledger state; the board/list share the wording. */
export const TASK_STATE_LABEL: Record<Task["state"], string> = {
  pending: "待办",
  placed: "已派发",
  running: "进行中",
  stalled: "停滞",
  done: "已完成",
  failed: "失败",
  parked: "已停放",
  deferred: "已延后",
};

/**
 * The one next-step line on a task row (acceptance 6). A wait on a human
 * wins; then a blocked/failed reason; then the ledger-state phrase. Never a
 * success inference: `done` reads as 已完成, never as an unlock signal.
 */
export function taskNextStep(row: Pick<TaskRow, "needsHuman" | "blocked" | "task" | "sessionCount">): string {
  if (row.needsHuman) return "需要你处理";
  const reason = row.task.blockedReason?.trim();
  if (reason) return reason;
  if (row.task.state === "pending" && row.sessionCount === 0) return "待派发";
  if (row.task.state === "placed" && row.sessionCount === 0) return "等待会话启动";
  return TASK_STATE_LABEL[row.task.state];
}

/**
 * The one signal line on a board card (ui-spec §2.9). The first established
 * condition wins, in this exact order:
 *
 *  1. failure — the badge row with the blocked reason (a failed card stays in
 *     its placement column, never folds into done);
 *  2. 需要你 — a pending human interaction or an owner-blocked reason;
 *  3. 已合入 — an authoritative gate/land sha (D-050 invariant I1);
 *  4. 尚未合入 — a done card without a land record; the board offers no land
 *     entry, land only ever goes through the gate;
 *  5. otherwise the same {@link taskNextStep} phrase the task list renders.
 *
 * Pure: the component never infers success and never invents a state.
 */
export type TaskCardSignal =
  | { kind: "failed"; reason: string | null }
  | { kind: "needs-human" }
  | { kind: "landed"; sha7: string }
  | { kind: "unlanded" }
  | { kind: "next-step"; text: string };

export function taskCardSignal(input: {
  task: Pick<Task, "state" | "blockedReason" | "landedSha">;
  needsHuman: boolean;
  sessionCount: number;
}): TaskCardSignal {
  const { task, needsHuman, sessionCount } = input;
  if (task.state === "failed") {
    return { kind: "failed", reason: task.blockedReason?.trim() || null };
  }
  if (needsHuman) return { kind: "needs-human" };
  const sha = task.landedSha?.trim();
  if (task.state === "done") {
    return sha ? { kind: "landed", sha7: sha.slice(0, 7) } : { kind: "unlanded" };
  }
  return {
    kind: "next-step",
    text: taskNextStep({
      needsHuman,
      blocked: Boolean(task.blockedReason?.trim()),
      task: task as Task,
      sessionCount,
    }),
  };
}

function matchesNeedle(
  row: Pick<Prepared, "displayKey" | "task">,
  needle: string,
  projectLabel: string,
): boolean {
  const haystack = [
    row.displayKey,
    row.task.title,
    row.task.blockedReason ?? "",
    projectLabel,
  ]
    .join(" ")
    .toLowerCase();
  return haystack.includes(needle);
}

/**
 * Project the ledger into the ordered list groups. Pure: the component
 * supplies the live sessions, interactions, spaces and branch hydration;
 * this function never fetches and never writes.
 */
export function buildTaskGroups(input: TaskRowsInput): TaskListGroup[] {
  const tasks = [...input.tasks].sort(byCreatedThenId);
  const keys = displayKeysByTaskId(tasks);
  const taskById = new Map(tasks.map((task) => [task.id, task]));

  const sessionsByTask = new Map<string, TaskSessionLike[]>();
  for (const instance of input.instances ?? []) {
    if (!instance.taskId) continue;
    const list = sessionsByTask.get(instance.taskId) ?? [];
    list.push(instance);
    sessionsByTask.set(instance.taskId, list);
  }

  const pendingInstanceIds = new Set(
    (input.interactions ?? [])
      .filter((interaction) => interaction.state === "pending")
      .map((interaction) => interaction.instanceId),
  );

  const spacesById = new Map((input.spaces ?? []).map((space) => [space.id, space]));
  const projectName = (projectId: string) => input.projectName?.(projectId) ?? projectId;

  const prepare = (task: Task): Prepared => {
    const sessions = sessionsByTask.get(task.id) ?? [];
    const spaceId = taskSpaceId(task, sessions, taskById, sessionsByTask);
    const sessionIds = sessions.map((session) => session.id);
    const blockedReason = task.blockedReason?.trim() ? task.blockedReason.trim() : null;
    return {
      task,
      id: task.id,
      displayKey: keys.get(task.id) ?? task.id,
      sessionIds,
      sessionCount: sessionIds.length,
      primarySessionId: primarySession(task, sessions),
      spaceId,
      branch:
        (spaceId ? input.branchOfSpace?.(spaceId) : null) ?? task.placement?.branch ?? null,
      needsHuman: taskAwaitsHuman(task, sessionIds, pendingInstanceIds),
      blocked: blockedReason != null || task.state === "failed",
      children: [],
    };
  };

  const needle = input.query?.trim().toLowerCase() ?? "";
  const prepared = tasks
    .map(prepare)
    .filter((row) => !needle || matchesNeedle(row, needle, projectName(row.task.projectId)));

  const archived: Prepared[] = [];
  const attention: Prepared[] = [];
  const projectBuckets = new Map<string, { projectId: string; spaceId: string | null; rows: Prepared[] }>();

  for (const row of prepared) {
    if (row.task.archivedAt != null) {
      archived.push(row);
      // Archived tasks never appear in active groups, attention included.
      continue;
    }
    // The attention group is a pin: a needs-you task stays in its project
    // group as well.
    if (row.needsHuman) attention.push(row);
    const bucketKey = `${row.task.projectId}|${row.spaceId ?? "-"}`;
    const bucket = projectBuckets.get(bucketKey) ?? {
      projectId: row.task.projectId,
      spaceId: row.spaceId,
      rows: [],
    };
    bucket.rows.push(row);
    projectBuckets.set(bucketKey, bucket);
  }

  const groups: TaskListGroup[] = [];

  if (attention.length > 0) {
    const rows = buildForest(attention);
    groups.push({
      id: ATTENTION_GROUP_ID,
      kind: "attention",
      projectId: null,
      spaceId: null,
      project: "需要你",
      branch: null,
      blockedCount: 0,
      count: countRows(rows),
      rows,
    });
  }

  const projectGroups = [...projectBuckets.values()]
    .map((bucket) => {
      const rows = buildForest(bucket.rows);
      const space = bucket.spaceId ? spacesById.get(bucket.spaceId) : undefined;
      const branch = flatten(rows).find((row) => row.branch != null)?.branch ?? null;
      return {
        id: `prj:${bucket.projectId}:${bucket.spaceId ?? "-"}`,
        kind: "project" as const,
        projectId: bucket.projectId,
        spaceId: bucket.spaceId,
        project: projectName(bucket.projectId),
        branch,
        blockedCount: space?.blockedCount ?? 0,
        count: countRows(rows),
        rows,
      };
    })
    .sort((a, b) => {
      const nameOrder = a.project.localeCompare(b.project);
      if (nameOrder !== 0) return nameOrder;
      // Both branches null sorts equal; a known branch beats null within one
      // project, then the stable space id breaks ties (never merge spaces).
      const branchOrder = (a.branch ?? "￿").localeCompare(b.branch ?? "￿");
      if (branchOrder !== 0) return branchOrder;
      return (a.spaceId ?? "").localeCompare(b.spaceId ?? "");
    });
  groups.push(...projectGroups);

  if (archived.length > 0) {
    const rows = buildForest(archived);
    groups.push({
      id: ARCHIVE_GROUP_ID,
      kind: "archived",
      projectId: null,
      spaceId: null,
      project: "已归档",
      branch: null,
      blockedCount: 0,
      count: countRows(rows),
      rows,
    });
  }

  return groups;
}
