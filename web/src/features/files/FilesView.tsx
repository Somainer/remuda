import { useCallback, useEffect, useMemo, useState } from "react";
import {
  fetchChanges,
  fetchEntryDiff,
  fetchEntryFile,
} from "./filesApi";
import {
  formatBytes,
  formatCollectedAt,
  isUntracked,
  kindLabel,
  phaseFromError,
  projectDiff,
  projectFile,
  projectStatus,
  type EntryDetailState,
  type FilesViewModel,
  type ScmEntry,
} from "./filesViewModel";
import css from "./files.module.css";
import { filterTaskEntries } from "../tasks/taskSpaceFilter";

/**
 * The optional task-space projection (D-050 §7, t-taskspace). When supplied,
 * the view gains a 项目空间 / 任务空间 tab pair: both tabs read the same live
 * status/diff/file payload (no new endpoint), and the task tab narrows the
 * rows by the task's `owns[]` globs. Availability states are untouched.
 */
export interface FilesTaskFilter {
  /** Label for the task-space tab, e.g. the task title or its SE-nn key. */
  label: string;
  /** The task's ownership globs as stored on the task ledger. */
  owns: readonly string[];
}

interface FilesViewProps {
  hostId: string;
  workspaceId: string;
  /** Human host label for the subtitle (host name, not attribution). */
  hostLabel?: string;
  /** When set, the file view offers the task-space filter tab. */
  taskFilter?: FilesTaskFilter;
  onBack: () => void;
}

/**
 * Real-time, read-only 「工作区当前变更」 view (files-view-contract §3). It only
 * reads the registered workspace's current git state; the title never implies
 * the changes belong to this session. All loads are manual — no polling.
 */
export function FilesView({ hostId, workspaceId, hostLabel, taskFilter, onBack }: FilesViewProps) {
  const [view, setView] = useState<FilesViewModel>({
    phase: "loading",
    status: null,
    contentChanged: false,
  });
  const [activePath, setActivePath] = useState<string | null>(null);
  const [staged, setStaged] = useState(false);
  const [detail, setDetail] = useState<EntryDetailState>({ kind: "idle" });
  const [detailLoading, setDetailLoading] = useState(false);
  // The project space is the unchanged axis; the task space is its projection.
  // Default stays on the project space so an open file view is byte-similar.
  const [space, setSpace] = useState<"project" | "task">("project");
  const taskActive = taskFilter != null && space === "task";

  const load = useCallback(async () => {
    setView((current) => ({ phase: "loading", status: current.status, contentChanged: false }));
    setActivePath(null);
    setDetail({ kind: "idle" });
    try {
      const status = await fetchChanges(hostId, workspaceId);
      setView(projectStatus(status));
    } catch (error) {
      setView(phaseFromError(error));
    }
  }, [hostId, workspaceId]);

  useEffect(() => {
    void load();
  }, [load]);

  const status = view.status;
  const headOid = status?.headOid ?? null;

  const openEntry = useCallback(
    async (entry: ScmEntry, wantStaged: boolean) => {
      if (!status) return;
      const path = entry.path;
      setActivePath(path);
      setDetailLoading(true);
      setDetail({ kind: "loading" });
      try {
        if (isUntracked(entry)) {
          const file = await fetchEntryFile(hostId, workspaceId, path);
          setDetail(projectFile(entry, status, file));
        } else {
          const diff = await fetchEntryDiff(hostId, workspaceId, path, wantStaged);
          setDetail(projectDiff(entry, status, diff));
        }
      } catch (error) {
        const message = error instanceof Error ? error.message : "load-failed";
        setDetail({ kind: "error", message });
      } finally {
        setDetailLoading(false);
      }
    },
    [hostId, workspaceId, status],
  );

  // §3.7: any stale detail flips the view-level "content changed" flag.
  useEffect(() => {
    const stale =
      (detail.kind === "diff" && detail.changed) ||
      (detail.kind === "preview" && detail.changed);
    if (stale) setView((current) => ({ ...current, contentChanged: true }));
  }, [detail]);

  const collectedAt = status?.observedAt;
  const activeEntry = useMemo(
    () => status?.entries.find((entry) => entry.path === activePath) ?? null,
    [status, activePath],
  );

  // Task-space projection (D-050 §7): the same rows narrowed by the task's
  // owns[] globs. It can legitimately be an empty list — that renders the
  // 「还没有文件」 state, never a synthesised entry.
  const visibleEntries = useMemo(() => {
    if (!status) return [];
    if (!taskActive || !taskFilter) return status.entries;
    return filterTaskEntries(status.entries, { owns: taskFilter.owns });
  }, [status, taskActive, taskFilter]);

  const switchSpace = useCallback((next: "project" | "task") => {
    setSpace(next);
    setActivePath(null);
    setDetail({ kind: "idle" });
  }, []);

  return (
    <div className={css.pane} data-testid="files-pane">
      <button type="button" className={css.back} data-testid="files-back" onClick={onBack}>
        ← 返回会话
      </button>

      <h2 className={css.title} data-testid="files-title">
        工作区当前变更
      </h2>
      <p className={css.subtitle} data-testid="files-subtitle">
        {hostLabel ? `${hostLabel} · ` : ""}
        {status?.root ?? workspaceId}
        {" · 采集于 "}
        {formatCollectedAt(collectedAt)}
      </p>
      <p className={css.note} data-testid="files-attribution">
        这些变更来自该工作区，可能由本会话或同目录的其他会话产生。
      </p>

      {taskFilter ? (
        <div className={css.toolbar} role="tablist" aria-label="空间切换" data-testid="files-space-tabs">
          <button
            type="button"
            role="tab"
            aria-selected={space === "project"}
            className={css.refresh}
            style={space === "project" ? { borderColor: "var(--paper)" } : undefined}
            data-testid="files-space-project"
            onClick={() => switchSpace("project")}
          >
            项目空间
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={space === "task"}
            className={css.refresh}
            style={space === "task" ? { borderColor: "var(--paper)" } : undefined}
            data-testid="files-space-task"
            onClick={() => switchSpace("task")}
          >
            任务空间 · {taskFilter.label}
          </button>
        </div>
      ) : null}

      <div className={css.toolbar}>
        <button
          type="button"
          className={css.refresh}
          data-testid="files-refresh"
          onClick={() => void load()}
        >
          刷新
        </button>
        {headOid ? (
          <span className={css.detailMeta} data-testid="files-head">
            HEAD {headOid.slice(0, 12)}
          </span>
        ) : null}
      </div>

      {view.contentChanged ? (
        <div className={css.changed} role="status" data-testid="files-content-changed">
          <span>内容在采集后变化，请刷新</span>
          <button
            type="button"
            className={css.refresh}
            onClick={() => void load()}
            data-testid="files-content-refresh"
          >
            手动刷新
          </button>
        </div>
      ) : null}

      <Body
        view={view}
        entries={visibleEntries}
        taskActive={taskActive}
        activePath={activePath}
        activeEntry={activeEntry}
        staged={staged}
        detail={detail}
        detailLoading={detailLoading}
        onStaged={(value) => {
          setStaged(value);
          if (activeEntry) void openEntry(activeEntry, value);
        }}
        onOpen={(entry) => void openEntry(entry, staged)}
        onRetry={() => void load()}
      />
    </div>
  );
}

