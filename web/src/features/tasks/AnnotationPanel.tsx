import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from "react";
import { rest } from "../../lib/api";
import type { components } from "../../lib/api.generated";
import type { Task } from "../../types/generated";
import {
  addAnnotation,
  anchorMark,
  anchorNumber,
  annotationsVersion,
  clearAnnotations,
  createAnchorDraft,
  createCardDraft,
  readAnnotations,
  removeAnnotation,
  subscribeAnnotations,
  ANNOTATION_SURFACE_LABEL,
  type AnnotationAnchor,
  type AnnotationDraft,
} from "./annotations";
import css from "./annotation.module.css";

/**
 * Annotation UI: provider, badge and the 卡片 / 标记 two-tab panel (plan
 * task-model task 9, D-050 §7 / ui-spec §2.9).
 *
 * The provider mounts once in the app Shell so a ① anchor created from the
 * board detail body survives the 在工作台打开 navigation to the session
 * composer — the drafts themselves live in localStorage (see annotations.ts),
 * this context only holds panel open state and React subscriptions.
 */

type PanelState = {
  instanceId: string;
  tab: "card" | "anchor";
  /** Quote queued from the selection affordance, shown until saved. */
  anchor: AnnotationAnchor | null;
};

type AnnotationContextValue = {
  draftsFor: (instanceId: string) => AnnotationDraft[];
  addCard: (
    instanceId: string,
    body: string,
    task?: { id?: string | null; title?: string | null },
  ) => void;
  addAnchor: (instanceId: string, anchor: AnnotationAnchor, body: string) => void;
  remove: (instanceId: string, id: string) => void;
  clear: (instanceId: string) => void;
  panel: PanelState | null;
  openPanel: (instanceId: string, tab?: PanelState["tab"], anchor?: AnnotationAnchor | null) => void;
  closePanel: () => void;
};

const AnnotationContext = createContext<AnnotationContextValue | null>(null);

/**
 * Fallback when SessionPage is mounted without the Shell provider (unit
 * harnesses): drafts stay fully functional against localStorage; panel open
 * state is a no-op — the full panel experience lives under AnnotationProvider.
 */
const fallbackContext: AnnotationContextValue = {
  draftsFor: readAnnotations,
  addCard: (instanceId, body, task) => {
    if (body.trim()) {
      addAnnotation(
        instanceId,
        createCardDraft(body, { taskId: task?.id ?? null, taskTitle: task?.title ?? null }),
      );
    }
  },
  addAnchor: (instanceId, anchor, body) => {
    if (body.trim()) addAnnotation(instanceId, createAnchorDraft(anchor, body));
  },
  remove: removeAnnotation,
  clear: clearAnnotations,
  panel: null,
  openPanel: () => undefined,
  closePanel: () => undefined,
};

export function AnnotationProvider({ children }: { children: ReactNode }) {
  // Bumped on every external (capture/popover) mutation so useDrafts re-reads.
  const [, setTick] = useState(0);
  const [panel, setPanel] = useState<PanelState | null>(null);

  const draftsFor = useCallback((instanceId: string) => readAnnotations(instanceId), []);

  const addCard = useCallback<AnnotationContextValue["addCard"]>(
    (instanceId, body, task) => {
      if (!body.trim()) return;
      addAnnotation(
        instanceId,
        createCardDraft(body, { taskId: task?.id ?? null, taskTitle: task?.title ?? null }),
      );
      setTick((n) => n + 1);
    },
    [],
  );

  const addAnchor = useCallback<AnnotationContextValue["addAnchor"]>(
    (instanceId, anchor, body) => {
      if (!body.trim()) return;
      addAnnotation(instanceId, createAnchorDraft(anchor, body));
      setTick((n) => n + 1);
    },
    [],
  );

  const remove = useCallback<AnnotationContextValue["remove"]>((instanceId, id) => {
    removeAnnotation(instanceId, id);
    setTick((n) => n + 1);
  }, []);

  const clear = useCallback<AnnotationContextValue["clear"]>((instanceId) => {
    clearAnnotations(instanceId);
    setTick((n) => n + 1);
  }, []);

  const openPanel = useCallback<AnnotationContextValue["openPanel"]>(
    (instanceId, tab = "card", anchor = null) => setPanel({ instanceId, tab, anchor }),
    [],
  );
  const closePanel = useCallback(() => setPanel(null), []);

  const value = useMemo<AnnotationContextValue>(
    () => ({ draftsFor, addCard, addAnchor, remove, clear, panel, openPanel, closePanel }),
    [draftsFor, addCard, addAnchor, remove, clear, panel, openPanel, closePanel],
  );

  return <AnnotationContext.Provider value={value}>{children}</AnnotationContext.Provider>;
}

