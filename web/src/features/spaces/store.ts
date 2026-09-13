import { useSyncExternalStore } from "react";
import { projectStatus } from "../../lib/status";
import type { Instance } from "../../types/instance";
import type { Workspace } from "../../types/workspace";

export const SPACES_PREFS_KEY = "remuda.spaces.v1";
export const OTHER_SPACE = "other";

export type Space = {
  id: string;
  hostId?: string;
  workspaceId?: string;
  rootPath?: string;
  name: string;
  instances: Instance[];
  liveCount: number;
  blockedCount: number;
};

/**
 * A tab the device dismissed (D-024 addendum). `resurface` re-opens the tab the
 * next time the session becomes blocked. Dismissing a tab that is blocked right
 * now clears the flag, so the dismissal is not undone by the same episode;
 * `rearmDismissed` sets it again once that episode ends.
 */
export type DismissedTab = { id: string; resurface: boolean };

export type SpacePrefs = {
  version: 1;
  collapsed: boolean;
  groupCollapsed: Record<string, boolean>;
  exitedOpen: Record<string, boolean>;
  names: Record<string, string>;
  order: string[];
  selectedSpaceId?: string;
  selectedTabs: Record<string, string>;
  closedTabs: Record<string, DismissedTab[]>;
};

export function defaultSpacePrefs(): SpacePrefs {
  return { version: 1, collapsed: false, groupCollapsed: {}, exitedOpen: {}, names: {}, order: [],
    selectedTabs: {}, closedTabs: {} };
}

export function spaceKey(hostId: string, workspaceId: string): string {
  return JSON.stringify([hostId, workspaceId]);
}

function basename(root: string): string {
  return root.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || root || "Workspace";
}

export function buildSpaces(workspaces: Workspace[], instances: Instance[], prefs: SpacePrefs): Space[] {
  const spaces = new Map<string, Space>();
  for (const workspace of workspaces) {
    const id = spaceKey(workspace.hostId, workspace.id);
    if (!spaces.has(id)) spaces.set(id, {
      id, hostId: workspace.hostId, workspaceId: workspace.id, rootPath: workspace.rootPath,
      name: prefs.names[id] || basename(workspace.rootPath), instances: [], liveCount: 0, blockedCount: 0,
    });
  }
  const seen = new Set<string>();
  for (const instance of instances) {
    if (seen.has(instance.id)) continue;
    seen.add(instance.id);
    const key = spaceKey(instance.hostId, instance.workspaceId);
    if (!spaces.has(key) && !spaces.has(OTHER_SPACE)) spaces.set(OTHER_SPACE, {
      id: OTHER_SPACE, name: "其他", instances: [], liveCount: 0, blockedCount: 0,
    });
    const space = spaces.get(key) ?? spaces.get(OTHER_SPACE)!;
    space.instances.push(instance);
    const status = projectStatus(instance);
    if (status === "blocked") space.blockedCount += 1;
    if (status === "blocked" || status === "working" || status === "starting" || status === "idle") space.liveCount += 1;
  }
  for (const space of spaces.values()) {
    space.instances.sort((a, b) => a.createdAt.localeCompare(b.createdAt) || a.id.localeCompare(b.id));
  }
  const order = new Map(prefs.order.map((id, index) => [id, index]));
  return [...spaces.values()].sort((a, b) => {
    if (a.id === OTHER_SPACE) return 1;
    if (b.id === OTHER_SPACE) return -1;
    return (order.get(a.id) ?? Infinity) - (order.get(b.id) ?? Infinity)
      || a.name.localeCompare(b.name) || a.id.localeCompare(b.id);
  });
}

function dismissedIn(space: Space, prefs: SpacePrefs): Map<string, DismissedTab> {
  return new Map((prefs.closedTabs[space.id] ?? []).map((entry) => [entry.id, entry]));
}

