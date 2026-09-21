import { projectStatus } from "../../lib/status";
import type { Instance, UiStatus } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import type { Observation } from "../../types/observation";
import type { Task } from "../../types/generated";
import type { UsageRollup } from "../session/contextUsage";
import { nextStep, type RowScreen } from "../session/nextStep";
import { OTHER_SPACE, type Space } from "../spaces/store";
import { rankQuickFind } from "../search/quickFindSearch";
import { buildTaskGroups, type TaskListGroup } from "../tasks/taskRows";

/**
 * Pure derivation for the `/m` phone home (ui-spec §4.7 / §2.1, D-038/D-049).
 *
 * It owns no state of its own: `buildSpaces()` groups the instances,
 * `nextStep()` produces the one sentence, `rankQuickFind()` owns the
 * title/project/host search boundary, and the Hub rollup owns the context
 * percentage. This module only decides grouping order, the error-as-body
 * rule and the row shape, so every rule below is unit-testable without a
 * DOM.
 */

export type HomeOrder = "clock" | "list";

/** Device-local (never cross-device) last home ordering, ui-spec §1.4 style. */
export const HOME_ORDER_KEY = "remuda.mobile.home.order.v1";

type OrderStorage = Pick<Storage, "getItem" | "setItem"> | null | undefined;

export function readHomeOrder(storage?: OrderStorage): HomeOrder {
  try {
    return storage?.getItem(HOME_ORDER_KEY) === "list" ? "list" : "clock";
  } catch {
    return "clock";
  }
}

export function writeHomeOrder(order: HomeOrder, storage?: OrderStorage): void {
  try {
    storage?.setItem(HOME_ORDER_KEY, order);
  } catch {
    /* The ordering is a convenience; never break the home on storage failure. */
  }
}

export type HomeRow = {
  id: string;
  instance: Instance;
  title: string;
  status: UiStatus;
  /** The one body sentence: nextStep() verbatim, unless an error owns the slot. */
  body: string;
  bodyIsError: boolean;
  /** 0..100 used-context share; null = unknown, and the ring must not render. */
  contextPct: number | null;
  blocked: boolean;
  /** Exited rows advertise 恢复 only when the resume capability is really there (D-026). */
  canResume: boolean;
  updatedAt: string;
};

export type HomeGroup = {
  id: string;
  project: string;
  hostName: string;
  /** Git branch of the project; null when the Node gave no branch. */
  branch: string | null;
  /** Always the buildSpaces() count, even while a search hides some rows. */
  blockedCount: number;
  rows: HomeRow[];
};

export type HomeRowsInput = {
  spaces: Space[];
  interactions: Interaction[];
  order: HomeOrder;
  query: string;
  titleOf: (instanceId: string) => string;
  hostNameOf: (hostId?: string) => string;
  /** Live git branch per Space id; undefined/null = branch unknown. */
  branchOf?: (spaceId: string) => string | null | undefined;
  rollupOf: (instanceId: string) => UsageRollup | null;
  screenOf?: (instanceId: string) => RowScreen;
  summaryOf?: (instanceId: string) => string | undefined;
  eventsOf?: (instanceId: string) => Observation[] | undefined;
};

/**
 * The error text that owns the row body when the session has one
 * (report §11.3 point 3: Moshi puts the raw error here, not a wire string).
 *
 * `instance.lastError` is the Hub-merged channel (hub store.rs folds native
 * severity=error lifecycle events into it); the journal fallback mirrors the
 * same merge for events the polled instance row has not folded yet.
 */
export function homeError(instance: Instance, events?: Observation[]): string | null {
  const direct = instance.lastError?.trim();
  if (direct) return direct;
  if (!events) return null;
  for (let i = events.length - 1; i >= 0; i--) {
    const event = events[i];
    if (event.kind !== "lifecycle") continue;
    const payload = event.payload as {
      type?: string;
      severity?: string;
      nativeName?: string;
      relatedIds?: { lastError?: unknown };
      status?: { value?: unknown };
    };
    if (payload?.type !== "native") continue;
    const name = String(payload.nativeName ?? "").toLowerCase();
    const failed =
      payload.severity === "error" ||
      name.includes("error") ||
      (typeof payload.relatedIds?.lastError === "string" &&
        payload.relatedIds.lastError.trim().length > 0);
    if (!failed) continue;
    const text =
      typeof payload.relatedIds?.lastError === "string"
        ? payload.relatedIds.lastError
        : typeof payload.status?.value === "string"
          ? payload.status.value
          : "";
    const trimmed = text.trim();
    if (trimmed) return trimmed;
  }
  return null;
}

/**
 * Body-slot decision. Unknown connectivity/lifecycle wins over everything:
 * the row reads 状态待确认 and an error can never turn it into a positive
 * phrase (D-038). On every other status an error replaces the next-step
 * sentence verbatim.
 */
