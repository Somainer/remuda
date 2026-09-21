import { useCallback, useEffect, useMemo, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { rest } from "../../lib/api";
import type { components } from "../../lib/api.generated";
import { HubHttpError } from "../../lib/httpError";
import { useHub } from "../../lib/store";
import { formatListTime } from "../../lib/format";
import { buildSpaces, useSpacesPrefs } from "../spaces/store";
import { HarnessGlyph } from "../spaces/SpacesPanel";
import { TaskGroups, useLiveBranches } from "./TaskList";
import { buildTaskGroups } from "./taskRows";
import { TaskDetailPanel } from "./TaskDetailPanel";
import { boardPath, useProjectFilter } from "./ProjectSwitcher";
import {
  BOARD_WORK_COLUMNS,
  buildBoardModel,
  type BoardCard,
  type BoardView,
  type CardSession,
  type WorkColumn,
} from "./boardModel";
import type { Task } from "../../types/generated";
import listCss from "./tasklist.module.css";
import css from "./board.module.css";

/**
 * Desktop kanban at `/board` (plan task-model task 6 t-board-ui, D-050 §5/§9,
 * ui-spec §2.9). Three work columns consume the server projection
 * `GET /v1/board`; archived is a fold behind a filter, never another column.
 * The same projection feeds the 280px task rail on the left. Drag legality
 * is precomputed per card from the state machine: an unreachable column
 * renders its reason and refuses the drag up front instead of failing a drop
 * with a 4xx. The card detail is an explicit read-only preview — there is no
 * composer on this surface; full control lives in the shared workbench.
 *
 * Compact widths never render this component: the route sits behind the
 * D-049 ViewportGate and collapses onto the `/m` task layer.
 */

type ProjectPage = components["schemas"]["ProjectPage"];

const DRAG_MIME = "text/x-remuda-task";

async function fetchBoard(path: string): Promise<BoardView> {
  return rest<BoardView>(path);
}

/** Polls the board projection for the active project filter. Read-only. */
function useBoardView(projectId: string | null): {
  view: BoardView | null;
  reload: () => Promise<void>;
} {
  const [view, setView] = useState<BoardView | null>(null);
  const path = boardPath(projectId);

  const reload = useCallback(async () => {
    try {
      setView(await fetchBoard(path));
    } catch {
      /* Keep the last good projection; AuthGate handles 401. */
    }
  }, [path]);

  useEffect(() => {
    let cancelled = false;
    setView(null);
    const tick = async () => {
      try {
        const next = await fetchBoard(path);
        if (!cancelled) setView(next);
      } catch {
        /* Stale projection stays on screen while the fetch fails. */
      }
    };
    void tick();
    const timer = window.setInterval(tick, 5_000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [path]);

  return { view, reload };
}

/** Project names for the rail's project groups (ids render otherwise). */
function useProjectNames(): Map<string, string> {
  const [names, setNames] = useState<Map<string, string>>(new Map());
  useEffect(() => {
    let cancelled = false;
    const tick = async () => {
      try {
        const page = await rest<ProjectPage>("/v1/projects");
        if (!cancelled) {
          setNames(
            new Map(
              (page.items ?? [])
                .filter((project) => project.id && project.name)
                .map((project) => [project.id, project.name]),
            ),
          );
        }
      } catch {
        /* Raw project ids are an acceptable degraded rail. */
      }
    };
    void tick();
    const timer = window.setInterval(tick, 30_000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, []);
  return names;
}

function moveErrorMessage(err: unknown): string {
  if (err instanceof HubHttpError) {
    if (err.status === 403) {
      return `没有 Dispatch 授权，Hub 拒绝了拖卡：${err.message}`;
    }
    if (err.status === 409) return `状态机拒绝这次移动：${err.message}`;
    return `拖卡失败（${err.status}）：${err.message}`;
  }
  return err instanceof Error ? `拖卡失败：${err.message}` : "拖卡失败";
}

// ── Card ──────────────────────────────────────────────────────────────────

function SessionLine({ session, onNavigate }: { session: CardSession; onNavigate?: () => void }) {
  return (
    <Link
      className={css.sessionLine}
      to={`/s/${session.id}`}
      data-testid="board-session"
      data-lifecycle={session.lifecycle ?? "unknown"}
      title={`${session.name || session.id} · ${formatListTime(session.updatedAt)}`}
      onClick={(event) => {
        event.stopPropagation();
        onNavigate?.();
      }}
    >
      <HarnessGlyph kind={session.kind ?? "generic"} />
      <span className={css.sessionName}>{session.name || session.id}</span>
      <span className={css.sessionTime}>{formatListTime(session.updatedAt)}</span>
    </Link>
  );
}

function BoardCardView({
  card,
  selected,
  busy,
  onSelect,
  onArchive,
  onDragStart,
  onDragEnd,
}: {
  card: BoardCard;
  selected: boolean;
  busy: boolean;
  onSelect: () => void;
  onArchive: () => void;
  onDragStart: (event: React.DragEvent) => void;
  onDragEnd: () => void;
}) {
  // Terminal (done/failed) and archived cards compute zero legal drops, so
  // they are not draggable at all rather than offering a move every column
  // must refuse.
  const movable =
    !busy && BOARD_WORK_COLUMNS.some((workColumn) => card.drops[workColumn].allowed);
  return (
    <article
      className={`${css.card} ${selected ? css.cardSelected : ""} ${
        card.failed ? css.cardFailed : ""
      } ${card.archived ? css.cardArchived : ""}`}
      data-testid="board-card"
      data-task-id={card.id}
      data-state={card.item.state}
      data-column={card.column}
      data-failed={card.failed ? "1" : "0"}
      data-archived={card.archived ? "1" : "0"}
      draggable={movable}
      onDragStart={movable ? onDragStart : undefined}
      onDragEnd={onDragEnd}
    >
      <header className={css.cardHead}>
        <button
          type="button"
          className={css.cardOpen}
          data-testid="board-card-open"
          onClick={onSelect}
        >
          <span className={css.cardKey}>{card.displayKey}</span>
          <span className={css.cardTitle} title={card.title}>
            {card.title}
          </span>
        </button>
        {!card.archived ? (
          <button
            type="button"
            className={css.cardArchive}
            data-testid="board-card-archive"
            title="归档此任务（不改状态）"
            onClick={(event) => {
              event.stopPropagation();
              onArchive();
            }}
          >
            归档
          </button>
        ) : null}
      </header>

      {card.failed ? (
        <p className={css.failBadge} data-testid="board-failed-badge" role="status">
          <span className={css.failMark} aria-hidden="true">
            ⚠
          </span>
          <span className={css.failText}>失败{card.blockedReason ? ` · ${card.blockedReason}` : ""}</span>
        </p>
      ) : null}

      <div className={css.cardSessions} data-testid="board-card-sessions">
        {card.sessions.length > 0 ? (
          card.sessions.map((session) => <SessionLine key={session.id} session={session} />)
        ) : (
          <p className={css.cardNoSession}>还没有会话</p>
        )}
      </div>

      <footer className={css.cardFoot}>
        <span className={css.configChip} data-testid="board-config" title="当前应用的配置（config-reuse）">
          {card.configLabel}
        </span>
        {card.sharedLabel ? (
          <span className={css.shared} data-testid="board-shared" title="同目录串行轮用（attach 期间独占）">
            {card.sharedLabel}
          </span>
        ) : (
          <span className={css.sharedMute} data-testid="board-shared">
            独占目录
          </span>
        )}
      </footer>
    </article>
  );
}

// ── Column ────────────────────────────────────────────────────────────────

function BoardColumnView({
  column,
  label,
  cards,
  selectedId,
  draggedCard,
  over,
  busy,
  onSelect,
  onArchive,
  onDragCardStart,
  onDragCardEnd,
  onColumnDragOver,
  onColumnDragLeave,
  onColumnDrop,
}: {
  column: WorkColumn;
  label: string;
  cards: BoardCard[];
  selectedId: string | null;
  draggedCard: BoardCard | null;
  over: boolean;
  busy: boolean;
  onSelect: (card: BoardCard) => void;
  onArchive: (card: BoardCard) => void;
  onDragCardStart: (card: BoardCard, event: React.DragEvent) => void;
  onDragCardEnd: () => void;
  onColumnDragOver: (column: WorkColumn, event: React.DragEvent) => void;
  onColumnDragLeave: (column: WorkColumn) => void;
  onColumnDrop: (column: WorkColumn, event: React.DragEvent) => void;
}) {
  const legality = draggedCard?.drops[column] ?? null;
  const blocked = draggedCard != null && legality != null && !legality.allowed;
  return (
    <section
      className={`${css.column} ${over ? css.columnOver : ""} ${blocked ? css.columnBlocked : ""}`}
      data-testid="board-column"
      data-column={column}
      data-drop={draggedCard == null ? undefined : legality?.allowed ? "allowed" : "disabled"}
      onDragOver={(event) => onColumnDragOver(column, event)}
      onDragLeave={() => onColumnDragLeave(column)}
      onDrop={(event) => onColumnDrop(column, event)}
    >
      <header className={css.columnHead}>
        <span className={css.columnTitle}>{label}</span>
        <span className={css.columnCount}>{cards.length}</span>
      </header>
      {blocked && legality?.reason ? (
        <p className={css.dropReason} data-testid="board-drop-reason">
          {legality.reason}
        </p>
      ) : null}
      <div className={css.columnBody}>
        {cards.length === 0 ? <p className={css.columnEmpty}>—</p> : null}
        {cards.map((card) => (
          <BoardCardView
            key={card.id}
            card={card}
            selected={selectedId === card.id}
            busy={busy}
            onSelect={() => onSelect(card)}
            onArchive={() => onArchive(card)}
            onDragStart={(event) => onDragCardStart(card, event)}
            onDragEnd={onDragCardEnd}
          />
        ))}
      </div>
    </section>
  );
}

// ── Page ──────────────────────────────────────────────────────────────────

export function BoardPage() {
  const hub = useHub();
  const prefs = useSpacesPrefs();
  const [params] = useSearchParams();
  // A deep link ?project= wins (ui-spec route table); otherwise the top-bar
  // switcher's device-local selection scopes the projection (task 8).
  const switcherProject = useProjectFilter();
  const projectId = params.get("project") ?? switcherProject;
  const { view, reload } = useBoardView(projectId);
  const projectNames = useProjectNames();

  const [query, setQuery] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [showArchived, setShowArchived] = useState(false);
  const [dragId, setDragId] = useState<string | null>(null);
  const [overColumn, setOverColumn] = useState<WorkColumn | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const items = useMemo<Task[]>(
    () =>
      view
        ? ([
            ...view.columns.todo,
            ...view.columns["in-progress"],
            ...view.columns.done,
            ...view.columns.archived,
          ] as Task[])
        : [],
    [view],
  );

  const spaces = useMemo(
    () => buildSpaces(hub.workspaces, hub.instances, prefs),
    [hub.workspaces, hub.instances, prefs],
  );
  const { branchOf } = useLiveBranches(spaces);

  const model = useMemo(
    () =>
      buildBoardModel({
        view,
        instances: hub.instances as readonly CardSession[],
        query,
      }),
    [view, hub.instances, query],
  );

  const groups = useMemo(
    () =>
      buildTaskGroups({
        tasks: items,
        instances: hub.instances,
        interactions: hub.interactions,
        spaces,
        projectName: (id) => projectNames.get(id),
        branchOfSpace: branchOf,
        query,
      }),
    [items, hub.instances, hub.interactions, spaces, projectNames, branchOf, query],
  );

  // Clear a selection the projection no longer carries.
  useEffect(() => {
    if (selectedId && !model.byId.has(selectedId)) setSelectedId(null);
  }, [model, selectedId]);

  const selectedCard = selectedId ? model.byId.get(selectedId) ?? null : null;
  const selectedItem = (selectedCard?.item ?? null) as Task | null;
  const draggedCard = dragId ? model.byId.get(dragId) ?? null : null;

  const moveCard = useCallback(
    async (card: BoardCard, column: WorkColumn) => {
      const plan = card.drops[column];
      if (!plan.allowed || busyId) return;
      setBusyId(card.id);
      setError(null);
      try {
        for (const state of plan.hops) {
          await rest(`/v1/tasks/${encodeURIComponent(card.id)}`, {
            method: "PATCH",
            body: JSON.stringify({ state }),
          });
        }
        await reload();
      } catch (err) {
        // Grant-verb gating: a caller without Dispatch gets a 403 from the
        // Hub; the board offered the move and renders the refusal here.
        setError(moveErrorMessage(err));
      } finally {
        setBusyId(null);
      }
    },
    [busyId, reload],
  );

  const archiveCard = useCallback(
    async (card: BoardCard) => {
      setError(null);
      try {
        await rest(`/v1/tasks/${encodeURIComponent(card.id)}/archive`, { method: "POST" });
        await reload();
      } catch (err) {
        setError(moveErrorMessage(err));
      }
    },
    [reload],
  );

  const onDragCardStart = (card: BoardCard, event: React.DragEvent) => {
    event.dataTransfer.setData(DRAG_MIME, card.id);
    event.dataTransfer.effectAllowed = "move";
    setError(null);
    setDragId(card.id);
  };

  const onColumnDragOver = (column: WorkColumn, event: React.DragEvent) => {
    const card = dragId ? model.byId.get(dragId) : null;
    if (!card || busyId) return;
    if (card.drops[column].allowed) {
      event.preventDefault();
      event.dataTransfer.dropEffect = "move";
      setOverColumn(column);
    } else {
      event.dataTransfer.dropEffect = "none";
      setOverColumn(null);
    }
  };

  const onColumnDrop = (column: WorkColumn, event: React.DragEvent) => {
    event.preventDefault();
    setOverColumn(null);
    const id = event.dataTransfer.getData(DRAG_MIME);
    setDragId(null);
    const card = id ? model.byId.get(id) : null;
    if (card) void moveCard(card, column);
  };

  return (
    <div className={css.page} data-testid="board-page">
      <div className={listCss.rail} data-testid="task-list">
        <div className={listCss.toolbar}>
          <input
            className={listCss.search}
            type="search"
            data-testid="task-search"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="搜索 Task"
            aria-label="搜索看板任务"
          />
        </div>
        <TaskGroups
          groups={groups}
          variant="desktop"
          selectedId={selectedId}
          onSelect={(task) => setSelectedId(task.id)}
        />
      </div>

      <main className={css.boardMain}>
        <header className={css.boardToolbar}>
          <h1 className={css.boardTitle}>看板</h1>
          <label className={css.archiveToggle}>
            <input
              type="checkbox"
              data-testid="board-archive-toggle"
              checked={showArchived}
              onChange={(event) => setShowArchived(event.target.checked)}
            />
            <span>
              已归档 · {model.archived.length}
            </span>
          </label>
          {error ? (
            <p className={css.moveError} data-testid="board-move-error" role="alert">
              {error}
            </p>
          ) : null}
        </header>

        <div className={css.columns} data-testid="board-columns">
          {BOARD_WORK_COLUMNS.map((column) => (
            <BoardColumnView
              key={column}
              column={column}
              label={model.columns.find((entry) => entry.column === column)?.label ?? ""}
              cards={model.columns.find((entry) => entry.column === column)?.cards ?? []}
              selectedId={selectedId}
              draggedCard={draggedCard}
              over={overColumn === column}
              busy={busyId != null}
              onSelect={(card) => setSelectedId(card.id)}
              onArchive={(card) => void archiveCard(card)}
              onDragCardStart={onDragCardStart}
              onDragCardEnd={() => {
                setDragId(null);
                setOverColumn(null);
              }}
              onColumnDragOver={onColumnDragOver}
              onColumnDragLeave={(current) =>
                setOverColumn((value) => (value === current ? null : value))
              }
              onColumnDrop={onColumnDrop}
            />
          ))}
        </div>

        {showArchived ? (
          <section className={css.archiveFold} data-testid="board-archive-fold">
            <header className={css.archiveHead}>
              <span>已归档（过滤器，不是看板列；归档不改变状态）</span>
              <span className={css.archiveCount}>{model.archived.length}</span>
            </header>
            {model.archived.length === 0 ? (
              <p className={css.archiveEmpty}>暂无已归档任务</p>
            ) : (
              <div className={css.archiveCards}>
                {model.archived.map((card) => (
                  <BoardCardView
                    key={card.id}
                    card={card}
                    selected={selectedId === card.id}
                    busy={busyId != null}
                    onSelect={() => setSelectedId(card.id)}
                    onArchive={() => undefined}
                    onDragStart={() => undefined}
                    onDragEnd={() => undefined}
                  />
                ))}
              </div>
            )}
          </section>
        ) : null}
      </main>

      {selectedItem ? (
        <div className={css.preview}>
          <div className={css.previewBanner} data-testid="board-preview-banner">
            <span className={css.previewMode}>预览模式</span>
            {selectedCard?.primarySessionId ? (
              <Link
                className={css.previewLink}
                to={`/s/${selectedCard.primarySessionId}`}
                data-testid="board-preview-open"
              >
                在工作台打开以完整操作
              </Link>
            ) : (
              <span className={css.previewMute}>启动会话后可在工作台完整操作</span>
            )}
          </div>
          <TaskDetailPanel
            task={selectedItem}
            displayKey={selectedCard?.displayKey}
            sessionIds={selectedCard?.sessions.map((session) => session.id)}
            primarySessionId={selectedCard?.primarySessionId}
          />
        </div>
      ) : null}
    </div>
  );
}