/** A dismissed session keeps running; it returns to the strip once it needs a human. */
export function visibleTabs(space: Space, prefs: SpacePrefs): Instance[] {
  const dismissed = dismissedIn(space, prefs);
  return space.instances.filter((instance) => {
    const entry = dismissed.get(instance.id);
    return !entry || (entry.resurface && projectStatus(instance) === "blocked");
  });
}

/** The sidebar lists live sessions inline and collects exited ones into a group. */
export function spaceSessions(space: Space): { live: Instance[]; exited: Instance[] } {
  return {
    live: space.instances.filter((instance) => projectStatus(instance) !== "exited"),
    exited: space.instances.filter((instance) => projectStatus(instance) === "exited"),
  };
}

export function selectedTab(space: Space, prefs: SpacePrefs): Instance | undefined {
  const tabs = visibleTabs(space, prefs);
  return tabs.find((instance) => instance.id === prefs.selectedTabs[space.id]) ?? tabs[0];
}

/** A deep link takes precedence over remembered navigation, including a previously closed tab. */
export function selectedSpace(spaces: Space[], prefs: SpacePrefs, instanceId?: string): Space | undefined {
  return (instanceId ? spaces.find((space) => space.instances.some((instance) => instance.id === instanceId)) : undefined)
    ?? spaces.find((space) => space.id === prefs.selectedSpaceId) ?? spaces[0];
}

export function newSessionPath(space?: Space): string {
  if (!space?.hostId || !space.workspaceId) return "/sessions/new";
  const params = new URLSearchParams({ host: space.hostId, workspace: space.workspaceId, cwd: space.rootPath ?? "" });
  return `/sessions/new?${params}`;
}

function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : {};
}

function strings(value: unknown): string[] {
  return Array.isArray(value) ? [...new Set(value.filter((item): item is string => typeof item === "string" && !!item))] : [];
}

function stringMap(value: unknown): Record<string, string> {
  return Object.fromEntries(Object.entries(record(value)).filter((entry): entry is [string, string] =>
    typeof entry[1] === "string" && !!entry[1].trim(),
  ).map(([key, text]) => [key, text.trim()]));
}

function booleanMap(value: unknown): Record<string, boolean> {
  return Object.fromEntries(Object.entries(record(value)).filter((entry): entry is [string, boolean] => typeof entry[1] === "boolean"));
}

/** Reads both the original id list and the dismissal records that replaced it. */
function dismissedTabs(value: unknown): DismissedTab[] {
  if (!Array.isArray(value)) return [];
  const byId = new Map<string, DismissedTab>();
  for (const item of value) {
    if (typeof item === "string" && item) byId.set(item, { id: item, resurface: true });
    else {
      const row = record(item);
      if (typeof row.id === "string" && row.id) byId.set(row.id, { id: row.id, resurface: row.resurface !== false });
    }
  }
  return [...byId.values()];
}

export function parseSpacePrefs(raw: string | null): SpacePrefs {
  try {
    const value = record(JSON.parse(raw ?? "null"));
    if (value.version !== 1) return defaultSpacePrefs();
    return {
      version: 1,
      collapsed: value.collapsed === true,
      groupCollapsed: booleanMap(value.groupCollapsed),
      exitedOpen: booleanMap(value.exitedOpen),
      names: stringMap(value.names),
      order: strings(value.order),
      selectedSpaceId: typeof value.selectedSpaceId === "string" ? value.selectedSpaceId : undefined,
      selectedTabs: stringMap(value.selectedTabs),
      closedTabs: Object.fromEntries(Object.entries(record(value.closedTabs)).map(([key, item]) => [key, dismissedTabs(item)])),
    };
  } catch {
    return defaultSpacePrefs();
  }
}

type StoragePort = Pick<Storage, "getItem" | "setItem">;

function browserStorage(): StoragePort | undefined {
  try { return typeof localStorage === "undefined" ? undefined : localStorage; } catch { return undefined; }
}