interface BodyProps {
  view: FilesViewModel;
  /** Rows to render: all status rows (project space) or the owns-filtered projection (task space). */
  entries: ScmEntry[];
  /** Rendering the task-space projection; only changes the empty state wording. */
  taskActive: boolean;
  activePath: string | null;
  activeEntry: ScmEntry | null;
  staged: boolean;
  detail: EntryDetailState;
  detailLoading: boolean;
  onStaged: (value: boolean) => void;
  onOpen: (entry: ScmEntry) => void;
  onRetry: () => void;
}

function Body({
  view,
  entries,
  taskActive,
  activePath,
  activeEntry,
  staged,
  detail,
  detailLoading,
  onStaged,
  onOpen,
  onRetry,
}: BodyProps) {
  if (view.phase === "loading") {
    return <p className={css.loading} data-testid="files-loading">正在采集工作区状态…</p>;
  }
  if (view.phase === "not-collected") {
    return <p className={css.loading} data-testid="files-not-collected">尚未采集</p>;
  }
  if (view.phase === "offline") {
    return (
      <StateBox
        testId="files-offline"
        title="离线"
        reason="主机当前没有活动的 Node 连接，无法采集工作区状态。"
      />
    );
  }
  if (view.phase === "missing") {
    return (
      <StateBox
        testId="files-missing"
        title="工作区不存在"
        reason="该工作区未在此 Node 注册，或注册目录已迁移。"
      />
    );
  }
  if (view.phase === "forbidden") {
    return (
      <StateBox
        testId="files-forbidden"
        title="权限不足"
        reason={
          view.reason === "operator"
            ? "需要操作员凭据才能查看工作区变更。"
            : `Node 无法读取该工作区（${view.reason ?? "permission-denied"}）。`
        }
      />
    );
  }
  if (view.phase === "unsupported") {
    return (
      <StateBox
        testId="files-unsupported"
        title="不支持"
        reason={unsupportedReasonText(view.reason)}
      />
    );
  }
  if (view.phase === "failed") {
    return (
      <div className={css.stateBox} data-testid="files-failed">
        <p className={css.stateTitle}>采集失败</p>
        <p className={css.stateReason}>{view.reason ?? "network"}</p>
        <button type="button" className={css.refresh} onClick={onRetry}>
          重试
        </button>
      </div>
    );
  }

  const status = view.status;
  if (!status) return null;

  if (view.phase === "clean") {
    return (
      <div className={css.stateBox} data-testid="files-clean">
        <p className={css.stateTitle}>无变化</p>
        <p className={css.stateReason}>工作区干净，没有已修改或未跟踪的文件。</p>
      </div>
    );
  }

  // Task-space empty projection: the worktree has changes, but none of them
  // lie inside the task's owns[]. The panel says so plainly — it never
  // synthesises rows (D-050 §7).
  if (taskActive && entries.length === 0) {
    return (
      <div className={css.stateBox} data-testid="files-task-empty">
        <p className={css.stateTitle}>还没有文件</p>
        <p className={css.stateReason}>
          该任务的 owns 范围内还没有出现在工作区当前变更里的文件。
        </p>
      </div>
    );
  }

  return (
    <>
      {status.truncated.entries ? (
        <p className={css.trunc} data-testid="files-trunc-entries">
          条目过多：仅显示前 {status.entries.length} 项，另有 {status.truncated.entriesOmitted} 项被省略。
        </p>
      ) : null}
      <ul className={css.list} data-testid="files-list">
        {entries.map((entry) => (
          <li key={`${entry.xy}:${entry.origPath ?? ""}:${entry.path}`}>
            <button
              type="button"
              className={`${css.row} ${activePath === entry.path ? css.rowActive : ""}`}
              data-testid="files-entry"
              data-path={entry.path}
              aria-expanded={activePath === entry.path}
              onClick={() => onOpen(entry)}
            >
              <span className={css.xy}>{entry.xy}</span>
              <span className={css.path}>
                {entry.path}
                {entry.origPath ? (
                  <span className={css.orig}>← {entry.origPath}</span>
                ) : null}
              </span>
              <span className={css.meta}>
                {kindLabel(entry)} · {formatBytes(entry.sizeBytes)}
              </span>
            </button>
          </li>
        ))}
      </ul>

      {activeEntry ? (
        <Detail
          entry={activeEntry}
          staged={staged}
          detail={detail}
          loading={detailLoading}
          onStaged={onStaged}
        />
      ) : null}
    </>
  );
}

