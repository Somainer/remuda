/**
 * Pure read-only board projection mirroring `Task::board_column` on the Hub
 * (Rust: crates/remuda-protocol/src/task.rs; plan task-model B.4 / D-050).
 *
 * The eight ledger states are authoritative; a board column is derived from
 * `state + archivedAt + placement`, never stored. Column drags are not a
 * board verb: this module turns a column-to-column drag into a sequence of
 * legal `set_task_state` hops the caller PATCHes one by one. When any hop is
 * illegal the whole move rejects up front (a null plan), so the UI never
 * sends a partial move and never needs a board-specific write endpoint.
 */
import type { BoardColumn, Task, TaskState } from "../../types/generated";

/** Column order the board renders; `archived` is an orthogonal filter group. */
export const BOARD_COLUMNS = [
  "todo",
  "in-progress",
  "done",
  "archived",
] as const satisfies readonly BoardColumn[];

/**
 * The three live work columns (everything except the archive group). Staged
 * for the board UI in task 6 (t-board-ui), which renders these as columns
 * and treats `archived` as a separate filter.
 */
export const WORK_COLUMNS = ["todo", "in-progress", "done"] as const;

/** Legal ledger edges; mirrors `TaskState::can_transition_to` exactly. */
const LEGAL_TRANSITIONS: Partial<Record<TaskState, readonly TaskState[]>> = {
  pending: ["placed", "deferred", "failed"],
  placed: ["running", "pending", "deferred", "failed"],
  running: ["stalled", "done", "failed", "parked"],
  stalled: ["running", "failed", "parked"],
  parked: ["running", "pending", "deferred", "failed"],
  deferred: ["pending", "placed", "failed"],
};

export function canTransition(from: TaskState, to: TaskState): boolean {
  return LEGAL_TRANSITIONS[from]?.includes(to) ?? false;
}

/** Terminal states never move and never backfill a column via state edits. */
export function isTerminalState(state: TaskState): boolean {
  return state === "done" || state === "failed";
}

/**
 * Project one task onto its board column. `archivedAt` is orthogonal and
 * wins without changing state. `failed` has no history field, so it is
 * placed deterministically from existing storage: a held placement means it
 * failed mid-flight (in-progress), none means it never left to-do. Only
 * `done` reads as done, so the done column can never be mistaken for the
 * dependency-unlock gate.
 */
export function boardColumn(
  task: Pick<Task, "state" | "archivedAt" | "placement">,
): BoardColumn {
  if (task.archivedAt != null) return "archived";
  switch (task.state) {
    case "pending":
    case "placed":
    case "deferred":
    case "parked":
      return "todo";
    case "running":
    case "stalled":
      return "in-progress";
    case "done":
      return "done";
    case "failed":
      return task.placement != null ? "in-progress" : "todo";
  }
}

/** True for the red failure badge a failed card carries inside its column. */
export function isFailedCard(
  task: Pick<Task, "state">,
): task is Task & { state: "failed" } {
  return task.state === "failed";
}

/**
 * Intermediate ledger states for a column-to-column drag, excluding the
 * current state and ending at the final target state. Null means the drag
 * is illegal as a whole and must be rejected before any PATCH is sent.
 *
 * Every path is *computed* from {@link canTransition} (BFS over the state
 * machine), so the two can never drift: terminal states are never used as
 * intermediates, and when any hop lacks an edge the whole move rejects.
 *
 * Allowed column pairs and their target-state set (plan B.4):
 * - to-do → in-progress lands on `running`/`stalled`. BFS yields
 *   `pending/deferred → placed → running` (multi-hop) and the direct
 *   `placed/parked → running`;
 * - in-progress → done lands on `done`; there is no stalled→done edge, so a
 *   stalled card takes `stalled → running → done`;
 * - in-progress → to-do lands on the nearest legal to-do state, `parked`.
 *
 * To-do → done is deliberately not a board move (the card must pass through
 * the in-progress column deliberately); archiving uses the archive route,
 * not a state transition.
 */
