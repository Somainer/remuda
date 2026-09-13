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

export type SpacePrefs = {
  version: 1;
  collapsed: boolean;
  groupCollapsed: Record<string, boolean>;
  names: Record<string, string>;
  order: string[];
  selectedSpaceId?: string;
  selectedTabs: Record<string, string>;
  closedTabs: Record<string, string[]>;
};

export function defaultSpacePrefs(): SpacePrefs {
  return { version: 1, collapsed: false, groupCollapsed: {}, names: {}, order: [], selectedTabs: {}, closedTabs: {} };
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

export function visibleTabs(space: Space, prefs: SpacePrefs): Instance[] {
  const closed = new Set(prefs.closedTabs[space.id] ?? []);
  return space.instances.filter((instance) => !closed.has(instance.id));
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

export function parseSpacePrefs(raw: string | null): SpacePrefs {
  try {
    const value = record(JSON.parse(raw ?? "null"));
    if (value.version !== 1) return defaultSpacePrefs();
    return {
      version: 1,
      collapsed: value.collapsed === true,
      groupCollapsed: Object.fromEntries(Object.entries(record(value.groupCollapsed)).filter((entry): entry is [string, boolean] => typeof entry[1] === "boolean")),
      names: stringMap(value.names),
      order: strings(value.order),
      selectedSpaceId: typeof value.selectedSpaceId === "string" ? value.selectedSpaceId : undefined,
      selectedTabs: stringMap(value.selectedTabs),
      closedTabs: Object.fromEntries(Object.entries(record(value.closedTabs)).map(([key, item]) => [key, strings(item)])),
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
    selectSpace(selectedSpaceId: string) { update({ selectedSpaceId }); },
    selectTab(spaceId: string, instanceId: string) {
      update({ selectedSpaceId: spaceId, selectedTabs: { ...prefs.selectedTabs, [spaceId]: instanceId },
        closedTabs: { ...prefs.closedTabs, [spaceId]: (prefs.closedTabs[spaceId] ?? []).filter((id) => id !== instanceId) } });
    },
    /** The caller closes the remote instance before hiding its tab. */
    closeTab(spaceId: string, instanceId: string) {
      const selectedTabs = { ...prefs.selectedTabs };
      if (selectedTabs[spaceId] === instanceId) delete selectedTabs[spaceId];
      update({ selectedTabs, closedTabs: { ...prefs.closedTabs, [spaceId]: [...new Set([...(prefs.closedTabs[spaceId] ?? []), instanceId])] } });
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
