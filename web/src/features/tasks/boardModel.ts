/**
 * Pure view model for the desktop kanban board (plan task-model task 6
 * t-board-ui; D-050 §5/§9, ui-spec §2.9).
 *
 * The Hub stays the projection authority: `GET /v1/board` already groups
 * every task into a derived `boardColumn` (see boardColumns.ts, task 4) plus
 * the display-only `SE-nn` key. This module never re-derives a column for
 * rendering — it composes the wire view into card view models:
 *
 *  - exactly three work columns (待办/进行中/已完成); archived items live in
 *    a separate fold the UI shows through a filter, never as a fourth column;
 *  - each card carries its sessions (the `instances.task_id` membership,
 *    placement first), the config-reuse label, and the sharing count;
 *  - drop legality is precomputed per card from the same legal-transition
 *    graph the Hub enforces (columnMoveHops), so an unreachable column
 *    carries a reason instead of silently rejecting the drop;
 *  - 「与 N 个 task 共用」 is derived from the lease identity key
 *    `(hostId, workspaceId, dir_key)` — the Space key is never relaxed
 *    (D-024): same-name directories on another host or another workspace id
 *    never merge.
 *
 * No React, no fetch: every rule is unit-tested in boardModel.test.ts.
 */
import type { components } from "../../lib/api.generated";
import type { BoardColumn, Task, TaskState } from "../../types/generated";
import { sharedWithTasksLabel } from "./binding";
import {
  WORK_COLUMNS,
  boardColumn,
  columnMoveHops,
  isTerminalState,
} from "./boardColumns";

export type BoardView = components["schemas"]["BoardView"];
export type BoardItem = components["schemas"]["BoardItem"];

/** The three live columns, in render order. Archived is a filter fold. */
export const BOARD_WORK_COLUMNS = WORK_COLUMNS;

export type WorkColumn = (typeof WORK_COLUMNS)[number];

/** Column headings the board renders; the archive fold uses 已归档. */
export const BOARD_COLUMN_LABEL: Record<BoardColumn, string> = {
  todo: "待办",
  "in-progress": "进行中",
  done: "已完成",
  archived: "已归档",
};

/**
 * Structural session slice a card renders. The normalized Instance carries
 * these; tests pass minimal fixtures.
 */
export type CardSession = {
  id: string;
  taskId?: string | null;
  kind?: string;
  name?: string | null;
  lifecycle?: string;
  providerProfileId?: string | null;
  updatedAt: string;
};

/** One work column's drop legality for a card right now. */
export type DropLegality = {
  allowed: boolean;
  /** Legal set_task_state hops (excluding the current state); [] when blocked. */
  hops: TaskState[];
  /** Operator-facing reason when the column must reject the drag. */
  reason: string | null;
};

export type BoardCard = {
  id: string;
  item: BoardItem;
  /** Server-derived column; the UI never recomputes it for placement. */
  column: BoardColumn;
  displayKey: string;
  title: string;
  failed: boolean;
  archived: boolean;
  blockedReason: string | null;
  sessions: CardSession[];
  sessionCount: number;
  /** Placement instance first, then a live session, then the first one. */
  primarySessionId: string | null;
  /** Tasks bound to the same lease identity key (the refcount view). */
  sharedCount: number;
  /** 「与 N 个 task 共用」, or null when the task has the directory alone. */
  sharedLabel: string | null;
  /** Current config-reuse profile label; `default` unless a session names one. */
  configLabel: string;
  drops: Record<WorkColumn, DropLegality>;
};

export type BoardColumnModel = {
  column: WorkColumn;
  label: string;
  cards: BoardCard[];
};

export type BoardModel = {
  /** Always exactly three work columns, even when empty. */
  columns: BoardColumnModel[];
  /** Archived cards, rendered only through the archive filter. */
  archived: BoardCard[];
  byId: Map<string, BoardCard>;
  total: number;
};

// ── Sharing (lease identity key, D-024 key never relaxed) ─────────────────

/**
 * The lease row's composite identity: `(hostId, workspaceId, dir_key)` where
 * `dir_key` is the bare sibling name, or `.` for a reuse-to-root binding.
 * Null when the task has no directory binding — such a task never shares.
 */
export function bindingShareKey(
  task: Pick<Task, "workspaceBinding">,
): string | null {
  const binding = task.workspaceBinding;
  if (!binding?.hostId || !binding.workspaceId) return null;
  const dirKey = binding.worktreeName?.trim() || ".";
  return `${binding.hostId}|${binding.workspaceId}|${dirKey}`;
}

/**
 * Refcount view over the board slice: tasks per lease identity key. Counted
 * across every item (including archived ones), because archiving is not a
 * lease return — the refcount only changes on the explicit return route.
 */
