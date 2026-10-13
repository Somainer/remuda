const KEY = "runtime.new-session";

export type NewSessionPrefs = {
  hostId: string;
  workspaceId: string;
  model: string;
  permissionMode: string;
  driver: string;
  delegation: string;
  effortIndex: number;
  /** Remembered tier; D-056 legacy word `ultracode` migrates to {xhigh,on}. */
  effortName: string;
  /** D-056 orthogonal launch switch remembered with the tier. */
  effortUltracode?: boolean;
  /** Extra CLI args from the last successful create, as typed. */
  launchArgs: string;
  recentHostIds: string[];
  recentWorkspaceIds: string[];
};

const empty: NewSessionPrefs = {
  hostId: "",
  workspaceId: "",
  model: "passthrough/auto",
  permissionMode: "manual",
  // No remembered carrier yet. Left empty rather than naming one, so the sheet
  // falls through to `defaultDriver(host, kind)`, which reads the host's own
  // `driverInventory`. This used to say `claude-print`, which is never a valid
  // default: a print session ends after one turn and needs a manual resume
  // (D-035; docs/design/evidence/dispatch-driver-1.md).
  driver: "",
  delegation: "none",
  // D-056: no remembered tier. A fresh draft is unpinned ("follow the model
  // default"); the per-model marker comes from the Hub capability catalog.
  effortIndex: 2,
  effortName: "",
  effortUltracode: false,
  launchArgs: "",
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
  patch: Pick<
    NewSessionPrefs,
    | "hostId"
    | "workspaceId"
    | "model"
    | "permissionMode"
    | "driver"
    | "delegation"
    | "effortIndex"
    | "effortName"
    | "effortUltracode"
    | "launchArgs"
  >,
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
