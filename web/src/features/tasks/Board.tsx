import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { rest } from "../../lib/api";
import { HubHttpError } from "../../lib/httpError";
import { useHub } from "../../lib/store";
import { formatListTime } from "../../lib/format";
import { buildSpaces, useSpacesPrefs } from "../spaces/store";
import { HarnessGlyph } from "../spaces/SpacesPanel";
import { TaskGroups, useLiveBranches } from "./TaskList";
import { buildTaskGroups, taskCardSignal } from "./taskRows";
import { TaskDetailPanel } from "./TaskDetailPanel";
import { boardPath, useProjectFilter, useProjects } from "./ProjectSwitcher";
import { PageHeader } from "../../components/PageHeader";
import { CommitProbe } from "../../components/CommitProbe";
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
 * Desktop kanban at `/board` (D-050 §5/§9, ui-spec §2.9, UO-8). Three work
 * columns consume the server projection `GET /v1/board`; archived is a fold
 * behind a filter, never another column. The same projection feeds the task
 * rail on the left, which below 1024px folds into a header-button overlay.
 *
 * Drag legality is precomputed per card from the state machine: an
 * unreachable column renders its reason and refuses the drag up front
 * instead of failing a drop with a 4xx. The card detail is an overlay
 * drawer — an explicit read-only preview with no composer; Esc closes it and
 * returns focus to the card that opened it. Full control lives in the
 * shared workbench `/s/:id`.
 *
 * Compact widths never render this component: the route sits behind the
 * D-049 ViewportGate and collapses onto the `/m` task layer.
 */

const DRAG_MIME = "text/x-remuda-task";
/** The card paints at most two session rows; the rest collapse to a count. */
const CARD_SESSION_ROWS = 2;

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

/**
 * The one signal line (ui-spec §2.9). Failure renders as the badge row
 * above this component; every other card gets the first established signal:
 * 需要你 (amber), the landed sha7 (success), 尚未合入 (muted, with no land
 * entry), or the shared next-step phrase.
 */
function CardSignal({ card }: { card: BoardCard }) {
  const signal = taskCardSignal({
    task: card.item,
    needsHuman: card.needsHuman,
    sessionCount: card.sessionCount,
  });
  if (signal.kind === "failed") return null;
  if (signal.kind === "needs-human") {
    return (
      <p className={`${css.signal} ${css.signalAttention}`} data-testid="board-signal" data-kind="needs-human">
        <span className={css.signalDot} aria-hidden="true" />
        需要你处理
      </p>
    );
  }
  if (signal.kind === "landed") {
    return (
      <p className={`${css.signal} ${css.signalLanded}`} data-testid="board-signal" data-kind="landed">
        <span className={css.signalSha}>{signal.sha7}</span>
        <span>已合入</span>
      </p>
    );
  }
  if (signal.kind === "unlanded") {
    return (
      <p className={`${css.signal} ${css.signalMute}`} data-testid="board-signal" data-kind="unlanded">
        尚未合入
      </p>
    );
  }
  return (
    <p className={`${css.signal} ${css.signalMute}`} data-testid="board-signal" data-kind="next-step">
      {signal.text}
    </p>
  );
}

type CardProps = {
  card: BoardCard;
  selected: boolean;
  busy: boolean;
  onSelect: (id: string) => void;
  onArchive: (id: string) => void;
  onDragStart: (id: string, event: React.DragEvent) => void;
  onDragEnd: () => void;
};

/**
 * Memoized on the card's render signature (boardModel.cardSignature) plus
 * selection/busy: a 5s poll that brings an unchanged projection commits no
 * BoardCard — only cards whose painted inputs changed re-render
 * (`commit:BoardCard`, perf scenario E).
 */
const BoardCardView = memo(
  function BoardCardView({ card, selected, busy, onSelect, onArchive, onDragStart, onDragEnd }: CardProps) {
    // Terminal (done/failed) and archived cards compute zero legal drops, so
    // they are not draggable at all rather than offering a move every column
    // must refuse.
    const movable =
      !busy && BOARD_WORK_COLUMNS.some((workColumn) => card.drops[workColumn].allowed);
    const shownSessions = card.sessions.slice(0, CARD_SESSION_ROWS);
    return (
      <CommitProbe name="BoardCard">
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
          onDragStart={movable ? (event) => onDragStart(card.id, event) : undefined}
          onDragEnd={onDragEnd}
        >
          <header className={css.cardHead}>
            <button
              type="button"
              className={css.cardOpen}
              data-testid="board-card-open"
              onClick={() => onSelect(card.id)}
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
                  onArchive(card.id);
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
              <span className={css.failText}>
                失败{card.blockedReason ? ` · ${card.blockedReason}` : ""}
              </span>
            </p>
          ) : null}
          <CardSignal card={card} />

          <div className={css.cardSessions} data-testid="board-card-sessions">
            {shownSessions.length > 0 ? (
              shownSessions.map((session) => <SessionLine key={session.id} session={session} />)
            ) : (
              <p className={css.cardNoSession}>还没有会话</p>
            )}
            {card.sessions.length > CARD_SESSION_ROWS ? (
              <p className={css.cardMoreSessions}>另有 {card.sessions.length - CARD_SESSION_ROWS} 个会话</p>
            ) : null}
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
      </CommitProbe>
    );
  },
  (prev, next) =>
    prev.card.sig === next.card.sig &&
    prev.selected === next.selected &&
    prev.busy === next.busy &&
    prev.onSelect === next.onSelect &&
    prev.onArchive === next.onArchive &&
    prev.onDragStart === next.onDragStart &&
    prev.onDragEnd === next.onDragEnd,
);