export function homeBody(
  status: UiStatus,
  step: { text: string },
  error: string | null,
): { text: string; isError: boolean } {
  if (status === "unknown") return { text: step.text, isError: false };
  if (error) return { text: error, isError: true };
  return { text: step.text, isError: false };
}

function rowTime(row: HomeRow): number {
  return Date.parse(row.updatedAt) || 0;
}

/**
 * Blocked rows are pinned to the top of every group in every ordering
 * (ui-spec §2.1 待处理置顶). Clock = most recent change first; list = title.
 */
function compareRows(a: HomeRow, b: HomeRow, order: HomeOrder): number {
  if (a.blocked !== b.blocked) return a.blocked ? -1 : 1;
  if (order === "list") {
    return a.title.localeCompare(b.title) || a.id.localeCompare(b.id);
  }
  return rowTime(b) - rowTime(a) || a.id.localeCompare(b.id);
}

function compareGroups(a: HomeGroup, b: HomeGroup, order: HomeOrder): number {
  if (a.id === OTHER_SPACE) return 1;
  if (b.id === OTHER_SPACE) return -1;
  if (order === "list") {
    return a.project.localeCompare(b.project) || a.id.localeCompare(b.id);
  }
  const latest = (group: HomeGroup) =>
    group.rows.reduce((max, row) => Math.max(max, rowTime(row)), 0);
  return latest(b) - latest(a) || a.id.localeCompare(b.id);
}

export function buildHomeGroups(input: HomeRowsInput): HomeGroup[] {
  const needle = input.query.trim().toLowerCase();
  // The QuickFind ranker is the fixed title > project(Space) > host > id
  // boundary. Mobile search drops id hits: the phone home search promises
  // 标题 / 项目 / 主机 only, and an opaque id must not leak out here.
  const ranked = rankQuickFind({
    spaces: input.spaces,
    query: needle,
    titleOf: input.titleOf,
    hostNameOf: input.hostNameOf,
    connection: "live",
  });
  const allowed = new Set(
    (needle ? ranked.hits.filter((hit) => hit.field !== "id") : ranked.hits).map(
      (hit) => hit.instance.id,
    ),
  );

  const groups: HomeGroup[] = [];
  for (const space of input.spaces) {
    const rows: HomeRow[] = [];
    for (const instance of space.instances) {
      if (needle && !allowed.has(instance.id)) continue;
      const status = projectStatus(instance);
      const pending =
        input.interactions.find(
          (interaction) =>
            interaction.instanceId === instance.id && interaction.state === "pending",
        ) ?? null;
      const step = nextStep(
        instance,
        pending,
        input.screenOf?.(instance.id),
        input.summaryOf?.(instance.id),
      );
      const error = homeError(instance, input.eventsOf?.(instance.id));
      const body = homeBody(status, step, error);
      rows.push({
        id: instance.id,
        instance,
        title: input.titleOf(instance.id) || "会话",
        status,
        body: body.text,
        bodyIsError: body.isError,
        contextPct: input.rollupOf(instance.id)?.contextPct ?? null,
        blocked: status === "blocked",
        canResume:
          status === "exited" &&
          instance.capabilities.capabilities.resume?.state === "supported",
        updatedAt: instance.updatedAt,
      });
    }
    if (!rows.length) continue;
    rows.sort((a, b) => compareRows(a, b, input.order));
    groups.push({
      id: space.id,
      project: space.name,
      hostName: input.hostNameOf(space.hostId),
      branch: input.branchOf?.(space.id) ?? null,
      blockedCount: space.blockedCount,
      rows,
    });
  }
  groups.sort((a, b) => compareGroups(a, b, input.order));
  return groups;
}

/**
 * D-050 (task-model task 5): the task grouping layer the `/m` home stacks
 * above its existing project + git branch session groups. This is only an
 * additive projection — it delegates wholesale to the task list's pure
 * `buildTaskGroups()` (需要你 first, project+branch groups with the
 * buildSpaces() blocked count, parent/child nesting, SE-nn keys, 已归档
 * folded away) and renders no second transcript: every task row links to the
 * shared `/s/:id`.
 */
export type HomeTaskLayerInput = {
  tasks: readonly Task[];
  instances: readonly Instance[];
  interactions: readonly Pick<Interaction, "instanceId" | "state">[];
  spaces: readonly Space[];
  projectName?: (projectId: string) => string | null | undefined;
  branchOfSpace?: (spaceId: string) => string | null | undefined;
  query?: string;
};

export function buildHomeTaskLayer(input: HomeTaskLayerInput): TaskListGroup[] {
  return buildTaskGroups({
    tasks: input.tasks,
    instances: input.instances,
    interactions: input.interactions,
    spaces: input.spaces,
    projectName: input.projectName,
    branchOfSpace: input.branchOfSpace,
    query: input.query,
  });
}
