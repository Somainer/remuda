/**
 * Which model groups an operator has collapsed, per provider profile.
 *
 * A gateway can serve a few hundred ids, so the shape an operator folds the
 * list into is worth keeping across reloads. Purely cosmetic: a device without
 * usable storage still gets a working list, every group open.
 */
const KEY = "runtime.provider-model-groups";

type StoragePort = Pick<Storage, "getItem" | "setItem">;

function browserStorage(): StoragePort | undefined {
  try {
    return typeof localStorage === "undefined" ? undefined : localStorage;
  } catch {
    return undefined;
  }
}

type Stored = Record<string, string[]>;

function readAll(storage: StoragePort | undefined): Stored {
  try {
    const raw = storage?.getItem(KEY);
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    const out: Stored = {};
    for (const [profileId, keys] of Object.entries(parsed as Record<string, unknown>)) {
      if (Array.isArray(keys)) out[profileId] = keys.filter((k): k is string => typeof k === "string");
    }
    return out;
  } catch {
    return {};
  }
}

/** Collapsed group keys for one profile. `new` covers a profile not yet saved. */
export function readCollapsedGroups(
  profileId: string,
  storage = browserStorage(),
): Set<string> {
  return new Set(readAll(storage)[profileId] ?? []);
}

export function writeCollapsedGroups(
  profileId: string,
  keys: Iterable<string>,
  storage = browserStorage(),
): void {
  const all = readAll(storage);
  const list = [...keys];
  // Drop the entry rather than storing `[]`, so the blob does not grow one
  // empty key per profile ever opened.
  if (list.length) all[profileId] = list;
  else delete all[profileId];
  try {
    storage?.setItem(KEY, JSON.stringify(all));
  } catch {
    /* Collapse state is cosmetic; a full quota must not break the list. */
  }
}