export function createSpaceStore(storage = browserStorage()) {
  function read(): SpacePrefs {
    try { return parseSpacePrefs(storage?.getItem(SPACES_PREFS_KEY) ?? null); } catch { return defaultSpacePrefs(); }
  }
  let prefs = read();
  const listeners = new Set<() => void>();
  function emit() { for (const listener of listeners) listener(); }
  function update(patch: Partial<SpacePrefs>) {
    const next = { ...prefs, ...patch };
    if (JSON.stringify(next) === JSON.stringify(prefs)) return;
    prefs = next;
    try { storage?.setItem(SPACES_PREFS_KEY, JSON.stringify(prefs)); } catch { /* Keep device navigation usable without storage. */ }
    emit();
  }
  return {
    getSnapshot: () => prefs,
    subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener); }; },
    reload() { prefs = read(); emit(); },
    setCollapsed(collapsed: boolean) { update({ collapsed }); },
    rename(spaceId: string, name: string) {
      const names = { ...prefs.names };
      if (name.trim()) names[spaceId] = name.trim();
      else delete names[spaceId];
      update({ names });
    },
    setOrder(order: string[]) { update({ order: strings(order).filter((id) => id !== OTHER_SPACE) }); },
    moveSpace(spaceId: string, direction: -1 | 1, orderedIds: string[]) {
      const order = strings(orderedIds).filter((id) => id !== OTHER_SPACE);
      const index = order.indexOf(spaceId);
      const target = index + direction;
      if (index < 0 || target < 0 || target >= order.length) return;
      [order[index], order[target]] = [order[target], order[index]];
      update({ order });
    },
    toggleGroup(spaceId: string) { update({ groupCollapsed: { ...prefs.groupCollapsed, [spaceId]: !prefs.groupCollapsed[spaceId] } }); },
    toggleExited(spaceId: string) { update({ exitedOpen: { ...prefs.exitedOpen, [spaceId]: !prefs.exitedOpen[spaceId] } }); },
    selectSpace(selectedSpaceId: string) { update({ selectedSpaceId }); },
    /** Opening a session from the sidebar or a deep link always restores its tab. */
    selectTab(spaceId: string, instanceId: string) {
      update({ selectedSpaceId: spaceId, selectedTabs: { ...prefs.selectedTabs, [spaceId]: instanceId },
        closedTabs: { ...prefs.closedTabs, [spaceId]: (prefs.closedTabs[spaceId] ?? []).filter((entry) => entry.id !== instanceId) } });
    },
    /**
     * Hides the tab only. The caller stops the session first when the user
     * chose to; a dismissal on its own never ends a run.
     */
    closeTab(spaceId: string, instanceId: string, resurface = true) {
      const selectedTabs = { ...prefs.selectedTabs };
      if (selectedTabs[spaceId] === instanceId) delete selectedTabs[spaceId];
      update({ selectedTabs, closedTabs: { ...prefs.closedTabs,
        [spaceId]: [...(prefs.closedTabs[spaceId] ?? []).filter((entry) => entry.id !== instanceId), { id: instanceId, resurface }] } });
    },
    /** Ends a suppressed blocked episode so the next one can re-open the tab. */
    rearmDismissed(blockedIds: string[]) {
      const blocked = new Set(blockedIds);
      const closedTabs = Object.fromEntries(Object.entries(prefs.closedTabs).map(([spaceId, entries]) =>
        [spaceId, entries.map((entry) => entry.resurface || blocked.has(entry.id) ? entry : { ...entry, resurface: true })]));
      update({ closedTabs });
    },
  };
}

export const spaceStore = createSpaceStore();
if (typeof window !== "undefined") window.addEventListener("storage", (event) => {
  if (event.key === SPACES_PREFS_KEY || event.key === null) spaceStore.reload();
});

export function useSpacesPrefs(): SpacePrefs {
  return useSyncExternalStore(spaceStore.subscribe, spaceStore.getSnapshot, spaceStore.getSnapshot);
}
