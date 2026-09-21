/**
 * Top-bar project switcher (task-model task 8 t-project-switcher; D-050 §9,
 * ui-spec §2.9).
 *
 * `全局 ▾ <project>`: the selected projectId is the filter the task list
 * (t-tasklist) and the board (t-board-ui) read. Global — `null` — means no
 * filter, so every project in scope shows. The selection is device-local
 * (an external store persisted to localStorage, mirroring spaces/store.ts);
 * it is never a wire field. Surfaces that need Hub-side scoping append it
 * as the documented `?project=` query on `GET /v1/tasks` / `GET /v1/board`
 * (both endpoints already accept it), via {@link tasksPath} / {@link boardPath}.
 *
 * D-024: `Project.members[]` pairs are Space keys verbatim. A member maps to
 * a Space only when BOTH hostId and workspaceId match a registered
 * workspace — same-named directories on two hosts stay two rows, and a
 * member whose workspace is not registered renders as the bare pair rather
 * than being merged into anything.
 */
import { useCallback, useEffect, useState, useSyncExternalStore } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { rest } from "../../lib/api";
import type { components } from "../../lib/api.generated";
import { spaceKey, type Space } from "../spaces/store";
import type { Workspace } from "../../types/workspace";
import ui from "../../styles/ui.module.css";

export type Project = components["schemas"]["Project"];
export type ProjectMember = components["schemas"]["ProjectMember"];
type ProjectPage = components["schemas"]["ProjectPage"];

/** Global scope: no project filter, every project in scope shows. */
export type ProjectFilter = string | null;
export const GLOBAL_PROJECT: ProjectFilter = null;
const GLOBAL_VALUE = "";
const FILTER_STORAGE_KEY = "remuda.project-filter.v1";

// ── Pure filtering ────────────────────────────────────────────────────────

export function isGlobalFilter(projectId: ProjectFilter): boolean {
  return projectId === GLOBAL_PROJECT;
}

/**
 * Keep the tasks of one project. Global keeps the whole list; the predicate
 * only ever reads `projectId`, so it works for the task list and for any
 * board card shape (a BoardItem is a Task plus projection fields).
 */
export function filterTasksByProject<T extends { projectId: string }>(
  tasks: readonly T[],
  projectId: ProjectFilter,
): T[] {
  if (isGlobalFilter(projectId)) return [...tasks];
  return tasks.filter((task) => task.projectId === projectId);
}

/**
 * The documented Hub reads for the two task surfaces, scoped when a project
 * is selected and unscoped (every project in caller scope) on global. The
 * Hub is the filter authority for board projection; these builders keep the
 * query spelling in one place instead of each surface re-deriving it.
 */
export function tasksPath(projectId: ProjectFilter): string {
  return isGlobalFilter(projectId) ? "/v1/tasks" : `/v1/tasks?project=${encodeURIComponent(projectId!)}`;
}

export function boardPath(projectId: ProjectFilter): string {
  return isGlobalFilter(projectId) ? "/v1/board" : `/v1/board?project=${encodeURIComponent(projectId!)}`;
}

// ── members → Spaces (D-024, key never relaxed) ───────────────────────────

/** The Space key of a project member is the (hostId, workspaceId) pair. */
export function memberSpaceKey(member: Pick<ProjectMember, "hostId" | "workspaceId">): string {
  return spaceKey(member.hostId, member.workspaceId);
}

export type ProjectMemberRow = {
  member: ProjectMember;
  /** `spaceKey(hostId, workspaceId)` — the same id a Space carries. */
  key: string;
  /** The registered workspace for the exact pair; absent when not registered. */
  workspace?: Workspace;
  /** The Space built for the exact pair; absent for an unregistered member. */
  space?: Space;
};

/**
 * Resolve one member against the registered workspace directory. Both halves
 * of the pair must match: a workspace id that happens to exist on another
 * host does not resolve (same-name / cross-host directories never merge).
 * An unresolved member is still returned (with no Space), so the UI can show
 * the bare pair — it is never folded into the "other" Space bucket.
 */
export function resolveProjectMember(
  member: ProjectMember,
  workspaces: readonly Workspace[],
): ProjectMemberRow {
  const key = memberSpaceKey(member);
  const workspace = workspaces.find(
    (w) => w.hostId === member.hostId && w.id === member.workspaceId,
  );
  const row: ProjectMemberRow = { member, key };
  if (workspace) {
    row.workspace = workspace;
    row.space = {
      id: key,
      hostId: member.hostId,
      workspaceId: member.workspaceId,
      rootPath: workspace.rootPath,
      name: workspace.label,
      instances: [],
      liveCount: 0,
      blockedCount: 0,
    };
  }
  return row;
}

/** Members in wire order, each mapped through the exact-pair Space key. */
export function projectMemberRows(
  project: Pick<Project, "members">,
  workspaces: readonly Workspace[],
): ProjectMemberRow[] {
  return (project.members ?? []).map((member) => resolveProjectMember(member, workspaces));
}

/**
 * Distinct host ids the members span. Members on two hosts surface as two
 * hosts — the cross-host project is the point of this entity, never a merge
 * (D-024).
 */
export function projectMemberHostIds(project: Pick<Project, "members">): string[] {
  return [...new Set((project.members ?? []).map((member) => member.hostId))];
}

/**
 * Display name for a project reference, falling back to the raw id. The
 * fallback matters for references seeded independently of the projects list
 * (a bot channel's `defaultProject`): an out-of-scope or otherwise
 * unresolvable reference stays the same valid string, it is never blanked
 * or rewritten.
 */
export function projectLabel(
  projectId: string,
  projects: readonly Pick<Project, "id" | "name">[],
): string {
  return projects.find((project) => project.id === projectId)?.name ?? projectId;
}

