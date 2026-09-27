import type { Instance } from "../types/instance";
import type { Workspace } from "../types/workspace";
import { projectStatus } from "./status";
import {
  buildSpaces,
  OTHER_SPACE,
  selectedSpace,
  visibleTabs,
  type Space,
  type SpacePrefs,
} from "../features/spaces/store";

/**
 * The single ordered list behind ⌘/Ctrl+1–9 session switching on /sessions.
 *
 * Both the Shell keydown handler (the action) and the SessionList badges (the
 * affordance) resolve digit slots through this one function, so a badge can
 * never promise a session the keypress does not open. The order is exactly
 * the Space tab strip's: Space instances sorted by createdAt then id, with
 * dismissed tabs removed (`visibleTabs`), capped at the nine digit slots.
 */
export const SWITCH_SLOT_COUNT = 9;

export function switchSlots(space: Space | undefined, prefs: SpacePrefs): Instance[] {
  if (!space) return [];
  return visibleTabs(space, prefs).slice(0, SWITCH_SLOT_COUNT);
}

/** 1-based digit slot of an instance in the shared ordering, or 0 if unnumbered. */
export function switchSlotOf(slots: Array<{ id: string }>, instanceId: string): number {
  const index = slots.findIndex((slot) => slot.id === instanceId);
  return index < 0 ? 0 : index + 1;
}

/**
 * The tab strip's own ordered list on /s/*, capped at the nine digit slots
 * (D-053 §10 / ui-spec §1.4, owner decision 4A). Digit chords on a session
 * page number the strip the user is looking at — `workbench.tabs` — never a
 * second ordering, so ⌘n always opens the nth rendered tab.
 */
export function tabSlots(tabs: readonly Instance[]): Instance[] {
  return tabs.slice(0, SWITCH_SLOT_COUNT);
}

/**
 * The container a session page's tab strip represents (D-053 §10):
 *
 * - the current instance is bound to a Task → every hub-snapshot instance
 *   with the same taskId, filtered by each owning Space's dismissal record;
 * - otherwise → the current Space's `visibleTabs`.
 *
 * Pure over the snapshot the Hub already loaded: no requests, no polling.
 * `ownerOf` is the Space a tab's close is recorded against — always the
 * session's own Space, so both container kinds share one dismissal record.
 */
export type TabSet = {
  kind: "task" | "space";
  taskId?: string;
  /** The fallback Space container; absent only when no Space exists at all. */
  space?: Space;
  tabs: Instance[];
  /** tablist accessible name: 「本任务的会话」 or 「空间 {名称} 的会话」. */
  ariaLabel: string;
  ownerOf: (instance: Instance) => string;
};

/** Mirrors visibleTabs' rule for a session that may live in another Space. */
function tabIsVisible(instance: Instance, ownerSpaceId: string, prefs: SpacePrefs): boolean {
  const entry = (prefs.closedTabs[ownerSpaceId] ?? []).find((row) => row.id === instance.id);
  return !entry || (entry.resurface && projectStatus(instance) === "blocked");
}

export function resolveTabSet(
  workspaces: Workspace[],
  instances: Instance[],
  prefs: SpacePrefs,
  instanceId?: string,
): TabSet {
  // buildSpaces dedupes instance ids, applies the 其他 fallback and gives
  // every instance exactly one owning Space — the dismissal bucket key.
  const spaces = buildSpaces(workspaces, instances, prefs);
  const ownerById = new Map<string, string>();
  for (const space of spaces) {
    for (const instance of space.instances) ownerById.set(instance.id, space.id);
  }
  const byId = new Map<string, Instance>();
  for (const space of spaces) {
    for (const instance of space.instances) byId.set(instance.id, instance);
  }
  const ownerOf = (instance: Instance): string => ownerById.get(instance.id) ?? OTHER_SPACE;

  const space = selectedSpace(spaces, prefs, instanceId);
  const current = instanceId ? byId.get(instanceId) : undefined;
  const taskId = current?.taskId || undefined;

  if (taskId) {
    const tabs = [...byId.values()]
      .filter((instance) => instance.taskId === taskId && tabIsVisible(instance, ownerOf(instance), prefs))
      .sort((a, b) => a.createdAt.localeCompare(b.createdAt) || a.id.localeCompare(b.id));
    return { kind: "task", taskId, space, tabs, ariaLabel: "本任务的会话", ownerOf };
  }

  return {
    kind: "space",
    space,
    tabs: space ? visibleTabs(space, prefs) : [],
    ariaLabel: `空间 ${space?.name ?? ""} 的会话`,
    ownerOf,
  };
}
