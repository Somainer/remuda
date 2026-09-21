import { useEffect, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { rest } from "../../lib/api";
import type { components } from "../../lib/api.generated";
import { useHub } from "../../lib/store";
import { buildSpaces, useSpacesPrefs } from "../spaces/store";
import { fetchChanges } from "../files/filesApi";
import type { Task } from "../../types/generated";
import { TaskDetailPanel } from "./TaskDetailPanel";
import {
  buildTaskGroups,
  taskNextStep,
  type TaskListGroup,
  type TaskRow,
} from "./taskRows";
import css from "./tasklist.module.css";

/**
 * Task list mounted at `/board` (D-050, ui-spec §2.9; plan task-model 5).
 * The rail is the operator's work list — 需要你 first, project + git branch
 * groups (header blocked count is buildSpaces() blockedCount), children
 * nested under parents, SE-nn derived keys, 已归档 folded away — and the
 * detail panel renders mandate/title/blockedReason as the body a later task
 * anchors annotations to. Ledger data comes from GET /v1/tasks; sessions and
 * interactions from the same hub store every other surface polls.
 */

type TaskPage = components["schemas"]["TaskPage"];
type ProjectPage = components["schemas"]["ProjectPage"];

export type TaskLedger = {
  tasks: Task[];
  projectName: (projectId: string) => string | undefined;
};

/** Polls the task ledger (and the project list for names). Read-only. */
export function useTaskLedger(projectId?: string | null): TaskLedger {
  const [tasks, setTasks] = useState<Task[]>([]);
  const [names, setNames] = useState<Map<string, string>>(new Map());

  useEffect(() => {
    let cancelled = false;
    const projectTick = async () => {
      try {
        const page = await rest<ProjectPage>("/v1/projects");
        if (cancelled) return;
        setNames(
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
    const taskTick = async () => {
      try {
        const query = projectId ? `?project=${encodeURIComponent(projectId)}` : "";
        const page = await rest<TaskPage>(`/v1/tasks${query}`);
        if (!cancelled) setTasks((page.items ?? []) as Task[]);
      } catch {
        /* Keep the last good ledger slice; AuthGate handles 401. */
      }
    };
    void projectTick();
    void taskTick();
    const projectTimer = window.setInterval(projectTick, 30_000);
    const taskTimer = window.setInterval(taskTick, 5_000);
    return () => {
      cancelled = true;
      window.clearInterval(projectTimer);
      window.clearInterval(taskTimer);
    };
  }, [projectId]);

  return useMemo(
    () => ({
      tasks,
      projectName: (id: string) => names.get(id),
    }),
    [tasks, names],
  );
}

/**
 * Live git branch per Space, hydrated through the same read-only changes
 * proxy the phone home uses. Keyed by Space id so polling recreating the
 * spaces array never refetches.
 */
export function useLiveBranches(spaces: { id: string; hostId?: string; workspaceId?: string }[]): {
  branchOf: (spaceId: string) => string | null;
} {
  const [branches, setBranches] = useState<Record<string, string>>({});
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const fetched = useRef<Set<string>>(new Set());
  const key = spaces.map((space) => space.id).join(",");
  const targets = useMemo(
    () =>
      spaces
        .filter((space) => space.hostId && space.workspaceId)
        .map((space) => ({ id: space.id, hostId: space.hostId!, workspaceId: space.workspaceId! })),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [key],
  );
  useEffect(() => {
    const fresh = targets.filter((target) => !fetched.current.has(target.id));
    if (fresh.length === 0) return;
    for (const target of fresh) fetched.current.add(target.id);
    void Promise.all(
      fresh.map(async (target) => {
        try {
          const status = await fetchChanges(target.hostId, target.workspaceId);
          const branch = status.branch?.state === "known" ? status.branch.value?.trim() : "";
          return { id: target.id, branch: branch ?? "" };
        } catch {
          return { id: target.id, branch: "" };
        }
      }),
    ).then((results) => {
      if (!mounted.current) return;
      setBranches((current) => {
        const next = { ...current };
        let changed = false;
        for (const result of results) {
          if (result.branch && next[result.id] !== result.branch) {
            next[result.id] = result.branch;
            changed = true;
          }
        }
        return changed ? next : current;
      });
    });
  }, [targets]);
  return useMemo(() => ({ branchOf: (spaceId: string) => branches[spaceId] ?? null }), [branches]);
}

export type TaskGroupsProps = {
  groups: TaskListGroup[];
  /** "desktop" rows select into the detail panel; "phone" rows are links. */
  variant?: "desktop" | "phone";
  selectedId?: string | null;
  onSelect?: (task: Task) => void;
};

export function TaskGroups({
  groups,
  variant = "desktop",
  selectedId = null,
  onSelect,
}: TaskGroupsProps) {
  if (groups.length === 0) {
    return <p className={css.empty} data-testid="task-list-empty">暂无任务</p>;
  }
  return (
    <div
      className={`${css.groups} ${variant === "phone" ? css.groupsPhone : ""}`}
      data-testid="task-groups"
    >
      {groups.map((group) => (
        <section
          key={group.id}
          className={css.group}
          data-testid="task-group"
          data-kind={group.kind}
          data-blocked={group.blockedCount}
        >
          <header
            className={`${css.groupHead} ${group.kind === "attention" ? css.groupAttention : ""}`}
          >
            <span className={css.groupProject}>
              {group.project}
              {group.kind === "attention" || group.kind === "archived" ? ` · ${group.count}` : ""}
            </span>
            {group.branch ? (
              <span className={css.groupBranch}>{group.branch}</span>
            ) : group.kind === "project" && !group.spaceId ? (
              <span className={css.groupBranch}>未绑定目录</span>
            ) : null}
            {group.kind === "project" ? (
              <>
                <span className={css.groupCount}>{group.count}</span>
                <span
                  className={css.groupBlocked}
                  data-zero={group.blockedCount === 0 ? "1" : "0"}
                  data-testid="task-group-blocked"
                >
                  {group.blockedCount} 待处理
                </span>
              </>
            ) : null}
          </header>
          {group.rows.map((row) => (
            <TaskRowView
              key={row.id}
              row={row}
              variant={variant}
              selectedId={selectedId}
              onSelect={onSelect}
            />
          ))}
        </section>
      ))}
    </div>
  );
}

function TaskRowView({
  row,
  variant,
  selectedId,
  onSelect,
}: {
  row: TaskRow;
  variant: "desktop" | "phone";
  selectedId: string | null;
  onSelect?: (task: Task) => void;
}) {
  const step = taskNextStep(row);
  const className = `${css.row} ${
    variant === "desktop" && selectedId === row.id ? css.rowSelected : ""
  }`;
  const style = { ["--depth" as string]: row.depth } as React.CSSProperties;
  // On the phone the whole row opens the shared session (never a second
  // transcript); session-less tasks render as plain rows. Desktop rows are
  // selection buttons and keep the per-session 打开 link inside.
  const linked = variant === "phone" && row.primarySessionId != null;
  const content = (
    <>
      <span className={css.rowMain}>
        <span className={css.rowKey}>{row.displayKey}</span>
        <span
          className={`${css.rowTitle} ${row.blocked ? css.rowFail : ""}`}
          title={row.task.title}
        >
          {row.task.title}
        </span>
        {row.childCount > 0 ? (
          <span className={css.rowChildCount} data-testid="task-child-count">
            ▸ {row.childCount}
          </span>
        ) : null}
      </span>
      <span className={css.rowMeta}>
        <span
          className={`${css.rowStep} ${row.blocked ? css.rowReason : ""}`}
          title={step}
          data-testid="task-row-step"
        >
          {step}
        </span>
        <span className={css.rowSession}>{row.sessionCount} 会话</span>
        {variant === "desktop" && row.primarySessionId ? (
          <Link
            className={css.rowSessionLink}
            to={`/s/${row.primarySessionId}`}
            data-testid="task-session-link"
            onClick={(event) => event.stopPropagation()}
          >
            打开
          </Link>
        ) : null}
      </span>
    </>
  );

  const dataAttrs = {
    "data-testid": "task-row",
    "data-task-id": row.id,
    "data-depth": row.depth,
    "data-needs-human": row.needsHuman ? "1" : "0",
    "data-has-children": row.childCount > 0 ? "1" : "0",
  } as const;

  let wrapper: React.ReactNode;
  if (variant === "desktop" && onSelect) {
    wrapper = (
      <button
        type="button"
        className={className}
        style={style}
        {...dataAttrs}
        onClick={() => onSelect(row.task)}
      >
        {content}
      </button>
    );
  } else if (linked) {
    wrapper = (
      <Link className={`${className} ${css.rowLink}`} style={style} to={`/s/${row.primarySessionId}`} {...dataAttrs}>
        {content}
      </Link>
    );
  } else {
    wrapper = (
      <span className={className} style={style} {...dataAttrs}>
        {content}
      </span>
    );
  }

  return (
    <>
      {wrapper}
      {row.children.map((child) => (
        <TaskRowView
          key={child.id}
          row={child}
          variant={variant}
          selectedId={selectedId}
          onSelect={onSelect}
        />
      ))}
    </>
  );
}

export function TaskListPage() {
  const hub = useHub();
  const prefs = useSpacesPrefs();
  const [params] = useSearchParams();
  const projectId = params.get("project");
  const ledger = useTaskLedger(projectId);
  const [query, setQuery] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const spaces = useMemo(
    () => buildSpaces(hub.workspaces, hub.instances, prefs),
    [hub.workspaces, hub.instances, prefs],
  );
  const { branchOf } = useLiveBranches(spaces);

  const groups = useMemo(
    () =>
      buildTaskGroups({
        tasks: ledger.tasks,
        instances: hub.instances,
        interactions: hub.interactions,
        spaces,
        projectName: ledger.projectName,
        branchOfSpace: branchOf,
        query,
      }),
    [ledger.tasks, ledger.projectName, hub.instances, hub.interactions, spaces, branchOf, query],
  );

  // Keep a selection: the chosen task, the first live row on arrival, or null.
  const flatRows = useMemo(
    () => groups.flatMap((group) => flattenRows(group.rows)),
    [groups],
  );
  useEffect(() => {
    if (flatRows.length === 0) {
      setSelectedId(null);
      return;
    }
    if (!selectedId || !flatRows.some((row) => row.id === selectedId)) {
      setSelectedId(flatRows[0].id);
    }
  }, [flatRows, selectedId]);

  const selectedRow = flatRows.find((row) => row.id === selectedId) ?? null;

  return (
    <div className={css.board} data-testid="task-list">
      <div className={css.rail}>
        <div className={css.toolbar}>
          <input
            className={css.search}
            type="search"
            data-testid="task-search"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="搜索 Task"
            aria-label="搜索任务"
          />
        </div>
        <TaskGroups groups={groups} selectedId={selectedId} onSelect={(task) => setSelectedId(task.id)} />
      </div>
      <TaskDetailPanel
        task={selectedRow?.task ?? null}
        displayKey={selectedRow?.displayKey}
        sessionIds={selectedRow?.sessionIds}
        primarySessionId={selectedRow?.primarySessionId}
      />
    </div>
  );
}

function flattenRows(rows: TaskRow[]): TaskRow[] {
  return rows.flatMap((row) => [row, ...flattenRows(row.children)]);
}