// ── Selection store (device-local) ────────────────────────────────────────

type Listener = () => void;
type StoragePort = Pick<Storage, "getItem" | "setItem" | "removeItem">;

function browserStorage(): StoragePort | undefined {
  try {
    return typeof localStorage === "undefined" ? undefined : localStorage;
  } catch {
    return undefined;
  }
}

function readStored(storage: StoragePort | undefined): ProjectFilter {
  try {
    const raw = storage?.getItem(FILTER_STORAGE_KEY);
    // Only a non-empty id is a selection; anything else is global.
    return typeof raw === "string" && raw.trim() ? raw.trim() : GLOBAL_PROJECT;
  } catch {
    return GLOBAL_PROJECT;
  }
}

export type ProjectFilterStore = {
  getSnapshot: () => ProjectFilter;
  subscribe: (listener: Listener) => () => void;
  select: (projectId: string) => void;
  clear: () => void;
  /**
   * Drop the selection when it points at a project the directory no longer
   * contains (deleted, or out of the caller's scope). Keeps the store and
   * the control consistent: after reconcile every consumer reads the same
   * global scope the control shows, instead of filtering the task list and
   * board to an invisible id. Global is always left untouched.
   */
  reconcile: (projectIds: Iterable<string>) => void;
};

export function createProjectFilterStore(storage = browserStorage()): ProjectFilterStore {
  let selected: ProjectFilter = readStored(storage);
  const listeners = new Set<Listener>();
  function emit() {
    for (const listener of listeners) listener();
  }
  function persist(next: ProjectFilter) {
    try {
      if (next === null) storage?.removeItem(FILTER_STORAGE_KEY);
      else storage?.setItem(FILTER_STORAGE_KEY, next);
    } catch {
      // Filtering stays usable without storage.
    }
  }
  function update(next: ProjectFilter) {
    if (next === selected) return;
    selected = next;
    persist(next);
    emit();
  }
  return {
    getSnapshot: () => selected,
    subscribe(listener) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    select(projectId) {
      const id = projectId.trim();
      if (id) update(id);
    },
    clear() {
      update(GLOBAL_PROJECT);
    },
    reconcile(projectIds) {
      if (selected === GLOBAL_PROJECT) return;
      for (const id of projectIds) {
        if (id === selected) return;
      }
      update(GLOBAL_PROJECT);
    },
  };
}

export const projectFilterStore = createProjectFilterStore();

export function useProjectFilter(): ProjectFilter {
  return useSyncExternalStore(
    projectFilterStore.subscribe,
    projectFilterStore.getSnapshot,
    projectFilterStore.getSnapshot,
  );
}

// ── Project directory read (generated client) ─────────────────────────────

export type ProjectsState = {
  projects: Project[];
  loading: boolean;
  error: string | null;
  reload: () => void;
};

/**
 * `GET /v1/projects` through the generated client's path typing; the Hub
 * returns the ProjectPage envelope (`{items, nextCursor}`), scoped to the
 * caller's delegation scope.
 */
export async function listProjects(): Promise<Project[]> {
  const page = await rest<ProjectPage>("/v1/projects");
  return page.items ?? [];
}

export function useProjects(): ProjectsState {
  const [projects, setProjects] = useState<Project[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const reload = useCallback(() => {
    setLoading(true);
    listProjects()
      .then((items) => {
        setProjects(items);
        setError(null);
        // Normalize against the freshly loaded directory: a stored selection
        // for a vanished/out-of-scope project clears here, so the control and
        // every filter consumer agree on global.
        projectFilterStore.reconcile(items.map((item) => item.id));
      })
      .catch((err: unknown) => setError(err instanceof Error ? err.message : "项目加载失败"))
      .finally(() => setLoading(false));
  }, []);
  useEffect(() => {
    reload();
  }, [reload]);
  return { projects, loading, error, reload };
}

// ── Component ─────────────────────────────────────────────────────────────

export type ProjectSwitcherProps = {
  projects: readonly Project[];
  /**
   * When rendered on the projects surface, selecting an entry navigates to
   * the scoped project page (and global back to the list). Other surfaces
   * just read {@link useProjectFilter}.
   */
  navigateOnSelect?: boolean;
};

/**
 * `全局 ▾ <project>` dropdown. Global (the empty option) clears the filter;
 * a project id filters the task list and the board. The directory load
 * reconciles a stale stored id back to global, so the value shown here and
 * the value consumers read only ever diverge on the first render before the
 * directory arrives; the empty fallback covers that transient.
 */
export function ProjectSwitcher({ projects, navigateOnSelect = false }: ProjectSwitcherProps) {
  const selected = useProjectFilter();
  const navigate = useNavigate();
  const location = useLocation();
  const value = selected && projects.some((project) => project.id === selected) ? selected : GLOBAL_VALUE;

  function onChange(next: string) {
    if (next === GLOBAL_VALUE) projectFilterStore.clear();
    else projectFilterStore.select(next);
    if (navigateOnSelect && location.pathname.startsWith("/projects")) {
      navigate(next === GLOBAL_VALUE ? "/projects" : `/projects/${encodeURIComponent(next)}`);
    }
  }

  return (
    <label className={ui.field} style={{ margin: 0 }}>
      <span className={ui.listMeta}>项目范围</span>
      <select
        className={`${ui.select} ${ui.touchSelect}`}
        style={{ width: "auto", minWidth: 160 }}
        data-testid="project-switcher"
        aria-label="按项目过滤任务列表与看板"
        value={value}
        onChange={(event) => onChange(event.target.value)}
      >
        <option value={GLOBAL_VALUE}>全局</option>
        {projects.map((project) => (
          <option key={project.id} value={project.id}>
            {project.name}
          </option>
        ))}
      </select>
    </label>
  );
}
