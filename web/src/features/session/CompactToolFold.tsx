/**
 * The compact process fold: 「N 次工具 · M 段思考」 on one quiet row.
 *
 * Shared by the structured transcript (Transcript.tsx) and the subagent
 * drill-in view (SubagentView.tsx) so both get the same toggle contract:
 * clicking the summary row opens AND closes again, the caret flips
 * (▸ collapsed / ▾ expanded) and aria-expanded always matches. The main
 * transcript additionally bumps `expandTick` when an in-fold search hit must
 * force the row open.
 */
import { useEffect, useState, type ReactNode } from "react";
import sessionCss from "./toolCard.module.css";
import css from "./transcript.module.css";

export function CompactToolFold({
  toolCount,
  thoughtCount,
  expandTick = 0,
  hitChildId = null,
  children,
}: {
  toolCount: number;
  thoughtCount: number;
  /** Bumped to force the fold open (in-transcript search hit). */
  expandTick?: number;
  /** Current search-hit child id, surfaced for row-anchor tests. */
  hitChildId?: string | null;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  // A search hit inside the fold opens it and keeps it open.
  useEffect(() => {
    if (expandTick > 0) setOpen(true);
  }, [expandTick]);
  return (
    <div data-testid="compact-fold-wrap" data-hit-child={hitChildId ?? undefined}>
      <button
        type="button"
        className={sessionCss.fold}
        data-testid="compact-fold"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        <span aria-hidden="true">{open ? "▾" : "▸"}</span>
        <span>{open ? "收起过程" : `${toolCount} 次工具 · ${thoughtCount} 段思考`}</span>
      </button>
      {open ? <div className={css.foldBody}>{children}</div> : null}
    </div>
  );
}
