import type { Instance, Kind, UiStatus } from "../../types/instance";
import type { Workspace } from "../../types/workspace";

/**
 * The session-list filter model (exploration §5 P0-1).
 *
 * The URL is the single source of truth for what is filtered; this module is
 * the pure translation between `URLSearchParams` and the conditions the list
 * renders, plus the rules for what a condition is allowed to mean inside a
 * Space. Keeping it free of React means the round-trip, the scope derivation
 * and the Space-switch pruning are all directly testable.
 *
 * Two invariants hold the URL and the Space store from fighting each other
 * (UX plan §4 risk 2):
 *
 * - Derivation is one-way. Params are read into conditions; nothing here ever
 *   writes back into the Space store from an effect.
 * - Switching Space rewrites the params exactly once, via {@link pruneForScope}.
 */

export const STATUSES: UiStatus[] = ["blocked", "working", "starting", "idle", "exited", "unknown"];
export const KINDS: Kind[] = ["claude", "codex", "grok", "agy", "terminal"];

export const STATUS_LABELS: Record<string, string> = {
  blocked: "待处理",
  working: "进行中",
  starting: "启动中",
  idle: "空闲",
  exited: "已退出",
  unknown: "状态待确认",
};

/** `scope=all` is the explicit opt-out of the current Space (P0-1 rule 2). */
export const SCOPE_KEY = "scope";
export const GLOBAL_SCOPE = "all";

export type FilterConditions = {
  /** Free text over title / cwd / native id / instance id. */
  q: string;
  status: string[];
  kind: string[];
  host: string[];
  workspace: string[];
  /** `space` keeps the list inside the selected Space; `all` searches every Space. */
  scope: "space" | "all";
};

/** Where the list is looking right now, for the toolbar's scope line. */
export type FilterScope = {
  kind: "space" | "all";
  /** Name of the Space the list is pinned to, when pinned. */
  spaceName?: string;
  hostId?: string;
  workspaceId?: string;
  label: string;
};

export type SelectedChip = {
  /** Param this chip lives in; removing the chip removes this value. */
  key: "q" | "status" | "kind" | "host" | "workspace";
  value: string;
  label: string;
};

export function csv(params: URLSearchParams, key: string): string[] {
  return (params.get(key) ?? "").split(",").filter(Boolean);
}

export function readConditions(params: URLSearchParams): FilterConditions {
  return {
    q: params.get("q") ?? "",
    status: csv(params, "status"),
    kind: csv(params, "kind"),
    host: csv(params, "host"),
    workspace: csv(params, "workspace"),
    scope: params.get(SCOPE_KEY) === GLOBAL_SCOPE ? "all" : "space",
  };
}

/**
 * Conditions back to params, preserving any unrelated params already present
 * (a deep link may carry navigation state this module knows nothing about).
 */
export function writeConditions(params: URLSearchParams, next: FilterConditions): URLSearchParams {
  const out = new URLSearchParams(params);
  const set = (key: string, value: string) => (value ? out.set(key, value) : out.delete(key));
  set("q", next.q);
  set("status", next.status.join(","));
  set("kind", next.kind.join(","));
  set("host", next.host.join(","));
  set("workspace", next.workspace.join(","));
  set(SCOPE_KEY, next.scope === "all" ? GLOBAL_SCOPE : "");
  return out;
}

export function toggleValue(values: string[], value: string): string[] {
  return values.includes(value) ? values.filter((item) => item !== value) : [...values, value];
}

/** True when any condition would narrow the list. */
export function hasConditions(conditions: FilterConditions): boolean {
  return Boolean(
    conditions.q.trim() || conditions.status.length || conditions.kind.length || conditions.host.length || conditions.workspace.length,
  );
}

/** How many distinct conditions are active, for the 筛选 button's badge. */
export function conditionCount(conditions: FilterConditions): number {
  return (
    (conditions.q.trim() ? 1 : 0) +
    conditions.status.length +
    conditions.kind.length +
    conditions.host.length +
    conditions.workspace.length
  );
}

export function describeScope(
  conditions: FilterConditions,
  space: { name?: string; hostId?: string; workspaceId?: string } | undefined,
  hostName: (hostId: string) => string,
): FilterScope {
  if (conditions.scope === "all" || !space?.hostId || !space.workspaceId) {
    return { kind: "all", label: "全局：所有空间" };
  }
  const host = hostName(space.hostId);
  return {
    kind: "space",
    spaceName: space.name,
    hostId: space.hostId,
    workspaceId: space.workspaceId,
    // Same-name workspaces on different hosts are only distinguishable with the
    // host spelled out, so the fixed scope always names both (§2.2).
    label: `当前 Space 固定范围：${space.name ?? "会话"} · ${host}`,
  };
}

/**
 * The conditions the filter panel may offer.
 *
 * Inside a Space the list is already pinned to one `hostId + workspaceId`, so
 * offering host/workspace conditions there can only ever produce a
 * mutually-exclusive, zero-result query (P0-1 rule 2). Those two conditions
 * appear only in global scope.
 */
export function availableConditions(scope: FilterScope): { host: boolean; workspace: boolean; status: true; kind: true } {
  const global = scope.kind === "all";
  return { host: global, workspace: global, status: true, kind: true };
}