// ── Column ────────────────────────────────────────────────────────────────

const COLUMN_GLYPH: Record<WorkColumn, string> = {
  todo: "○",
  "in-progress": "◐",
  done: "✓",
};

function BoardColumnView({
  column,
  label,
  cards,
  selectedId,
  draggedCard,
  over,
  busyId,
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
  busyId: string | null;
  onSelect: (id: string) => void;
  onArchive: (id: string) => void;
  onDragCardStart: (id: string, event: React.DragEvent) => void;
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
        <span className={css.columnGlyph} aria-hidden="true">
          {COLUMN_GLYPH[column]}
        </span>
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
            busy={busyId === card.id}
            onSelect={onSelect}
            onArchive={onArchive}
            onDragStart={onDragCardStart}
            onDragEnd={onDragCardEnd}
          />
        ))}
      </div>
    </section>
  );
}

const noop = () => undefined;

// ── Page ──────────────────────────────────────────────────────────────────

export function BoardPage() {
  const hub = useHub();
  const prefs = useSpacesPrefs();
  const [params] = useSearchParams();
  // A deep link ?project= wins (ui-spec route table); otherwise the sidebar
  // project rows / switcher write one device-local selection (task 8).
  const switcherProject = useProjectFilter();
  const projectId = params.get("project") ?? switcherProject;
  const { view, reload } = useBoardView(projectId);
  // The Shell already loads the project directory once per mount; reuse it
  // for rail group names instead of polling /v1/projects from this surface.
  const { projects } = useProjects();

  const [query, setQuery] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [showArchived, setShowArchived] = useState(false);
  const [indexOpen, setIndexOpen] = useState(false);
  const [dragId, setDragId] = useState<string | null>(null);
  const [overColumn, setOverColumn] = useState<WorkColumn | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Focus restoration for the preview drawer: the element that opened it.
  const previewTrigger = useRef<HTMLElement | null>(null);
  const drawerRef = useRef<HTMLDivElement | null>(null);

  const projectName = useCallback(
    (id: string) => projects.find((project) => project.id === id)?.name,
    [projects],
  );

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

  // One Set per hub snapshot, shared by the board model and nothing else.
  const pendingByInstance = useMemo(
    () =>
      new Set(
        hub.interactions
          .filter((interaction) => interaction.state === "pending")
          .map((interaction) => interaction.instanceId),
      ),
    [hub.interactions],
  );

  const model = useMemo(
    () =>
      buildBoardModel({
        view,
        instances: hub.instances as readonly CardSession[],
        pendingInstanceIds: pendingByInstance,
        query,
      }),
    [view, hub.instances, pendingByInstance, query],
  );

  const groups = useMemo(
    () =>
      buildTaskGroups({
        tasks: items,
        instances: hub.instances,
        interactions: hub.interactions,
        spaces,
        projectName,
        branchOfSpace: branchOf,
        query,
      }),
    [items, hub.instances, hub.interactions, spaces, projectName, branchOf, query],
  );

  // Clear a selection the projection no longer carries.
  useEffect(() => {
    if (selectedId && !model.byId.has(selectedId)) setSelectedId(null);
  }, [model, selectedId]);

  const selectedCard = selectedId ? model.byId.get(selectedId) ?? null : null;
  const selectedItem = (selectedCard?.item ?? null) as Task | null;
  const draggedCard = dragId ? model.byId.get(dragId) ?? null : null;

  // Latest model for stable event handlers (the card memo must not see a new
  // onArchive identity every 5s poll — that would defeat the sig comparison).
  const modelRef = useRef(model);
  modelRef.current = model;

  const openTask = useCallback((id: string) => {
    previewTrigger.current =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setSelectedId(id);
  }, []);

  const closePreview = useCallback(() => {
    setSelectedId(null);
    // Restore focus to the triggering card after React removes the drawer.
    const trigger = previewTrigger.current;
    if (trigger) window.requestAnimationFrame(() => trigger.focus());
  }, []);

  // Esc closes the preview; focus returns to the triggering card. Esc also
  // dismisses the folded rail overlay below 1024px.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (selectedId) {
        event.preventDefault();
        closePreview();
      } else if (indexOpen) {
        setIndexOpen(false);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [selectedId, indexOpen, closePreview]);

  // Move focus into the drawer while it is open.
  useEffect(() => {
    if (selectedItem) drawerRef.current?.focus();
  }, [selectedItem]);

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
        // Hub; the board offered the move and renders the refusal here. A
        // 409 means the state changed under us — refetch so a partial hop is
        // never shown from a stale column.
        setError(moveErrorMessage(err));
        await reload();
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

  const onCardDragStart = useCallback((id: string, event: React.DragEvent) => {
    event.dataTransfer.setData(DRAG_MIME, id);
    event.dataTransfer.effectAllowed = "move";
    setError(null);
    setDragId(id);
  }, []);

  const onCardDragEnd = useCallback(() => {
    setDragId(null);
    setOverColumn(null);
  }, []);

  const onColumnDragOver = useCallback(
    (column: WorkColumn, event: React.DragEvent) => {
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
    },
    [dragId, model, busyId],
  );

  const onColumnDragLeave = useCallback((column: WorkColumn) => {
    setOverColumn((value) => (value === column ? null : value));
  }, []);

  const onColumnDrop = useCallback(
    (column: WorkColumn, event: React.DragEvent) => {
      event.preventDefault();
      setOverColumn(null);
      const id = event.dataTransfer.getData(DRAG_MIME);
      setDragId(null);
      const card = id ? model.byId.get(id) : null;
      if (card) void moveCard(card, column);
    },
    [model, moveCard],
  );

  const onSelectStable = openTask;
  const onArchiveStable = useCallback(
    (id: string) => {
      const card = modelRef.current.byId.get(id);
      if (card) void archiveCard(card);
    },
    [archiveCard],
  );

  const scopeTitle = projectId ? (projectName(projectId) ?? projectId) : "全局";

  return (
    <div className={css.page} data-testid="board-page">
      {indexOpen ? (
        <div
          className={css.railScrim}
          data-testid="board-index-scrim"
          onClick={() => setIndexOpen(false)}
        />
      ) : null}
      <aside
        className={`${listCss.rail} ${css.rail}`}
        data-testid="task-list"
        data-open={indexOpen ? "1" : undefined}
        aria-label="任务清单"
      >
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
        <TaskGroups groups={groups} variant="desktop" selectedId={selectedId} onSelect={(task) => openTask(task.id)} />
      </aside>

      <main className={css.boardMain}>
        <PageHeader
          testId="board-header"
          crumbs={[{ label: "任务看板", to: "/board" }]}
          title={scopeTitle}
          actions={
            <>
              <span className={css.taskTotal} data-testid="board-total">
                {model.total} 个任务
              </span>
              <label className={css.archiveToggle}>
                <input
                  type="checkbox"
                  data-testid="board-archive-toggle"
                  checked={showArchived}
                  onChange={(event) => setShowArchived(event.target.checked)}
                />
                <span>已归档 · {model.archived.length}</span>
              </label>
              <button
                type="button"
                className={css.indexOpen}
                data-testid="board-index-open"
                aria-expanded={indexOpen}
                onClick={() => setIndexOpen((value) => !value)}
              >
                清单
              </button>
            </>
          }
        />

        {error ? (
          <p className={css.moveError} data-testid="board-move-error" role="alert">
            <span aria-hidden="true">⚠ </span>
            {error}
          </p>
        ) : null}

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
              busyId={busyId}
              onSelect={onSelectStable}
              onArchive={onArchiveStable}
              onDragCardStart={onCardDragStart}
              onDragCardEnd={onCardDragEnd}
              onColumnDragOver={onColumnDragOver}
              onColumnDragLeave={onColumnDragLeave}
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
                    busy={busyId === card.id}
                    onSelect={onSelectStable}
                    onArchive={noop}
                    onDragStart={noop}
                    onDragEnd={onCardDragEnd}
                  />
                ))}
              </div>
            )}
          </section>
        ) : null}

        {selectedItem ? (
          <div className={css.previewScrim} data-testid="board-preview-scrim" onClick={closePreview}>
            <div
              ref={drawerRef}
              className={css.previewDrawer}
              role="dialog"
              aria-label="任务预览"
              tabIndex={-1}
              onClick={(event) => event.stopPropagation()}
            >
              <div className={css.previewBanner} data-testid="board-preview-banner">
                <span className={css.previewMode}>预览模式</span>
                {selectedCard?.primarySessionId ? (
                  <Link
                    className={css.previewLink}
                    to={`/s/${selectedCard.primarySessionId}`}
                    data-testid="board-preview-open"
                  >
                    在工作台打开
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
          </div>
        ) : null}
      </main>
    </div>
  );
}