function StateBox({
  testId,
  title,
  reason,
}: {
  testId: string;
  title: string;
  reason: string;
}) {
  return (
    <div className={css.stateBox} data-testid={testId}>
      <p className={css.stateTitle}>{title}</p>
      <p className={css.stateReason}>{reason}</p>
    </div>
  );
}

function unsupportedReasonText(reason: string | undefined): string {
  switch (reason) {
    case "not-a-git-repository":
      return "该注册目录不是 git 仓库，无法计算与 HEAD 的差异。";
    case "git-error":
      return "git 无法读取该目录的状态。";
    default:
      return `该工作区不受支持（${reason ?? "unknown"}）。`;
  }
}

interface DetailProps {
  entry: ScmEntry;
  staged: boolean;
  detail: EntryDetailState;
  loading: boolean;
  onStaged: (value: boolean) => void;
}

function Detail({ entry, staged, detail, loading, onStaged }: DetailProps) {
  const showStaged = !isUntracked(entry);
  return (
    <div className={css.detail} data-testid="files-detail">
      <div className={css.detailHead}>
        <p className={css.detailPath}>{entry.path}</p>
        {showStaged ? (
          <label className={css.stagedToggle}>
            <input
              type="checkbox"
              checked={staged}
              onChange={(event) => onStaged(event.target.checked)}
              data-testid="files-staged"
            />
            已暂存
          </label>
        ) : null}
      </div>

      {loading ? <span className={css.detailMeta}>读取中…</span> : null}

      {detail.kind === "binary" ? (
        <StateBox testId="files-detail-binary" title="不支持" reason="二进制文件不提供文本内联。" />
      ) : null}
      {detail.kind === "too-large" ? (
        <p className={css.trunc} data-testid="files-detail-toolarge">
          文件超过内联上限，不返回字节内容（仅提供大小与媒体类型）。
        </p>
      ) : null}
      {detail.kind === "error" ? (
        <p className={css.trunc} data-testid="files-detail-error">
          读取失败：{detail.message}
        </p>
      ) : null}

      {detail.kind === "diff" ? (
        <>
          {detail.item.truncated ? (
            <p className={css.trunc} data-testid="files-diff-trunc">
              diff 超过单请求字节上限，已截断显示。
            </p>
          ) : null}
          <DiffPatch patch={detail.item.patch ?? ""} />
        </>
      ) : null}

      {detail.kind === "preview" ? (
        <>
          <p className={css.digest} data-testid="files-file-digest">
            {detail.file.sizeBytes != null ? `${formatBytes(detail.file.sizeBytes)} · ` : ""}
            {detail.file.digest.state === "known" ? detail.file.digest.value : "摘要不可用"}
          </p>
          <pre className={css.preview} data-testid="files-file-preview">
            {detail.file.content}
          </pre>
        </>
      ) : null}
    </div>
  );
}

/** Render a unified diff with per-line add/del/hunk classes. */
function DiffPatch({ patch }: { patch: string }) {
  const lines = patch.split("\n");
  return (
    <pre className={css.patch} data-testid="files-diff">
      {lines.map((line, index) => {
        const cls = line.startsWith("+++") || line.startsWith("---")
          ? css.hunk
          : line.startsWith("@@")
            ? css.hunk
            : line.startsWith("+")
              ? css.add
              : line.startsWith("-")
                ? css.del
                : undefined;
        return (
          <span key={index} className={`${css.patchLine} ${cls ?? ""}`}>
            {line}
            {"\n"}
          </span>
        );
      })}
    </pre>
  );
}
