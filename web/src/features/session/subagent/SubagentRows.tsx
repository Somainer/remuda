/**
 * Folded live tool rows of one subagent, rendered UNDER its parent row.
 *
 * The main transcript keeps only main-agent turns plus the parent rows; here
 * the reader expands a subagent's in-flight tool calls without leaving the
 * session. The full transcript (prompt, model, tokens, final text) lives in
 * the drill-in view the header links to.
 */
import { useState, type ReactNode } from "react";
import { Link, useParams } from "react-router-dom";
import type { SubagentRef } from "../assemble";
import { ToolCard } from "../ToolCard";
import { isToolFailure } from "../assemble";
import sessionCss from "../session.module.css";
import css from "./subagent.module.css";

/** Route for a subagent's full drill-in transcript. */
export function subagentHref(instanceId: string, agentId: string): string {
  return `/s/${encodeURIComponent(instanceId)}/agents/${encodeURIComponent(agentId)}`;
}

function lastToolName(ref: SubagentRef): string | null {
  for (let i = ref.nodes.length - 1; i >= 0; i -= 1) {
    const name = ref.nodes[i].name;
    if (name) return name;
  }
  return null;
}

function DrillInLink({ agentId, children }: { agentId: string; children: ReactNode }) {
  const { instanceId = "" } = useParams();
  return (
    <Link
      className={sessionCss.openBtn}
      data-testid="subagent-open"
      to={subagentHref(instanceId, agentId)}
    >
      {children}
    </Link>
  );
}

/**
 * One subagent fold under a plain Task row.
 *
 * Live-only summary: hook observations carry no token usage, so the strip
 * shows tool count and the last tool; tokens/elapsed live in the drill-in
 * view (parsed from the agent transcript).
 */
export function SubagentFold({
  subagent,
  openChildId,
}: {
  subagent: SubagentRef;
  openChildId?: string | null;
}) {
  const [manualOpen, setManualOpen] = useState(false);
  // A search hit inside one of the folded rows forces the fold open.
  const forceOpen = Boolean(openChildId && subagent.nodes.some((node) => node.id === openChildId));
  const open = manualOpen || forceOpen;
  const calls = subagent.nodes.length;
  const last = lastToolName(subagent);
  return (
    <div className={css.fold} data-testid="subagent-fold" data-agent-id={subagent.agentId}>
      <div className={css.head}>
        <button
          type="button"
          className={css.toggle}
          aria-expanded={open}
          data-testid="subagent-fold-toggle"
          onClick={() => setManualOpen((value) => !value)}
        >
          <span className={css.caret}>{open ? "▾" : "▸"}</span>
          <span>
            {calls} tool calls{last ? ` · ${last}` : ""}
          </span>
        </button>
        <DrillInLink agentId={subagent.agentId}>打开子会话</DrillInLink>
      </div>
      {open ? (
        <div className={css.rows}>
          {subagent.nodes.map((node) =>
            node.result && isToolFailure(node) ? (
              <div key={node.id} className={css.failWrap} data-tool-outcome={node.result.outcome}>
                <ToolCard
                  driverKind={node.driverKind}
                  call={node.call}
                  result={node.result}
                  completeness={node.completeness}
                  diffState={node.diffState}
                />
              </div>
            ) : (
              <ToolCard
                key={node.id}
                driverKind={node.driverKind}
                call={node.call}
                result={node.result}
                completeness={node.completeness}
                diffState={node.diffState}
              />
            ),
          )}
        </div>
      ) : null}
    </div>
  );
}

/** All Task-spawned subagent folds on one parent tool row. */
export function SubagentFolds({ refs, openChildId }: { refs: SubagentRef[]; openChildId?: string | null }) {
  if (!refs.length) return null;
  return (
    <div className={css.folds}>
      {refs.map((subagent) => (
        <SubagentFold key={subagent.agentId} subagent={subagent} openChildId={openChildId} />
      ))}
    </div>
  );
}