export function columnMoveHops(
  task: Pick<Task, "state" | "archivedAt" | "placement">,
  target: BoardColumn,
): TaskState[] | null {
  if (task.archivedAt != null) return null;
  const fromColumn = boardColumn(task);
  if (fromColumn === target || fromColumn === "archived") return null;
  if (isTerminalState(task.state)) return null;

  let targetStates: readonly TaskState[];
  switch (target) {
    case "in-progress":
      if (fromColumn !== "todo") return null;
      targetStates = ["running", "stalled"];
      break;
    case "done":
      if (fromColumn !== "in-progress") return null;
      targetStates = ["done"];
      break;
    case "todo":
      if (fromColumn !== "in-progress") return null;
      targetStates = ["pending", "placed", "deferred", "parked"];
      break;
    case "archived":
      // Archiving is the dedicated archive route, not a state transition.
      return null;
  }

  const path = shortestLegalPath(task.state, new Set(targetStates));
  // BFS includes the start node; the PATCH sequence is everything after it.
  return path && path.length > 1 ? path.slice(1) : null;
}

/**
 * Shortest legal path from `start` to any state in `targets`, computed
 * purely from {@link canTransition}. Terminal states (`done`, `failed`)
 * never serve as intermediate steps: done cannot be traversed and failed
 * must never be introduced by a drag. Returns the full path including
 * `start`, or null when no legal path exists.
 */
function shortestLegalPath(
  start: TaskState,
  targets: ReadonlySet<TaskState>,
): TaskState[] | null {
  if (targets.has(start)) return [start];
  const queue: TaskState[] = [start];
  const predecessor = new Map<TaskState, TaskState>();
  const seen = new Set<TaskState>([start]);
  while (queue.length > 0) {
    const current = queue.shift()!;
    if (targets.has(current) && current !== start) {
      const path: TaskState[] = [];
      let node: TaskState | undefined = current;
      while (node !== undefined) {
        path.unshift(node);
        node = predecessor.get(node);
      }
      return path;
    }
    for (const next of LEGAL_TRANSITIONS[current] ?? []) {
      if (seen.has(next)) continue;
      // Never traverse a terminal state to reach a column target.
      if (!targets.has(next) && isTerminalState(next)) continue;
      seen.add(next);
      predecessor.set(next, current);
      queue.push(next);
    }
  }
  return null;
}

/** Columns the card may legally be dragged to right now (UI disables rest). */
export function reachableColumns(
  task: Pick<Task, "state" | "archivedAt" | "placement">,
): BoardColumn[] {
  return BOARD_COLUMNS.filter((column) => columnMoveHops(task, column) != null);
}

/**
 * Invariant I1: a dependency edge unlocks only when the upstream task
 * carries a landed sha. A task sitting in the done column with no
 * `landedSha` still holds every dependent's edge. Mirrors
 * `Task::locked_deps`.
 */
export function isUnlockedByLandedSha(
  upstream: Pick<Task, "landedSha">,
): boolean {
  return upstream.landedSha != null;
}

/** Ids of a task's dependency edges that have not unlocked yet. */
export function lockedDepIds(
  task: Pick<Task, "deps">,
  tasksById: ReadonlyMap<string, Pick<Task, "landedSha">>,
): string[] {
  return (task.deps ?? [])
    .map((dep) => dep.taskId)
    .filter((id) => {
      const upstream = tasksById.get(id);
      // A missing upstream never silently unlocks, matching the Rust helper.
      return upstream == null || !isUnlockedByLandedSha(upstream);
    });
}

/** Format the display-only per-project sequence as `SE-nn` (D12). */
export function formatDisplayKey(seq: number): string {
  return `SE-${String(seq).padStart(2, "0")}`;
}

/**
 * Assign display-only SE-nn keys per project in oldest-first order. The
 * sequence is derived, never stored; call it at projection time.
 */
export function displayKeysByTaskId(
  tasks: readonly Pick<Task, "id" | "projectId" | "createdAt">[],
): Map<string, string> {
  const ordered = [...tasks].sort((a, b) =>
    a.createdAt < b.createdAt ? -1 : a.createdAt > b.createdAt ? 1 : 0,
  );
  const counters = new Map<string, number>();
  const keys = new Map<string, string>();
  for (const task of ordered) {
    const seq = (counters.get(task.projectId) ?? 0) + 1;
    counters.set(task.projectId, seq);
    keys.set(task.id, formatDisplayKey(seq));
  }
  return keys;
}
