import { Link } from "react-router-dom";
import type { Task } from "../../types/generated";
import { TASK_STATE_LABEL } from "./taskRows";
import css from "./tasklist.module.css";

/**
 * Task detail body (plan task-model task 5 acceptance 5, D-050 §9 /
 * ui-spec §2.9). Read-only rendering of the task mandate, title and blocked
 * reason — the surface a later task (t-annotations) anchors its `①` text
 * anchors to. This component owns no write actions and no second transcript:
 * the session links open the shared `/s/:id`.
 */
export type TaskDetailPanelProps = {
  task: Task | null;
  displayKey?: string;
  sessionIds?: readonly string[];
  /** First link target for the task's sessions; null when it has none. */
  primarySessionId?: string | null;
};

export function TaskDetailPanel({
  task,
  displayKey,
  sessionIds = [],
  primarySessionId = null,
}: TaskDetailPanelProps) {
  if (!task) {
    return (
      <aside className={css.detail} data-testid="task-detail-empty" aria-label="任务详情">
        <p className={css.detailEmpty}>选择一个任务查看 mandate 与阻塞原因</p>
      </aside>
    );
  }

  const failed = task.state === "failed";
  const chain = [...(task.mandate?.chain ?? [])].sort((a, b) => a.depth - b.depth);
  const detailSessionId = primarySessionId ?? sessionIds[0] ?? null;

  return (
    <aside className={css.detail} data-testid="task-detail" aria-label="任务详情">
      <header className={css.detailHead}>
        {displayKey ? <span className={css.detailKey}>{displayKey}</span> : null}
        <span
          className={css.detailState}
          data-state={task.state}
          data-failed={failed ? "1" : "0"}
        >
          {TASK_STATE_LABEL[task.state]}
        </span>
      </header>

      <h2 className={css.detailTitle} data-testid="task-detail-title">
        {task.title}
      </h2>

      {task.blockedReason?.trim() ? (
        <p className={css.detailBlocked} data-testid="task-detail-blocked" role="status">
          <span className={css.detailBlockedMark} aria-hidden="true">
            ⚠
          </span>
          {task.blockedReason.trim()}
        </p>
      ) : null}

      {/*
       * The annotation anchor surface (t-annotations): the mandate prose is
       * rendered as stable selectable text nodes under data-anchor-surface.
       */}
      <section
        className={css.detailMandate}
        data-testid="task-detail-mandate"
        data-anchor-surface="task-detail"
      >
        <h3 className={css.detailSectionTitle}>Mandate</h3>
        {chain.length > 0 ? (
          <ol className={css.mandateChain}>
            {chain.map((link) => (
              <li
                key={`${link.depth}:${link.taskId}`}
                className={css.mandateLink}
                data-depth={link.depth}
              >
                <span className={css.mandateTitle}>{link.title}</span>
                <span className={css.mandateIntent}>{link.intent}</span>
              </li>
            ))}
          </ol>
        ) : (
          <p className={css.detailSubtle}>该任务没有继承的 mandate 链。</p>
        )}
      </section>

      <section className={css.detailSessions}>
        <h3 className={css.detailSectionTitle}>
          会话{sessionIds.length > 0 ? ` · ${sessionIds.length}` : ""}
        </h3>
        {detailSessionId ? (
          <Link
            className={css.detailOpenSession}
            to={`/s/${detailSessionId}`}
            data-testid="task-detail-open-session"
          >
            在工作台打开
          </Link>
        ) : (
          <p className={css.detailSubtle}>还没有会话。</p>
        )}
      </section>
    </aside>
  );
}
