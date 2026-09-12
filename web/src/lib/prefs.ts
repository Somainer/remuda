const KEY = "runtime.new-session";

export type NewSessionPrefs = {
  hostId: string;
  workspaceId: string;
  model: string;
  permissionMode: string;
  driver: string;
  delegation: string;
  recentHostIds: string[];
  recentWorkspaceIds: string[];
};

const empty: NewSessionPrefs = {
  hostId: "",
  workspaceId: "",
  model: "passthrough/auto",
  permissionMode: "manual",
  driver: "claude-print",
  delegation: "none",
  recentHostIds: [],
  recentWorkspaceIds: [],
};

export function readNewSessionPrefs(): NewSessionPrefs {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return empty;
    const parsed = JSON.parse(raw) as Partial<NewSessionPrefs>;
    return { ...empty, ...parsed, recentHostIds: parsed.recentHostIds ?? [], recentWorkspaceIds: parsed.recentWorkspaceIds ?? [] };
  } catch {
    return empty;
  }
}

function touch(list: string[], value: string): string[] {
  return [value, ...list.filter((id) => id !== value)].slice(0, 5);
}

export function rememberNewSessionSuccess(
  patch: Pick<NewSessionPrefs, "hostId" | "workspaceId" | "model" | "permissionMode" | "driver" | "delegation">,
): void {
  const prev = readNewSessionPrefs();
  const next: NewSessionPrefs = {
    ...prev,
    ...patch,
    recentHostIds: touch(prev.recentHostIds, patch.hostId),
    recentWorkspaceIds: touch(prev.recentWorkspaceIds, patch.workspaceId),
  };
  try {
    localStorage.setItem(KEY, JSON.stringify(next));
  } catch {
    /* ignore quota */
  }
}

export function sortRecent<T extends { id: string }>(items: T[], recentIds: string[]): T[] {
  const rank = new Map(recentIds.map((id, i) => [id, i]));
  return items.slice().sort((a, b) => {
    const ra = rank.has(a.id) ? rank.get(a.id)! : 99;
    const rb = rank.has(b.id) ? rank.get(b.id)! : 99;
    return ra - rb;
  });
}