export function useAnnotationsContext(): AnnotationContextValue {
  return useContext(AnnotationContext) ?? fallbackContext;
}

/** Re-render whenever this instance's drafts mutate. */
export function useAnnotationDrafts(instanceId: string | null | undefined): AnnotationDraft[] {
  const ctx = useAnnotationsContext();
  useSyncExternalStore(subscribeAnnotations, () => annotationsVersion(instanceId ?? null));
  return instanceId ? ctx.draftsFor(instanceId) : [];
}

// ── Badge ─────────────────────────────────────────────────────────────────

export function AnnotationBadge({
  instanceId,
  readonly = false,
}: {
  instanceId: string;
  /** Archived-task sessions and terminal segments offer no entry point. */
  readonly?: boolean;
}) {
  const { openPanel, panel, closePanel } = useAnnotationsContext();
  const drafts = useAnnotationDrafts(instanceId);
  const count = drafts.length;
  if (count === 0) return null;

  const open = panel?.instanceId === instanceId;
  // Read-only (archived) sessions may still OPEN the panel to review or
  // remove drafts created before the read-only state resolved; the badge is
  // only inert when there is nothing to open.
  return (
    <button
      type="button"
      className={css.badge}
      data-testid="annotation-badge"
      data-active={open ? "1" : "0"}
      data-readonly={readonly ? "1" : "0"}
      aria-expanded={open}
      title={readonly ? "只读预览：仅可查看或撤回已有批注" : "查看随下一次发送投递的批注"}
      onClick={() => (open ? closePanel() : openPanel(instanceId, "card"))}
    >
      <span aria-hidden="true">批注</span>
      <span className={css.badgeCount} data-testid="annotation-badge-count">
        {count}
      </span>
      <span>本次发送带 {count} 条批注</span>
    </button>
  );
}

// ── Panel with the two carriers as tabs ───────────────────────────────────

