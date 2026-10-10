import { Link } from "react-router-dom";
import { StateDot } from "../components/StateDot";
import type { EndReason } from "../lib/endReason";
import type { ResumeMode } from "../lib/api";
import ui from "../styles/ui.module.css";
import css from "./endedBar.module.css";

export type EndedBarProps = {
  /** The shared end-reason sentence; only `tone === "failed"` is painted red. */
  reason: EndReason | null;
  /** `lastError: node-epoch-changed` — the suffix carries `node-restart-banner`. */
  nodeRestarted: boolean;
  /** Remuda-held queue rows that never reached the agent. */
  heldCount: number;
  canResume: boolean;
  resuming: boolean;
  onResume: (mode: ResumeMode) => void;
  newHref: string;
};

/**
 * The ended session's one surface in the dock (ui-spec §2.2): it replaces the
 * Composer, the restart banner and the old header resume row, so resume has a
 * single entry point.
 */
export function EndedBar({
  reason,
  nodeRestarted,
  heldCount,
  canResume,
  resuming,
  onResume,
  newHref,
}: EndedBarProps) {
  // A plain ending says nothing beyond 「会话已结束」; any other reason follows
  // it in its own tone (c-endreason: a restart is neutral, never failed).
  const suffix = nodeRestarted || (reason && reason.tone !== "ended") ? reason : null;
  return (
    <section className={css.bar} data-testid="ended-bar" aria-label="会话已结束">
      <div className={css.head}>
        <StateDot status="exited" />
        <span className={css.title}>会话已结束</span>
        {suffix ? (
          <>
            <span className={css.sep} aria-hidden="true">·</span>
            <span
              className={css.reason}
              data-tone={suffix.tone}
              data-testid={nodeRestarted ? "node-restart-banner" : "ended-reason"}
              title={suffix.detail ?? undefined}
            >
              {suffix.label}
            </span>
          </>
        ) : null}
      </div>
      {heldCount > 0 ? (
        <p className={css.held} data-testid="ended-held-note">
          有 {heldCount} 条排队消息未送出
        </p>
      ) : null}
      {canResume ? (
        <div className={css.actions} data-testid="resume-control">
          {/* D-026: resume continues the same native session on a NEW
              instance, so both targets navigate away from this one. */}
          <button
            type="button"
            className={`${ui.btnPrimary} ${css.btnLg}`}
            data-testid={nodeRestarted ? "node-restart-resume" : undefined}
            disabled={resuming}
            onClick={() => onResume("structured")}
          >
            继续（结构化）
          </button>
          <button
            type="button"
            className={`${ui.btn} ${css.btnLg}`}
            data-testid="resume-terminal"
            disabled={resuming}
            onClick={() => onResume("terminal")}
          >
            在终端中继续
          </button>
        </div>
      ) : (
        <div className={css.fallback}>
          <span className={css.note}>该会话没有可续接的 transcript</span>
          <Link to={newHref} className={css.inherit}>
            开新会话继承 cwd
          </Link>
        </div>
      )}
    </section>
  );
}