/**
 * Drop conditions that cannot apply in the new scope, keep the ones that still
 * can (P0-1 rule 5).
 *
 * Text and status survive a Space switch because they mean the same thing
 * anywhere. Host and workspace do not: they were chosen against the previous
 * Space's inventory, and inside a new fixed Space they are either redundant or
 * contradictory. The caller applies the result as a single `replace: true`
 * navigation, so the switch never leaves a stale condition in the URL and never
 * stacks a history entry per Space change.
 */
export function pruneForScope(conditions: FilterConditions, scope: FilterScope): { conditions: FilterConditions; dropped: SelectedChip[] } {
  const allowed = availableConditions(scope);
  const dropped: SelectedChip[] = [];
  if (!allowed.host) for (const value of conditions.host) dropped.push({ key: "host", value, label: value });
  if (!allowed.workspace) for (const value of conditions.workspace) dropped.push({ key: "workspace", value, label: value });
  if (!dropped.length) return { conditions, dropped };
  return {
    conditions: { ...conditions, host: allowed.host ? conditions.host : [], workspace: allowed.workspace ? conditions.workspace : [] },
    dropped,
  };
}

/** Conditions with every narrowing cleared, keeping the current scope. */
export function clearedConditions(conditions: FilterConditions): FilterConditions {
  return { q: "", status: [], kind: [], host: [], workspace: [], scope: conditions.scope };
}

export type MatchContext = {
  titleOf: (id: string) => string;
  workspaces: Workspace[];
};

function matchesText(instance: Instance, q: string, context: MatchContext): boolean {
  const needle = q.trim().toLowerCase();
  if (!needle) return true;
  const title = context.titleOf(instance.id).toLowerCase();
  const workspace = context.workspaces.find((w) => w.id === instance.workspaceId && w.hostId === instance.hostId);
  const cwd = workspace?.rootPath.toLowerCase() ?? "";
  const native = instance.nativeRef.sessionId.state === "known" ? instance.nativeRef.sessionId.value.toLowerCase() : "";
  return title.includes(needle) || cwd.includes(needle) || native.includes(needle) || instance.id.toLowerCase().includes(needle);
}

export function applyFilters(
  instances: Instance[],
  conditions: FilterConditions,
  statusOf: (instance: Instance) => UiStatus,
  context: MatchContext,
): Instance[] {
  return instances.filter((instance) => {
    if (conditions.status.length && !conditions.status.includes(statusOf(instance))) return false;
    if (conditions.kind.length && !conditions.kind.includes(instance.kind)) return false;
    if (conditions.host.length && !conditions.host.includes(instance.hostId)) return false;
    if (conditions.workspace.length && !conditions.workspace.includes(instance.workspaceId)) return false;
    return matchesText(instance, conditions.q, context);
  });
}

/**
 * The active conditions as removable chips, in a stable display order.
 *
 * Host and workspace resolve to human names here; a workspace shared by name
 * across hosts is qualified with its host so the chip stays unambiguous.
 */
export function selectedChips(
  conditions: FilterConditions,
  names: { hostName: (id: string) => string; workspaceLabel: (id: string) => string; workspaceAmbiguous: (id: string) => boolean; workspaceHost: (id: string) => string },
): SelectedChip[] {
  const chips: SelectedChip[] = [];
  if (conditions.q.trim()) chips.push({ key: "q", value: conditions.q, label: `搜索：${conditions.q.trim()}` });
  for (const value of conditions.status) chips.push({ key: "status", value, label: STATUS_LABELS[value] ?? value });
  for (const value of conditions.kind) chips.push({ key: "kind", value, label: value });
  for (const value of conditions.host) chips.push({ key: "host", value, label: `主机：${names.hostName(value)}` });
  for (const value of conditions.workspace) {
    const label = names.workspaceLabel(value);
    chips.push({
      key: "workspace",
      value,
      label: names.workspaceAmbiguous(value) ? `目录：${label} · ${names.workspaceHost(value)}` : `目录：${label}`,
    });
  }
  return chips;
}

/** Remove one chip's value from the conditions it came from. */
export function withoutChip(conditions: FilterConditions, chip: SelectedChip): FilterConditions {
  if (chip.key === "q") return { ...conditions, q: "" };
  return { ...conditions, [chip.key]: conditions[chip.key].filter((value) => value !== chip.value) };
}

/**
 * Which empty state the list is in, if any (P0-1 rule 4).
 *
 * These are four different situations with four different next steps, and
 * collapsing them into one "nothing here" is what makes a filtered-to-zero list
 * look like data loss. `no-matches` is the only one caused by the user's own
 * conditions, and the only one offering to clear them.
 */
export type EmptyState = "no-hosts" | "no-workspaces" | "no-sessions" | "no-matches" | null;

export function emptyState(input: {
  hostCount: number;
  workspaceCount: number;
  sourceCount: number;
  matchCount: number;
  conditions: FilterConditions;
}): EmptyState {
  if (input.hostCount === 0) return "no-hosts";
  if (input.workspaceCount === 0) return "no-workspaces";
  if (input.sourceCount === 0) return "no-sessions";
  if (input.matchCount === 0) return hasConditions(input.conditions) ? "no-matches" : "no-sessions";
  return null;
}