export function AnnotationPanel({
  instanceId,
  taskId = null,
  taskTitle = null,
  readonly = false,
}: {
  instanceId: string;
  taskId?: string | null;
  taskTitle?: string | null;
  readonly?: boolean;
}) {
  const ctx = useAnnotationsContext();
  const drafts = useAnnotationDrafts(instanceId);
  const panel = ctx.panel;
  const [body, setBody] = useState("");

  if (!panel || panel.instanceId !== instanceId) return null;
  const tab = panel.tab;

  const cards = drafts.filter((draft) => draft.carrier === "card");
  const anchors = drafts.filter((draft) => draft.carrier === "anchor");
  const queuedAnchor = panel.anchor;

  const saveCard = () => {
    if (!body.trim() || readonly) return;
    ctx.addCard(instanceId, body, { id: taskId, title: taskTitle });
    setBody("");
  };
  const saveAnchor = () => {
    if (!body.trim() || readonly || !queuedAnchor) return;
    ctx.addAnchor(instanceId, queuedAnchor, body);
    setBody("");
    ctx.openPanel(instanceId, "anchor", null);
  };

  return (
    <div className={css.panel} data-testid="annotation-panel" data-readonly={readonly ? "1" : "0"}>
      <div className={css.tabs} role="tablist" aria-label="批注载体">
        <button
          type="button"
          role="tab"
          aria-selected={tab === "card"}
          className={css.tab}
          data-testid="annotation-tab-card"
          data-active={tab === "card" ? "1" : "0"}
          onClick={() => ctx.openPanel(instanceId, "card", null)}
        >
          卡片{cards.length ? ` · ${cards.length}` : ""}
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "anchor"}
          className={css.tab}
          data-testid="annotation-tab-anchor"
          data-active={tab === "anchor" ? "1" : "0"}
          onClick={() => ctx.openPanel(instanceId, "anchor", null)}
        >
          标记{anchors.length ? ` · ${anchors.length}` : ""}
        </button>
        <span className={css.spacer} />
        <button
          type="button"
          className={css.remove}
          data-testid="annotation-panel-close"
          aria-label="关闭批注面板"
          onClick={() => ctx.closePanel()}
        >
          ✕
        </button>
      </div>

      {readonly ? (
        <p className={css.empty} data-testid="annotation-readonly-note">
          只读预览：归档任务的会话不能批注。
        </p>
      ) : null}

      <div className={css.list} data-testid="annotation-list">
        {tab === "card"
          ? cards.map((draft) => (
              <div className={css.item} key={draft.id} data-testid="annotation-item" data-carrier="card">
                <span className={css.itemMark} aria-hidden="true">
                  ▤
                </span>
                <span className={css.itemBody}>
                  <span>{draft.body}</span>
                  {draft.taskTitle ? (
                    <span className={css.itemWhere}>挂在任务：{draft.taskTitle}</span>
                  ) : null}
                </span>
                <button
                  type="button"
                  className={css.remove}
                  data-testid="annotation-item-remove"
                  aria-label="删除批注"
                  // Read-only only blocks creating annotations; removing a
                  // local draft is always allowed (otherwise a draft made
                  // before the archived state resolved would be trapped).
                  onClick={() => ctx.remove(instanceId, draft.id)}
                >
                  ✕
                </button>
              </div>
            ))
          : anchors.map((draft) => (
              <div
                className={css.item}
                key={draft.id}
                data-testid="annotation-item"
                data-carrier="anchor"
              >
                <span className={css.itemMark} aria-hidden="true">
                  {anchorMark(anchorNumber(drafts, draft.id))}
                </span>
                <span className={css.itemBody}>
                  <span>{draft.body}</span>
                  <span className={css.itemQuote}>
                    {ANNOTATION_SURFACE_LABEL[draft.anchor!.surface]}「{draft.anchor!.quote}」
                  </span>
                </span>
                <button
                  type="button"
                  className={css.remove}
                  data-testid="annotation-item-remove"
                  aria-label="删除批注"
                  // Read-only only blocks creating annotations; removing a
                  // local draft is always allowed (otherwise a draft made
                  // before the archived state resolved would be trapped).
                  onClick={() => ctx.remove(instanceId, draft.id)}
                >
                  ✕
                </button>
              </div>
            ))}
        {((tab === "card" && cards.length === 0) || (tab === "anchor" && anchors.length === 0)) ? (
          <p className={css.empty}>
            {tab === "card"
              ? "还没有卡片批注。写一条挂在任务卡上，随下一次发送投递。"
              : "还没有文本标记。选中任务详情或会话记录正文后点 批注。"}
          </p>
        ) : null}
      </div>

      {!readonly && tab === "card" ? (
        <div className={css.form}>
          <textarea
            className={css.textarea}
            data-testid="annotation-card-input"
            value={body}
            onChange={(event) => setBody(event.target.value)}
            placeholder={
              taskTitle ? `对任务「${taskTitle}」的卡片批注…` : "该会话未关联任务，卡片批注将挂在本会话上…"
            }
          />
          <div className={css.actions}>
            <button
              type="button"
              className={css.save}
              data-testid="annotation-card-save"
              data-disabled={body.trim() ? "0" : "1"}
              disabled={!body.trim()}
              onClick={saveCard}
            >
              加入批注
            </button>
          </div>
        </div>
      ) : null}

      {!readonly && tab === "anchor" && queuedAnchor ? (
        <div className={css.form} data-testid="annotation-anchor-form">
          <p className={css.quotePreview}>
            {ANNOTATION_SURFACE_LABEL[queuedAnchor.surface]}「{queuedAnchor.quote}」
          </p>
          <textarea
            className={css.textarea}
            data-testid="annotation-anchor-input"
            value={body}
            autoFocus
            onChange={(event) => setBody(event.target.value)}
            placeholder="对这段正文的批注…"
          />
          <div className={css.actions}>
            <button
              type="button"
              className={css.save}
              data-testid="annotation-anchor-save"
              data-disabled={body.trim() ? "0" : "1"}
              disabled={!body.trim()}
              onClick={saveAnchor}
            >
              加入标记
            </button>
            <button
              type="button"
              className={css.cancel}
              onClick={() => ctx.openPanel(instanceId, "anchor", null)}
            >
              取消
            </button>
          </div>
        </div>
      ) : null}
    </div>
  );
}

// ── Session → task resolution (read-only ledger poll) ────────────────────

type TaskPage = components["schemas"]["TaskPage"];

/**
 * Resolve the task a session belongs to, matching the instance's Hub
 * `taskId` membership first and the task's placement instance otherwise.
 * Read-only (GET /v1/tasks); the annotation card tab needs the title and the
 * archived flag gates the read-only preview. Polls slowly — the relation
 * changes rarely.
 */
export function useSessionTask(
  instanceId: string | null | undefined,
  instanceTaskId?: string | null,
): Task | null {
  const [task, setTask] = useState<Task | null>(null);
  useEffect(() => {
    if (!instanceId) {
      setTask(null);
      return;
    }
    let cancelled = false;
    const tick = async () => {
      try {
        const page = await rest<TaskPage>("/v1/tasks");
        if (cancelled) return;
        const items = (page.items ?? []) as Task[];
        const match =
          (instanceTaskId ? items.find((t) => t.id === instanceTaskId) : undefined) ??
          items.find((t) => t.placement?.instanceId === instanceId) ??
          null;
        setTask(match);
      } catch {
        /* Keep the last known task; the rail stays usable without it. */
      }
    };
    void tick();
    const timer = window.setInterval(tick, 15_000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [instanceId, instanceTaskId]);
  return task;
}