export function sharedCounts(
  items: readonly Pick<Task, "workspaceBinding">[],
): Map<string, number> {
  const counts = new Map<string, number>();
  for (const item of items) {
    const key = bindingShareKey(item);
    if (!key) continue;
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  return counts;
}

// ── Drop legality ─────────────────────────────────────────────────────────

export const TERMINAL_REASON = "终态任务不能通过拖卡改变状态";
export const ARCHIVED_REASON = "已归档任务不参与看板流转";
export const SAME_COLUMN_REASON = "卡片已在该列";

/**
 * Precompute the legality of dropping this card on a work column. The path
 * is exactly {@link columnMoveHops} (BFS over `can_transition_to`): a legal
 * drag carries its hop sequence, an illegal one carries the operator-facing
 * reason — the column disables itself with that reason rather than letting
 * the drop 4xx.
 */
export function dropLegality(
  task: Pick<Task, "state" | "archivedAt" | "placement" | "blockedReason">,
  target: WorkColumn,
): DropLegality {
  if (task.archivedAt != null) return block(ARCHIVED_REASON);
  if (isTerminalState(task.state)) {
    if (task.state === "failed") {
      const reason = task.blockedReason?.trim();
      return block(reason ? `失败任务：${reason}` : TERMINAL_REASON);
    }
    return block(TERMINAL_REASON);
  }
  if (boardColumn(task) === target) return block(SAME_COLUMN_REASON);
  const hops = columnMoveHops(task, target);
  if (hops) return { allowed: true, hops, reason: null };
  // Non-terminal + illegal pair: the only skip across adjacent work columns
  // is to-do → done; the card must pass through in-progress deliberately.
  if (boardColumn(task) === "todo" && target === "done") {
    return block("需先移到进行中列");
  }
  return block("当前状态无法移动到该列");
}

function block(reason: string): DropLegality {
  return { allowed: false, hops: [], reason };
}

/** Work columns this card may legally be dragged to right now. */
export function reachableWorkColumns(task: BoardCard["item"]): WorkColumn[] {
  return BOARD_WORK_COLUMNS.filter((column) => dropLegality(task, column).allowed);
}

// ── Sessions ──────────────────────────────────────────────────────────────

function cardSessions(
  task: BoardItem,
  instances: readonly CardSession[],
): CardSession[] {
  const sessions = instances.filter((instance) => instance.taskId === task.id);
  const placedId = task.placement?.instanceId;
  return [...sessions].sort((a, b) => {
    if (a.id === placedId) return -1;
    if (b.id === placedId) return 1;
    const aLive = a.lifecycle !== "exited" && a.lifecycle !== "failed" ? 0 : 1;
    const bLive = b.lifecycle !== "exited" && b.lifecycle !== "failed" ? 0 : 1;
    if (aLive !== bLive) return aLive - bLive;
    return a.updatedAt < b.updatedAt ? 1 : a.updatedAt > b.updatedAt ? -1 : 0;
  });
}

function primarySession(task: BoardItem, sessions: readonly CardSession[]): string | null {
  const placedId = task.placement?.instanceId;
  if (placedId && sessions.some((session) => session.id === placedId)) return placedId;
  return sessions[0]?.id ?? null;
}

/** The applied config-reuse label (B.8): a session's profile, else default. */
export function configLabelOf(sessions: readonly CardSession[]): string {
  return sessions.find((session) => session.providerProfileId?.trim())?.providerProfileId
    ?? "default";
}

// ── Model composition ─────────────────────────────────────────────────────

function matchesNeedle(item: BoardItem, needle: string): boolean {
  return [item.displayKey, item.title, item.blockedReason ?? ""]
    .join(" ")
    .toLowerCase()
    .includes(needle);
}

function buildCard(
  item: BoardItem,
  instances: readonly CardSession[],
  counts: ReadonlyMap<string, number>,
): BoardCard {
  const sessions = cardSessions(item, instances);
  const key = bindingShareKey(item);
  const sharedCount = key ? counts.get(key) ?? 1 : 1;
  return {
    id: item.id,
    item,
    column: item.boardColumn,
    displayKey: item.displayKey,
    title: item.title,
    failed: item.state === "failed",
    archived: item.archivedAt != null,
    blockedReason: item.blockedReason?.trim() || null,
    sessions,
    sessionCount: sessions.length,
    primarySessionId: primarySession(item, sessions),
    sharedCount,
    sharedLabel: sharedWithTasksLabel(sharedCount),
    configLabel: configLabelOf(sessions),
    drops: {
      todo: dropLegality(item, "todo"),
      "in-progress": dropLegality(item, "in-progress"),
      done: dropLegality(item, "done"),
    },
  };
}

/**
 * Compose the `GET /v1/board` view into the three-column board model. The
 * free-text search filters cards (SE key / title / blocked reason, same
 * mechanism as the task-list rail); archived cards are always returned
 * separately and the component shows them only behind the archive filter.
 */
export function buildBoardModel(input: {
  view: BoardView | null;
  instances?: readonly CardSession[];
  query?: string;
}): BoardModel {
  const groups = input.view?.columns;
  const all: BoardItem[] = groups
    ? [...groups.todo, ...groups["in-progress"], ...groups.done, ...groups.archived]
    : [];
  const counts = sharedCounts(all);
  const instances = input.instances ?? [];
  const needle = input.query?.trim().toLowerCase() ?? "";

  const cardOf = (item: BoardItem): BoardCard => buildCard(item, instances, counts);
  const visible = (item: BoardItem): boolean => !needle || matchesNeedle(item, needle);

  const makeColumn = (column: WorkColumn): BoardColumnModel => ({
    column,
    label: BOARD_COLUMN_LABEL[column],
    cards: (groups?.[column] ?? []).filter(visible).map(cardOf),
  });

  const archived = (groups?.archived ?? []).filter(visible).map(cardOf);
  const columns = BOARD_WORK_COLUMNS.map(makeColumn);
  const byId = new Map<string, BoardCard>();
  for (const card of [...columns.flatMap((column) => column.cards), ...archived]) {
    byId.set(card.id, card);
  }

  return {
    columns,
    archived,
    byId,
    total: groups
      ? groups.todo.length + groups["in-progress"].length + groups.done.length + groups.archived.length
      : 0,
  };
}
