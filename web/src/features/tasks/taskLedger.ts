import { useCallback, useSyncExternalStore } from "react";
import { rest } from "../../lib/api";
import type { components } from "../../lib/api.generated";
import type { Task } from "../../types/generated";

/**
 * The shared task-ledger poller (home.md §5.1). One scope — an optional
 * project id — owns ONE 5 s /v1/tasks poll and one 30 s /v1/projects poll,
 * however many components subscribe: the phone ① count and the phone task
 * layer read the same ledger, and used to poll it twice.
 *
 * The ledger lives outside React so a subscriber's render never restarts the
 * timers; the last unsubscribe stops them and forgets the scope.
 */

type TaskPage = components["schemas"]["TaskPage"];
type ProjectPage = components["schemas"]["ProjectPage"];

export type TaskLedger = {
  tasks: Task[];
  projectName: (projectId: string) => string | undefined;
};

export const TASK_POLL_MS = 5_000;
export const PROJECT_POLL_MS = 30_000;

type Entry = {
  projectId: string | null;
  tasks: Task[];
  names: Map<string, string>;
  snapshot: TaskLedger;
  listeners: Set<() => void>;
  timers: ReturnType<typeof setInterval>[] | null;
};

const entries = new Map<string, Entry>();
const EMPTY: TaskLedger = { tasks: [], projectName: () => undefined };

function makeSnapshot(tasks: Task[], names: Map<string, string>): TaskLedger {
  return { tasks, projectName: (id) => names.get(id) };
}

/** Test seam: the scopes that currently have a live poller. */
export function taskLedgerScopesForTest(): string[] {
  return [...entries.entries()].filter(([, entry]) => entry.timers !== null).map(([key]) => key);
}

/** Test seam: forget every cached scope (module state outlives a test file). */
export function resetTaskLedgerForTest(): void {
  for (const key of entries.keys()) stopPolling(key);
  entries.clear();
}

function entryFor(key: string, projectId: string | null): Entry {
  let entry = entries.get(key);
  if (!entry) {
    entry = {
      projectId,
      tasks: [],
      names: new Map(),
      // The module-level EMPTY: a fresh object here would make the mount-time
      // subscribe look like a changed snapshot and re-render in a loop.
      snapshot: EMPTY,
      listeners: new Set(),
      timers: null,
    };
    entries.set(key, entry);
  }
  return entry;
}

/** A quiet poll must not replace the snapshot: the count row and the task
 *  layer would re-render every 5 s for identical data. */
function sameTasks(current: Task[], next: Task[]): boolean {
  if (current.length !== next.length) return false;
  for (let index = 0; index < next.length; index += 1) {
    const a = current[index];
    const b = next[index];
    if (a.id !== b.id || a.updatedAt !== b.updatedAt || a.state !== b.state
      || a.revision !== b.revision || (a.blockedReason ?? null) !== (b.blockedReason ?? null)) {
      return false;
    }
  }
  return true;
}

function publishTasks(entry: Entry, tasks: Task[]) {
  if (sameTasks(entry.tasks, tasks)) return;
  entry.tasks = tasks;
  entry.snapshot = makeSnapshot(entry.tasks, entry.names);
  for (const listener of entry.listeners) listener();
}

function publishNames(entry: Entry, names: Map<string, string>) {
  if (entry.names.size === names.size) {
    let same = true;
    for (const [id, name] of names) {
      if (entry.names.get(id) !== name) { same = false; break; }
    }
    if (same) return;
  }
  entry.names = names;
  entry.snapshot = makeSnapshot(entry.tasks, entry.names);
  for (const listener of entry.listeners) listener();
}

function startPolling(key: string) {
  const entry = entries.get(key);
  if (!entry || entry.timers) return;
  const query = entry.projectId ? `?project=${encodeURIComponent(entry.projectId)}` : "";
  const taskTick = async () => {
    try {
      const page = await rest<TaskPage>(`/v1/tasks${query}`);
      // The scope may have been released while this read was in flight.
      if (entries.get(key) !== entry) return;
      publishTasks(entry, (page.items ?? []) as Task[]);
    } catch {
      /* Keep the last good ledger slice; AuthGate handles 401. */
    }
  };
  const projectTick = async () => {
    try {
      const page = await rest<ProjectPage>("/v1/projects");
      if (entries.get(key) !== entry) return;
      publishNames(
        entry,
        new Map(
          (page.items ?? [])
            .filter((project) => project.id && project.name)
            .map((project) => [project.id, project.name]),
        ),
      );
    } catch {
      /* The rail stays usable with raw project ids while projects fail. */
    }
  };
  void taskTick();
  void projectTick();
  entry.timers = [
    setInterval(() => void taskTick(), TASK_POLL_MS),
    setInterval(() => void projectTick(), PROJECT_POLL_MS),
  ];
}

function stopPolling(key: string) {
  const entry = entries.get(key);
  if (!entry?.timers) return;
  for (const timer of entry.timers) clearInterval(timer);
  entry.timers = null;
}

/**
 * Subscribe outside React (the OS badge sync, lib/push.ts): the same shared
 * poller per scope as {@link useTaskLedger}. Returns the unsubscribe.
 */
export function subscribeTaskLedger(projectId: string | null, listener: () => void): () => void {
  const key = projectId ?? "";
  const entry = entryFor(key, projectId);
  entry.listeners.add(listener);
  startPolling(key);
  return () => {
    const current = entries.get(key);
    if (!current) return;
    current.listeners.delete(listener);
    if (current.listeners.size === 0) stopPolling(key);
  };
}

/** The current snapshot of a scope (empty until its first poll lands). */
export function taskLedgerSnapshot(projectId: string | null): TaskLedger {
  return entries.get(projectId ?? "")?.snapshot ?? EMPTY;
}

/**
 * Poll the task ledger (and the project list for names). Read-only. Every
 * caller with the same `projectId` shares one poller; the returned snapshot is
 * replaced only when the data changes, so quiet polls do not re-render.
 */
export function useTaskLedger(projectId?: string | null): TaskLedger {
  // Stable per scope: an identity that changed on every render would make
  // React unsubscribe/resubscribe each time, restarting the poller.
  // Keep the scope's last snapshot cached (identity-stable for a remount);
  // the last unsubscribe only stops the timers.
  const subscribe = useCallback(
    (listener: () => void) => subscribeTaskLedger(projectId ?? null, listener),
    [projectId],
  );
  return useSyncExternalStore(
    subscribe,
    () => taskLedgerSnapshot(projectId ?? null),
    () => EMPTY,
  );
}
