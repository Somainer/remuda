import { useCallback, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { rest } from "../../lib/api";
import type { components } from "../../lib/api.generated";
import { FilesView } from "../files/FilesView";
import {
  taskScopeFrom,
  taskServesAxis,
  type TaskPlacementRow,
  type TaskSpaceScope,
} from "./taskSpaceFilter";

/**
 * 任务空间 vs 项目空间 file panel (plan task-model task 7 t-taskspace,
 * D-050 §7). It reuses the existing 「工作区当前变更」 view with no new
 * Node/Hub endpoint: task metadata comes from the task ledger that already
 * exists (`GET /v1/tasks`, `GET /v1/tasks/{id}/placements`) and file rows
 * from the existing `workspace.scm.status/diff/file` proxy. The panel only
 * decides which task the open session belongs to, then hands the task's
 * owns[] to {@link FilesView}'s task filter.
 *
 * Task 5 (task list / detail surface) will mount this with an explicit
 * `taskId`. Until that surface lands, an instance id resolves the owning
 * task through its placement ledger rows — the wire-exposed session set
 * (`instances.task_id`). A session with no owning task renders the plain
 * project-space file view unchanged.
 */
export interface TaskSpacePanelProps {
  hostId: string;
  workspaceId: string;
  /** The open session; resolves the owning task when `taskId` is omitted. */
  instanceId?: string;
  /** Explicit task, supplied by the future task-list/detail surface. */
  taskId?: string;
  /** Human host label, forwarded verbatim to the file view subtitle. */
  hostLabel?: string;
  onBack: () => void;
}

type TaskDoc = components["schemas"]["Task"];
type TaskPage = { items: TaskDoc[] };
type PlacementPage = { items: TaskPlacementRow[] };

type PanelState =
  | { phase: "loading" }
  | { phase: "error"; message: string }
  /** `null` task: no ledger task owns this session, so no task tab is shown. */
  | { phase: "ready"; task: TaskDoc | null; scope: TaskSpaceScope | null };

type PanelAxis = { hostId: string; workspaceId: string };

/** Fetch one task plus its placement-ledger session set, on-axis only. */
async function loadTaskScope(
  taskId: string,
  axis: PanelAxis,
): Promise<{ task: TaskDoc; scope: TaskSpaceScope } | null> {
  const [task, placements] = await Promise.all([
    rest<TaskDoc>(`/v1/tasks/${encodeURIComponent(taskId)}`),
    rest<PlacementPage>(`/v1/tasks/${encodeURIComponent(taskId)}/placements`),
  ]);
  // A task bound to a different host/workspace must not label this panel.
  if (!taskServesAxis(task, axis)) return null;
  return { task, scope: taskScopeFrom(task, placements.items) };
}

/**
 * Discover the task whose session set contains `instanceId`. The task
 * document's current placement answers the common case with one list call;
 * only when nothing matches do we scan placement ledgers in parallel.
 *
 * The instance match alone is not sufficient: a rebound task can still hold
 * a placement for this session while its binding names another workspace.
 * Such a match is dropped — the panel renders the plain project-space view
 * exactly as it does with no owning task.
 */
async function resolveTaskByInstance(
  instanceId: string,
  axis: PanelAxis,
): Promise<{
  task: TaskDoc;
  scope: TaskSpaceScope;
} | null> {
  const page = await rest<TaskPage>("/v1/tasks");
  const tasks = page.items ?? [];

  const current = tasks.find((item) => item.placement?.instanceId === instanceId);
  if (current && taskServesAxis(current, axis)) {
    const resolved = await loadTaskScope(current.id, axis);
    if (resolved) return resolved;
  }

  const ledgers = await Promise.all(
    tasks.map(async (task) => {
      try {
        const placements = await rest<PlacementPage>(
          `/v1/tasks/${encodeURIComponent(task.id)}/placements`,
        );
        return { task, rows: placements.items ?? [] };
      } catch {
        // A scope/visibility miss on one task must not hide another match.
        return { task, rows: [] };
      }
    }),
  );
  const match = ledgers.find(
    ({ task, rows }) =>
      taskServesAxis(task, axis) && rows.some((row) => row.instanceId === instanceId),
  );
  if (!match) return null;
  return {
    task: match.task,
    scope: taskScopeFrom(match.task, match.rows),
  };
}

async function resolveTask(
  taskId: string | undefined,
  instanceId: string | undefined,
  axis: PanelAxis,
): Promise<{ task: TaskDoc | null; scope: TaskSpaceScope | null }> {
  if (taskId) {
    const found = await loadTaskScope(taskId, axis);
    if (found) return found;
    return { task: null, scope: null };
  }
  if (instanceId) {
    const found = await resolveTaskByInstance(instanceId, axis);
    if (found) return found;
  }
  return { task: null, scope: null };
}

export function TaskSpacePanel({
  hostId,
  workspaceId,
  instanceId,
  taskId,
  hostLabel,
  onBack,
}: TaskSpacePanelProps) {
  const [state, setState] = useState<PanelState>({ phase: "loading" });

  const load = useCallback(async () => {
    setState({ phase: "loading" });
    try {
      const resolved = await resolveTask(taskId, instanceId, { hostId, workspaceId });
      setState({ phase: "ready", task: resolved.task, scope: resolved.scope });
    } catch (error) {
      setState({
        phase: "error",
        message: error instanceof Error ? error.message : "load-failed",
      });
    }
  }, [taskId, instanceId, hostId, workspaceId]);

  useEffect(() => {
    void load();
  }, [load]);

  if (state.phase === "loading") {
    return <p data-testid="taskspace-loading" style={{ padding: 16, color: "var(--mute)" }}>正在加载任务空间…</p>;
  }
  if (state.phase === "error") {
    return (
      <div data-testid="taskspace-error" style={{ padding: 16 }}>
        <p style={{ color: "var(--danger-strong)" }}>任务空间加载失败：{state.message}</p>
        <button type="button" onClick={() => void load()}>重试</button>
      </div>
    );
  }

  // No owning task: fall through to the unchanged project-space view.
  if (!state.task || !state.scope) {
    return (
      <FilesView hostId={hostId} workspaceId={workspaceId} hostLabel={hostLabel} onBack={onBack} />
    );
  }

  return (
    <FilesView
      hostId={hostId}
      workspaceId={workspaceId}
      hostLabel={hostLabel}
      onBack={onBack}
      taskFilter={{ label: state.task.title, owns: state.scope.owns }}
    />
  );
}

/**
 * Imperative mount point for standalone harnesses (task 5 wires the panel
 * into the app surface). Returns the unmount callback. Lives in this owned
 * module so the Playwright hub spec can render the real Remuda component
 * from the already-loaded shell via a Vite-graph dynamic import — no lab
 * HTML, no app route, no second entry gets committed.
 */
export function mountTaskSpacePanel(
  container: HTMLElement,
  props: TaskSpacePanelProps,
): () => void {
  const root = createRoot(container);
  root.render(<TaskSpacePanel {...props} />);
  return () => root.unmount();
}
